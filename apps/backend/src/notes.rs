/*!
Phase 3 课堂笔记状态机。

N1 只把 Archive Bridge 当作异步黑盒：提交一次拿 CID，随后只读取 Archive canonical
conversation/messages；Notes 完成后再独立发布到 GitHub。任一阶段都以 SQLite checkpoint 为准，
因此 backend / Windows / Archive 重启都不会重复创建同一堂课的 conversation。
*/

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::{Datelike, FixedOffset, NaiveDate, NaiveTime, TimeZone, Timelike, Utc};
use reqwest::{Client, StatusCode, Url, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::{
    app::AppState,
    course::{
        LECTURE_MAX_GAP_MINUTES, LectureGroup, build_lecture_asr_markdown,
        group_tasks_into_lectures,
    },
    error::{AppError, AppResult},
    models::{LectureNotesRecord, LectureNotesStatus},
};

const SHANGHAI_OFFSET_SECS: i32 = 8 * 3600;
const TITLE_MAX_CHARS: usize = 24;
const TITLE_PROMPT_PREFIX: &str =
    "请只为你刚刚生成的整堂课堂笔记起一个简洁、具体、可区分的中文主题名。";
const SUMMARY_MAX_CHARS: usize = 2_000;
/// 摘要协议版本。
///
/// 这个值不仅用于 SQLite 标记，还写进 Archive idempotency key 和 prompt marker。
/// 因此模板升级后，旧摘要可以真正追加一个新的 turn，而不会被旧 submission 误认领。
pub const SUMMARY_PROMPT_VERSION: i64 = 2;
const SUMMARY_PROMPT_PREFIX: &str = "【ClassFlow 摘要模板 v2】请基于你刚刚生成的整堂课堂笔记，再给出一份用于课后邮件快速查看的中文摘要。";

pub fn spawn_notes_worker(state: AppState) {
    if !state.config.notes_enabled {
        info!("Phase 3 Notes worker 未启用");
        return;
    }
    tokio::spawn(async move {
        let interval = Duration::from_secs_f64(state.config.notes_poll_interval_secs.max(2.0));
        loop {
            if let Err(error) = run_notes_cycle(state.clone()).await {
                warn!("Phase 3 Notes worker 本轮失败: {}", error);
            }
            sleep(interval).await;
        }
    });
}

pub async fn run_notes_cycle(state: AppState) -> AppResult<()> {
    discover_ready_lectures(&state).await?;
    let jobs = state.repo.list_active_lecture_notes(24).await?;
    let has_active_jobs = !jobs.is_empty();
    // 历史 backfill 必须严格串行：只要已经有一堂处于 generating / publishing，
    // 本轮就不再打开新的 pending conversation。这样既保护 Archive/ChatGPT 的限流，
    // 也让每堂课的 title-only 与 GitHub 发布先完整收口，再进入下一堂。
    for job in jobs {
        match job.status {
            LectureNotesStatus::Pending => process_pending(&state, &job).await?,
            LectureNotesStatus::Generating => process_generating(&state, &job).await?,
            LectureNotesStatus::Publishing => process_publishing(&state, &job).await?,
            LectureNotesStatus::Succeeded | LectureNotesStatus::Failed => {}
        }
    }
    // 历史摘要永远让位于正常课堂流水线；只有当前没有 pending/generating/publishing
    // Notes 时才推进一条旧 succeeded 记录。真正的 POST 节流仍由 Archive queue 负责。
    if !has_active_jobs {
        process_historical_summary_backfill(&state).await?;
    }
    Ok(())
}

pub async fn enqueue_lecture_notes(
    state: &AppState,
    course_key: &str,
    lecture_key: &str,
) -> AppResult<LectureNotesRecord> {
    let lecture = state
        .repo
        .get_lecture_group(course_key, lecture_key)
        .await?;
    let source_markdown = build_lecture_asr_markdown(&lecture)
        .ok_or_else(|| AppError::BadRequest("完整课堂 ASR 尚未齐备，不能生成 Notes".to_string()))?;
    let transcript_hash = sha256_text(&source_markdown);
    match state.repo.get_lecture_notes(lecture_key).await? {
        Some(existing) => Ok(existing),
        None => {
            state
                .repo
                .ensure_lecture_notes_job(&lecture, &transcript_hash)
                .await
        }
    }
}

pub async fn reconcile_lecture_notes_now(
    state: &AppState,
    lecture_key: &str,
) -> AppResult<LectureNotesRecord> {
    let job = state
        .repo
        .get_lecture_notes(lecture_key)
        .await?
        .ok_or_else(|| AppError::NotFound("Notes 任务不存在".to_string()))?;
    if job.recovery_kind.as_deref() != Some("submission_unknown") {
        return Err(AppError::BadRequest(
            "当前任务不是“提交结果未知”，无需执行 conversation 核对".to_string(),
        ));
    }
    reconcile_unknown_submission(state, &job).await?;
    state
        .repo
        .get_lecture_notes(lecture_key)
        .await?
        .ok_or_else(|| AppError::Internal("核对后无法读回 Notes 任务".to_string()))
}

async fn discover_ready_lectures(state: &AppState) -> AppResult<()> {
    let course_keys = state
        .repo
        .list_course_keys_since(&state.config.notes_auto_since_date)
        .await?;
    let now = Utc::now();
    for course_key in course_keys {
        let tasks = state.repo.list_tasks_by_course_key(&course_key).await?;
        for lecture in group_tasks_into_lectures(&tasks) {
            if !lecture.transcript_ready() || !lecture_is_closed(state, &lecture, now) {
                continue;
            }
            let Some(source_markdown) = build_lecture_asr_markdown(&lecture) else {
                continue;
            };
            state
                .repo
                .ensure_lecture_notes_job(&lecture, &sha256_text(&source_markdown))
                .await?;
        }
    }
    Ok(())
}

pub fn lecture_is_closed(
    state: &AppState,
    lecture: &LectureGroup,
    now: chrono::DateTime<Utc>,
) -> bool {
    if let Some(planned_end) = state.course_schedule.planned_end_utc(
        &lecture.semester,
        &lecture.course_name,
        &lecture.date,
    ) {
        return now
            >= planned_end
                + chrono::Duration::minutes(state.config.notes_schedule_grace_minutes.max(0));
    }

    let Some(offset) = FixedOffset::east_opt(SHANGHAI_OFFSET_SECS) else {
        return false;
    };
    let Ok(date) = NaiveDate::parse_from_str(&lecture.date, "%Y-%m-%d") else {
        return false;
    };
    let Some(end) = parse_clock(&lecture.end_time) else {
        return false;
    };
    let Some(local) = offset
        .with_ymd_and_hms(
            date.year(),
            date.month(),
            date.day(),
            end.hour(),
            end.minute(),
            end.second(),
        )
        .single()
    else {
        return false;
    };
    now >= local.with_timezone(&Utc) + chrono::Duration::minutes(LECTURE_MAX_GAP_MINUTES)
}

fn parse_clock(value: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(value, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(value, "%H:%M:%S"))
        .ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArchiveSubmissionOutcome {
    Queued {
        status: String,
        next_attempt_at: Option<i64>,
        last_error: Option<String>,
    },
    Accepted {
        conversation_id: String,
    },
    Failed {
        error: String,
    },
}

/// RQCI 的正式公共任务结果。消费者不理解 Archive 的消息、SSE 和 ordinal。
/// 未完成只意味着以后再次查询同一个任务 ID，不会产生新的 ChatGPT 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
enum PublicJobState {
    Missing,
    Legacy,
    Pending,
    Completed(String),
    Failed(String),
}

async fn get_public_job_state(
    state: &AppState,
    client: &Client,
    base: &str,
    key: &str,
) -> AppResult<PublicJobState> {
    let response = apply_bridge_auth(
        client.get(format!("{base}/v1/jobs")).query(&[("key", key)]),
        state,
    )
    .send()
    .await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(PublicJobState::Missing);
    }
    if !response.status().is_success() {
        return Err(AppError::Internal(format!(
            "RQCI Job API 查询失败 HTTP {}",
            response.status()
        )));
    }
    let body: Value = response.json().await?;
    let job = body
        .get("job")
        .ok_or_else(|| AppError::Internal("RQCI Job API 缺少 job".to_string()))?;
    if job.get("tracked").and_then(Value::as_bool) == Some(false) {
        return Ok(PublicJobState::Legacy);
    }
    match job
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "completed" => {
            let text = job
                .pointer("/result/text")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| AppError::Internal("RQCI completed job 缺少完整正文".to_string()))?;
            Ok(PublicJobState::Completed(text.to_string()))
        }
        "failed" => Ok(PublicJobState::Failed(
            job.get("error")
                .and_then(Value::as_str)
                .unwrap_or("RQCI job failed")
                .to_string(),
        )),
        "queued" | "submitting" | "retry_waiting" | "accepted" => Ok(PublicJobState::Pending),
        other => Err(AppError::Internal(format!("RQCI Job API 未知状态 {other}"))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryStageMode {
    Live,
    HistoricalBackfill,
}

/// 低速补齐“摘要功能上线前”已经 succeeded 的历史课堂。
///
/// 这条路径最重要的约束是：绝不重做 Notes/标题，也绝不重新进入 GitHub publishing。
/// 旧记录缺少 title_final_ordinal 时，先从原 conversation 的 canonical messages 中恢复
/// 标题边界；随后仍复用正常 summary prompt、幂等 key 和 Archive durable queue。
async fn process_historical_summary_backfill(state: &AppState) -> AppResult<()> {
    if !state.config.summary_backfill_enabled {
        return Ok(());
    }
    let Some(mut job) = state
        .repo
        .get_historical_summary_backfill_candidate(
            &state.config.summary_backfill_since_date,
            &state.config.summary_backfill_only_lecture_key,
            SUMMARY_PROMPT_VERSION,
        )
        .await?
    else {
        return Ok(());
    };

    let Some(conversation_id) = job.conversation_id.clone() else {
        return Ok(());
    };
    let client = bridge_client(state)?;
    let base = state.config.archive_bridge_base_url.trim_end_matches('/');

    if job.title_final_ordinal.is_none() {
        let Some(notes_final_ordinal) = job.notes_final_ordinal else {
            state
                .repo
                .mark_historical_summary_backfill_failed(
                    &job.lecture_key,
                    SUMMARY_PROMPT_VERSION,
                    "历史摘要回填缺少 notes_final_ordinal，无法安全定位标题边界",
                )
                .await?;
            return Ok(());
        };
        let Some(expected_title) = job
            .notes_title
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            state
                .repo
                .mark_historical_summary_backfill_failed(
                    &job.lecture_key,
                    SUMMARY_PROMPT_VERSION,
                    "历史摘要回填缺少已发布标题，无法安全恢复标题 final",
                )
                .await?;
            return Ok(());
        };

        let messages_url = format!("{base}/v1/conversations/{conversation_id}/messages?limit=200");
        let response = match apply_bridge_auth(client.get(messages_url), state)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                state
                    .repo
                    .update_lecture_notes_error(
                        &job.lecture_key,
                        &format!("历史摘要回填读取 canonical conversation 失败: {error}"),
                    )
                    .await?;
                return Ok(());
            }
        };
        if !response.status().is_success() {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!(
                        "历史摘要回填读取 canonical conversation 失败: HTTP {}",
                        response.status()
                    ),
                )
                .await?;
            return Ok(());
        }

        let payload: Value = response.json().await?;
        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(title_final_ordinal) =
            recover_historical_title_final_ordinal(&messages, notes_final_ordinal, expected_title)
        else {
            state
                .repo
                .mark_historical_summary_backfill_failed(
                    &job.lecture_key,
                    SUMMARY_PROMPT_VERSION,
                    "历史摘要回填未能从 canonical messages 恢复已发布标题 final",
                )
                .await?;
            return Ok(());
        };

        state
            .repo
            .mark_historical_summary_title_checkpoint(&job.lecture_key, title_final_ordinal)
            .await?;
        job.title_final_ordinal = Some(title_final_ordinal);
        info!(
            "历史摘要回填恢复标题 checkpoint: lecture_key={}, conversation_id={}, title_final_ordinal={}",
            job.lecture_key, conversation_id, title_final_ordinal
        );
    }

    process_summary_stage(
        state,
        &job,
        &conversation_id,
        &client,
        base,
        SummaryStageMode::HistoricalBackfill,
    )
    .await
}

