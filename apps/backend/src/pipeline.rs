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

use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use reqwest::{Client, multipart::{Form, Part}};
use serde_json::{Value, json};
use tokio::{fs, process::Command, sync::Semaphore};
use tracing::info;
use uuid::Uuid;

use crate::{
    config::AppConfig,
    error::{AppError, AppResult},
    models::NormalizedTranscript,
};

#[async_trait]
pub trait PipelineIo: Send + Sync {
    async fn download_video(&self, url: &str, target_path: &Path) -> AppResult<()>;
    async fn extract_audio(&self, source_video_path: &Path, target_audio_path: &Path) -> AppResult<()>;
    async fn upload_audio_for_transcription(&self, audio_path: &Path) -> AppResult<String>;
    async fn transcribe_file_url(&self, file_url: &str) -> AppResult<NormalizedTranscript>;
    async fn cleanup_dir(&self, dir_path: &Path) -> AppResult<()>;
}

pub struct RealPipelineIo {
    client: Client,
    config: AppConfig,
    download_semaphore: Arc<Semaphore>,
    dashscope_semaphore: Arc<Semaphore>,
}

impl RealPipelineIo {
    pub fn new(config: AppConfig) -> Self {
        Self {
            client: Client::new(),
            download_semaphore: Arc::new(Semaphore::new(config.download_concurrency.max(1))),
            dashscope_semaphore: Arc::new(Semaphore::new(config.dashscope_concurrency.max(1))),
            config,
        }
    }

    async fn request_dashscope_policy(&self) -> AppResult<Value> {
        let response = self
            .client
            .get(&self.config.dashscope_upload_policy_url)
            .query(&[
                ("action", "getPolicy"),
                ("model", self.config.dashscope_model.as_str()),
            ])
            .bearer_auth(&self.config.dashscope_api_key)
            .send()
            .await?;

        ensure_success(response.status().as_u16(), response.text().await?, "获取百炼上传凭证失败")
    }

