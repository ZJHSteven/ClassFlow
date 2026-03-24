/*!
该模块封装“视频 -> 音频 -> 百炼转写”的外部流水线。

这里故意把外部交互切成四个明确阶段：

1. 下载视频。
2. FFmpeg 抽取音频。
3. 上传音频到百炼临时 OSS。
4. 提交异步转写并轮询结果。

好处是：

1. worker 可以在每个阶段更新任务状态。
2. 测试时可以只替换这一层，不必真的访问网络或执行 ffmpeg。
*/

use std::{
    collections::HashMap,
    future::Future,
    path::Path,
    process::ExitStatus,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use http_body::Frame;
use http_body_util::StreamBody;
use reqwest::{
    Body, Client,
    multipart::{Form, Part},
};
use serde_json::{Value, json};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::{Semaphore, mpsc},
};
use tokio_util::io::ReaderStream;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    config::AppConfig,
    error::{AppError, AppResult},
    models::NormalizedTranscript,
};

#[derive(Debug, Clone, Default)]
pub struct TransferProgressSnapshot {
    pub progress_percent: Option<f64>,
    pub transferred_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub rate_bytes_per_sec: Option<u64>,
    pub eta_seconds: Option<u64>,
}

/**
 * 汇总命令执行期间采集到的标准输出、标准错误与最新进度快照。
 *
 * 下载阶段之所以单独抽这个结构，是因为我们有两个互相独立的需求：
 *
 * 1. 运行中就要把最新进度立刻写回数据库，供前端轮询展示。
 * 2. 命令最终失败时，仍然要把完整 stdout / stderr 拼进错误信息，便于排障。
 *
 * 这两个目标都需要“边跑边读”，而不是等进程结束后再统一处理。
 */
#[derive(Debug)]
struct CommandOutputSummary {
    exit_status: ExitStatus,
    stdout_lines: Vec<String>,
    stderr_lines: Vec<String>,
    latest_progress: TransferProgressSnapshot,
}

#[async_trait]
pub trait ProgressSink: Send + Sync {
    async fn report(&self, snapshot: TransferProgressSnapshot) -> AppResult<()>;
}

#[async_trait]
pub trait PipelineIo: Send + Sync {
    async fn download_video(
        &self,
        url: &str,
        target_path: &Path,
        progress_sink: Option<Arc<dyn ProgressSink>>,
    ) -> AppResult<()>;
    async fn extract_audio(
        &self,
        source_video_path: &Path,
        target_audio_path: &Path,
    ) -> AppResult<()>;
    async fn upload_audio_for_transcription(
        &self,
        audio_path: &Path,
        progress_sink: Option<Arc<dyn ProgressSink>>,
    ) -> AppResult<String>;
    async fn transcribe_file_url(&self, file_url: &str) -> AppResult<NormalizedTranscript>;
    async fn cleanup_dir(&self, dir_path: &Path) -> AppResult<()>;
}

pub struct RealPipelineIo {
    client: Client,
    config: AppConfig,
    download_semaphore: Arc<Semaphore>,
    upload_semaphore: Arc<Semaphore>,
    transcribe_semaphore: Arc<Semaphore>,
}