fn parse_archive_submission_outcome(body: &str) -> AppResult<ArchiveSubmissionOutcome> {
    let payload: Value = serde_json::from_str(body).map_err(|error| {
        AppError::Internal(format!("解析 Archive submission 响应失败: {error}"))
    })?;

    if let Some(job) = payload.get("job") {
        let status = job
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return match status {
            "accepted" | "completed" => {
                let conversation_id = job
                    .get("conversationId")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        AppError::Internal("RQCI 已接收任务但未返回 conversationId".to_string())
                    })?;
                Ok(ArchiveSubmissionOutcome::Accepted {
                    conversation_id: conversation_id.to_string(),
                })
            }
            "failed" => Ok(ArchiveSubmissionOutcome::Failed {
                error: job
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("RQCI job failed")
                    .to_string(),
            }),
            "queued" | "submitting" | "retry_waiting" => Ok(ArchiveSubmissionOutcome::Queued {
                status: status.to_string(),
                next_attempt_at: job.get("nextAttemptAt").and_then(Value::as_i64),
                last_error: job
                    .get("error")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            }),
            other => Err(AppError::Internal(format!("RQCI Job API 未知状态 {other}"))),
        };
    }
    if let Some(submission) = payload.get("submission") {
        let status = submission
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match status {
            "accepted" => {
                let conversation_id = submission
                    .pointer("/result/conversationId")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        AppError::Internal(
                            "Archive submission 已 accepted 但缺少 conversationId".to_string(),
                        )
                    })?;
                return Ok(ArchiveSubmissionOutcome::Accepted {
                    conversation_id: conversation_id.to_string(),
                });
            }
            "failed" => {
                let error = submission
                    .get("lastError")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("Archive submission failed");
                return Ok(ArchiveSubmissionOutcome::Failed {
                    error: error.to_string(),
                });
            }
            "queued" | "processing" | "retry_waiting" => {
                return Ok(ArchiveSubmissionOutcome::Queued {
                    status: status.to_string(),
                    next_attempt_at: submission.get("nextAttemptAt").and_then(Value::as_i64),
                    last_error: submission
                        .get("lastError")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                });
            }
            other => {
                return Err(AppError::Internal(format!(
                    "Archive submission 返回未知状态: {other}"
                )));
            }
        }
    }

    if let Some(conversation_id) = payload
        .pointer("/result/conversationId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(ArchiveSubmissionOutcome::Accepted {
            conversation_id: conversation_id.to_string(),
        });
    }

    Err(AppError::Internal(
        "Archive submission 响应既无 submission 也无 conversationId".to_string(),
    ))
}

async fn get_archive_submission(
    state: &AppState,
    client: &Client,
    base: &str,
    idempotency_key: &str,
) -> Result<Option<ArchiveSubmissionOutcome>, (Option<StatusCode>, String)> {
    let lookup = apply_bridge_auth(
        client
            .get(format!("{base}/v1/submissions"))
            .query(&[("key", idempotency_key)]),
        state,
    )
    .send()
    .await
    .map_err(|error| (None, format!("Archive submission 查询失败: {error}")))?;

    if lookup.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !lookup.status().is_success() {
        let status = lookup.status();
        let body = lookup.text().await.unwrap_or_default();
        return Err((
            Some(status),
            format!(
                "Archive submission 查询失败: HTTP {status} {}",
                truncate(&body, 200)
            ),
        ));
    }

    let body = lookup.text().await.unwrap_or_default();
    parse_archive_submission_outcome(&body)
        .map(Some)
        .map_err(|error| (None, error.to_string()))
}

async fn get_or_enqueue_archive_submission(
    state: &AppState,
    client: &Client,
    base: &str,
    idempotency_key: &str,
    submit_body: Value,
) -> Result<ArchiveSubmissionOutcome, (Option<StatusCode>, String)> {
    if let Some(existing) = get_archive_submission(state, client, base, idempotency_key).await? {
        return Ok(existing);
    }

    let response = apply_bridge_auth(
        client.post(format!("{base}/v1/jobs")).json(&submit_body),
        state,
    )
    .send()
    .await
    .map_err(|error| (None, format!("Archive submission 入队失败: {error}")))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err((
            Some(status),
            format!(
                "Archive submission 入队失败: HTTP {status} {}",
                truncate(&body, 240)
            ),
        ));
    }
    parse_archive_submission_outcome(&body).map_err(|error| (None, error.to_string()))
}
/// 判断当前是否处于 Archive 429 的全局 submission 冷却期。
///
/// 冷却只影响“新建 Notes conversation”。已经拿到 CID 的 generating、以及 GitHub publishing
/// 仍会正常推进，因此限流不会把整个 Phase 3 状态机冻结。
async fn process_pending(state: &AppState, job: &LectureNotesRecord) -> AppResult<()> {
    if job.recovery_kind.as_deref() == Some("submission_unknown") {
        // 历史兼容：旧版 ClassFlow 在没有幂等 submission 时留下的未知提交仍按旧证据链核对。
        // 新提交路径不再创建 submission_unknown。
        reconcile_unknown_submission(state, job).await?;
        return Ok(());
    }
    if let Some(conversation_id) = job.conversation_id.as_deref() {
        state
            .repo
            .mark_lecture_notes_generating(&job.lecture_key, conversation_id)
            .await?;
        return Ok(());
    }

    let lecture = state
        .repo
        .get_lecture_group(&job.course_key, &job.lecture_key)
        .await?;
    let source_markdown = build_lecture_asr_markdown(&lecture)
        .ok_or_else(|| AppError::BadRequest("Notes 提交时完整课堂 ASR 已不可用".to_string()))?;
    let client = bridge_client(state)?;
    let base = state.config.archive_bridge_base_url.trim_end_matches('/');
    let idempotency_key = format!("classflow:lecture:{}:notes", job.lecture_key);
    let outcome = get_or_enqueue_archive_submission(
        state,
        &client,
        base,
        &idempotency_key,
        json!({
            "mode": "new",
            "targetMode": "guardian",
            "projectName": state.config.notes_project_name,
            "idempotencyKey": idempotency_key,
            "text": source_markdown,
        }),
    )
    .await;

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err((status, message)) => {
            let fatal_client_error = status.is_some_and(|value| {
                value.is_client_error() && value != StatusCode::TOO_MANY_REQUESTS
            });
            if fatal_client_error {
                state
                    .repo
                    .mark_lecture_notes_failed_with_kind(
                        &job.lecture_key,
                        &message,
                        "manual_review",
                    )
                    .await?;
            } else {
                state
                    .repo
                    .mark_lecture_notes_pending_error(&job.lecture_key, &message)
                    .await?;
            }
            return Ok(());
        }
    };

    match outcome {
        ArchiveSubmissionOutcome::Queued {
            status,
            next_attempt_at,
            last_error,
        } => {
            let retry = next_attempt_at
                .map(|value| format!("，nextAttemptAt={value}"))
                .unwrap_or_default();
            let detail = last_error
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("，{value}"))
                .unwrap_or_default();
            state
                .repo
                .mark_lecture_notes_pending_error(
                    &job.lecture_key,
                    &format!("Archive 已接管 Notes submission：{status}{retry}{detail}"),
                )
                .await?;
        }
        ArchiveSubmissionOutcome::Accepted { conversation_id } => {
            state
                .repo
                .mark_lecture_notes_generating(&job.lecture_key, &conversation_id)
                .await?;
            info!(
                "Phase 3 Notes submission 已由 Archive 接受: lecture_key={}, conversation_id={}",
                job.lecture_key, conversation_id
            );
        }
        ArchiveSubmissionOutcome::Failed { error } => {
            state
                .repo
                .mark_lecture_notes_failed_with_kind(
                    &job.lecture_key,
                    &format!("Archive submission 终止: {error}"),
                    "manual_review",
                )
                .await?;
        }
    }
    Ok(())
}
async fn reconcile_unknown_submission(state: &AppState, job: &LectureNotesRecord) -> AppResult<()> {
    let client = bridge_client(state)?;
    let base = state.config.archive_bridge_base_url.trim_end_matches('/');
    let response = match apply_bridge_auth(
        client
            .get(format!("{base}/v1/search"))
            .query(&[("q", job.lecture_key.as_str()), ("limit", "20")]),
        state,
    )
    .send()
    .await
    {
        Ok(response) => response,
        Err(error) => {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!("提交结果未知；Archive 搜索暂不可达，稍后继续核对: {error}"),
                )
                .await?;
            return Ok(());
        }
    };
    if !response.status().is_success() {
        state
            .repo
            .update_lecture_notes_error(
                &job.lecture_key,
                &format!(
                    "提交结果未知；Archive 搜索返回 HTTP {}，稍后继续核对",
                    response.status()
                ),
            )
            .await?;
        return Ok(());
    }
    let payload: Value = response.json().await?;
    let mut candidates = payload
        .get("hits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|hit| hit.get("role").and_then(Value::as_str) == Some("user"))
        .filter_map(|hit| hit.get("conversationId").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();

    let marker = format!("Lecture Key：{}", job.lecture_key);
    let mut verified = Vec::new();
    for conversation_id in candidates {
        let messages = match apply_bridge_auth(
            client.get(format!(
                "{base}/v1/conversations/{}/messages?limit=80",
                urlencoding::encode(&conversation_id)
            )),
            state,
        )
        .send()
        .await
        {
            Ok(response) if response.status().is_success() => response.json::<Value>().await?,
            _ => continue,
        };
        let exact_match = messages
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|message| {
                message.get("role").and_then(Value::as_str) == Some("user")
                    && message
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.contains(&marker))
            });
        if exact_match {
            verified.push(conversation_id);
        }
    }

    match verified.as_slice() {
        [conversation_id] => {
            state
                .repo
                .mark_lecture_notes_generating(&job.lecture_key, conversation_id)
                .await?;
            info!(
                "Phase 3 Notes 已从提交结果未知状态认领 conversation: lecture_key={}, conversation_id={}",
                job.lecture_key, conversation_id
            );
        }
        [] => {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    "提交结果未知；Archive 暂未找到带相同 Lecture Key 的用户消息，将继续异步核对。若人工确认不存在，可在前端强制重建 conversation。",
                )
                .await?;
        }
        _ => {
            state
                .repo
                .mark_lecture_notes_failed_with_kind(
                    &job.lecture_key,
                    "Archive 找到多个带同一 Lecture Key 的 conversation，无法自动判定，应人工选择后再继续",
                    "manual_review",
                )
                .await?;
        }
    }
    Ok(())
}

