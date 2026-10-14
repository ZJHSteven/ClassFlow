/*!
本地 Qwen3 ASR provider。

这一版已经从“每任务启动一次 Python 脚本”升级成“常驻 sidecar + 模型复用”：

1. 第一个本地任务到来时，Rust 懒启动 Python sidecar。
2. sidecar 启动后立即加载模型，并通过本地 HTTP 暴露 `health / transcribe / shutdown`。
3. 后续任务复用同一个 sidecar，不再重复把模型读入内存。
4. 如果超过空闲阈值没有新任务使用，就自动关闭 sidecar，释放内存。

为什么仍然保留 Rust 侧串行控制：

1. 当前真实目标是“连续任务稳定跑通”，不是最大化并发吞吐。
2. `CapsWriter` 的 Qwen3 路线并没有现成保证“多请求同时进一个模型实例”完全安全。
3. 因此这里先采用最稳妥的策略：Rust 侧全局串行 + sidecar 内部再做一层自保锁。
*/

use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    fs,
    process::{Child, Command},
    sync::{Mutex, Semaphore},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{info, warn};

use crate::{
    config::AppConfig,
    error::{AppError, AppResult},
    models::NormalizedTranscript,
};

use super::{provider::AsrProvider, types::AsrInput};

#[derive(Debug, Deserialize)]
struct LocalQwenErrorPayload {
    kind: String,
    message: String,
    traceback: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LocalQwenTimestampItem {
    text: String,
    start_time: f64,
    end_time: f64,
}

#[derive(Debug, Deserialize)]
struct LocalQwenResult {
    ok: bool,
    provider: String,
    text: Option<String>,
    tokens: Option<Vec<String>>,
    timestamps: Option<Vec<LocalQwenTimestampItem>>,
    duration_seconds: Option<f64>,
    performance: Option<Value>,
    raw: Option<Value>,
    error: Option<LocalQwenErrorPayload>,
}

#[derive(Debug, Deserialize)]
struct SidecarHealthResponse {
    ok: bool,
    provider: String,
    status: String,
    load_seconds: f64,
}

#[derive(Debug, Serialize)]
struct SidecarTranscribeRequest<'a> {
    audio_path: &'a str,
    language: &'a str,
    context: &'a str,
}

struct SidecarProcess {
    child: Child,
    base_url: String,
    last_used_at: Instant,
    load_seconds: f64,
}

struct SidecarState {
    process: Option<SidecarProcess>,
    idle_shutdown_task: Option<JoinHandle<()>>,
}

impl SidecarState {
    fn new() -> Self {
        Self {
            process: None,
            idle_shutdown_task: None,
        }
    }
}

pub struct LocalQwenAsrProvider {
    config: Arc<AppConfig>,
    semaphore: Arc<Semaphore>,
    http_client: HttpClient,
    sidecar_state: Arc<Mutex<SidecarState>>,
}