    async fn submit_transcription_task(&self, file_url: &str) -> AppResult<String> {
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

            let response = self
                .client
                .get(&task_url)
                .bearer_auth(&self.config.dashscope_api_key)
                .send()
                .await?;

            let data = ensure_success(
                response.status().as_u16(),
                response.text().await?,
                "查询百炼转写任务失败",
            )?;

            let status = pick_string(&data, &["output.task_status"]).unwrap_or_default();
            match status.as_str() {
                "SUCCEEDED" => return Ok(data),
                "FAILED" | "CANCELED" => {
                    return Err(AppError::External(format!(
                        "百炼转写任务失败: {}",
                        data
                    )))
                }
                _ => tokio::time::sleep(std::time::Duration::from_secs_f64(
                    self.config.dashscope_poll_interval_secs,
                ))
                .await,
            }
        }
    }

    async fn download_transcription_url(&self, url: &str) -> AppResult<Value> {
        let response = self.client.get(url).send().await?;
        ensure_success(
            response.status().as_u16(),
            response.text().await?,
            "下载 transcription_url 失败",
        )
    }

    async fn parse_transcript_payload(&self, task_output: Value) -> AppResult<NormalizedTranscript> {
        let results = task_output
            .get("output")
            .and_then(|value| value.get("results"))
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::External(format!("百炼结果缺少 output.results: {task_output}")))?;

        for item in results {
            if let Some(subtask_status) = pick_string(item, &["subtask_status"]) {
                if subtask_status != "SUCCEEDED" && subtask_status != "SUCCESS" {
                    return Err(AppError::External(format!(
                        "百炼子任务失败: status={subtask_status}, payload={item}"
                    )));
                }
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
    async fn download_video(&self, url: &str, target_path: &Path) -> AppResult<()> {
        let _permit = self
            .download_semaphore
            .acquire()
            .await
            .map_err(|error| AppError::Internal(format!("下载信号量获取失败: {error}")))?;

        let response = self.client.get(url).send().await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await?;
        if !(200..300).contains(&status) {
            return Err(AppError::External(format!("下载视频失败，HTTP={status}")));
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(target_path, bytes).await?;
        Ok(())
    }

    async fn extract_audio(&self, source_video_path: &Path, target_audio_path: &Path) -> AppResult<()> {
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

    async fn upload_audio_for_transcription(&self, audio_path: &Path) -> AppResult<String> {
        let _permit = self
            .dashscope_semaphore
            .acquire()
            .await
            .map_err(|error| AppError::Internal(format!("百炼信号量获取失败: {error}")))?;

        if self.config.dashscope_api_key.is_empty() {
            return Err(AppError::Config(
                "缺少 DASHSCOPE_API_KEY，无法上传音频并转写".to_string(),
            ));
        }

        let policy = self.request_dashscope_policy().await?;
        let policy_data = policy.get("data").unwrap_or(&policy);
        let upload_host = pick_string(policy_data, &["upload_host", "uploadHost"])
            .ok_or_else(|| AppError::External(format!("百炼上传凭证缺少 upload_host: {policy}")))?;
        let upload_dir = pick_string(policy_data, &["upload_dir", "uploadDir"])
            .ok_or_else(|| AppError::External(format!("百炼上传凭证缺少 upload_dir: {policy}")))?;
        let policy_text = pick_string(policy_data, &["policy"])
            .ok_or_else(|| AppError::External(format!("百炼上传凭证缺少 policy: {policy}")))?;
        let signature = pick_string(policy_data, &["signature"])
            .ok_or_else(|| AppError::External(format!("百炼上传凭证缺少 signature: {policy}")))?;
        let access_key_id = pick_string(
            policy_data,
            &["oss_access_key_id", "ossAccessKeyId", "OSSAccessKeyId"],
        )
        .ok_or_else(|| AppError::External(format!("百炼上传凭证缺少 oss_access_key_id: {policy}")))?;

        let security_token = pick_string(
            policy_data,
            &["x_oss_security_token", "x-oss-security-token", "xOssSecurityToken"],
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
                if !key.starts_with("x_oss_") {
                    continue;
                }
                if value.is_null() {
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

        let audio_bytes = fs::read(audio_path).await?;
        form = form.part(
            "file",
            Part::bytes(audio_bytes)
                .file_name(filename.to_string())
                .mime_str("audio/wav")
                .map_err(|error| AppError::Internal(format!("音频 MIME 设置失败: {error}")))?,
        );

        let normalized_host = if upload_host.starts_with("http://") || upload_host.starts_with("https://") {
            upload_host
        } else {
            format!("https://{}", upload_host.trim_start_matches('/'))
        };

        let response = self
            .client
            .post(normalized_host)
            .multipart(form)
            .send()
            .await?;

        let status = response.status().as_u16();
        let text = response.text().await?;
        if !matches!(status, 200 | 201 | 204) {
            return Err(AppError::External(format!(
                "上传音频到百炼临时 OSS 失败，HTTP={status}，响应={text}"
            )));
        }

        Ok(format!("oss://{object_key}"))
    }

    async fn transcribe_file_url(&self, file_url: &str) -> AppResult<NormalizedTranscript> {
        let _permit = self
            .dashscope_semaphore
            .acquire()
            .await
            .map_err(|error| AppError::Internal(format!("百炼信号量获取失败: {error}")))?;

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

fn ensure_success(status: u16, body: String, prefix: &str) -> AppResult<Value> {
    if !(200..300).contains(&status) {
        return Err(AppError::External(format!(
            "{prefix}，HTTP={status}，响应={body}"
        )));
    }

    serde_json::from_str::<Value>(&body)
        .map_err(|error| AppError::External(format!("{prefix}，响应不是合法 JSON: {error}，原始响应={body}")))
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
    use serde_json::json;

    use super::*;

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
}