impl RealPipelineIo {
    pub fn new(config: AppConfig) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            client,
            download_semaphore: Arc::new(Semaphore::new(config.download_concurrency.max(1))),
            upload_semaphore: Arc::new(Semaphore::new(config.upload_concurrency.max(1))),
            transcribe_semaphore: Arc::new(Semaphore::new(config.transcribe_concurrency.max(1))),
            config,
        }
    }

    /**
     * 对“可能只是网络抖动”的操作执行有限次重试。
     *
     * 这里只重试外部/IO 错误，不重试配置错误、鉴权错误或参数错误。
     * 这样既能增强鲁棒性，又能避免把明显的坏配置无意义地重试很多次。
     */
    async fn run_with_retry<T, Factory, Fut>(
        &self,
        label: &str,
        attempts: u32,
        wait_secs: f64,
        mut factory: Factory,
    ) -> AppResult<T>
    where
        Factory: FnMut() -> Fut,
        Fut: Future<Output = AppResult<T>>,
    {
        let total_attempts = attempts.max(1);
        let wait_duration = Duration::from_secs_f64(wait_secs.max(0.0));

        for attempt in 1..=total_attempts {
            match factory().await {
                Ok(result) => return Ok(result),
                Err(error) => {
                    if !is_retryable_error(&error) || attempt == total_attempts {
                        return Err(error);
                    }

                    warn!(
                        label,
                        attempt,
                        total_attempts,
                        error = %error,
                        "外部步骤失败，准备重试"
                    );
                    if !wait_duration.is_zero() {
                        tokio::time::sleep(wait_duration).await;
                    }
                }
            }
        }

        Err(AppError::Internal(format!(
            "{label} 进入了不可能到达的重试分支"
        )))
    }

    async fn request_dashscope_policy(&self) -> AppResult<Value> {
        self.run_with_retry(
            "获取百炼上传凭证",
            self.config.dashscope_request_retry_attempts,
            self.config.dashscope_request_retry_wait_secs,
            || async {
                let response = self
                    .client
                    .get(&self.config.dashscope_upload_policy_url)
                    .query(&[
                        ("action", "getPolicy"),
                        ("model", self.config.dashscope_model.as_str()),
                    ])
                    .bearer_auth(&self.config.dashscope_api_key)
                    .timeout(Duration::from_secs_f64(
                        self.config.dashscope_request_timeout_secs.max(1.0),
                    ))
                    .send()
                    .await?;

                ensure_success(
                    response.status().as_u16(),
                    response.text().await?,
                    "获取百炼上传凭证失败",
                )
            },
        )
        .await
    }

    async fn submit_transcription_task(&self, file_url: &str) -> AppResult<String> {
        self.run_with_retry(
            "提交百炼转写任务",
            self.config.dashscope_request_retry_attempts,
            self.config.dashscope_request_retry_wait_secs,
            || async {
                let mut request = self
                    .client
                    .post(&self.config.dashscope_submit_url)
                    .bearer_auth(&self.config.dashscope_api_key)
                    .header("Content-Type", "application/json")
                    .header("X-DashScope-Async", "enable");

                if file_url.trim().to_lowercase().starts_with("oss://") {
                    request = request.header("X-DashScope-OssResourceResolve", "enable");
                }

                let response = request
                    .json(&json!({
                        "model": self.config.dashscope_model,
                        "input": {
                            "file_urls": [file_url]
                        },
                        "parameters": {
                            "channel_id": [0]
                        }
                    }))
                    .timeout(Duration::from_secs_f64(
                        self.config.dashscope_request_timeout_secs.max(1.0),
                    ))
                    .send()
                    .await?;

                let data = ensure_success(
                    response.status().as_u16(),
                    response.text().await?,
                    "提交百炼转写任务失败",
                )?;

                pick_string(&data, &["output.task_id", "data.task_id"]).ok_or_else(|| {
                    AppError::External(format!("百炼提交成功但没有返回 task_id: {data}"))
                })
            },
        )
        .await
    }

    async fn poll_transcription_task(&self, task_id: &str) -> AppResult<Value> {
        let task_url = self
            .config
            .dashscope_task_url_template
            .replace("{task_id}", task_id);

        let started_at = std::time::Instant::now();
        loop {
            if started_at.elapsed().as_secs_f64() > self.config.dashscope_poll_timeout_secs {
                return Err(AppError::External(format!(
                    "百炼转写任务轮询超时: {task_id}"
                )));
            }

            let data = self
                .run_with_retry(
                    "查询百炼转写任务",
                    self.config.dashscope_request_retry_attempts,
                    self.config.dashscope_request_retry_wait_secs,
                    || async {
                        let response = self
                            .client
                            .get(&task_url)
                            .bearer_auth(&self.config.dashscope_api_key)
                            .timeout(Duration::from_secs_f64(
                                self.config.dashscope_request_timeout_secs.max(1.0),
                            ))
                            .send()
                            .await?;

                        ensure_success(
                            response.status().as_u16(),
                            response.text().await?,
                            "查询百炼转写任务失败",
                        )
                    },
                )
                .await?;

            let status = pick_string(&data, &["output.task_status"]).unwrap_or_default();
            match status.as_str() {
                "SUCCEEDED" => return Ok(data),
                "FAILED" | "CANCELED" => {
                    return Err(AppError::External(format!("百炼转写任务失败: {}", data)));
                }
                _ => {
                    tokio::time::sleep(std::time::Duration::from_secs_f64(
                        self.config.dashscope_poll_interval_secs,
                    ))
                    .await
                }
            }
        }
    }

    async fn download_transcription_url(&self, url: &str) -> AppResult<Value> {
        self.run_with_retry(
            "下载百炼转写结果文件",
            self.config.dashscope_request_retry_attempts,
            self.config.dashscope_request_retry_wait_secs,
            || async {
                let response = self
                    .client
                    .get(url)
                    .timeout(Duration::from_secs_f64(
                        self.config.dashscope_request_timeout_secs.max(1.0),
                    ))
                    .send()
                    .await?;
                ensure_success(
                    response.status().as_u16(),
                    response.text().await?,
                    "下载 transcription_url 失败",
                )
            },
        )
        .await
    }

    async fn parse_transcript_payload(
        &self,
        task_output: Value,
    ) -> AppResult<NormalizedTranscript> {
        let results = task_output
            .get("output")
            .and_then(|value| value.get("results"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AppError::External(format!("百炼结果缺少 output.results: {task_output}"))
            })?;

        for item in results {
            if let Some(subtask_status) = pick_string(item, &["subtask_status"])
                && subtask_status != "SUCCEEDED"
                && subtask_status != "SUCCESS"
            {
                return Err(AppError::External(format!(
                    "百炼子任务失败: status={subtask_status}, payload={item}"
                )));
            }
        }

        let first = results
            .first()
            .ok_or_else(|| AppError::External("百炼返回了空结果数组".to_string()))?;

        let transcripts = if let Some(items) = first.get("transcripts").and_then(Value::as_array) {
            items.to_vec()
        } else if let Some(url) = pick_string(first, &["transcription_url"]) {
            let downloaded = self.download_transcription_url(&url).await?;
            downloaded
                .get("data")
                .unwrap_or(&downloaded)
                .get("transcripts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let transcript = transcripts.first().ok_or_else(|| {
            AppError::External(format!("百炼结果中没有 transcripts: {task_output}"))
        })?;

        let text_display = pick_string(transcript, &["text"]).unwrap_or_default();
        let sentences = transcript
            .get("sentences")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut tokens = Vec::new();
        let mut timestamps = Vec::new();
        let mut text_accu = String::new();

        for sentence in &sentences {
            if let Some(text) = pick_string(sentence, &["text"]) {
                text_accu.push_str(&text);
            }

            if let Some(words) = sentence.get("words").and_then(Value::as_array) {
                for word in words {
                    let text = pick_string(word, &["text"]).unwrap_or_default();
                    let punctuation = pick_string(word, &["punctuation"]).unwrap_or_default();
                    let token = format!("{text}{punctuation}").trim().to_string();
                    if token.is_empty() {
                        continue;
                    }

                    if let Some(begin_ms) = word.get("begin_time").and_then(Value::as_f64) {
                        tokens.push(token);
                        timestamps.push(begin_ms / 1000.0);
                    }
                }
            }
        }

        if text_accu.trim().is_empty() {
            text_accu = text_display.clone();
        }

        let duration_seconds = transcript
            .get("content_duration_in_milliseconds")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            / 1000.0;

        if tokens.is_empty() && !text_accu.is_empty() {
            let chars = text_accu
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .map(|ch| ch.to_string())
                .collect::<Vec<_>>();

            if !chars.is_empty() {
                let unit = if duration_seconds > 0.0 {
                    duration_seconds / chars.len() as f64
                } else {
                    0.0
                };

                for (index, ch) in chars.into_iter().enumerate() {
                    tokens.push(ch);
                    timestamps.push(index as f64 * unit);
                }
            }
        }

        Ok(NormalizedTranscript {
            text_display,
            text_accu,
            tokens,
            timestamps,
            duration_seconds,
            raw_task_output: task_output,
        })
    }
}

