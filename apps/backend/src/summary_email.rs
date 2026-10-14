/*!
课堂摘要邮件投递 worker。

这个模块故意与 Notes/Archive/GitHub 主状态机解耦：

1. 只有已经 status=succeeded 的课堂才会进入邮件队列。
2. 投递失败只记录独立的 email retry checkpoint，不改变 Notes 成功状态。
3. backend 只向 ClassFlow Worker 私有端点发送结构化数据；真正的邮件发送由 Worker 上
   的 Cloudflare Email Service binding 完成，因此 N1 不需要 SMTP 密码或第三方 SDK。
*/

use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::{
    app::AppState,
    error::{AppError, AppResult},
    models::LectureNotesRecord,
};

pub fn spawn_summary_email_worker(state: AppState) {
    if !state.config.summary_email_enabled {
        info!("ClassFlow 课堂摘要邮件 worker 未启用");
        return;
    }
    if state.config.summary_email_webhook_url.trim().is_empty() {
        warn!("摘要邮件 worker 已启用但 CLASSFLOW_SUMMARY_EMAIL_WEBHOOK_URL 为空，暂不启动");
        return;
    }

    tokio::spawn(async move {
        let interval =
            Duration::from_secs_f64(state.config.summary_email_poll_interval_secs.max(5.0));
        loop {
            if let Err(error) = run_summary_email_cycle(&state).await {
                warn!("课堂摘要邮件 worker 单轮执行失败: {}", error);
            }
            sleep(interval).await;
        }
    });
}

pub async fn run_summary_email_cycle(state: &AppState) -> AppResult<()> {
    let jobs = state
        .repo
        .list_deliverable_summary_emails(10, state.config.summary_email_max_attempts)
        .await?;
    if jobs.is_empty() {
        return Ok(());
    }

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(AppError::from)?;

    for job in jobs {
        match deliver_summary_email(&client, state, &job).await {
            Ok(message_id) => {
                state
                    .repo
                    .mark_summary_email_sent(&job.lecture_key, message_id.as_deref())
                    .await?;
                info!(
                    "课堂摘要邮件已投递: lecture_key={}, title={}",
                    job.lecture_key,
                    job.notes_title.as_deref().unwrap_or("课堂摘要")
                );
            }
            Err(error) => {
                state
                    .repo
                    .mark_summary_email_delivery_failed(
                        &job.lecture_key,
                        &error.to_string(),
                        state.config.summary_email_max_attempts,
                    )
                    .await?;
                warn!(
                    "课堂摘要邮件投递失败，将独立重试: lecture_key={}, error={}",
                    job.lecture_key, error
                );
            }
        }
    }
    Ok(())
}

async fn deliver_summary_email(
    client: &Client,
    state: &AppState,
    job: &LectureNotesRecord,
) -> AppResult<Option<String>> {
    let summary = job
        .summary_markdown
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::Internal("摘要邮件任务缺少 summary_markdown".to_string()))?;
    let title = job
        .notes_title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("课堂摘要");

    let response = client
        .post(state.config.summary_email_webhook_url.trim())
        .bearer_auth(&state.config.bearer_token)
        .json(&json!({
            "lecture_key": job.lecture_key,
            "course_name": job.course_name,
            "teacher_name": job.teacher_name,
            "date": job.date,
            "start_time": job.start_time,
            "end_time": job.end_time,
            "title": title,
            "summary": summary,
        }))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AppError::Internal(format!(
            "摘要邮件 webhook 返回 HTTP {}: {}",
            status,
            body.chars().take(240).collect::<String>()
        )));
    }
    let parsed = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    Ok(parsed
        .get("message_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned))
}

