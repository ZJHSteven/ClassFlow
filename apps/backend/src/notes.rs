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
const TITLE_PROMPT: &str = "请只为你刚刚生成的整堂课堂笔记起一个简洁、具体、可区分的中文主题名。要求：1）概括整堂课最核心的知识主题；2）不要包含课程名、日期、老师姓名、‘课堂笔记’、‘理论课’、‘第几部分’等元信息；3）尽量控制在 12～32 个汉字，必要时可用顿号、‘与’连接多个并列主题；4）只输出标题本身，不要加序号、引号、解释、Markdown 标记或句号。";

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

    if job.title_requested_at.is_none() {
        let request = apply_bridge_auth(
            client.post(format!("{base}/v1/turns")).json(&json!({
                "mode": "continue",
                "conversationId": conversation_id,
                "text": TITLE_PROMPT,
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
    if title.is_empty() {
        state
            .repo
            .mark_lecture_notes_failed(&job.lecture_key, "标题模型返回空标题")
            .await?;
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
    truncate(&title, 40)
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
    fn path_component_removes_windows_and_github_hostile_chars() {
        assert_eq!(
            sanitize_path_component("肾病/肿瘤:总论?", "笔记"),
            "肾病-肿瘤-总论-"
        );
    }
}