#[async_trait]
impl PipelineIo for RealPipelineIo {
    async fn download_video(
        &self,
        url: &str,
        target_path: &Path,
        progress_sink: Option<Arc<dyn ProgressSink>>,
    ) -> AppResult<()> {
        let _permit = self
            .download_semaphore
            .acquire()
            .await
            .map_err(|error| AppError::Internal(format!("下载信号量获取失败: {error}")))?;

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let output_name = target_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                AppError::Internal(format!("下载目标文件名非法: {}", target_path.display()))
            })?;
        let output_dir = target_path.parent().ok_or_else(|| {
            AppError::Internal(format!("下载目标缺少父目录: {}", target_path.display()))
        })?;

        if let Some(sink) = &progress_sink {
            sink.report(TransferProgressSnapshot::default()).await?;
        }

        let mut command = Command::new(&self.config.aria2_bin);
        command
            .arg("--continue=true")
            .arg("--auto-file-renaming=false")
            .arg("--allow-overwrite=true")
            .arg("--summary-interval=1")
            .arg("--console-log-level=notice")
            .arg("--file-allocation=none")
            .arg(format!(
                "--max-tries={}",
                self.config.download_retry_attempts.max(1)
            ))
            .arg(format!(
                "--retry-wait={}",
                self.config.download_retry_wait_secs.max(0.0).ceil() as u64
            ))
            .arg(format!(
                "--connect-timeout={}",
                self.config.download_connect_timeout_secs.max(1.0).ceil() as u64
            ))
            .arg(format!(
                "--timeout={}",
                self.config.download_timeout_secs.max(1.0).ceil() as u64
            ))
            .arg(format!("--split={}", self.config.download_split.max(1)))
            .arg(format!(
                "--max-connection-per-server={}",
                self.config.download_connections_per_server.max(1)
            ))
            .arg("--max-resume-failure-tries=0")
            .arg("--dir")
            .arg(output_dir)
            .arg("--out")
            .arg(output_name)
            .arg(url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if self.config.download_lowest_speed_limit_bytes > 0 {
            command.arg(format!(
                "--lowest-speed-limit={}",
                self.config.download_lowest_speed_limit_bytes
            ));
        }

        let mut child = command
            .spawn()
            .map_err(|error| AppError::External(format!("调用 aria2c 失败: {error}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Internal("aria2 stdout 管道获取失败".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::Internal("aria2 stderr 管道获取失败".to_string()))?;

        let (line_tx, line_rx) = mpsc::unbounded_channel::<(bool, String)>();
        let stdout_handle = tokio::spawn(read_command_lines(stdout, false, line_tx.clone()));
        let stderr_handle = tokio::spawn(read_command_lines(stderr, true, line_tx.clone()));
        drop(line_tx);

        let command_output =
            collect_command_output_while_running(child.wait(), line_rx, progress_sink.clone())
                .await?;

        stdout_handle.await.map_err(|error| {
            AppError::Internal(format!("读取 aria2 stdout 任务失败: {error}"))
        })??;
        stderr_handle.await.map_err(|error| {
            AppError::Internal(format!("读取 aria2 stderr 任务失败: {error}"))
        })??;

        if !command_output.exit_status.success() {
            let stderr = command_output.stderr_lines.join("\n");
            let stdout = command_output.stdout_lines.join("\n");
            return Err(AppError::External(format!(
                "aria2 下载视频失败: {}，stderr={}，stdout={}",
                describe_aria2_exit(command_output.exit_status.code()),
                stderr,
                stdout
            )));
        }

        ensure_non_empty_file(target_path, "aria2 下载完成后目标文件为空").await?;
        if let Some(sink) = &progress_sink {
            let completed_bytes = fs::metadata(target_path)
                .await
                .map(|metadata| metadata.len())
                .ok();
            sink.report(TransferProgressSnapshot {
                progress_percent: Some(100.0),
                transferred_bytes: completed_bytes,
                total_bytes: completed_bytes.or(command_output.latest_progress.total_bytes),
                rate_bytes_per_sec: command_output.latest_progress.rate_bytes_per_sec,
                eta_seconds: Some(0),
            })
            .await?;
        }
        Ok(())
    }

    async fn extract_audio(
        &self,
        source_video_path: &Path,
        target_audio_path: &Path,
    ) -> AppResult<()> {
        if let Some(parent) = target_audio_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let output = Command::new("ffmpeg")
            .arg("-y")
            .arg("-i")
            .arg(source_video_path)
            .arg("-vn")
            .arg("-ac")
            .arg("1")
            .arg("-ar")
            .arg("16000")
            .arg("-c:a")
            .arg("pcm_s16le")
            .arg(target_audio_path)
            .output()
            .await
            .map_err(|error| AppError::External(format!("调用 ffmpeg 失败: {error}")))?;

        if !output.status.success() {
            return Err(AppError::External(format!(
                "ffmpeg 抽音频失败: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }

    async fn upload_audio_for_transcription(
        &self,
        audio_path: &Path,
        progress_sink: Option<Arc<dyn ProgressSink>>,
    ) -> AppResult<String> {
        let _permit = self
            .upload_semaphore
            .acquire()
            .await
            .map_err(|error| AppError::Internal(format!("上传信号量获取失败: {error}")))?;

        if self.config.dashscope_api_key.is_empty() {
            return Err(AppError::Config(
                "缺少 DASHSCOPE_API_KEY，无法上传音频并转写".to_string(),
            ));
        }

        self.run_with_retry(
            "上传音频到百炼临时 OSS",
            self.config.upload_retry_attempts,
            self.config.upload_retry_wait_secs,
            || async {
                let file_metadata = fs::metadata(audio_path).await?;
                let total_bytes = file_metadata.len();
                if let Some(sink) = &progress_sink {
                    sink.report(TransferProgressSnapshot {
                        progress_percent: Some(0.0),
                        transferred_bytes: Some(0),
                        total_bytes: Some(total_bytes),
                        rate_bytes_per_sec: Some(0),
                        eta_seconds: None,
                    })
                    .await?;
                }

                let policy = self.request_dashscope_policy().await?;
                let policy_data = policy.get("data").unwrap_or(&policy);
                let upload_host = pick_string(policy_data, &["upload_host", "uploadHost"])
                    .ok_or_else(|| {
                        AppError::External(format!("百炼上传凭证缺少 upload_host: {policy}"))
                    })?;
                let upload_dir = pick_string(policy_data, &["upload_dir", "uploadDir"])
                    .ok_or_else(|| {
                        AppError::External(format!("百炼上传凭证缺少 upload_dir: {policy}"))
                    })?;
                let policy_text = pick_string(policy_data, &["policy"]).ok_or_else(|| {
                    AppError::External(format!("百炼上传凭证缺少 policy: {policy}"))
                })?;
                let signature = pick_string(policy_data, &["signature"]).ok_or_else(|| {
                    AppError::External(format!("百炼上传凭证缺少 signature: {policy}"))
                })?;
                let access_key_id = pick_string(
                    policy_data,
                    &["oss_access_key_id", "ossAccessKeyId", "OSSAccessKeyId"],
                )
                .ok_or_else(|| {
                    AppError::External(format!("百炼上传凭证缺少 oss_access_key_id: {policy}"))
                })?;

                let security_token = pick_string(
                    policy_data,
                    &[
                        "x_oss_security_token",
                        "x-oss-security-token",
                        "xOssSecurityToken",
                    ],
                );

                let filename = audio_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("audio.wav");
                let object_key = format!(
                    "{}/{}_{}",
                    upload_dir.trim_matches('/'),
                    Uuid::new_v4().simple(),
                    filename
                );

                let mut form_map = HashMap::from([
                    ("key".to_string(), object_key.clone()),
                    ("policy".to_string(), policy_text),
                    ("OSSAccessKeyId".to_string(), access_key_id),
                    ("Signature".to_string(), signature),
                    ("success_action_status".to_string(), "200".to_string()),
                ]);

                if let Some(token) = security_token {
                    form_map.insert("x-oss-security-token".to_string(), token);
                }

                if let Some(object) = policy_data.as_object() {
                    for (key, value) in object {
                        if !key.starts_with("x_oss_") || value.is_null() {
                            continue;
                        }
                        let mapped_key = key.replace("x_oss_", "x-oss-").replace('_', "-");
                        form_map.entry(mapped_key).or_insert_with(|| match value {
                            Value::String(text) => text.clone(),
                            _ => value.to_string(),
                        });
                    }
                }

                let mut form = Form::new();
                for (key, value) in form_map {
                    form = form.text(key, value);
                }

                let file = fs::File::open(audio_path).await?;
                let started_at = Instant::now();
                let transferred_bytes = Arc::new(AtomicU64::new(0));
                let last_reported_bytes = Arc::new(AtomicU64::new(0));
                let progress_sink_for_stream = progress_sink.clone();
                let transferred_bytes_for_stream = transferred_bytes.clone();
                let last_reported_bytes_for_stream = last_reported_bytes.clone();

                let stream = ReaderStream::new(file).inspect_ok(move |chunk| {
                    let current = transferred_bytes_for_stream
                        .fetch_add(chunk.len() as u64, Ordering::Relaxed)
                        + chunk.len() as u64;
                    let last_reported = last_reported_bytes_for_stream.load(Ordering::Relaxed);
                    let should_report = current == total_bytes
                        || current.saturating_sub(last_reported) >= 256 * 1024;
                    if !should_report {
                        return;
                    }

                    last_reported_bytes_for_stream.store(current, Ordering::Relaxed);
                    let elapsed = started_at.elapsed().as_secs_f64();
                    let rate = if elapsed > 0.0 {
                        Some((current as f64 / elapsed).round() as u64)
                    } else {
                        Some(0)
                    };
                    let remaining = total_bytes.saturating_sub(current);
                    let eta_seconds = rate.and_then(|speed| {
                        if speed == 0 {
                            None
                        } else {
                            Some((remaining as f64 / speed as f64).ceil() as u64)
                        }
                    });
                    let snapshot = TransferProgressSnapshot {
                        progress_percent: Some(calculate_progress_percent(
                            current,
                            Some(total_bytes),
                        )),
                        transferred_bytes: Some(current),
                        total_bytes: Some(total_bytes),
                        rate_bytes_per_sec: rate,
                        eta_seconds,
                    };
                    if let Some(sink) = &progress_sink_for_stream {
                        let sink = sink.clone();
                        tokio::spawn(async move {
                            let _ = sink.report(snapshot).await;
                        });
                    }
                });
                let request_body = Body::wrap(StreamBody::new(stream.map_ok(Frame::data)));
                form = form.part(
                    "file",
                    Part::stream(request_body)
                        .file_name(filename.to_string())
                        .mime_str("audio/wav")
                        .map_err(|error| {
                            AppError::Internal(format!("音频 MIME 设置失败: {error}"))
                        })?,
                );

                let normalized_host =
                    if upload_host.starts_with("http://") || upload_host.starts_with("https://") {
                        upload_host
                    } else {
                        format!("https://{}", upload_host.trim_start_matches('/'))
                    };

                let response = self
                    .client
                    .post(normalized_host)
                    .multipart(form)
                    .timeout(Duration::from_secs_f64(
                        self.config.upload_timeout_secs.max(1.0),
                    ))
                    .send()
                    .await?;

                let status = response.status().as_u16();
                let text = response.text().await?;
                if !matches!(status, 200 | 201 | 204) {
                    return Err(AppError::External(format!(
                        "上传音频到百炼临时 OSS 失败，HTTP={status}，响应={text}"
                    )));
                }

                if let Some(sink) = &progress_sink {
                    sink.report(TransferProgressSnapshot {
                        progress_percent: Some(100.0),
                        transferred_bytes: Some(total_bytes),
                        total_bytes: Some(total_bytes),
                        rate_bytes_per_sec: Some(
                            (total_bytes as f64 / started_at.elapsed().as_secs_f64().max(0.001))
                                .round() as u64,
                        ),
                        eta_seconds: Some(0),
                    })
                    .await?;
                }

                Ok(format!("oss://{object_key}"))
            },
        )
        .await
    }

    async fn transcribe_file_url(&self, file_url: &str) -> AppResult<NormalizedTranscript> {
        let _permit = self
            .transcribe_semaphore
            .acquire()
            .await
            .map_err(|error| AppError::Internal(format!("转写信号量获取失败: {error}")))?;

        let task_id = self.submit_transcription_task(file_url).await?;
        info!("百炼任务已提交: {task_id}");
        let task_output = self.poll_transcription_task(&task_id).await?;
        self.parse_transcript_payload(task_output).await
    }

    async fn cleanup_dir(&self, dir_path: &Path) -> AppResult<()> {
        if dir_path.exists() {
            fs::remove_dir_all(dir_path).await?;
        }
        Ok(())
    }
}

async fn ensure_non_empty_file(path: &Path, error_message: &str) -> AppResult<()> {
    let metadata = fs::metadata(path).await.map_err(|error| {
        AppError::Io(format!(
            "读取文件元信息失败: path={}, error={error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(AppError::External(format!(
            "{error_message}: {}",
            path.display()
        )));
    }
    Ok(())
}

async fn read_command_lines<R>(
    reader: R,
    is_stderr: bool,
    sender: mpsc::UnboundedSender<(bool, String)>,
) -> AppResult<()>
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let _ = sender.send((is_stderr, line));
    }
    Ok(())
}

/**
 * 在命令执行期间持续消费 stdout / stderr，并把可解析出的进度实时推给上层。
 *
 * 这是这次修复的关键点：
 *
 * 1. 不能先 `wait()` 再读管道，否则数据库要等命令结束才会收到进度。
 * 2. 也不能只顾着读管道不等进程结束，否则会丢掉最终退出码，无法正确判断成功/失败。
 *
 * 因此这里采用 `tokio::select!` 同时等待两件事：
 *
 * - 子进程退出；
 * - 管道里继续有新行到达。
 *
 * 只有在“退出码已拿到”且“输出管道也完全读空”之后，才允许真正返回。
 */
async fn collect_command_output_while_running<F>(
    wait_future: F,
    mut line_rx: mpsc::UnboundedReceiver<(bool, String)>,
    progress_sink: Option<Arc<dyn ProgressSink>>,
) -> AppResult<CommandOutputSummary>
where
    F: Future<Output = Result<ExitStatus, std::io::Error>>,
{
    let mut stdout_lines = Vec::new();
    let mut stderr_lines = Vec::new();
    let mut latest_progress = TransferProgressSnapshot::default();
    let mut exit_status = None;
    let mut channel_closed = false;
    tokio::pin!(wait_future);

    loop {
        if channel_closed && exit_status.is_some() {
            break;
        }

        tokio::select! {
            status = &mut wait_future, if exit_status.is_none() => {
                exit_status = Some(
                    status.map_err(|error| AppError::External(format!("等待 aria2c 结束失败: {error}")))?
                );
            }
            maybe_line = line_rx.recv(), if !channel_closed => {
                match maybe_line {
                    Some((is_stderr, line)) => {
                        if is_stderr {
                            stderr_lines.push(line.clone());
                        } else {
                            stdout_lines.push(line.clone());
                        }

                        if let Some(snapshot) = parse_aria2_progress_line(&line) {
                            latest_progress = snapshot.clone();
                            if let Some(sink) = &progress_sink {
                                sink.report(snapshot).await?;
                            }
                        }
                    }
                    None => {
                        channel_closed = true;
                    }
                }
            }
        }
    }

    let exit_status =
        exit_status.ok_or_else(|| AppError::Internal("aria2 退出状态未成功采集".to_string()))?;

    Ok(CommandOutputSummary {
        exit_status,
        stdout_lines,
        stderr_lines,
        latest_progress,
    })
}

fn parse_aria2_progress_line(line: &str) -> Option<TransferProgressSnapshot> {
    let candidate = line
        .split('[')
        .filter_map(|chunk| chunk.split(']').next())
        .rev()
        .find(|chunk| chunk.contains("DL:") && chunk.contains('/') && chunk.contains('%'))?;
    let tokens = candidate.split_whitespace().collect::<Vec<_>>();
    if tokens.len() < 4 {
        return None;
    }

    let progress_token = tokens
        .iter()
        .find(|token| token.contains('/') && token.contains('(') && token.contains("%)"))?;
    let (done_text, total_with_percent) = progress_token.split_once('/')?;
    let (total_text, percent_with_suffix) = total_with_percent.split_once('(')?;
    let percent = percent_with_suffix
        .trim_end_matches("%)")
        .parse::<f64>()
        .ok();

    let transferred_bytes = parse_human_bytes(done_text);
    let total_bytes = parse_human_bytes(total_text);
    let rate_bytes_per_sec = tokens
        .iter()
        .find_map(|token| token.strip_prefix("DL:"))
        .and_then(parse_human_bytes);
    let eta_seconds = tokens
        .iter()
        .find_map(|token| token.strip_prefix("ETA:"))
        .and_then(parse_eta_seconds);

    let computed_percent = match (percent, transferred_bytes, total_bytes) {
        (Some(value), _, _) => Some(value),
        (None, Some(transferred), total) => Some(calculate_progress_percent(transferred, total)),
        (None, None, _) => None,
    };

    Some(TransferProgressSnapshot {
        progress_percent: computed_percent,
        transferred_bytes,
        total_bytes,
        rate_bytes_per_sec,
        eta_seconds,
    })
}

fn parse_human_bytes(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let split_index = trimmed
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(trimmed.len());
    let number_part = trimmed[..split_index].trim();
    let unit_part = trimmed[split_index..].trim().to_ascii_lowercase();
    let value = number_part.parse::<f64>().ok()?;
    let multiplier = match unit_part.as_str() {
        "" | "b" => 1.0,
        "kib" | "kb" | "k" => 1024.0,
        "mib" | "mb" | "m" => 1024.0 * 1024.0,
        "gib" | "gb" | "g" => 1024.0 * 1024.0 * 1024.0,
        "tib" | "tb" | "t" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * multiplier).round() as u64)
}

fn parse_eta_seconds(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut buffer = String::new();
    let mut total_seconds = 0u64;
    for ch in trimmed.chars() {
        if ch.is_ascii_digit() {
            buffer.push(ch);
            continue;
        }

        let value = buffer.parse::<u64>().ok()?;
        buffer.clear();
        total_seconds += match ch {
            'h' | 'H' => value * 3600,
            'm' | 'M' => value * 60,
            's' | 'S' => value,
            _ => return None,
        };
    }

    if !buffer.is_empty() {
        total_seconds += buffer.parse::<u64>().ok()?;
    }
    Some(total_seconds)
}

fn calculate_progress_percent(transferred_bytes: u64, total_bytes: Option<u64>) -> f64 {
    match total_bytes {
        Some(total) if total > 0 => {
            ((transferred_bytes as f64 / total as f64) * 100.0).clamp(0.0, 100.0)
        }
        _ => 0.0,
    }
}

fn is_retryable_error(error: &AppError) -> bool {
    matches!(error, AppError::External(_) | AppError::Io(_))
}

fn describe_aria2_exit(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(2) => "超时".to_string(),
        Some(3) => "资源不存在".to_string(),
        Some(5) => "速度过低触发退出".to_string(),
        Some(6) => "网络问题".to_string(),
        Some(8) => "服务端不支持断点续传".to_string(),
        Some(9) => "磁盘空间不足".to_string(),
        Some(19) => "DNS 解析失败".to_string(),
        Some(24) => "鉴权失败".to_string(),
        Some(code) => format!("退出码 {code}"),
        None => "进程被信号终止".to_string(),
    }
}

fn ensure_success(status: u16, body: String, prefix: &str) -> AppResult<Value> {
    if !(200..300).contains(&status) {
        return Err(AppError::External(format!(
            "{prefix}，HTTP={status}，响应={body}"
        )));
    }

    serde_json::from_str::<Value>(&body).map_err(|error| {
        AppError::External(format!(
            "{prefix}，响应不是合法 JSON: {error}，原始响应={body}"
        ))
    })
}

fn pick_string(root: &Value, paths: &[&str]) -> Option<String> {
    for path in paths {
        let mut current = root;
        let mut found = true;
        for part in path.split('.') {
            if let Some(next) = current.get(part) {
                current = next;
            } else {
                found = false;
                break;
            }
        }

        if found {
            match current {
                Value::String(text) if !text.trim().is_empty() => return Some(text.clone()),
                Value::Number(number) => return Some(number.to_string()),
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::process::ExitStatusExt,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::{mpsc, oneshot};

    use super::*;

    #[derive(Default)]
    struct RecordingProgressSink {
        snapshots: Mutex<Vec<TransferProgressSnapshot>>,
    }

    #[async_trait]
    impl ProgressSink for RecordingProgressSink {
        async fn report(&self, snapshot: TransferProgressSnapshot) -> AppResult<()> {
            self.snapshots
                .lock()
                .expect("测试锁不应中毒")
                .push(snapshot);
            Ok(())
        }
    }

    #[tokio::test]
    async fn should_parse_transcription_url_payload() {
        let pipeline = RealPipelineIo::new(AppConfig::from_env().expect("配置应该可构建"));
        let payload = json!({
            "output": {
                "results": [{
                    "subtask_status": "SUCCEEDED",
                    "transcripts": [{
                        "text": "你好世界",
                        "content_duration_in_milliseconds": 2000,
                        "sentences": [{
                            "text": "你好世界",
                            "words": [
                                {"text": "你", "begin_time": 0, "punctuation": ""},
                                {"text": "好", "begin_time": 500, "punctuation": ""}
                            ]
                        }]
                    }]
                }]
            }
        });

        let result = pipeline
            .parse_transcript_payload(payload)
            .await
            .expect("解析应该成功");

        assert_eq!(result.text_display, "你好世界");
        assert_eq!(result.tokens.len(), 2);
        assert_eq!(result.timestamps, vec![0.0, 0.5]);
    }

    #[test]
    fn should_parse_aria2_progress_snapshot() {
        let line = "[#f0b77a 35MiB/282MiB(12%) CN:6 DL:407KiB ETA:10m8s]";
        let snapshot = parse_aria2_progress_line(line).expect("应能解析 aria2 进度行");

        assert_eq!(snapshot.progress_percent, Some(12.0));
        assert_eq!(snapshot.transferred_bytes, Some(35 * 1024 * 1024));
        assert_eq!(snapshot.total_bytes, Some(282 * 1024 * 1024));
        assert_eq!(snapshot.rate_bytes_per_sec, Some(407 * 1024));
        assert_eq!(snapshot.eta_seconds, Some(10 * 60 + 8));
    }

    #[tokio::test]
    async fn should_report_download_progress_before_process_exit() {
        let (line_tx, line_rx) = mpsc::unbounded_channel::<(bool, String)>();
        let (exit_tx, exit_rx) = oneshot::channel::<()>();
        let sink = Arc::new(RecordingProgressSink::default());
        let sink_for_assert = sink.clone();

        let collector = tokio::spawn(async move {
            collect_command_output_while_running(
                async move {
                    let _ = exit_rx.await;
                    Ok(ExitStatus::from_raw(0))
                },
                line_rx,
                Some(sink),
            )
            .await
        });

        line_tx
            .send((
                false,
                "[#f0b77a 35MiB/282MiB(12%) CN:6 DL:407KiB ETA:10m8s]".to_string(),
            ))
            .expect("测试应能送入进度行");

        tokio::time::sleep(Duration::from_millis(50)).await;

        let snapshots = sink_for_assert
            .snapshots
            .lock()
            .expect("测试锁不应中毒")
            .clone();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].progress_percent, Some(12.0));
        assert_eq!(snapshots[0].rate_bytes_per_sec, Some(407 * 1024));

        exit_tx.send(()).expect("测试应能通知子进程退出");
        drop(line_tx);

        let summary = collector
            .await
            .expect("收集任务应正常结束")
            .expect("收集逻辑应成功");
        assert!(summary.exit_status.success());
        assert_eq!(summary.latest_progress.progress_percent, Some(12.0));
        assert_eq!(summary.stdout_lines.len(), 1);
    }
}