impl LocalQwenAsrProvider {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self::new_with_http_client(
            config,
            HttpClient::builder()
                .build()
                .expect("本地 sidecar HTTP client 应能构建成功"),
        )
    }

    fn new_with_http_client(config: Arc<AppConfig>, http_client: HttpClient) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(config.local_asr_concurrency.max(1))),
            config,
            http_client,
            sidecar_state: Arc::new(Mutex::new(SidecarState::new())),
        }
    }

    fn output_json_path(&self, task_id: &str) -> PathBuf {
        self.config
            .temp_root
            .join("jobs")
            .join(task_id)
            .join("local_asr_result.json")
    }

    fn sidecar_script_path(&self) -> PathBuf {
        let configured = PathBuf::from(&self.config.local_asr_script);
        let current = configured
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if current == "local_qwen_asr_sidecar.py" {
            return configured;
        }

        configured.with_file_name("local_qwen_asr_sidecar.py")
    }

    fn map_language(&self, language: Option<&str>) -> String {
        match language.map(|value| value.trim().to_lowercase()) {
            Some(value) if value == "zh" || value == "zh-cn" || value == "chinese" => {
                "Chinese".to_string()
            }
            Some(value) if value == "en" || value == "english" => "English".to_string(),
            Some(value) if value == "ja" || value == "japanese" => "Japanese".to_string(),
            Some(value) if value == "ko" || value == "korean" => "Korean".to_string(),
            Some(value) if value == "yue" || value == "cantonese" => "Cantonese".to_string(),
            Some(other) if !other.is_empty() => {
                let mut chars = other.chars();
                match chars.next() {
                    Some(first) => {
                        let head = first.to_uppercase().collect::<String>();
                        format!("{head}{}", chars.as_str())
                    }
                    None => "Chinese".to_string(),
                }
            }
            _ => "Chinese".to_string(),
        }
    }

    async fn ensure_parent_dir(path: &Path) -> AppResult<()> {
        let parent = path
            .parent()
            .ok_or_else(|| AppError::Internal(format!("输出路径缺少父目录: {}", path.display())))?;
        fs::create_dir_all(parent).await?;
        Ok(())
    }

    fn reserve_local_port(&self) -> AppResult<u16> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| {
            AppError::Internal(format!("为本地 ASR sidecar 保留端口失败: {error}"))
        })?;
        let port = listener
            .local_addr()
            .map_err(|error| AppError::Internal(format!("读取本地 ASR sidecar 端口失败: {error}")))?
            .port();
        drop(listener);
        Ok(port)
    }

    async fn poll_sidecar_health(
        &self,
        base_url: &str,
        timeout_secs: f64,
    ) -> AppResult<SidecarHealthResponse> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs.max(1.0));
        let health_url = format!("{base_url}/health");
        loop {
            match self.http_client.get(&health_url).send().await {
                Ok(response) if response.status().is_success() => {
                    let health = response
                        .json::<SidecarHealthResponse>()
                        .await
                        .map_err(|error| {
                            AppError::External(format!(
                                "解析本地 ASR sidecar health 响应失败: url={health_url}, error={error}"
                            ))
                        })?;
                    if health.ok && health.provider == "local_qwen" && health.status == "ready" {
                        return Ok(health);
                    }
                }
                Ok(_) | Err(_) => {}
            }

            if Instant::now() >= deadline {
                return Err(AppError::External(format!(
                    "等待本地 ASR sidecar 就绪超时: timeout_secs={timeout_secs}"
                )));
            }

            sleep(Duration::from_millis(300)).await;
        }
    }

    async fn start_sidecar_process(&self) -> AppResult<SidecarProcess> {
        let port = self.reserve_local_port()?;
        let base_url = format!("http://127.0.0.1:{port}");
        let sidecar_script = self.sidecar_script_path();
        info!(
            "准备启动本地 ASR sidecar: script={}, base_url={}",
            sidecar_script.display(),
            base_url
        );

        let mut command = Command::new(&self.config.local_asr_python);
        command
            .arg(&sidecar_script)
            .arg("--capswriter-root")
            .arg(&self.config.local_asr_workdir)
            .arg("--model-dir")
            .arg(&self.config.local_asr_model_dir)
            .arg("--aligner-dir")
            .arg(&self.config.local_asr_aligner_dir)
            .arg("--threads")
            .arg(self.config.local_asr_threads.to_string())
            .arg("--threads-batch")
            .arg(self.config.local_asr_threads_batch.to_string())
            .arg("--port")
            .arg(port.to_string())
            .arg("--host")
            .arg("127.0.0.1")
            .current_dir(&self.config.local_asr_workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let child = command.spawn().map_err(|error| {
            AppError::External(format!(
                "启动本地 ASR sidecar 失败: script={}, error={error}",
                sidecar_script.display()
            ))
        })?;

        let health = self
            .poll_sidecar_health(&base_url, self.config.local_asr_start_timeout_secs)
            .await?;
        info!(
            "本地 ASR sidecar 已就绪: base_url={}, load_seconds={}",
            base_url, health.load_seconds
        );

        Ok(SidecarProcess {
            child,
            base_url,
            last_used_at: Instant::now(),
            load_seconds: health.load_seconds,
        })
    }

    async fn ensure_sidecar_ready(&self) -> AppResult<(String, f64)> {
        let mut state = self.sidecar_state.lock().await;

        if let Some(existing_shutdown_task) = state.idle_shutdown_task.take() {
            existing_shutdown_task.abort();
        }

        if let Some(process) = state.process.as_mut() {
            if let Some(exit_status) = process.child.try_wait()? {
                warn!("本地 ASR sidecar 已提前退出，准备重新拉起: status={exit_status}");
                state.process = None;
                return Err(AppError::External(format!(
                    "本地 ASR sidecar 已提前退出: status={exit_status}"
                )));
            }

            process.last_used_at = Instant::now();
            info!(
                "复用已存在的本地 ASR sidecar: base_url={}",
                process.base_url
            );
            return Ok((process.base_url.clone(), process.load_seconds));
        }

        let process = self.start_sidecar_process().await?;
        let base_url = process.base_url.clone();
        let load_seconds = process.load_seconds;
        state.process = Some(process);
        Ok((base_url, load_seconds))
    }

    async fn schedule_idle_shutdown(&self) {
        let idle_secs = self.config.local_asr_idle_shutdown_secs;
        if idle_secs == 0 {
            return;
        }

        let sidecar_state = self.sidecar_state.clone();
        let http_client = self.http_client.clone();
        let handle = tokio::spawn(async move {
            sleep(Duration::from_secs(idle_secs)).await;

            let mut guard = sidecar_state.lock().await;
            let Some(process) = guard.process.as_mut() else {
                guard.idle_shutdown_task = None;
                return;
            };

            if process.last_used_at.elapsed() < Duration::from_secs(idle_secs) {
                guard.idle_shutdown_task = None;
                return;
            }

            let shutdown_url = format!("{}/shutdown", process.base_url);
            info!("本地 ASR sidecar 空闲超时，准备关闭: url={shutdown_url}");
            let _ = http_client.post(&shutdown_url).send().await;

            if let Err(_error) = timeout(Duration::from_secs(5), process.child.wait()).await {
                let _ = process.child.kill().await;
                let _ = process.child.wait().await;
            }

            guard.process = None;
            guard.idle_shutdown_task = None;
        });

        let mut guard = self.sidecar_state.lock().await;
        if let Some(old_task) = guard.idle_shutdown_task.replace(handle) {
            old_task.abort();
        }
    }

    async fn call_sidecar_transcribe(
        &self,
        base_url: &str,
        input: &AsrInput,
        output_path: &Path,
    ) -> AppResult<()> {
        Self::ensure_parent_dir(output_path).await?;

        let request = SidecarTranscribeRequest {
            audio_path: input
                .audio_path
                .to_str()
                .ok_or_else(|| AppError::BadRequest("音频路径包含非法 UTF-8".to_string()))?,
            language: &self.map_language(input.language.as_deref()),
            context: input.context.as_deref().unwrap_or_default(),
        };

        let request_future = self
            .http_client
            .post(format!("{base_url}/transcribe"))
            .json(&request)
            .send();
        info!(
            "开始请求本地 ASR sidecar 转写: task_id={}, base_url={}, audio_path={}",
            input.task_id, base_url, request.audio_path
        );

        let response = if self.config.local_asr_task_timeout_secs > 0.0 {
            timeout(
                Duration::from_secs_f64(self.config.local_asr_task_timeout_secs),
                request_future,
            )
            .await
            .map_err(|_| {
                AppError::External(format!(
                    "本地 ASR sidecar 转写超时: timeout_secs={}",
                    self.config.local_asr_task_timeout_secs
                ))
            })??
        } else {
            request_future.await?
        };

        let status = response.status();
        let body = response.text().await.map_err(|error| {
            AppError::External(format!("读取本地 ASR sidecar 响应体失败: {error}"))
        })?;
        info!(
            "本地 ASR sidecar 已返回响应: task_id={}, base_url={}, http_status={}",
            input.task_id, base_url, status
        );

        if !status.is_success() {
            return Err(AppError::External(format!(
                "本地 ASR sidecar 转写失败: http={}, body={body}",
                status
            )));
        }

        fs::write(output_path, body).await.map_err(|error| {
            AppError::Io(format!(
                "写入本地 ASR sidecar 输出 JSON 失败: path={}, error={error}",
                output_path.display()
            ))
        })?;

        Ok(())
    }

    async fn read_result_json(&self, output_path: &Path) -> AppResult<LocalQwenResult> {
        let raw = fs::read_to_string(output_path).await.map_err(|error| {
            AppError::Io(format!(
                "读取本地 ASR 结果 JSON 失败: path={}, error={error}",
                output_path.display()
            ))
        })?;
        serde_json::from_str::<LocalQwenResult>(&raw).map_err(|error| {
            AppError::External(format!(
                "解析本地 ASR 结果 JSON 失败: path={}, error={error}",
                output_path.display()
            ))
        })
    }

    fn normalize_result(&self, result: LocalQwenResult) -> AppResult<NormalizedTranscript> {
        if !result.ok {
            let error = result.error.ok_or_else(|| {
                AppError::External("本地 ASR 返回失败，但缺少 error 字段".to_string())
            })?;
            let traceback = error.traceback.unwrap_or_default();
            return Err(AppError::External(format!(
                "本地 ASR 失败: provider={}, kind={}, message={}, traceback={}",
                result.provider, error.kind, error.message, traceback
            )));
        }

        let text = result.text.unwrap_or_default();
        if text.trim().is_empty() {
            return Err(AppError::External("本地 ASR 返回空文本".to_string()));
        }

        let timestamps = result.timestamps.unwrap_or_default();
        Ok(NormalizedTranscript {
            text_display: text.clone(),
            text_accu: text,
            tokens: result
                .tokens
                .unwrap_or_else(|| timestamps.iter().map(|item| item.text.clone()).collect()),
            timestamps: timestamps.iter().map(|item| item.start_time).collect(),
            duration_seconds: result
                .duration_seconds
                .unwrap_or_else(|| timestamps.last().map(|item| item.end_time).unwrap_or(0.0)),
            raw_task_output: json!({
                "provider": result.provider,
                "performance": result.performance,
                "raw": result.raw,
                "timestamps": timestamps,
            }),
        })
    }
}

