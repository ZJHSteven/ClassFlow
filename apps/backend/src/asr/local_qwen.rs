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
use serde::Deserialize;
use serde_json::Value;
use tokio::{fs, process::Command, sync::Semaphore, time::timeout};

use crate::{
    config::AppConfig,
    error::{AppError, AppResult},
    models::NormalizedTranscript,
};

use super::{
    provider::AsrProvider,
    types::AsrInput,
};

#[derive(Debug, Deserialize)]
struct LocalQwenErrorPayload {
    kind: String,
    message: String,
    traceback: Option<String>,
}

#[derive(Debug, Deserialize)]
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
        let parent = path.parent().ok_or_else(|| {
            AppError::Internal(format!("输出路径缺少父目录: {}", path.display()))
        })?;
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
            timeout(
                Duration::from_secs_f64(task_timeout),
                command.output(),
            )
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
            return Err(AppError::External(
                "本地 ASR 返回空文本".to_string(),
            ));
        }

        let timestamps = result.timestamps.unwrap_or_default();
        Ok(NormalizedTranscript {
            text_display: text.clone(),
            text_accu: text,
            tokens: result
                .tokens
                .unwrap_or_else(|| timestamps.iter().map(|item| item.text.clone()).collect()),
            timestamps: timestamps.iter().map(|item| item.start_time).collect(),
            duration_seconds: result.duration_seconds.unwrap_or_else(|| {
                timestamps
                    .last()
                    .map(|item| item.end_time)
                    .unwrap_or(0.0)
            }),
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
        let _permit = self.semaphore.acquire().await.map_err(|error| {
            AppError::Internal(format!("获取本地 ASR 并发信号量失败: {error}"))
        })?;

        let output_path = self.output_json_path(&input.task_id);
        self.run_script(&input, &output_path).await?;
        let result = self.read_result_json(&output_path).await?;
        self.normalize_result(result)
    }
}
