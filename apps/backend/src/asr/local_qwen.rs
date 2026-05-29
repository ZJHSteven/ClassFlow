/*!
本地 Qwen3 ASR provider。

第一轮只做“单次 Python 脚本模式”：

1. Rust 调起一次 Python 脚本；
2. 脚本内部加载模型并输出 JSON；
3. Rust 读取 JSON 并映射成 `NormalizedTranscript`。

这样做的核心目的不是极致性能，而是先把“云端 -> 本地”的能力稳定替换掉。
*/

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{fs, process::Command, sync::Semaphore, time::timeout};

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

pub struct LocalQwenAsrProvider {
    config: Arc<AppConfig>,
    semaphore: Arc<Semaphore>,
}

impl LocalQwenAsrProvider {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(config.local_asr_concurrency.max(1))),
            config,
        }
    }

    fn output_json_path(&self, task_id: &str) -> PathBuf {
        self.config
            .temp_root
            .join("jobs")
            .join(task_id)
            .join("local_asr_result.json")
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

    async fn run_script(&self, input: &AsrInput, output_path: &Path) -> AppResult<()> {
        Self::ensure_parent_dir(output_path).await?;

        let mut command = Command::new(&self.config.local_asr_python);
        command
            .arg(&self.config.local_asr_script)
            .arg("--capswriter-root")
            .arg(&self.config.local_asr_workdir)
            .arg("--audio")
            .arg(&input.audio_path)
            .arg("--model-dir")
            .arg(&self.config.local_asr_model_dir)
            .arg("--aligner-dir")
            .arg(&self.config.local_asr_aligner_dir)
            .arg("--output")
            .arg(output_path)
            .arg("--language")
            .arg(self.map_language(input.language.as_deref()))
            .arg("--context")
            .arg(input.context.clone().unwrap_or_default())
            .current_dir(&self.config.local_asr_workdir);

        let task_timeout = self.config.local_asr_task_timeout_secs;
        let output = if task_timeout > 0.0 {
            timeout(Duration::from_secs_f64(task_timeout), command.output())
                .await
                .map_err(|_| {
                    AppError::External(format!(
                        "本地 ASR 脚本执行超时: timeout_secs={task_timeout}"
                    ))
                })??
        } else {
            command.output().await?
        };

        if output.status.success() {
            return Ok(());
        }

        let stdout_text = String::from_utf8_lossy(&output.stdout);
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        Err(AppError::External(format!(
            "本地 ASR 脚本执行失败: status={}, stdout={}, stderr={}",
            output.status,
            stdout_text.trim(),
            stderr_text.trim(),
        )))
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
            raw_task_output: serde_json::json!({
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
        self.run_script(&input, &output_path).await?;
        let result = self.read_result_json(&output_path).await?;
        self.normalize_result(result)
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
            local_asr_idle_shutdown_secs: 1800,
            local_asr_start_timeout_secs: 120.0,
            local_asr_task_timeout_secs: 0.0,
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
        }
    }

    #[tokio::test]
    async fn should_normalize_success_result() {
        let temp = tempdir().expect("临时目录应创建成功");
        let provider = LocalQwenAsrProvider::new(Arc::new(build_test_config(temp.path())));
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
        let provider = LocalQwenAsrProvider::new(Arc::new(build_test_config(temp.path())));
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
}
