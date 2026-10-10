/*!
持久化流水线通知 outbox。

业务 worker 只负责把关键事件写入 SQLite；本模块只负责异步投递。这样 webhook、
邮件或其他推送渠道临时故障时，下载、ASR、Notes 主链不会被反向拖失败。
*/

use std::time::Duration;

use reqwest::Client;
use serde_json::json;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::{
    app::AppState,
    error::{AppError, AppResult},
    models::StageEventRecord,
};

pub fn spawn_notification_worker(state: AppState) {
    if !state.config.notifications_enabled {
        info!("ClassFlow 通知 worker 未启用");
        return;
    }
    if state.config.notification_webhook_url.trim().is_empty() {
        warn!("通知 worker 已启用但 CLASSFLOW_NOTIFICATION_WEBHOOK_URL 为空，暂不启动投递");
        return;
    }

    tokio::spawn(async move {
        let interval =
            Duration::from_secs_f64(state.config.notification_poll_interval_secs.max(5.0));
        loop {
            if let Err(error) = run_notification_cycle(&state).await {
                warn!("通知 worker 单轮执行失败: {}", error);
            }
            sleep(interval).await;
        }
    });
}

async fn run_notification_cycle(state: &AppState) -> AppResult<()> {
    let events = state.repo.list_deliverable_stage_events(20).await?;
    if events.is_empty() {
        return Ok(());
    }

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(AppError::from)?;

    for event in events {
        match deliver_event(&client, state, &event).await {
            Ok(()) => {
                state.repo.mark_stage_event_sent(event.id).await?;
                info!(
                    "流水线通知已投递: event_key={}, title={}",
                    event.event_key, event.title
                );
            }
            Err(error) => {
                state
                    .repo
                    .mark_stage_event_delivery_failed(
                        event.id,
                        &error.to_string(),
                        state.config.notification_max_attempts,
                    )
                    .await?;
                warn!(
                    "流水线通知投递失败，将按 outbox 策略重试: event_key={}, error={}",
                    event.event_key, error
                );
            }
        }
    }
    Ok(())
}

async fn deliver_event(
    client: &Client,
    state: &AppState,
    event: &StageEventRecord,
) -> AppResult<()> {
    let mut request = client
        .post(state.config.notification_webhook_url.trim())
        .json(&json!({
            "event_key": event.event_key,
            "source_kind": event.source_kind,
            "source_id": event.source_id,
            "stage": event.stage,
            "outcome": event.outcome,
            "title": event.title,
            "message": event.message,
            "payload": event.payload,
            "created_at": event.created_at,
        }));

    if !state.config.notification_webhook_token.trim().is_empty() {
        request = request.bearer_auth(state.config.notification_webhook_token.trim());
    }

    let response = request.send().await?;
    if response.status().is_success() {
        return Ok(());
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(AppError::Internal(format!(
        "通知 webhook 返回 HTTP {}: {}",
        status,
        body.chars().take(180).collect::<String>()
    )))
}

#[cfg(test)]
mod tests {
    use crate::repository::notification_retry_delay_secs;

    #[test]
    fn retry_backoff_should_grow_and_cap_at_one_hour() {
        assert_eq!(notification_retry_delay_secs(1), 30);
        assert_eq!(notification_retry_delay_secs(2), 60);
        assert_eq!(notification_retry_delay_secs(3), 120);
        assert_eq!(notification_retry_delay_secs(20), 3600);
    }
}
