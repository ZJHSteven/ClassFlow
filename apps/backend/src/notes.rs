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
    let mut submitted_this_cycle = false;
    for job in jobs {
        match job.status {
            LectureNotesStatus::Pending if !submitted_this_cycle => {
                process_pending(&state, &job).await?;
                submitted_this_cycle = true;
            }
            LectureNotesStatus::Generating => process_generating(&state, &job).await?,
            LectureNotesStatus::Publishing => process_publishing(&state, &job).await?,
            LectureNotesStatus::Pending
            | LectureNotesStatus::Succeeded
            | LectureNotesStatus::Failed => {}
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
    let transcript = lecture
        .combined_transcript()
        .ok_or_else(|| AppError::BadRequest("完整课堂 ASR 尚未齐备，不能生成 Notes".to_string()))?;
    let transcript_hash = sha256_text(&transcript);
    match state.repo.get_lecture_notes(lecture_key).await? {
        Some(existing) if existing.status == LectureNotesStatus::Failed => {
            state.repo.retry_lecture_notes(lecture_key).await?;
            state
                .repo
                .get_lecture_notes(lecture_key)
                .await?
                .ok_or_else(|| AppError::Internal("重试 Notes 后无法读回任务".to_string()))
        }
        Some(existing) => Ok(existing),
        None => {
            state
                .repo
                .ensure_lecture_notes_job(&lecture, &transcript_hash)
                .await
        }
    }
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
            if !lecture.transcript_ready() || !lecture_is_closed(&lecture, now) {
                continue;
            }
            let Some(transcript) = lecture.combined_transcript() else {
                continue;
            };
            state
                .repo
                .ensure_lecture_notes_job(&lecture, &sha256_text(&transcript))
                .await?;
        }
    }
    Ok(())
}

pub fn lecture_is_closed(lecture: &LectureGroup, now: chrono::DateTime<Utc>) -> bool {
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

async fn process_pending(state: &AppState, job: &LectureNotesRecord) -> AppResult<()> {
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
    let transcript = lecture
        .combined_transcript()
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
            "text": transcript,
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
                .mark_lecture_notes_failed(
                    &job.lecture_key,
                    &format!("Archive 提交结果不确定，已停止自动重试: {error}"),
                )
                .await?;
            return Ok(());
        }
    };

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if matches!(status.as_u16(), 429 | 502 | 503 | 504) {
        state
            .repo
            .mark_lecture_notes_pending_error(
                &job.lecture_key,
                &format!("Archive 暂不可用: HTTP {status} {}", truncate(&body, 180)),
            )
            .await?;
        return Ok(());
    }
    if !status.is_success() {
        state
            .repo
            .mark_lecture_notes_failed(
                &job.lecture_key,
                &format!("Archive 提交失败: HTTP {status} {}", truncate(&body, 240)),
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
        .ok_or_else(|| {
            AppError::Internal("Archive 已接受提交但没有返回 conversationId".to_string())
        })?;
    state
        .repo
        .mark_lecture_notes_generating(&job.lecture_key, conversation_id)
        .await?;
    info!(
        "Phase 3 Notes 已提交: lecture_key={}, conversation_id={}",
        job.lecture_key, conversation_id
    );
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
    let markdown = final_message
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if markdown.is_empty() {
        return Ok(());
    }
    let conversation_title = detail
        .pointer("/conversation/title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = derive_notes_title(markdown, conversation_title, &job.course_name, &job.date);
    state
        .repo
        .mark_lecture_notes_publishing(&job.lecture_key, &title, markdown, &sha256_text(markdown))
        .await?;
    Ok(())
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
    let time_part = format!(
        "{}-{}",
        job.start_time.replace(':', ""),
        job.end_time.replace(':', "")
    );
    let audio_path = format!("{course_dir}/Audio/{}-{time_part}-ASR.md", job.date);
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
    fn path_component_removes_windows_and_github_hostile_chars() {
        assert_eq!(
            sanitize_path_component("肾病/肿瘤:总论?", "笔记"),
            "肾病-肿瘤-总论-"
        );
    }
}