async fn process_generating(state: &AppState, job: &LectureNotesRecord) -> AppResult<()> {
    let Some(conversation_id) = job.conversation_id.as_deref() else {
        state
            .repo
            .mark_lecture_notes_failed(&job.lecture_key, "generating 状态缺少 conversation_id")
            .await?;
        return Ok(());
    };
    let client = bridge_client(state)?;
    let base = state.config.archive_bridge_base_url.trim_end_matches('/');
    if job
        .notes_markdown
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        if job
            .notes_title
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            return process_title_stage(state, job, conversation_id, &client, base).await;
        }
        if job
            .summary_markdown
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            return process_summary_stage(
                state,
                job,
                conversation_id,
                &client,
                base,
                SummaryStageMode::Live,
            )
            .await;
        }
        // 兼容极窄的崩溃窗口：摘要已落库但 status 仍意外保持 generating 时，
        // 下一轮直接把同一份摘要重新 checkpoint 为 publishing，不重发任何 ChatGPT turn。
        if let Some(summary_final_ordinal) = job.summary_final_ordinal {
            state
                .repo
                .mark_lecture_notes_summary_ready(
                    &job.lecture_key,
                    job.summary_markdown.as_deref().unwrap_or_default(),
                    summary_final_ordinal,
                )
                .await?;
        }
        return Ok(());
    }

    // 新任务只从 RQCI 对外 Job API 取最终正文，不再检查 Archive 内部 lifecycle/message。
    // 旧队列的未追踪记录仍可临时沿用下方历史兼容分支，避免发布切换时丢失任务。
    let job_key = format!("classflow:lecture:{}:notes", job.lecture_key);
    match get_public_job_state(state, &client, base, &job_key).await? {
        PublicJobState::Completed(text) => {
            let normalized = normalize_notes_markdown(&text);
            let markdown = normalized.markdown.trim();
            if markdown.is_empty() {
                state
                    .repo
                    .mark_lecture_notes_failed(&job.lecture_key, "RQCI 最终笔记正文为空")
                    .await?;
                return Ok(());
            }
            // 0 是新式公共任务检查点的哨兵：此后 title/summary 用稳定 Job Key
            // 关联结果，而不需要将 Archive 的内部 ordinal 透传给 ClassFlow。
            state
                .repo
                .mark_lecture_notes_notes_ready(
                    &job.lecture_key,
                    markdown,
                    &sha256_text(markdown),
                    0,
                )
                .await?;
            return Ok(());
        }
        PublicJobState::Pending => return Ok(()),
        PublicJobState::Failed(error) => {
            state
                .repo
                .mark_lecture_notes_failed(&job.lecture_key, &error)
                .await?;
            return Ok(());
        }
        PublicJobState::Legacy | PublicJobState::Missing => {}
    }

    let detail_url = format!("{base}/v1/conversations/{conversation_id}");
    let response = match apply_bridge_auth(client.get(detail_url), state)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!("等待 Archive 恢复: {error}"),
                )
                .await?;
            return Ok(());
        }
    };
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(());
    }
    if !response.status().is_success() {
        state
            .repo
            .update_lecture_notes_error(
                &job.lecture_key,
                &format!("读取 Archive 状态失败: HTTP {}", response.status()),
            )
            .await?;
        return Ok(());
    }
    let detail: Value = response.json().await?;
    let lifecycle = detail
        .pointer("/conversation/lifecycleState")
        .and_then(Value::as_str);
    if lifecycle == Some("aborted") {
        let reason = detail
            .pointer("/incidents/0/reason")
            .and_then(Value::as_str)
            .unwrap_or("Archive 报告异常终止");
        state
            .repo
            .mark_lecture_notes_failed(&job.lecture_key, reason)
            .await?;
        return Ok(());
    }
    if lifecycle != Some("completed") {
        return Ok(());
    }

    let messages_url = format!("{base}/v1/conversations/{conversation_id}/messages?limit=200");
    let messages_response = match apply_bridge_auth(client.get(messages_url), state)
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return Ok(()),
    };
    if !messages_response.status().is_success() {
        return Ok(());
    }
    let payload: Value = messages_response.json().await?;
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let final_message = messages.iter().rev().find(|message| {
        message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
            && !message
                .get("reasoningRedacted")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && assistant_final_is_complete(message)
    });
    let Some(final_message) = final_message else {
        return Ok(());
    };
    let raw_markdown = final_message
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if raw_markdown.is_empty() {
        return Ok(());
    }
    let normalized = normalize_notes_markdown(raw_markdown);
    let markdown = normalized.markdown.trim();
    if markdown.is_empty() {
        return Ok(());
    }
    let final_ordinal = final_message
        .get("ordinal")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            AppError::Internal("Archive final 缺少 ordinal，无法安全进入标题阶段".to_string())
        })?;
    state
        .repo
        .mark_lecture_notes_notes_ready(
            &job.lecture_key,
            markdown,
            &sha256_text(markdown),
            final_ordinal,
        )
        .await?;
    Ok(())
}

/// 新版 Title：仅处理 RQCI 的持久化 Job 输出。需要重试时使用另一枚稳定 key，
/// 不回溯 Archive 的 message ordinal，也不会重复触发已执行的 ChatGPT turn。
async fn process_public_title_result(
    state: &AppState,
    job: &LectureNotesRecord,
    conversation_id: &str,
    client: &Client,
    base: &str,
    initial_key: &str,
    existing_titles: &[String],
) -> AppResult<()> {
    let text = match get_public_job_state(state, client, base, initial_key).await? {
        PublicJobState::Completed(text) => text,
        PublicJobState::Failed(error) => {
            state
                .repo
                .mark_lecture_notes_failed(&job.lecture_key, &error)
                .await?;
            return Ok(());
        }
        PublicJobState::Pending => return Ok(()),
        PublicJobState::Legacy | PublicJobState::Missing => {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    "新的 RQCI Title Job 没有可追踪的完成状态",
                )
                .await?;
            return Ok(());
        }
    };
    let title = normalize_title_response(&text);
    let final_title = if let Some(reason) = title_rejection_reason(&title, existing_titles) {
        let retry_key = format!("classflow:lecture:{}:title:0:retry:0", job.lecture_key);
        match get_public_job_state(state, client, base, &retry_key).await? {
            PublicJobState::Completed(text) => normalize_title_response(&text),
            PublicJobState::Failed(error) => {
                state
                    .repo
                    .mark_lecture_notes_failed(&job.lecture_key, &error)
                    .await?;
                return Ok(());
            }
            PublicJobState::Pending => return Ok(()),
            PublicJobState::Legacy => {
                state
                    .repo
                    .mark_lecture_notes_failed(&job.lecture_key, "标题重试任务未启用完整结果跟踪")
                    .await?;
                return Ok(());
            }
            PublicJobState::Missing => {
                let prompt = build_title_prompt(existing_titles, Some((&title, reason)));
                match get_or_enqueue_archive_submission(
                    state,
                    client,
                    base,
                    &retry_key,
                    json!({
                        "mode": "continue",
                        "conversationId": conversation_id,
                        "idempotencyKey": retry_key,
                        "text": prompt,
                    }),
                )
                .await
                {
                    Ok(ArchiveSubmissionOutcome::Failed { error }) => {
                        state
                            .repo
                            .mark_lecture_notes_failed(&job.lecture_key, &error)
                            .await?;
                    }
                    Ok(_) => {
                        state
                            .repo
                            .update_lecture_notes_error(
                                &job.lecture_key,
                                "标题格式不合格，RQCI 已接管一次修正请求",
                            )
                            .await?;
                    }
                    Err((_, message)) => {
                        state
                            .repo
                            .update_lecture_notes_error(&job.lecture_key, &message)
                            .await?;
                    }
                }
                return Ok(());
            }
        }
    } else {
        title
    };
    if let Some(reason) = title_rejection_reason(&final_title, existing_titles) {
        state
            .repo
            .mark_lecture_notes_failed(&job.lecture_key, &format!("标题一次重试仍不合格: {reason}"))
            .await?;
        return Ok(());
    }
    state
        .repo
        .mark_lecture_notes_title_ready(&job.lecture_key, &final_title, 0)
        .await?;
    Ok(())
}