#[async_trait]
impl AsrProvider for LocalQwenAsrProvider {
    async fn transcribe_audio(&self, input: AsrInput) -> AppResult<NormalizedTranscript> {
        let _permit =
            self.semaphore.acquire().await.map_err(|error| {
                AppError::Internal(format!("获取本地 ASR 并发信号量失败: {error}"))
            })?;

        let output_path = self.output_json_path(&input.task_id);
        let (base_url, _load_seconds) = self.ensure_sidecar_ready().await?;
        self.call_sidecar_transcribe(&base_url, &input, &output_path)
            .await?;
        let result = self.read_result_json(&output_path).await?;
        let normalized = self.normalize_result(result)?;
        self.schedule_idle_shutdown().await;
        Ok(normalized)
    }
}

impl Drop for LocalQwenAsrProvider {
    fn drop(&mut self) {
        let sidecar_state = self.sidecar_state.clone();
        tokio::spawn(async move {
            let mut guard = sidecar_state.lock().await;
            if let Some(handle) = guard.idle_shutdown_task.take() {
                handle.abort();
            }

            let Some(mut process) = guard.process.take() else {
                return;
            };

            let _ = process.child.kill().await;
            let _ = process.child.wait().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tempfile::tempdir;

    use crate::config::{AppConfig, ArtifactStoreMode};

    use super::*;

    fn build_test_config(temp_root: &Path) -> AppConfig {
        AppConfig {
            bind_addr: "127.0.0.1:0".parse().expect("测试地址应合法"),
            db_url: "sqlite://:memory:".to_string(),
            bearer_token: "test-token".to_string(),
            default_semester: "2025-2026-2".to_string(),
            temp_root: temp_root.to_path_buf(),
            local_artifact_root: temp_root.join("artifacts"),
            task_worker_count: 1,
            download_concurrency: 1,
            upload_concurrency: 1,
            transcribe_concurrency: 1,
            r2_concurrency: 1,
            cleanup_hours: 24,
            artifact_store_mode: ArtifactStoreMode::Local,
            aria2_bin: "aria2c".to_string(),
            download_retry_attempts: 1,
            download_retry_wait_secs: 0.1,
            download_connect_timeout_secs: 5.0,
            download_timeout_secs: 10.0,
            download_split: 1,
            download_connections_per_server: 1,
            download_lowest_speed_limit_bytes: 0,
            asr_provider: "local_qwen".to_string(),
            dashscope_api_key: String::new(),
            dashscope_model: "fun-asr-mtl".to_string(),
            dashscope_submit_url: String::new(),
            dashscope_task_url_template: String::new(),
            dashscope_upload_policy_url: String::new(),
            dashscope_request_timeout_secs: 10.0,
            dashscope_request_retry_attempts: 1,
            dashscope_request_retry_wait_secs: 0.1,
            upload_retry_attempts: 1,
            upload_retry_wait_secs: 0.1,
            upload_timeout_secs: 10.0,
            dashscope_poll_interval_secs: 1.0,
            dashscope_poll_timeout_secs: 10.0,
            local_asr_python: "python3".to_string(),
            local_asr_script: "./tools/local_asr/local_qwen_asr_once.py".to_string(),
            local_asr_workdir: "/tmp/capswriter".to_string(),
            local_asr_model_dir: "/tmp/capswriter/models/qwen".to_string(),
            local_asr_aligner_dir: "/tmp/capswriter/models/aligner".to_string(),
            local_asr_concurrency: 1,
            local_asr_threads: 0,
            local_asr_threads_batch: 0,
            local_asr_idle_shutdown_secs: 1800,
            local_asr_start_timeout_secs: 120.0,
            local_asr_task_timeout_secs: 0.0,
            remote_capswriter_base_url: "http://127.0.0.1:6020".to_string(),
            remote_capswriter_token: String::new(),
            remote_capswriter_access_client_id: String::new(),
            remote_capswriter_access_client_secret: String::new(),
            remote_capswriter_request_timeout_secs: 30.0,
            remote_capswriter_upload_timeout_secs: 900.0,
            remote_capswriter_poll_interval_secs: 0.1,
            remote_capswriter_poll_timeout_secs: 60.0,
            remote_capswriter_retry_wait_secs: 0.1,
            remote_capswriter_retry_max_wait_secs: 1.0,
            r2_bucket: String::new(),
            r2_endpoint: String::new(),
            r2_access_key_id: String::new(),
            r2_secret_access_key: String::new(),
            r2_region: "auto".to_string(),
            artifact_proxy_base_url: String::new(),
            artifact_proxy_token: String::new(),
            artifact_proxy_access_client_id: String::new(),
            artifact_proxy_access_client_secret: String::new(),
            artifact_proxy_connect_timeout_secs: 10.0,
            artifact_proxy_timeout_secs: 10.0,
            artifact_proxy_retry_attempts: 1,
            artifact_proxy_retry_wait_secs: 0.1,
            network_health_enabled: false,
            network_health_probe_urls: Vec::new(),
            network_health_probe_timeout_secs: 1.0,
            network_health_check_interval_secs: 1.0,
            network_health_stable_successes: 1,
            task_event_retention_days: 30,
            task_event_retention_rows_per_task: 200,
            smartclass_collector_enabled: false,
            smartclass_transport: "browser_bridge".to_string(),
            smartclass_base_url: "https://tmu.smartclass.cn".to_string(),
            smartclass_session_file: PathBuf::from("./tmp/test-smartclass-session.json"),
            smartclass_csrf_aes_key: "80bdbdbaf7494add99198960d715d41b".to_string(),
            smartclass_csrf_aes_iv: "bdbaf7494add9919".to_string(),
            smartclass_bridge_base_url: "http://127.0.0.1:8790".to_string(),
            smartclass_poll_interval_secs: 600,
            smartclass_initial_backfill_days: 21,
            smartclass_incremental_lookback_days: 2,
            smartclass_page_size: 12,
            smartclass_request_timeout_secs: 30.0,
            notes_enabled: false,
            notes_auto_since_date: "9999-12-31".to_string(),
            notes_poll_interval_secs: 15.0,
            course_schedule_path: temp_root.join("class-schedule.json"),
            notes_schedule_grace_minutes: 15,
            notes_project_name: "课堂录音→笔记".to_string(),
            archive_bridge_base_url: "http://127.0.0.1:47832".to_string(),
            archive_bridge_token: String::new(),
            archive_bridge_access_client_id: String::new(),
            archive_bridge_access_client_secret: String::new(),
            archive_bridge_request_timeout_secs: 30.0,
            notes_github_repo: "ZJHSteven/med-notes-2026-fall-y3".to_string(),
            notes_github_token: String::new(),
            notifications_enabled: false,
            notification_webhook_url: String::new(),
            notification_webhook_token: String::new(),
            notification_poll_interval_secs: 15.0,
            notification_max_attempts: 8,
            summary_email_enabled: false,
            summary_email_webhook_url: String::new(),
            summary_email_poll_interval_secs: 30.0,
            summary_email_max_attempts: 8,
            summary_backfill_enabled: false,
            summary_backfill_since_date: "9999-12-31".to_string(),
            summary_backfill_only_lecture_key: String::new(),
        }
    }

    #[tokio::test]
    async fn should_normalize_success_result() {
        let temp = tempdir().expect("临时目录应创建成功");
        let provider = LocalQwenAsrProvider::new_with_http_client(
            Arc::new(build_test_config(temp.path())),
            HttpClient::builder()
                .build()
                .expect("测试 HTTP client 应能构建成功"),
        );
        let result = LocalQwenResult {
            ok: true,
            provider: "local_qwen".to_string(),
            text: Some("你好，世界".to_string()),
            tokens: Some(vec!["你好".to_string(), "世界".to_string()]),
            timestamps: Some(vec![
                LocalQwenTimestampItem {
                    text: "你好".to_string(),
                    start_time: 0.0,
                    end_time: 0.3,
                },
                LocalQwenTimestampItem {
                    text: "世界".to_string(),
                    start_time: 0.3,
                    end_time: 0.7,
                },
            ]),
            duration_seconds: Some(0.7),
            performance: Some(json!({"load_seconds": 1.0})),
            raw: Some(json!({"demo": true})),
            error: None,
        };

        let transcript = provider.normalize_result(result).expect("应能成功标准化");
        assert_eq!(transcript.text_display, "你好，世界");
        assert_eq!(transcript.tokens, vec!["你好", "世界"]);
        assert_eq!(transcript.timestamps, vec![0.0, 0.3]);
        assert_eq!(transcript.duration_seconds, 0.7);
    }

    #[tokio::test]
    async fn should_reject_empty_text_result() {
        let temp = tempdir().expect("临时目录应创建成功");
        let provider = LocalQwenAsrProvider::new_with_http_client(
            Arc::new(build_test_config(temp.path())),
            HttpClient::builder()
                .build()
                .expect("测试 HTTP client 应能构建成功"),
        );
        let result = LocalQwenResult {
            ok: true,
            provider: "local_qwen".to_string(),
            text: Some("   ".to_string()),
            tokens: None,
            timestamps: None,
            duration_seconds: None,
            performance: None,
            raw: None,
            error: None,
        };

        let error = provider
            .normalize_result(result)
            .expect_err("空文本应该被拒绝");
        assert!(error.to_string().contains("空文本"));
    }

    #[tokio::test]
    async fn should_derive_sidecar_script_path_from_once_script() {
        let temp = tempdir().expect("临时目录应创建成功");
        let mut config = build_test_config(temp.path());
        config.local_asr_script = "/demo/tools/local_qwen_asr_once.py".to_string();
        let provider = LocalQwenAsrProvider::new_with_http_client(
            Arc::new(config),
            HttpClient::builder()
                .build()
                .expect("测试 HTTP client 应能构建成功"),
        );

        assert_eq!(
            provider.sidecar_script_path(),
            PathBuf::from("/demo/tools/local_qwen_asr_sidecar.py")
        );
    }
}
