/*!
课表驱动的逐堂课完整性巡检。

此前课表只被 Notes 用于确定“下课后可以关闭 lecture”，SmartClass Collector 只扫描
服务器上已经存在的录像。因此“应有课程但根本没找到录像”不会产生 tasks，也不会有告警。

本 worker 每十分钟复查最近两周的已结束课程，超过三小时才发一次有去重键的预警。
它绝不把“本来就没有 SmartClass 录播的课程”武断标记为采集失败；最终结果要同时
满足任务、Notes 和邮件投递确认，不会将 ASR 成功误报为整堂课程完成。
*/

use chrono::{Duration, FixedOffset, NaiveDate, Utc};
use serde_json::json;
use std::time::Duration as StdDuration;
use tracing::{info, warn};

use crate::{
    app::AppState,
    error::{AppError, AppResult},
    repository::{ClassDeliverySnapshot, NewStageEvent},
};

const MONITOR_GRACE_HOURS: i64 = 3;
const MONITOR_LOOKBACK_DAYS: i64 = 14;

/// 把独立阶段状态折叠为可操作的逾期原因；None 才是真正的完整闭环。
///
/// 优先报告上游缺失/失败，避免将“根本没有录像”的课误报为邮件故障。
fn overdue_kind(snapshot: &ClassDeliverySnapshot) -> Option<(&'static str, &'static str)> {
    if snapshot.task_count == 0 {
        return Some((
            "recording_missing",
            "课表已结束，但 ClassFlow 尚未找到 SmartClass 录像；请确认本课是否提供录播",
        ));
    }
    if snapshot.task_failed > 0 {
        return Some(("asr_failed", "已有录像片段处理失败，需要检查任务错误与重试"));
    }
    if snapshot.task_unfinished > 0 {
        return Some((
            "asr_stalled",
            "录像任务尚未全部完成 ASR，处理明显晚于课表结束时间",
        ));
    }
    if snapshot.notes_count == 0 {
        return Some(("notes_missing", "录像已转录，但整堂 Notes 任务尚未创建"));
    }
    if snapshot.notes_failed > 0 {
        return Some(("notes_failed", "整堂 Notes 生成失败，需要处理失败原因"));
    }
    if snapshot.notes_unfinished > 0 {
        return Some(("notes_stalled", "整堂 Notes 尚未生成完毕，正在等待或重试"));
    }
    if snapshot.emails_sent < snapshot.notes_count {
        return Some((
            "email_missing",
            "Notes 已发布，但摘要邮件尚未获得发送接口确认",
        ));
    }
    None
}

/// 独立 worker 在数据库层记录事件；通知配送仍由持久化 outbox 负责。
pub fn spawn_class_monitor(state: AppState) {
    if !state.config.schedule_monitor_enabled {
        info!("课表完整性巡检未启用");
        return;
    }
    tokio::spawn(async move {
        loop {
            if let Err(error) = audit_classes(&state).await {
                warn!("课表完整性巡检失败: {}", error);
            }
            tokio::time::sleep(StdDuration::from_secs(600)).await;
        }
    });
}