async fn process_title_stage(
    state: &AppState,
    job: &LectureNotesRecord,
    conversation_id: &str,
    client: &Client,
    base: &str,
) -> AppResult<()> {
    let notes_final_ordinal = job
        .notes_final_ordinal
        .ok_or_else(|| AppError::Internal("标题阶段缺少 notes_final_ordinal".to_string()))?;

    let existing_titles = state
        .repo
        .list_lecture_note_titles_for_course(&job.course_name, &job.lecture_key)
        .await?;
    let initial_submission_key = format!(
        "classflow:lecture:{}:title:{}:initial",
        job.lecture_key, notes_final_ordinal
    );

    // 新式任务从 Notes 阶段传来 ordinal=0 哨兵：不再主动翻 Archive 消息。
    if notes_final_ordinal == 0 {
        if job.title_requested_at.is_none() {
            let outcome = get_or_enqueue_archive_submission(
                state,
                client,
                base,
                &initial_submission_key,
                json!({
                    "mode": "continue",
                    "conversationId": conversation_id,
                    "idempotencyKey": initial_submission_key,
                    "text": build_title_prompt(&existing_titles, None),
                }),
            )
            .await;
            match outcome {
                Ok(ArchiveSubmissionOutcome::Failed { error }) => {
                    state
                        .repo
                        .mark_lecture_notes_failed(&job.lecture_key, &error)
                        .await?;
                }
                Ok(_) => {
                    state
                        .repo
                        .mark_lecture_notes_title_requested(&job.lecture_key)
                        .await?;
                }
                Err((_, message)) => {
                    state
                        .repo
                        .update_lecture_notes_error(&job.lecture_key, &message)
                        .await?;
                }
            }
            return Ok(());
        }
        return process_public_title_result(
            state,
            job,
            conversation_id,
            client,
            base,
            &initial_submission_key,
            &existing_titles,
        )
        .await;
    }

    if job.title_requested_at.is_none() {
        // 历史兼容：升级 submission queue 之前，title turn 可能已经真正进入 canonical，
        // 但 ClassFlow 没来得及 checkpoint。先查 canonical 可以避免给旧 conversation 再发一遍。
        let messages_url = format!("{base}/v1/conversations/{conversation_id}/messages?limit=200");
        let response = match apply_bridge_auth(client.get(messages_url), state)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                state
                    .repo
                    .update_lecture_notes_error(
                        &job.lecture_key,
                        &format!("标题提交前读取 canonical conversation 失败: {error}"),
                    )
                    .await?;
                return Ok(());
            }
        };
        if !response.status().is_success() {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!(
                        "标题提交前读取 canonical conversation 失败: HTTP {}",
                        response.status()
                    ),
                )
                .await?;
            return Ok(());
        }
        let payload: Value = response.json().await?;
        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if find_existing_title_prompt_ordinal(&messages, notes_final_ordinal).is_some() {
            state
                .repo
                .mark_lecture_notes_title_requested(&job.lecture_key)
                .await?;
            info!(
                "Phase 3 Notes 已认领 canonical title 请求: lecture_key={}, conversation_id={}",
                job.lecture_key, conversation_id
            );
            return Ok(());
        }

        let title_prompt = build_title_prompt(&existing_titles, None);
        let outcome = get_or_enqueue_archive_submission(
            state,
            client,
            base,
            &initial_submission_key,
            json!({
                "mode": "continue",
                "conversationId": conversation_id,
                "idempotencyKey": initial_submission_key,
                "text": title_prompt,
            }),
        )
        .await;

        match outcome {
            Ok(ArchiveSubmissionOutcome::Queued {
                status,
                next_attempt_at,
                last_error,
            }) => {
                // Archive 已把请求持久化，因此从这里起 ClassFlow 不需要再次重建 prompt。
                state
                    .repo
                    .mark_lecture_notes_title_requested(&job.lecture_key)
                    .await?;
                let retry = next_attempt_at
                    .map(|value| format!("，nextAttemptAt={value}"))
                    .unwrap_or_default();
                let detail = last_error
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| format!("，{value}"))
                    .unwrap_or_default();
                state
                    .repo
                    .update_lecture_notes_error(
                        &job.lecture_key,
                        &format!("Archive 已接管标题 submission：{status}{retry}{detail}"),
                    )
                    .await?;
            }
            Ok(ArchiveSubmissionOutcome::Accepted {
                conversation_id: returned_cid,
            }) => {
                if returned_cid != conversation_id {
                    state
                        .repo
                        .mark_lecture_notes_failed(
                            &job.lecture_key,
                            "标题 submission 没有继续原 conversation，停止自动处理",
                        )
                        .await?;
                    return Ok(());
                }
                state
                    .repo
                    .mark_lecture_notes_title_requested(&job.lecture_key)
                    .await?;
                info!(
                    "Phase 3 Notes 标题 submission 已由 Archive 接受: lecture_key={}, conversation_id={}",
                    job.lecture_key, conversation_id
                );
            }
            Ok(ArchiveSubmissionOutcome::Failed { error }) => {
                state
                    .repo
                    .mark_lecture_notes_failed(
                        &job.lecture_key,
                        &format!("Archive 标题 submission 终止: {error}"),
                    )
                    .await?;
            }
            Err((status, message)) => {
                let fatal_client_error = status.is_some_and(|value| {
                    value.is_client_error() && value != StatusCode::TOO_MANY_REQUESTS
                });
                if fatal_client_error {
                    state
                        .repo
                        .mark_lecture_notes_failed(&job.lecture_key, &message)
                        .await?;
                } else {
                    state
                        .repo
                        .update_lecture_notes_error(&job.lecture_key, &message)
                        .await?;
                }
            }
        }
        return Ok(());
    }

    // title_requested_at 表示请求已经由 Archive 持久化接管（或是历史 canonical 已证实存在）。
    // 对新队列任务先只读 submission 状态；retry_waiting 不需要 ClassFlow 自己计时或重发。
    match get_archive_submission(state, client, base, &initial_submission_key).await {
        Ok(Some(ArchiveSubmissionOutcome::Queued {
            status,
            next_attempt_at,
            last_error,
        })) => {
            let retry = next_attempt_at
                .map(|value| format!("，nextAttemptAt={value}"))
                .unwrap_or_default();
            let detail = last_error
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("，{value}"))
                .unwrap_or_default();
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!("Archive 标题 submission 尚未正式发送：{status}{retry}{detail}"),
                )
                .await?;
            return Ok(());
        }
        Ok(Some(ArchiveSubmissionOutcome::Failed { error })) => {
            state
                .repo
                .mark_lecture_notes_failed(
                    &job.lecture_key,
                    &format!("Archive 标题 submission 终止: {error}"),
                )
                .await?;
            return Ok(());
        }
        Ok(Some(ArchiveSubmissionOutcome::Accepted {
            conversation_id: returned_cid,
        })) => {
            if returned_cid != conversation_id {
                state
                    .repo
                    .mark_lecture_notes_failed(
                        &job.lecture_key,
                        "标题 submission 返回了不同 conversationId",
                    )
                    .await?;
                return Ok(());
            }
        }
        Ok(None) => {
            // 旧版同步 Bridge 没有 submission 记录；直接按 canonical messages 继续。
        }
        Err((_status, message)) => {
            state
                .repo
                .update_lecture_notes_error(&job.lecture_key, &message)
                .await?;
            return Ok(());
        }
    }

    let messages_url = format!("{base}/v1/conversations/{conversation_id}/messages?limit=200");
    let response = match apply_bridge_auth(client.get(messages_url), state)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!("等待标题 final 时 Archive 暂不可达: {error}"),
                )
                .await?;
            return Ok(());
        }
    };
    if !response.status().is_success() {
        return Ok(());
    }
    let payload: Value = response.json().await?;
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let title_message = messages.iter().rev().find(|message| {
        message
            .get("ordinal")
            .and_then(Value::as_i64)
            .unwrap_or_default()
            > notes_final_ordinal
            && message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
            && !message
                .get("reasoningRedacted")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && assistant_final_is_complete(message)
    });
    let Some(title_message) = title_message else {
        return Ok(());
    };
    let title_message_ordinal = title_message
        .get("ordinal")
        .and_then(Value::as_i64)
        .unwrap_or(notes_final_ordinal + 1);
    let title = normalize_title_response(
        title_message
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let rejection = title_rejection_reason(&title, &existing_titles);
    if let Some(reason) = rejection {
        let marker = format!("标题重做请求已托管: {title}");
        let retry_submission_key = format!(
            "classflow:lecture:{}:title:{}:retry:{}",
            job.lecture_key, notes_final_ordinal, title_message_ordinal
        );

        if job.last_error.as_deref() == Some(marker.as_str()) {
            match get_archive_submission(state, client, base, &retry_submission_key).await {
                Ok(Some(ArchiveSubmissionOutcome::Failed { error })) => {
                    state
                        .repo
                        .mark_lecture_notes_failed(
                            &job.lecture_key,
                            &format!("Archive 标题重做 submission 终止: {error}"),
                        )
                        .await?;
                }
                Ok(_) => {}
                Err((_status, message)) => {
                    state
                        .repo
                        .update_lecture_notes_error(&job.lecture_key, &message)
                        .await?;
                }
            }
            return Ok(());
        }

        let retry_prompt = build_title_prompt(&existing_titles, Some((&title, reason)));
        let outcome = get_or_enqueue_archive_submission(
            state,
            client,
            base,
            &retry_submission_key,
            json!({
                "mode": "continue",
                "conversationId": conversation_id,
                "idempotencyKey": retry_submission_key,
                "text": retry_prompt,
            }),
        )
        .await;

        match outcome {
            Ok(ArchiveSubmissionOutcome::Queued { .. })
            | Ok(ArchiveSubmissionOutcome::Accepted { .. }) => {
                state
                    .repo
                    .update_lecture_notes_error(&job.lecture_key, &marker)
                    .await?;
            }
            Ok(ArchiveSubmissionOutcome::Failed { error }) => {
                state
                    .repo
                    .mark_lecture_notes_failed(
                        &job.lecture_key,
                        &format!("Archive 标题重做 submission 终止: {error}"),
                    )
                    .await?;
            }
            Err((_status, message)) => {
                state
                    .repo
                    .update_lecture_notes_error(&job.lecture_key, &message)
                    .await?;
            }
        }
        return Ok(());
    }

    state
        .repo
        .mark_lecture_notes_title_ready(&job.lecture_key, &title, title_message_ordinal)
        .await?;
    Ok(())
}

/// 第三轮仍然继续原 conversation，让模型利用刚刚生成的完整 Notes 与标题生成邮件摘要。
///
/// 与标题阶段一样，提交和结果读取严格分开：
/// - 发送使用 Archive durable submission queue + 稳定 idempotency key；
/// - checkpoint 丢失时先查 canonical messages 自动认领已存在的 prompt；
/// - 只接受标题 assistant final 之后的摘要 assistant final；
/// - 超过 2000 字不会本地粗暴截断，而是继续同一 conversation 请求模型压缩重做。
/// 最终完成与格式合规是两个独立条件：RQCI 保证消息完整，
/// ClassFlow 仍然负责校验摘要业务契约，避免模型正常结束却省略核心章节。
fn public_summary_rejection(markdown: &str) -> Option<String> {
    let count = markdown.chars().count();
    if count > SUMMARY_MAX_CHARS {
        return Some(format!("摘要超过 {SUMMARY_MAX_CHARS} 字符：{count}"));
    }
    if count < 120 {
        return Some(format!("摘要异常短：{count} 字符"));
    }
    if !markdown
        .lines()
        .any(|line| line.trim() == "## 本堂讲了什么")
    {
        return Some("摘要缺少必需章节 ## 本堂讲了什么".to_string());
    }
    None
}

