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

/// 判断当前是否处于 Archive 429 的全局 submission 冷却期。
///
/// 冷却只影响“新建 Notes conversation”。已经拿到 CID 的 generating、以及 GitHub publishing
/// 仍会正常推进，因此限流不会把整个 Phase 3 状态机冻结。
fn cooldown_until_is_active(
    cooldown_until: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> bool {
    cooldown_until.is_some_and(|until| now < until)
}

async fn notes_submission_cooldown_active(state: &AppState) -> bool {
    let now = Utc::now();
    let mut cooldown_until = state.notes_submit_cooldown_until.lock().await;
    if cooldown_until_is_active(*cooldown_until, now) {
        return true;
    }
    if cooldown_until.is_some() {
        // 到期后主动清空，避免后续日志/调试误以为仍处于 cooldown。
        *cooldown_until = None;
    }
    false
}

/// Archive 明确返回 429 时设置**全局**冷却。
///
/// 不能只给当前 job 加等待：429 是 actor/账号级资源状态，如果把当前 job 的 updated_at
/// 后移再换下一堂提交，就会让所有 pending 在几分钟内轮流撞同一个限流。
async fn start_notes_429_cooldown(state: &AppState, response_body: &str) {
    let seconds = state.config.notes_429_cooldown_secs.max(60);
    let configured_until = Utc::now() + chrono::Duration::seconds(seconds as i64);
    let archive_until = parse_archive_cooldown_until(response_body);
    let until = archive_until
        .filter(|value| *value > configured_until)
        .unwrap_or(configured_until);

    *state.notes_submit_cooldown_until.lock().await = Some(until);
    warn!(
        "Archive 返回 429，Notes/标题新提交进入全局冷却: 截止 {}{}",
        until,
        archive_until
            .map(|value| format!("（Archive 明示 {}）", value))
            .unwrap_or_default()
    );
}

fn parse_archive_cooldown_until(response_body: &str) -> Option<chrono::DateTime<Utc>> {
    let payload: Value = serde_json::from_str(response_body).ok()?;
    let message = payload.get("error")?.as_str()?;
    let (_, raw_until) = message.rsplit_once(" until ")?;
    chrono::DateTime::parse_from_rfc3339(raw_until.trim())
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

async fn process_pending(state: &AppState, job: &LectureNotesRecord) -> AppResult<()> {
    if job.recovery_kind.as_deref() == Some("submission_unknown") {
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
        .ok_or_else(|| AppError::BadRequest("Notes 提交时完整课堂 ASR 已不再可用".to_string()))?;
    let client = bridge_client(state)?;
    let url = format!(
        "{}/v1/turns",
        state.config.archive_bridge_base_url.trim_end_matches('/')
    );
    let request = apply_bridge_auth(
        client.post(url).json(&json!({
            "mode": "new",
            "targetMode": "guardian",
            "projectName": state.config.notes_project_name,
            // 和课程库下载的完整 lecture ASR 使用完全相同的 Markdown。
            // 不再为了给 ChatGPT 发送而另造一份“抽掉元数据”的正文，避免后续两套格式漂移。
            "text": source_markdown,
        })),
        state,
    );

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) if error.is_connect() => {
            state
                .repo
                .mark_lecture_notes_pending_error(
                    &job.lecture_key,
                    &format!("Archive Bridge 暂不可达: {error}"),
                )
                .await?;
            return Ok(());
        }
        Err(error) => {
            state
                .repo
                .mark_lecture_notes_submission_unknown(
                    &job.lecture_key,
                    &format!("Archive 提交结果未知，正在异步核对已有 conversation: {error}"),
                )
                .await?;
            return Ok(());
        }
    };

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status.as_u16() == 429 {
        start_notes_429_cooldown(state, &body).await;
        state
            .repo
            .mark_lecture_notes_pending_error(
                &job.lecture_key,
                &format!(
                    "Archive 暂不可用: HTTP {status} {}；已触发全局提交冷却",
                    truncate(&body, 180)
                ),
            )
            .await?;
        return Ok(());
    }
    if matches!(status.as_u16(), 502..=504) {
        state
            .repo
            .mark_lecture_notes_submission_unknown(
                &job.lecture_key,
                &format!(
                    "Archive/代理返回 {status}，不能证明提交是否已到达；正在异步核对已有 conversation: {}",
                    truncate(&body, 180)
                ),
            )
            .await?;
        return Ok(());
    }
    if !status.is_success() {
        if status == StatusCode::CONFLICT && body.contains("DOM drift detected before navigation") {
            state
                .repo
                .mark_lecture_notes_failed_with_kind(
                    &job.lecture_key,
                    &format!(
                        "Archive 明确在导航前拒绝提交，可安全重新提交: {}",
                        truncate(&body, 240)
                    ),
                    "safe_resubmit",
                )
                .await?;
            return Ok(());
        }
        state
            .repo
            .mark_lecture_notes_failed_with_kind(
                &job.lecture_key,
                &format!("Archive 提交失败: HTTP {status} {}", truncate(&body, 240)),
                "manual_review",
            )
            .await?;
        return Ok(());
    }

    let payload: Value = serde_json::from_str(&body)
        .map_err(|error| AppError::Internal(format!("解析 Archive submit 响应失败: {error}")))?;
    let conversation_id = payload
        .pointer("/result/conversationId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    let Some(conversation_id) = conversation_id else {
        state
            .repo
            .mark_lecture_notes_submission_unknown(
                &job.lecture_key,
                "Archive 返回成功但缺少 conversationId；正在异步核对已有 conversation",
            )
            .await?;
        return Ok(());
    };
    state
        .repo
        .mark_lecture_notes_generating(&job.lecture_key, &conversation_id)
        .await?;
    info!(
        "Phase 3 Notes 已提交: lecture_key={}, conversation_id={}",
        job.lecture_key, conversation_id
    );
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
        return process_title_stage(state, job, conversation_id, &client, base).await;
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
            && (message
                .get("endTurn")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || message.get("finishType").and_then(Value::as_str).is_some()
                || message.get("channel").and_then(Value::as_str) == Some("final"))
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

    if job.title_requested_at.is_none() {
        // title-only 提交也可能出现“服务端已接受，但本地没收到响应”。
        // 在任何重新提交前，先读取 canonical messages 检查我们自己的标题 prompt 是否已存在。
        // 只有明确不存在时才允许继续提交；读不到 canonical 状态时宁可等待，也不冒重复 title turn 的风险。
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
                        &format!("标题提交前核对已有会话失败，暂不重发: {error}"),
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
                        "标题提交前核对已有会话失败，暂不重发: HTTP {}",
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
                "Phase 3 Notes 已认领既有标题请求，避免重复提交: lecture_key={}, conversation_id={}",
                job.lecture_key, conversation_id
            );
            return Ok(());
        }

        if notes_submission_cooldown_active(state).await {
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    "Archive 仍处于全局 429 冷却，标题请求暂缓",
                )
                .await?;
            return Ok(());
        }

        let title_prompt = build_title_prompt(&existing_titles, None);
        let request = apply_bridge_auth(
            client.post(format!("{base}/v1/turns")).json(&json!({
                "mode": "continue",
                "conversationId": conversation_id,
                "text": title_prompt,
            })),
            state,
        );
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) if error.is_connect() => {
                state
                    .repo
                    .update_lecture_notes_error(
                        &job.lecture_key,
                        &format!("标题请求等待 Archive 恢复: {error}"),
                    )
                    .await?;
                return Ok(());
            }
            Err(error) => {
                state
                    .repo
                    .mark_lecture_notes_failed(
                        &job.lecture_key,
                        &format!("标题请求提交结果不确定，等待人工重试: {error}"),
                    )
                    .await?;
                return Ok(());
            }
        };
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.as_u16() == 429 {
            start_notes_429_cooldown(state, &body).await;
            state
                .repo
                .update_lecture_notes_error(
                    &job.lecture_key,
                    &format!(
                        "标题请求遇到 Archive 429，已进入全局冷却: {}",
                        truncate(&body, 200)
                    ),
                )
                .await?;
            return Ok(());
        }
        if !status.is_success() {
            state
                .repo
                .mark_lecture_notes_failed(
                    &job.lecture_key,
                    &format!("标题请求提交失败: HTTP {status} {}", truncate(&body, 200)),
                )
                .await?;
            return Ok(());
        }
        let payload: Value = serde_json::from_str(&body)
            .map_err(|error| AppError::Internal(format!("解析标题 submit 响应失败: {error}")))?;
        let returned_cid = payload
            .pointer("/result/conversationId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if returned_cid != conversation_id {
            state
                .repo
                .mark_lecture_notes_failed(
                    &job.lecture_key,
                    "标题请求没有继续原 conversation，已停止自动处理",
                )
                .await?;
            return Ok(());
        }
        state
            .repo
            .mark_lecture_notes_title_requested(&job.lecture_key)
            .await?;
        info!(
            "Phase 3 Notes 标题请求已提交: lecture_key={}, conversation_id={}",
            job.lecture_key, conversation_id
        );
        return Ok(());
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
            && (message
                .get("endTurn")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || message.get("finishType").and_then(Value::as_str).is_some()
                || message.get("channel").and_then(Value::as_str) == Some("final"))
    });
    let Some(title_message) = title_message else {
        return Ok(());
    };
    let title = normalize_title_response(
        title_message
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let rejection = title_rejection_reason(&title, &existing_titles);
    if let Some(reason) = rejection {
        let marker = format!("标题重做请求已提交: {title}");
        if job.last_error.as_deref() == Some(marker.as_str()) {
            return Ok(());
        }
        let retry_prompt = build_title_prompt(&existing_titles, Some((&title, reason)));
        let response = apply_bridge_auth(
            client.post(format!("{base}/v1/turns")).json(&json!({
                "mode": "continue",
                "conversationId": conversation_id,
                "text": retry_prompt,
            })),
            state,
        )
        .send()
        .await;
        let Ok(response) = response else {
            return Ok(());
        };
        if response.status().is_success() {
            state
                .repo
                .update_lecture_notes_error(&job.lecture_key, &marker)
                .await?;
        }
        return Ok(());
    }
    state
        .repo
        .mark_lecture_notes_publishing(&job.lecture_key, &title)
        .await?;
    Ok(())
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
    fn should_parse_archive_explicit_cooldown_deadline() {
        let parsed = parse_archive_cooldown_until(
            r#"{"error":"ChatGPT actor blocked by active API cooldown until 2026-10-10T19:27:38.462Z"}"#,
        )
        .expect("应能解析 Archive 明示的 cooldown 截止时间");
        assert_eq!(
            parsed.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "2026-10-10T19:27:38.462Z"
        );
    }

    #[test]
    fn notes_429_cooldown_should_only_be_active_before_deadline() {
        let now = Utc::now();
        assert!(cooldown_until_is_active(
            Some(now + chrono::Duration::seconds(60)),
            now
        ));
        assert!(!cooldown_until_is_active(
            Some(now - chrono::Duration::seconds(1)),
            now
        ));
        assert!(!cooldown_until_is_active(None, now));
    }

    #[test]
    fn path_component_removes_windows_and_github_hostile_chars() {
        assert_eq!(
            sanitize_path_component("肾病/肿瘤:总论?", "笔记"),
            "肾病-肿瘤-总论-"
        );
    }
}
