/*! 
远端 CapsWriter Headless HTTP API provider。

面向 N1 -> Windows RTX4060 的跨机 ASR：ClassFlow 只持有本地压缩音频
checkpoint，流式上传到 Windows Headless API，并轮询异步 job 获取最终结果。
Windows 侧继续使用 CapsWriter `source=file`，因此天然服从既有 `mic > file`
抢占调度；N1 不加载 Qwen 模型，也不生成长期 PCM/WAV。
*/

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use http_body::Frame;
use http_body_util::StreamBody;
use reqwest::{Body, Client, RequestBuilder};
use serde::Deserialize;
use serde_json::Value;
use tokio::{fs, time::sleep};
use tokio_util::io::ReaderStream;

use crate::{
    config::AppConfig,
    error::{AppError, AppResult},
    models::NormalizedTranscript,
};

use super::{provider::AsrProvider, types::AsrInput};

#[derive(Debug, Deserialize)]
struct SubmitResponse {
    id: String,
}

#[derive(Debug, Deserialize)]
struct JobStatusResponse {
    status: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JobResultEnvelope {
    status: String,
    #[serde(default)]
    error: Option<String>,
    result: Option<CapsWriterResult>,
}

#[derive(Debug, Deserialize)]
struct CapsWriterResult {
    #[serde(default)]
    text: String,
    #[serde(default)]
    text_accu: String,
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    tokens: Vec<String>,
    #[serde(default)]
    timestamps: Vec<f64>,
}

pub struct RemoteCapsWriterAsrProvider {
    config: Arc<AppConfig>,
    client: Client,
}

impl RemoteCapsWriterAsrProvider {
    pub fn new(config: Arc<AppConfig>) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { config, client }
    }

    fn base_url(&self) -> &str {
        self.config.remote_capswriter_base_url.trim_end_matches('/')
    }

    fn with_auth(&self, mut request: RequestBuilder) -> RequestBuilder {
        if !self.config.remote_capswriter_token.is_empty() {
            request = request.bearer_auth(&self.config.remote_capswriter_token);
        }
        if !self.config.remote_capswriter_access_client_id.is_empty() {
            request = request.header(
                "CF-Access-Client-Id",
                &self.config.remote_capswriter_access_client_id,
            );
        }
        if !self.config.remote_capswriter_access_client_secret.is_empty() {
            request = request.header(
                "CF-Access-Client-Secret",
                &self.config.remote_capswriter_access_client_secret,
            );
        }
        request
    }

    fn request_timeout(&self, request: RequestBuilder, seconds: f64) -> RequestBuilder {
        if seconds > 0.0 {
            request.timeout(Duration::from_secs_f64(seconds))
        } else {
            request
        }
    }

    async fn submit_audio(&self, input: &AsrInput) -> AppResult<String> {
        let metadata = fs::metadata(&input.audio_path).await.map_err(|error| {
            AppError::Io(format!(
                "读取远端 CapsWriter 输入文件元数据失败: path={}, error={error}",
                input.audio_path.display()
            ))
        })?;
        if metadata.len() == 0 {
            return Err(AppError::BadRequest(format!(
                "远端 CapsWriter 输入音频为空: {}",
                input.audio_path.display()
            )));
        }

        let filename = input
            .audio_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("audio.m4a")
            .to_string();
        let file = fs::File::open(&input.audio_path).await.map_err(|error| {
            AppError::Io(format!(
                "打开远端 CapsWriter 输入文件失败: path={}, error={error}",
                input.audio_path.display()
            ))
        })?;
        let stream = ReaderStream::new(file).map_ok(Frame::data);
        let body = Body::wrap(StreamBody::new(stream));

        let request = self
            .client
            .post(format!("{}/v1/transcriptions", self.base_url()))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header(reqwest::header::CONTENT_LENGTH, metadata.len())
            .header("X-Filename", filename)
            .body(body);
        let response = self
            .request_timeout(
                self.with_auth(request),
                self.config.remote_capswriter_upload_timeout_secs,
            )
            .send()
            .await
            .map_err(|error| AppError::External(format!("上传音频到远端 CapsWriter 失败: {error}")))?;

        let status = response.status();
        let body = response.text().await.map_err(|error| {
            AppError::External(format!("读取远端 CapsWriter 提交响应失败: {error}"))
        })?;
        if !status.is_success() {
            return Err(AppError::External(format!(
                "远端 CapsWriter 提交失败: http={status}, body={body}"
            )));
        }
        let submitted = serde_json::from_str::<SubmitResponse>(&body).map_err(|error| {
            AppError::External(format!(
                "解析远端 CapsWriter 提交响应失败: error={error}, body={body}"
            ))
        })?;
        if submitted.id.trim().is_empty() {
            return Err(AppError::External(
                "远端 CapsWriter 提交成功但缺少 job id".to_string(),
            ));
        }
        Ok(submitted.id)
    }