async fn process_public_summary_result(
    state: &AppState,
    job: &LectureNotesRecord,
    conversation_id: &str,
    client: &Client,
    base: &str,
    mode: SummaryStageMode,
    initial_key: &str,
) -> AppResult<()> {
    let text = match get_public_job_state(state, client, base, initial_key).await? {
        PublicJobState::Completed(text) => text,
        PublicJobState::Failed(error) => {
            mark_summary_stage_failure(state, job, mode, &error).await?;
            return Ok(());
        }
        PublicJobState::Pending => return Ok(()),
        PublicJobState::Legacy | PublicJobState::Missing => {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    "新的 RQCI Summary Job 没有可追踪的完成状态",
                )
                .await?;
            return Ok(());
        }
    };
    let normalized = normalize_notes_markdown(&text);
    let first = normalized.markdown.trim();
    let summary = if let Some(reason) = public_summary_rejection(first) {
        let retry_key = format!(
            "classflow:lecture:{}:summary:v{}:0:retry:0",
            job.lecture_key, SUMMARY_PROMPT_VERSION,
        );
        match get_public_job_state(state, client, base, &retry_key).await? {
            PublicJobState::Completed(text) => {
                let retry = normalize_notes_markdown(&text);
                retry.markdown.trim().to_string()
            }
            PublicJobState::Failed(error) => {
                mark_summary_stage_failure(state, job, mode, &error).await?;
                return Ok(());
            }
            PublicJobState::Pending => return Ok(()),
            PublicJobState::Legacy => {
                mark_summary_stage_failure(state, job, mode, "摘要重试任务没有完整结果跟踪")
                    .await?;
                return Ok(());
            }
            PublicJobState::Missing => {
                let outcome = get_or_enqueue_archive_submission(
                    state,
                    client,
                    base,
                    &retry_key,
                    json!({
                        "mode": "continue",
                        "conversationId": conversation_id,
                        "idempotencyKey": retry_key,
                        "text": build_summary_prompt(Some(first.chars().count())),
                    }),
                )
                .await;
                match outcome {
                    Ok(ArchiveSubmissionOutcome::Failed { error }) => {
                        mark_summary_stage_failure(state, job, mode, &error).await?;
                    }
                    Ok(_) => {
                        state
                            .repo
                            .update_lecture_notes_error(
                                &job.lecture_key,
                                &format!("摘要格式校验未通过（{reason}），RQCI 已接管修正请求"),
                            )
                            .await?;
                    }
                    Err((_, message)) => {
                        state
                            .repo
                            .update_lecture_notes_error(&job.lecture_key, &message)
                            .await?;
                    }
                }
                return Ok(());
            }
        }
    } else {
        first.to_string()
    };
    if let Some(reason) = public_summary_rejection(&summary) {
        mark_summary_stage_failure(state, job, mode, &format!("摘要一次重试仍不合格：{reason}"))
            .await?;
        return Ok(());
    }
    mark_summary_stage_ready(state, job, mode, &summary, 0).await?;
    Ok(())
}

async fn process_summary_stage(
    state: &AppState,
    job: &LectureNotesRecord,
    conversation_id: &str,
    client: &Client,
    base: &str,
    mode: SummaryStageMode,
) -> AppResult<()> {
    let title_final_ordinal = job
        .title_final_ordinal
        .ok_or_else(|| AppError::Internal("摘要阶段缺少 title_final_ordinal".to_string()))?;
    let version_matches = job.summary_prompt_version == Some(SUMMARY_PROMPT_VERSION);
    if !version_matches {
        // v1 -> v2 只清理摘要与邮件 checkpoint；Notes、标题、CID、GitHub 均保持原样。
        // 当前 job 仍是升级前快照，因此下面不能继续读取它的旧 summary_requested_at。
        state
            .repo
            .prepare_summary_prompt_version(&job.lecture_key, SUMMARY_PROMPT_VERSION)
            .await?;
    }
    let summary_requested = version_matches && job.summary_requested_at.is_some();
    let initial_submission_key = format!(
        "classflow:lecture:{}:summary:v{}:{}:initial",
        job.lecture_key, SUMMARY_PROMPT_VERSION, title_final_ordinal
    );

    // 新式链路用稳定 Job Key 完成整个摘要生命周期，不查询 Archive 消息。
    // title_final_ordinal=0 只是持久化检查点兼容标记，不是归档消息序号。
    if title_final_ordinal == 0 {
        if !summary_requested {
            let outcome = get_or_enqueue_archive_submission(
                state,
                client,
                base,
                &initial_submission_key,
                json!({
                    "mode": "continue",
                    "conversationId": conversation_id,
                    "idempotencyKey": initial_submission_key,
                    "text": build_summary_prompt(None),
                }),
            )
            .await;
            match outcome {
                Ok(ArchiveSubmissionOutcome::Failed { error }) => {
                    mark_summary_stage_failure(state, job, mode, &error).await?;
                }
                Ok(_) => {
                    state
                        .repo
                        .mark_lecture_notes_summary_requested(
                            &job.lecture_key,
                            SUMMARY_PROMPT_VERSION,
                        )
                        .await?;
                }
                Err((_, message)) => {
                    state
                        .repo
                        .update_lecture_notes_error(&job.lecture_key, &message)
                        .await?;
                }
            }
            return Ok(());
        }
        return process_public_summary_result(
            state,
            job,
            conversation_id,
            client,
            base,
            mode,
            &initial_submission_key,
        )
        .await;
    }

    if !summary_requested {
        // 和标题阶段相同：先认领历史 canonical prompt，防止请求其实已经提交成功、
        // 但 ClassFlow 在写 checkpoint 前退出时再次发送同一摘要请求。
        let messages_url = format!("{base}/v1/conversations/{conversation_id}/messages?limit=200");
        let response = match apply_bridge_auth(client.get(messages_url), state)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                state
                    .repo
                    .update_lecture_notes_error(
                        &job.lecture_key,
                        &format!("摘要提交前读取 canonical conversation 失败: {error}"),
                    )
                    .await?;
                return Ok(());
            }
        };
        if !response.status().is_success() {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!(
                        "摘要提交前读取 canonical conversation 失败: HTTP {}",
                        response.status()
                    ),
                )
                .await?;
            return Ok(());
        }
        let payload: Value = response.json().await?;
        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if find_existing_summary_prompt_ordinal(&messages, title_final_ordinal).is_some() {
            state
                .repo
                .mark_lecture_notes_summary_requested(&job.lecture_key, SUMMARY_PROMPT_VERSION)
                .await?;
            info!(
                "Phase 3 Notes 已认领 canonical summary 请求: lecture_key={}, conversation_id={}",
                job.lecture_key, conversation_id
            );
            return Ok(());
        }

        let outcome = get_or_enqueue_archive_submission(
            state,
            client,
            base,
            &initial_submission_key,
            json!({
                "mode": "continue",
                "conversationId": conversation_id,
                "idempotencyKey": initial_submission_key,
                "text": build_summary_prompt(None),
            }),
        )
        .await;
        match outcome {
            Ok(ArchiveSubmissionOutcome::Queued {
                status,
                next_attempt_at,
                last_error,
            }) => {
                state
                    .repo
                    .mark_lecture_notes_summary_requested(&job.lecture_key, SUMMARY_PROMPT_VERSION)
                    .await?;
                let retry = next_attempt_at
                    .map(|value| format!("，nextAttemptAt={value}"))
                    .unwrap_or_default();
                let detail = last_error
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| format!("，{value}"))
                    .unwrap_or_default();
                state
                    .repo
                    .update_lecture_notes_error(
                        &job.lecture_key,
                        &format!("Archive 已接管摘要 submission：{status}{retry}{detail}"),
                    )
                    .await?;
            }
            Ok(ArchiveSubmissionOutcome::Accepted {
                conversation_id: returned_cid,
            }) => {
                if returned_cid != conversation_id {
                    mark_summary_stage_failure(
                        state,
                        job,
                        mode,
                        "摘要 submission 没有继续原 conversation，停止自动处理",
                    )
                    .await?;
                    return Ok(());
                }
                state
                    .repo
                    .mark_lecture_notes_summary_requested(&job.lecture_key, SUMMARY_PROMPT_VERSION)
                    .await?;
            }
            Ok(ArchiveSubmissionOutcome::Failed { error }) => {
                mark_summary_stage_failure(
                    state,
                    job,
                    mode,
                    &format!("Archive 摘要 submission 终止: {error}"),
                )
                .await?;
            }
            Err((status, message)) => {
                let fatal_client_error = status.is_some_and(|value| {
                    value.is_client_error() && value != StatusCode::TOO_MANY_REQUESTS
                });
                if fatal_client_error {
                    mark_summary_stage_failure(state, job, mode, &message).await?;
                } else {
                    state
                        .repo
                        .update_lecture_notes_error(&job.lecture_key, &message)
                        .await?;
                }
            }
        }
        return Ok(());
    }

    match get_archive_submission(state, client, base, &initial_submission_key).await {
        Ok(Some(ArchiveSubmissionOutcome::Queued {
            status,
            next_attempt_at,
            last_error,
        })) => {
            let retry = next_attempt_at
                .map(|value| format!("，nextAttemptAt={value}"))
                .unwrap_or_default();
            let detail = last_error
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("，{value}"))
                .unwrap_or_default();
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!("Archive 摘要 submission 尚未正式发送：{status}{retry}{detail}"),
                )
                .await?;
            return Ok(());
        }
        Ok(Some(ArchiveSubmissionOutcome::Failed { error })) => {
            mark_summary_stage_failure(
                state,
                job,
                mode,
                &format!("Archive 摘要 submission 终止: {error}"),
            )
            .await?;
            return Ok(());
        }
        Ok(Some(ArchiveSubmissionOutcome::Accepted {
            conversation_id: returned_cid,
        })) if returned_cid != conversation_id => {
            mark_summary_stage_failure(
                state,
                job,
                mode,
                "摘要 submission 返回了不同 conversationId",
            )
            .await?;
            return Ok(());
        }
        Ok(_) => {}
        Err((_status, message)) => {
            state
                .repo
                .update_lecture_notes_error(&job.lecture_key, &message)
                .await?;
            return Ok(());
        }
    }

    let messages_url = format!("{base}/v1/conversations/{conversation_id}/messages?limit=200");
    let response = match apply_bridge_auth(client.get(messages_url), state)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!("等待摘要 final 时 Archive 暂不可达: {error}"),
                )
                .await?;
            return Ok(());
        }
    };
    if !response.status().is_success() {
        return Ok(());
    }
    let payload: Value = response.json().await?;
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // v2 重跑时，旧 v1 summary 同样位于 title final 之后。
    // 所以必须以“当前版本 summary prompt”而不是标题作为结果边界。
    let Some(summary_prompt_ordinal) =
        find_existing_summary_prompt_ordinal(&messages, title_final_ordinal)
    else {
        return Ok(());
    };
    let summary_message = messages.iter().rev().find(|message| {
        message
            .get("ordinal")
            .and_then(Value::as_i64)
            .unwrap_or_default()
            > summary_prompt_ordinal
            && message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
            && !message
                .get("reasoningRedacted")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && assistant_final_is_complete(message)
    });
    let Some(summary_message) = summary_message else {
        return Ok(());
    };
    let summary_message_ordinal = summary_message
        .get("ordinal")
        .and_then(Value::as_i64)
        .unwrap_or(summary_prompt_ordinal + 1);
    let normalized = normalize_notes_markdown(
        summary_message
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let summary = normalized.markdown.trim();
    if summary.is_empty() {
        return Ok(());
    }

    if summary.chars().count() > SUMMARY_MAX_CHARS {
        let marker = format!("摘要重做请求已托管: ordinal={summary_message_ordinal}");
        let retry_submission_key = format!(
            "classflow:lecture:{}:summary:v{}:{}:retry:{}",
            job.lecture_key, SUMMARY_PROMPT_VERSION, title_final_ordinal, summary_message_ordinal
        );
        if job.last_error.as_deref() == Some(marker.as_str()) {
            match get_archive_submission(state, client, base, &retry_submission_key).await {
                Ok(Some(ArchiveSubmissionOutcome::Failed { error })) => {
                    mark_summary_stage_failure(
                        state,
                        job,
                        mode,
                        &format!("Archive 摘要重做 submission 终止: {error}"),
                    )
                    .await?;
                }
                Ok(_) => {}
                Err((_status, message)) => {
                    state
                        .repo
                        .update_lecture_notes_error(&job.lecture_key, &message)
                        .await?;
                }
            }
            return Ok(());
        }
        match get_or_enqueue_archive_submission(
            state,
            client,
            base,
            &retry_submission_key,
            json!({
                "mode": "continue",
                "conversationId": conversation_id,
                "idempotencyKey": retry_submission_key,
                "text": build_summary_prompt(Some(summary.chars().count())),
            }),
        )
        .await
        {
            Ok(ArchiveSubmissionOutcome::Queued { .. })
            | Ok(ArchiveSubmissionOutcome::Accepted { .. }) => {
                state
                    .repo
                    .update_lecture_notes_error(&job.lecture_key, &marker)
                    .await?;
            }
            Ok(ArchiveSubmissionOutcome::Failed { error }) => {
                mark_summary_stage_failure(
                    state,
                    job,
                    mode,
                    &format!("Archive 摘要重做 submission 终止: {error}"),
                )
                .await?;
            }
            Err((_status, message)) => {
                state
                    .repo
                    .update_lecture_notes_error(&job.lecture_key, &message)
                    .await?;
            }
        }
        return Ok(());
    }

    mark_summary_stage_ready(state, job, mode, summary, summary_message_ordinal).await?;
    Ok(())
}