async fn audit_classes(state: &AppState) -> AppResult<()> {
    let since = NaiveDate::parse_from_str(&state.config.schedule_monitor_since_date, "%Y-%m-%d")
        .map_err(|error| AppError::Config(format!("课表巡检起始日期格式错误: {error}")))?;
    let shanghai = FixedOffset::east_opt(8 * 3600)
        .ok_or_else(|| AppError::Internal("Asia/Shanghai UTC 偏移不可用".into()))?;
    let now = Utc::now();
    let today = now.with_timezone(&shanghai).date_naive();
    let first_date = since.max(today - Duration::days(MONITOR_LOOKBACK_DAYS));
    let planned = state.course_schedule.planned_classes_between(
        &state.config.default_semester,
        first_date,
        today,
    );
    let mut audited = 0;
    for class in planned {
        // 允许 SmartClass 在课程结束之后上传录像，并容忍短期队列拥塞。
        if now < class.end_utc + Duration::hours(MONITOR_GRACE_HOURS) {
            continue;
        }
        audited += 1;
        let snapshot = state
            .repo
            .class_delivery_snapshot(&class.semester, &class.course_name, &class.date)
            .await?;
        // 录像已存在却少于课程时长估算的分段数时，也应留下独立告警。
        // 这是人工核对线索，不自动重跑 ASR 或取消既有 Notes。
        let incomplete_recording =
            snapshot.task_count > 0 && (snapshot.task_count as usize) < class.expected_segments;
        let kind_and_explanation = if incomplete_recording {
            Some((
                "recording_partial",
                "部分录播片段可能缺失，请按课表节数核对 SmartClass 原始录像",
            ))
        } else {
            overdue_kind(&snapshot)
        };
        let Some((kind, explanation)) = kind_and_explanation else {
            continue;
        };
        // 同一课次、同一异常类型只提醒一次；变化到另一个异常类型可再次说明。
        let event_key = format!(
            "schedule:{}:{}:{}:{}",
            class.semester, class.date, class.course_name, kind,
        );
        let source_id = format!("{}:{}:{}", class.semester, class.date, class.course_name);
        let title = format!("{} · {}", class.date, class.course_name);
        let explanation = format!(
            "{explanation}（课表估算录播片段 {} 段）",
            class.expected_segments
        );
        let message = format!(
            "{}（录播片段 {}、Notes {}、邮件确认 {}）。{}",
            explanation,
            snapshot.task_count,
            snapshot.notes_count,
            snapshot.emails_sent,
            snapshot
                .last_error
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect::<String>(),
        );
        state
            .repo
            .enqueue_stage_event(NewStageEvent {
                event_key: &event_key,
                source_kind: "schedule",
                source_id: &source_id,
                stage: "coverage",
                outcome: "warning",
                title: &title,
                message: &message,
                payload: &json!({
                    "date": class.date,
                    "semester": class.semester,
                    "course_name": class.course_name,
                    "reason": kind,
                    "task_count": snapshot.task_count,
                    "expected_segments_estimate": class.expected_segments,
                    "task_failed": snapshot.task_failed,
                    "notes_count": snapshot.notes_count,
                    "notes_failed": snapshot.notes_failed,
                    "emails_sent": snapshot.emails_sent,
                }),
            })
            .await?;
    }
    info!("课表完整性巡检完成: 已核对 {} 堂逾期课次", audited);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_course_should_warn_without_claiming_smartclass_failure() {
        let snapshot = ClassDeliverySnapshot::default();
        assert_eq!(overdue_kind(&snapshot).unwrap().0, "recording_missing");
    }

    #[test]
    fn should_not_treat_asr_or_notes_success_as_email_success() {
        let mut snapshot = ClassDeliverySnapshot {
            task_count: 2,
            notes_count: 1,
            ..Default::default()
        };
        assert_eq!(overdue_kind(&snapshot).unwrap().0, "email_missing");
        snapshot.emails_sent = 1;
        assert!(overdue_kind(&snapshot).is_none());
        snapshot.notes_unfinished = 1;
        assert_eq!(overdue_kind(&snapshot).unwrap().0, "notes_stalled");
        snapshot.notes_unfinished = 0;
        snapshot.task_failed = 1;
        assert_eq!(overdue_kind(&snapshot).unwrap().0, "asr_failed");
    }

    #[test]
    fn pipeline_stages_have_distinct_overdue_reasons() {
        let mut snapshot = ClassDeliverySnapshot {
            task_count: 3,
            task_unfinished: 1,
            ..Default::default()
        };
        assert_eq!(overdue_kind(&snapshot).unwrap().0, "asr_stalled");
        snapshot.task_unfinished = 0;
        assert_eq!(overdue_kind(&snapshot).unwrap().0, "notes_missing");
        snapshot.notes_count = 1;
        snapshot.notes_failed = 1;
        assert_eq!(overdue_kind(&snapshot).unwrap().0, "notes_failed");
    }
}