    async fn get_json(&self, url: String) -> AppResult<Value> {
        let request = self.client.get(url);
        let response = self
            .request_timeout(
                self.with_auth(request),
                self.config.remote_capswriter_request_timeout_secs,
            )
            .send()
            .await
            .map_err(|error| AppError::External(format!("请求远端 CapsWriter 失败: {error}")))?;
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            AppError::External(format!("读取远端 CapsWriter 响应失败: {error}"))
        })?;
        if !status.is_success() {
            return Err(AppError::External(format!(
                "远端 CapsWriter 请求失败: http={status}, body={body}"
            )));
        }
        serde_json::from_str::<Value>(&body).map_err(|error| {
            AppError::External(format!(
                "解析远端 CapsWriter JSON 失败: error={error}, body={body}"
            ))
        })
    }

    async fn wait_for_result(&self, job_id: &str) -> AppResult<NormalizedTranscript> {
        let started_at = Instant::now();
        loop {
            if self.config.remote_capswriter_poll_timeout_secs > 0.0
                && started_at.elapsed()
                    >= Duration::from_secs_f64(self.config.remote_capswriter_poll_timeout_secs)
            {
                return Err(AppError::External(format!(
                    "等待远端 CapsWriter 转写超时: job_id={job_id}, timeout_secs={}",
                    self.config.remote_capswriter_poll_timeout_secs
                )));
            }

            let status_value = self
                .get_json(format!("{}/v1/transcriptions/{job_id}", self.base_url()))
                .await?;
            let status = serde_json::from_value::<JobStatusResponse>(status_value).map_err(|error| {
                AppError::External(format!(
                    "解析远端 CapsWriter job 状态失败: job_id={job_id}, error={error}"
                ))
            })?;
            match status.status.as_str() {
                "queued" | "running" => {
                    sleep(Duration::from_secs_f64(
                        self.config.remote_capswriter_poll_interval_secs.max(0.05),
                    ))
                    .await;
                }
                "failed" => {
                    return Err(AppError::External(format!(
                        "远端 CapsWriter 转写失败: job_id={job_id}, error={}",
                        status.error.unwrap_or_else(|| "unknown".to_string())
                    )));
                }
                "completed" => break,
                other => {
                    return Err(AppError::External(format!(
                        "远端 CapsWriter 返回未知 job 状态: job_id={job_id}, status={other}"
                    )));
                }
            }
        }

        let raw = self
            .get_json(format!(
                "{}/v1/transcriptions/{job_id}/result",
                self.base_url()
            ))
            .await?;
        let envelope = serde_json::from_value::<JobResultEnvelope>(raw.clone()).map_err(|error| {
            AppError::External(format!(
                "解析远端 CapsWriter 最终结果失败: job_id={job_id}, error={error}"
            ))
        })?;
        if envelope.status != "completed" {
            return Err(AppError::External(format!(
                "远端 CapsWriter 最终结果状态异常: job_id={job_id}, status={}, error={}",
                envelope.status,
                envelope.error.unwrap_or_default()
            )));
        }
        let result = envelope.result.ok_or_else(|| {
            AppError::External(format!("远端 CapsWriter completed 但缺少 result: job_id={job_id}"))
        })?;
        let text_display = if result.text.trim().is_empty() {
            result.text_accu.clone()
        } else {
            result.text.clone()
        };
        let text_accu = if result.text_accu.trim().is_empty() {
            text_display.clone()
        } else {
            result.text_accu.clone()
        };
        if text_accu.trim().is_empty() {
            return Err(AppError::External(format!(
                "远端 CapsWriter 返回空文本: job_id={job_id}"
            )));
        }

        Ok(NormalizedTranscript {
            text_display,
            text_accu,
            tokens: result.tokens,
            timestamps: result.timestamps,
            duration_seconds: result.duration,
            raw_task_output: raw,
        })
    }
}

#[async_trait]
impl AsrProvider for RemoteCapsWriterAsrProvider {
    async fn transcribe_audio(&self, input: AsrInput) -> AppResult<NormalizedTranscript> {
        let job_id = self.submit_audio(&input).await?;
        self.wait_for_result(&job_id).await
    }
}