/// 正常新课的摘要错误沿用原 Notes failed 语义；历史回填则只能冻结摘要补跑，
/// 不能把已经成功发布的旧课堂改成 failed。
async fn mark_summary_stage_failure(
    state: &AppState,
    job: &LectureNotesRecord,
    mode: SummaryStageMode,
    error: &str,
) -> AppResult<()> {
    match mode {
        SummaryStageMode::Live => {
            state
                .repo
                .mark_lecture_notes_failed(&job.lecture_key, error)
                .await
        }
        SummaryStageMode::HistoricalBackfill => {
            state
                .repo
                .mark_historical_summary_backfill_failed(
                    &job.lecture_key,
                    SUMMARY_PROMPT_VERSION,
                    error,
                )
                .await
        }
    }
}

/// 两种摘要链路共用生成逻辑，只在最终 checkpoint 的状态迁移上分叉。
async fn mark_summary_stage_ready(
    state: &AppState,
    job: &LectureNotesRecord,
    mode: SummaryStageMode,
    summary: &str,
    summary_message_ordinal: i64,
) -> AppResult<()> {
    match mode {
        SummaryStageMode::Live => {
            state
                .repo
                .mark_lecture_notes_summary_ready(
                    &job.lecture_key,
                    summary,
                    summary_message_ordinal,
                )
                .await
        }
        SummaryStageMode::HistoricalBackfill => {
            state
                .repo
                .mark_historical_summary_ready(&job.lecture_key, summary, summary_message_ordinal)
                .await
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedNotes {
    markdown: String,
    writing_title: Option<String>,
}

/// 把 ChatGPT 聊天展示层的 writing block 还原成仓库需要的纯 Markdown。
///
/// ChatGPT 2026 Web 会把“可直接复制/编辑的完整文档”包装为：
///
/// `:::writing{variant="document" ... title="..."}`
/// `...真正 Markdown...`
/// `:::`
///
/// 这个 wrapper 属于聊天 UI 协议，不是课堂笔记正文。Project 还可能在 block 前后附一句说明，
/// 因而这里一旦识别到完整 writing block，就只保留 block 内正文，并把 metadata 中的 title
/// 作为标题兜底；如果没有 writing block，则保持原始 Markdown 不变。
fn normalize_notes_markdown(raw: &str) -> NormalizedNotes {
    let lines = raw.lines().collect::<Vec<_>>();
    let Some((start, opening)) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| line.trim_start().starts_with(":::writing{"))
    else {
        return NormalizedNotes {
            markdown: raw.trim().to_string(),
            writing_title: None,
        };
    };

    let Some(relative_end) = lines[start + 1..]
        .iter()
        .position(|line| line.trim() == ":::")
    else {
        // 不完整 wrapper 不应被擅自裁剪；保留完整 final，方便后续人工诊断。
        return NormalizedNotes {
            markdown: raw.trim().to_string(),
            writing_title: None,
        };
    };
    let end = start + 1 + relative_end;
    let body = lines[start + 1..end].join("\n").trim().to_string();
    if body.is_empty() {
        return NormalizedNotes {
            markdown: raw.trim().to_string(),
            writing_title: None,
        };
    }

    NormalizedNotes {
        markdown: body,
        writing_title: writing_block_title(opening),
    }
}

/// writing metadata 目前是单行属性列表；标题只作为兜底提示，不参与协议控制，因此用保守的
/// `title="..."` 提取即可。若将来 title 出现复杂转义，提取失败只会退回 conversation title。
fn writing_block_title(opening: &str) -> Option<String> {
    let marker = "title=\"";
    let start = opening.find(marker)? + marker.len();
    let rest = &opening[start..];
    let end = rest.find('"')?;
    let title = rest[..end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

async fn process_publishing(state: &AppState, job: &LectureNotesRecord) -> AppResult<()> {
    let notes = job
        .notes_markdown
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::Internal("publishing 状态缺少 Notes 正文".to_string()))?;
    let title = job
        .notes_title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("课堂笔记");
    let lecture = state
        .repo
        .get_lecture_group(&job.course_key, &job.lecture_key)
        .await?;
    let asr = build_lecture_asr_markdown(&lecture)
        .ok_or_else(|| AppError::Internal("发布 Notes 时完整课堂 ASR 不可用".to_string()))?;
    let course_dir = sanitize_path_component(&job.course_name, "课程");
    let audio_filename =
        build_legacy_audio_filename(&job.date, &job.course_name, &job.teacher_name);
    let audio_path = format!("{course_dir}/Audio/{audio_filename}");
    let notes_path = format!(
        "{course_dir}/Notes/{}-{}.md",
        job.sequence_no,
        sanitize_path_component(title, "课堂笔记")
    );

    let publisher = GithubPublisher::new(state)?;
    if let Err(error) = publisher
        .put_markdown(
            &audio_path,
            &asr,
            &format!("notes: publish {} ASR", job.lecture_key),
        )
        .await
    {
        state
            .repo
            .update_lecture_notes_error(&job.lecture_key, &format!("GitHub 发布 ASR 失败: {error}"))
            .await?;
        return Ok(());
    }
    let notes_commit = match publisher
        .put_markdown(
            &notes_path,
            notes,
            &format!("notes: publish {} Notes", job.lecture_key),
        )
        .await
    {
        Ok(value) => value,
        Err(error) => {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!("GitHub 发布 Notes 失败: {error}"),
                )
                .await?;
            return Ok(());
        }
    };
    for old_path in [
        job.github_audio_path.as_deref(),
        job.github_notes_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|old_path| *old_path != audio_path && *old_path != notes_path)
    {
        if let Err(error) = publisher
            .delete_if_exists(
                old_path,
                &format!("notes: remove obsolete path for {}", job.lecture_key),
            )
            .await
        {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!("GitHub 清理旧路径失败: {error}"),
                )
                .await?;
            return Ok(());
        }
    }
    state
        .repo
        .mark_lecture_notes_succeeded(
            &job.lecture_key,
            &audio_path,
            &notes_path,
            notes_commit.as_deref(),
        )
        .await?;
    info!(
        "Phase 3 Notes 已发布: {} -> {}",
        job.lecture_key, notes_path
    );
    Ok(())
}

fn bridge_client(state: &AppState) -> AppResult<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs_f64(
            state.config.archive_bridge_request_timeout_secs.max(10.0),
        ))
        .build()
        .map_err(AppError::from)
}

fn apply_bridge_auth(
    builder: reqwest::RequestBuilder,
    state: &AppState,
) -> reqwest::RequestBuilder {
    let mut builder = builder.bearer_auth(&state.config.archive_bridge_token);
    if !state.config.archive_bridge_access_client_id.is_empty() {
        builder = builder.header(
            "CF-Access-Client-Id",
            &state.config.archive_bridge_access_client_id,
        );
    }
    if !state.config.archive_bridge_access_client_secret.is_empty() {
        builder = builder.header(
            "CF-Access-Client-Secret",
            &state.config.archive_bridge_access_client_secret,
        );
    }
    builder
}

struct GithubPublisher {
    client: Client,
    repo: String,
    token: String,
}

impl GithubPublisher {
    fn new(state: &AppState) -> AppResult<Self> {
        if state.config.notes_github_token.trim().is_empty() {
            return Err(AppError::Config(
                "CLASSFLOW_NOTES_GITHUB_TOKEN 未配置，无法发布 Notes".to_string(),
            ));
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            repo: state.config.notes_github_repo.clone(),
            token: state.config.notes_github_token.clone(),
        })
    }

    async fn put_markdown(
        &self,
        path: &str,
        markdown: &str,
        message: &str,
    ) -> AppResult<Option<String>> {
        let url = github_contents_url(&self.repo, path)?;
        let get = self
            .client
            .get(url.clone())
            .header(header::USER_AGENT, "ClassFlow/1.0")
            .header(header::ACCEPT, "application/vnd.github+json")
            .bearer_auth(&self.token)
            .send()
            .await?;
        let sha = if get.status() == StatusCode::NOT_FOUND {
            None
        } else if get.status().is_success() {
            get.json::<Value>()
                .await?
                .get("sha")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        } else {
            return Err(AppError::Internal(format!(
                "GitHub 读取 {} 失败: HTTP {}",
                path,
                get.status()
            )));
        };
        let mut payload = json!({
            "message": message,
            "content": BASE64_STANDARD.encode(markdown.as_bytes()),
            "branch": "main",
        });
        if let Some(sha) = sha {
            payload["sha"] = Value::String(sha);
        }
        let put = self
            .client
            .put(url)
            .header(header::USER_AGENT, "ClassFlow/1.0")
            .header(header::ACCEPT, "application/vnd.github+json")
            .bearer_auth(&self.token)
            .json(&payload)
            .send()
            .await?;
        let status = put.status();
        let body: Value = put.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(AppError::Internal(format!(
                "GitHub 写入 {} 失败: HTTP {} {}",
                path,
                status,
                truncate(&body.to_string(), 240)
            )));
        }
        Ok(body
            .pointer("/commit/sha")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned))
    }

    async fn delete_if_exists(&self, path: &str, message: &str) -> AppResult<()> {
        let url = github_contents_url(&self.repo, path)?;
        let get = self
            .client
            .get(url.clone())
            .header(header::USER_AGENT, "ClassFlow/1.0")
            .header(header::ACCEPT, "application/vnd.github+json")
            .bearer_auth(&self.token)
            .send()
            .await?;
        if get.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if !get.status().is_success() {
            return Err(AppError::Internal(format!(
                "GitHub 读取旧路径 {} 失败: HTTP {}",
                path,
                get.status()
            )));
        }
        let sha = get
            .json::<Value>()
            .await?
            .get("sha")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| AppError::Internal(format!("GitHub 旧路径 {} 缺少 sha", path)))?;
        let response = self
            .client
            .delete(url)
            .header(header::USER_AGENT, "ClassFlow/1.0")
            .header(header::ACCEPT, "application/vnd.github+json")
            .bearer_auth(&self.token)
            .json(&json!({
                "message": message,
                "sha": sha,
                "branch": "main",
            }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(AppError::Internal(format!(
                "GitHub 删除旧路径 {} 失败: HTTP {}",
                path,
                response.status()
            )));
        }
        Ok(())
    }
}

fn build_legacy_audio_filename(date: &str, course_name: &str, teacher_name: &str) -> String {
    let month_day = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|date| format!("{}.{}", date.month(), date.day()))
        .unwrap_or_else(|_| sanitize_path_component(date, "未知日期"));
    format!(
        "{}-{}-{}.md",
        month_day,
        sanitize_path_component(course_name, "未命名课程"),
        sanitize_path_component(teacher_name, "未知老师")
    )
}

fn normalize_title_response(raw: &str) -> String {
    let first = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut title = first
        .trim_start_matches('#')
        .trim()
        .trim_start_matches("标题：")
        .trim_start_matches("标题:")
        .trim()
        .trim_matches(|ch| {
            matches!(
                ch,
                '“' | '”' | '‘' | '’' | '"' | '\'' | '`' | '。' | '！' | '!' | '；' | ';'
            )
        })
        .trim()
        .to_string();
    if title.to_ascii_lowercase().ends_with(".md") {
        title.truncate(title.len().saturating_sub(3));
        title = title.trim().to_string();
    }
    title
}

fn find_existing_title_prompt_ordinal(messages: &[Value], notes_final_ordinal: i64) -> Option<i64> {
    messages
        .iter()
        .filter_map(|message| {
            let ordinal = message.get("ordinal").and_then(Value::as_i64)?;
            let is_title_prompt = ordinal > notes_final_ordinal
                && message.get("role").and_then(Value::as_str) == Some("user")
                && message
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.trim_start().starts_with(TITLE_PROMPT_PREFIX));
            is_title_prompt.then_some(ordinal)
        })
        .max()
}

/// 旧记录没有持久化 title_final_ordinal，因此从 canonical messages 中恢复。
///
/// 为避免把 Notes 正文或别的 assistant final 误认成标题，这里同时要求：
/// 1. 先找到 notes final 之后最新的标题 prompt；
/// 2. assistant 消息位于该 prompt 之后且确实是 final；
/// 3. 经过与生产标题相同的 normalize 后，内容必须与数据库里已发布标题完全一致。
fn recover_historical_title_final_ordinal(
    messages: &[Value],
    notes_final_ordinal: i64,
    expected_title: &str,
) -> Option<i64> {
    let title_prompt_ordinal = find_existing_title_prompt_ordinal(messages, notes_final_ordinal)?;
    // 历史记录可能曾因为旧版重试逻辑而留下多个 title prompt。
    // 数据库里的 notes_title 可能来自较早一次成功响应，而 canonical conversation
    // 最后又出现了一次标题重试。回填摘要真正需要的是“标题阶段已经结束”的安全边界，
    // 因此优先锁定最新 title prompt 后、下一个 user turn 之前的 assistant final。
    // 如果内容仍等于已发布标题当然最好；若标题文本不同但仍是一个合法短标题，也可以
    // 只使用其 ordinal 作为边界，绝不改写数据库里已经发布的 notes_title。
    let next_user_ordinal = messages
        .iter()
        .filter_map(|message| {
            let ordinal = message.get("ordinal").and_then(Value::as_i64)?;
            (ordinal > title_prompt_ordinal
                && message.get("role").and_then(Value::as_str) == Some("user"))
            .then_some(ordinal)
        })
        .min();
    messages
        .iter()
        .filter_map(|message| {
            let ordinal = message.get("ordinal").and_then(Value::as_i64)?;
            if ordinal <= title_prompt_ordinal
                || next_user_ordinal.is_some_and(|next| ordinal >= next)
                || message.get("role").and_then(Value::as_str) != Some("assistant")
                || message
                    .get("text")
                    .and_then(Value::as_str)
                    .is_none_or(|text| text.trim().is_empty())
                || message
                    .get("reasoningRedacted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            {
                return None;
            }
            let is_final = assistant_final_is_complete(message);
            if !is_final {
                return None;
            }
            let recovered = normalize_title_response(
                message
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            let matches_published = recovered.trim() == expected_title.trim();
            let valid_boundary_title = title_rejection_reason(&recovered, &[]).is_none();
            (matches_published || valid_boundary_title).then_some(ordinal)
        })
        .min()
}

fn find_existing_summary_prompt_ordinal(
    messages: &[Value],
    title_final_ordinal: i64,
) -> Option<i64> {
    messages
        .iter()
        .filter_map(|message| {
            let ordinal = message.get("ordinal").and_then(Value::as_i64)?;
            let is_summary_prompt = ordinal > title_final_ordinal
                && message.get("role").and_then(Value::as_str) == Some("user")
                && message
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.trim_start().starts_with(SUMMARY_PROMPT_PREFIX));
            is_summary_prompt.then_some(ordinal)
        })
        .max()
}

fn build_summary_prompt(previous_chars: Option<usize>) -> String {
    let retry = previous_chars
        .map(|chars| {
            format!(
                "\n你上一版摘要约 {chars} 个字符，超过硬上限 {SUMMARY_MAX_CHARS}。请在不丢失通知、时间、范围、要求和特殊考勤细节的前提下进一步压缩。"
            )
        })
        .unwrap_or_default();
    format!(
        "{SUMMARY_PROMPT_PREFIX}\
要求：\
1）目标长度约 500～1000 个汉字，任何情况下都不得超过 {SUMMARY_MAX_CHARS} 个字符；课堂知识只保留最重要的主线、概念、结论和老师特别强调的点，不要复述完整笔记。\
2）如果课堂出现考试、小测、作业、截止时间、课程安排调整、材料提交、复习范围等通知，必须保留具体日期/时间/范围/要求，不能因为追求简短而概括掉关键细节。\
3）如果出现常规系统签到之外的特殊考勤或互动，例如老师临时点名、随机提问、缺勤排查、额外签到或明确要求某些同学回应，也要单独保留；没有则不要凭空添加。\
4）不得臆造笔记中没有的信息；没有通知或特殊考勤时直接省略对应部分，不要写‘无’或套模板。\
5）输出简洁 Markdown，章节顺序必须严格固定：①如果有通知与待办，先写 `## 通知与待办`；②如果有特殊签到或课堂互动，再写 `## 特殊签到与课堂互动`；③最后写 `## 本堂讲了什么`，这一节必须存在且必须是最后一节。前两节不存在时直接省略；无论是否存在前两节，都不得把“本堂讲了什么”放到通知或特殊考勤之前。6）不要加邮件称呼、寒暄或结尾。{retry}"
    )
}

fn build_title_prompt(existing_titles: &[String], rejected: Option<(&str, &str)>) -> String {
    let existing = if existing_titles.is_empty() {
        "（暂无）".to_string()
    } else {
        existing_titles.join("；")
    };
    let retry = rejected
        .map(|(title, reason)| {
            format!(
                "\n你上一版标题“{title}”不合格：{reason}。请重新命名，不要只加“一/二/上/下/续”等弱区分。"
            )
        })
        .unwrap_or_default();
    format!(
        "请只为你刚刚生成的整堂课堂笔记起一个简洁、具体、可区分的中文主题名。\
要求：1）概括本堂真正讲授的核心知识主题；2）不要包含课程名、日期、老师姓名、‘课堂笔记’、‘理论课’、‘第几部分’等元信息；\
3）优先 4～9 个汉字，尽量不超过 12 个汉字；只有确实无法准确概括时才允许更长，但不要超过 {TITLE_MAX_CHARS} 个汉字；\
4）同一课程不同课堂的名字必须能一眼区分，不能重名，也不要用‘病理一/病理二’、‘上/下/续’这类序号式弱区分；主题接近时，应写出真正不同的知识点；\
5）只输出标题本身，不要加序号、引号、解释、Markdown 标记或句号。\
同一课程已经使用过的标题：{existing}。{retry}"
    )
}

fn canonical_title(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_whitespace() && !ch.is_ascii_punctuation())
        .filter(|ch| {
            !matches!(
                ch,
                '，' | '。'
                    | '、'
                    | '；'
                    | '：'
                    | '！'
                    | '？'
                    | '（'
                    | '）'
                    | '《'
                    | '》'
                    | '“'
                    | '”'
                    | '‘'
                    | '’'
            )
        })
        .collect::<String>()
        .to_lowercase()
}

fn title_rejection_reason<'a>(title: &str, existing_titles: &'a [String]) -> Option<&'a str> {
    if title.trim().is_empty() {
        return Some("标题为空");
    }
    if title.chars().count() > TITLE_MAX_CHARS {
        return Some("标题过长");
    }
    let canonical = canonical_title(title);
    if existing_titles
        .iter()
        .any(|existing| canonical_title(existing) == canonical)
    {
        return Some("与同一课程已有标题重复");
    }
    None
}

fn github_contents_url(repo: &str, path: &str) -> AppResult<Url> {
    let mut url = Url::parse("https://api.github.com")
        .map_err(|error| AppError::Internal(format!("GitHub API URL 无效: {error}")))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| AppError::Internal("GitHub API URL 无法追加路径".to_string()))?;
        segments.push("repos");
        for part in repo.split('/') {
            segments.push(part);
        }
        segments.push("contents");
        for part in path.split('/') {
            segments.push(part);
        }
    }
    Ok(url)
}

pub fn derive_notes_title(
    markdown: &str,
    conversation_title: &str,
    course_name: &str,
    date: &str,
) -> String {
    let heading = markdown.lines().find_map(|line| {
        let trimmed = line.trim();
        let title = trimmed.trim_start_matches('#').trim();
        (trimmed.starts_with('#') && title.chars().count() >= 3).then_some(title)
    });
    let candidates = [heading.unwrap_or_default(), conversation_title, course_name];
    let selected = candidates
        .into_iter()
        .find(|value| !is_generic_title(value))
        .unwrap_or(course_name);
    let compact = selected.split_whitespace().collect::<Vec<_>>().join(" ");
    let limited = compact.chars().take(48).collect::<String>();
    if limited.trim().is_empty() {
        format!("{}-{}", course_name, date)
    } else {
        limited
    }
}

fn is_generic_title(value: &str) -> bool {
    let normalized = value.trim();
    normalized.is_empty()
        || matches!(
            normalized,
            "课堂笔记" | "课堂录音→笔记" | "ChatGPT" | "新聊天"
        )
        || normalized.chars().count() < 3
}

pub fn sanitize_path_component(value: &str, fallback: &str) -> String {
    let cleaned = value
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            ch if ch.is_control() => '-',
            _ => ch,
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .trim()
        .to_string();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.chars().take(64).collect()
    }
}

fn sha256_text(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_submission_parser_should_cover_queue_terminal_and_legacy_shapes() {
        assert_eq!(
            parse_archive_submission_outcome(
                r#"{"submission":{"status":"retry_waiting","nextAttemptAt":1791660458462,"lastError":"cooldown"}}"#
            )
            .expect("queued submission 应可解析"),
            ArchiveSubmissionOutcome::Queued {
                status: "retry_waiting".to_string(),
                next_attempt_at: Some(1_791_660_458_462),
                last_error: Some("cooldown".to_string()),
            }
        );

        assert_eq!(
            parse_archive_submission_outcome(
                r#"{"submission":{"status":"accepted","result":{"conversationId":"cid-accepted"}}}"#
            )
            .expect("accepted submission 应可解析"),
            ArchiveSubmissionOutcome::Accepted {
                conversation_id: "cid-accepted".to_string(),
            }
        );

        assert_eq!(
            parse_archive_submission_outcome(
                r#"{"submission":{"status":"failed","lastError":"bad request"}}"#
            )
            .expect("failed submission 应可解析"),
            ArchiveSubmissionOutcome::Failed {
                error: "bad request".to_string(),
            }
        );

        assert_eq!(
            parse_archive_submission_outcome(
                r#"{"accepted":true,"result":{"conversationId":"legacy-cid"}}"#
            )
            .expect("旧 Bridge result 应继续兼容"),
            ArchiveSubmissionOutcome::Accepted {
                conversation_id: "legacy-cid".to_string(),
            }
        );
    }

    #[test]
    fn title_prefers_readable_markdown_heading() {
        assert_eq!(
            derive_notes_title(
                "# 肾小球疾病与肾病综合征\n\n正文",
                "课堂录音→笔记",
                "泌尿生殖系统",
                "2026-10-08",
            ),
            "肾小球疾病与肾病综合征"
        );
    }

    #[test]
    fn writing_block_should_be_unwrapped_before_publishing() {
        let raw = r#"这是给你的课堂笔记：

:::writing{variant="document" id="48317" title="泌尿生殖系统疾病"}
# 泌尿生殖系统疾病

## 第一部分

正文内容。
:::

如果需要，我还可以再压缩。"#;
        let normalized = normalize_notes_markdown(raw);
        assert_eq!(
            normalized.writing_title.as_deref(),
            Some("泌尿生殖系统疾病")
        );
        assert_eq!(
            normalized.markdown,
            "# 泌尿生殖系统疾病\n\n## 第一部分\n\n正文内容。"
        );
    }

    #[test]
    fn plain_markdown_should_remain_unchanged() {
        let normalized = normalize_notes_markdown("\n# 标题\n\n正文\n");
        assert_eq!(normalized.writing_title, None);
        assert_eq!(normalized.markdown, "# 标题\n\n正文");
    }

    #[test]
    fn audio_filename_should_follow_second_year_spring_convention() {
        assert_eq!(
            build_legacy_audio_filename("2026-10-08", "泌尿生殖系统", "王栋"),
            "10.8-泌尿生殖系统-王栋.md"
        );
    }

    #[test]
    fn title_response_should_strip_chatty_wrappers() {
        assert_eq!(
            normalize_title_response(
                "# “宫颈癌、子宫内膜异位症与子宫体肿瘤”。\n解释不应进入文件名"
            ),
            "宫颈癌、子宫内膜异位症与子宫体肿瘤"
        );
    }

    #[test]
    fn should_recognize_existing_title_prompt_after_transport_unknown() {
        let messages = vec![
            json!({
                "ordinal": 28,
                "role": "assistant",
                "channel": "final",
                "text": "完整课堂笔记",
                "endTurn": true
            }),
            json!({
                "ordinal": 29,
                "role": "user",
                "text": format!("{}要求：只输出标题。", TITLE_PROMPT_PREFIX)
            }),
            json!({
                "ordinal": 31,
                "role": "assistant",
                "channel": "final",
                "text": "科研素养与科研诚信",
                "endTurn": true
            }),
        ];

        assert_eq!(find_existing_title_prompt_ordinal(&messages, 28), Some(29));
        assert_eq!(find_existing_title_prompt_ordinal(&messages, 29), None);
    }

    #[test]
    fn should_recover_historical_title_final_ordinal_from_canonical_messages() {
        let messages = vec![
            json!({
                "ordinal": 28,
                "role": "assistant",
                "channel": "final",
                "text": "# 完整课堂笔记",
                "endTurn": true
            }),
            json!({
                "ordinal": 29,
                "role": "user",
                "text": format!("{}要求：只输出标题。", TITLE_PROMPT_PREFIX)
            }),
            json!({
                "ordinal": 31,
                "role": "assistant",
                "channel": "final",
                "text": "生育水平与影响因素",
                "endTurn": true
            }),
        ];

        assert_eq!(
            recover_historical_title_final_ordinal(&messages, 28, "生育水平与影响因素"),
            Some(31)
        );
        assert_eq!(
            recover_historical_title_final_ordinal(&messages, 28, "其他标题"),
            Some(31)
        );
    }

    #[test]
    fn should_recover_latest_title_retry_boundary_without_rewriting_published_title() {
        let messages = vec![
            json!({
                "ordinal": 26,
                "role": "assistant",
                "channel": "final",
                "text": "# 完整课堂笔记",
                "endTurn": true
            }),
            json!({
                "ordinal": 27,
                "role": "user",
                "text": format!("{}要求：只输出标题。", TITLE_PROMPT_PREFIX)
            }),
            json!({
                "ordinal": 29,
                "role": "assistant",
                "channel": "final",
                "text": "统计流程与数据分类",
                "endTurn": true
            }),
            json!({
                "ordinal": 30,
                "role": "user",
                "text": format!("{}要求：只输出标题。", TITLE_PROMPT_PREFIX)
            }),
            json!({
                "ordinal": 32,
                "role": "assistant",
                "channel": "final",
                "text": "统计流程与基本概念",
                "endTurn": true
            }),
            json!({
                "ordinal": 33,
                "role": "user",
                "text": "后续别的请求"
            }),
            json!({
                "ordinal": 35,
                "role": "assistant",
                "channel": "final",
                "text": "这不是标题",
                "endTurn": true
            }),
        ];

        // 数据库仍保留较早已发布标题，但摘要边界应使用最新标题重试的 final。
        assert_eq!(
            recover_historical_title_final_ordinal(&messages, 26, "统计流程与数据分类"),
            Some(32)
        );
    }

    #[test]
    fn summary_v2_prompt_should_put_notifications_before_knowledge_body() {
        let prompt = build_summary_prompt(None);
        assert!(prompt.starts_with(SUMMARY_PROMPT_PREFIX));
        let notice = prompt.find("## 通知与待办").expect("应包含通知标题");
        let attendance = prompt
            .find("## 特殊签到与课堂互动")
            .expect("应包含特殊考勤标题");
        let knowledge = prompt.find("## 本堂讲了什么").expect("应包含知识正文标题");
        assert!(notice < attendance);
        assert!(attendance < knowledge);
        assert!(prompt.contains("这一节必须存在且必须是最后一节"));
    }

    #[test]
    fn summary_v2_should_ignore_v1_prompt_when_recovering_canonical_turn() {
        let messages = vec![
            json!({
                "ordinal": 25,
                "role": "assistant",
                "channel": "final",
                "text": "生育水平与影响因素",
                "endTurn": true
            }),
            json!({
                "ordinal": 26,
                "role": "user",
                "text": "请基于你刚刚生成的整堂课堂笔记，再给出一份用于课后邮件快速查看的中文摘要。"
            }),
            json!({
                "ordinal": 28,
                "role": "assistant",
                "channel": "final",
                "text": "旧版摘要",
                "endTurn": true
            }),
            json!({
                "ordinal": 29,
                "role": "user",
                "text": build_summary_prompt(None)
            }),
            json!({
                "ordinal": 31,
                "role": "assistant",
                "channel": "final",
                "text": "新版摘要",
                "endTurn": true
            }),
        ];

        assert_eq!(
            find_existing_summary_prompt_ordinal(&messages, 25),
            Some(29)
        );
    }

    #[test]
    fn title_prompt_should_prefer_short_distinct_names() {
        let prompt = build_title_prompt(&["肾功能不全".to_string()], None);
        assert!(prompt.contains("优先 4～9 个汉字"));
        assert!(prompt.contains("尽量不超过 12 个汉字"));
        assert!(prompt.contains("肾功能不全"));
    }

    #[test]
    fn duplicate_title_should_be_rejected_after_punctuation_normalization() {
        assert_eq!(
            title_rejection_reason("肾功能不全！", &["肾功能不全".to_string()]),
            Some("与同一课程已有标题重复")
        );
    }

    #[test]
    fn overly_long_title_should_be_rejected_for_retry() {
        let title = "这是一个明显超过二十四个汉字并且完全没有必要这么长的课堂主题名称";
        assert_eq!(title_rejection_reason(title, &[]), Some("标题过长"));
    }

    #[test]
    fn path_component_removes_windows_and_github_hostile_chars() {
        assert_eq!(
            sanitize_path_component("肾病/肿瘤:总论?", "笔记"),
            "肾病-肿瘤-总论-"
        );
    }
}
