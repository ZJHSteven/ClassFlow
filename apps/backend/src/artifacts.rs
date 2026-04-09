/*!
该模块负责“产物存取”。

当前实现提供三种后端：

1. `LocalArtifactStore`：开发与测试使用，直接把文件写到本地目录。
2. `R2ArtifactStore`：部署时使用 Cloudflare R2（通过 S3 兼容 API）。
3. `WorkerArtifactStore`：后端不直连对象存储，而是通过 Cloudflare Worker 私有接口写入 Worker 绑定的 R2。

这样分层之后：

1. 课程 Markdown / JSON 的业务逻辑不需要知道文件最终去了哪里。
2. 测试可以稳定落在本地临时目录，不依赖真实云存储。
*/

use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, primitives::ByteStream};
use reqwest::{Client as HttpClient, Url};
use tokio::{fs, sync::Semaphore};
use tracing::warn;

use crate::{
    config::{AppConfig, ArtifactStoreMode},
    error::{AppError, AppResult},
    models::StoredObject,
};

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn put_bytes(&self, path: &str, content_type: &str, bytes: Vec<u8>) -> AppResult<()>;
    async fn get_bytes(&self, path: &str) -> AppResult<StoredObject>;
    async fn delete(&self, path: &str) -> AppResult<()>;
}

pub async fn build_artifact_store(config: &AppConfig) -> AppResult<Arc<dyn ArtifactStore>> {
    match config.artifact_store_mode {
        ArtifactStoreMode::Local => Ok(Arc::new(LocalArtifactStore {
            root: config.local_artifact_root.clone(),
        })),
        ArtifactStoreMode::R2 => Ok(Arc::new(R2ArtifactStore::new(config).await?)),
        ArtifactStoreMode::Worker => Ok(Arc::new(WorkerArtifactStore::new(config)?)),
    }
}

pub struct LocalArtifactStore {
    root: PathBuf,
}

impl LocalArtifactStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl ArtifactStore for LocalArtifactStore {
    async fn put_bytes(&self, path: &str, _content_type: &str, bytes: Vec<u8>) -> AppResult<()> {
        let full_path = self.root.join(path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(full_path, bytes).await?;
        Ok(())
    }

    async fn get_bytes(&self, path: &str) -> AppResult<StoredObject> {
        let full_path = self.root.join(path);
        let bytes = fs::read(&full_path).await.map_err(|error| {
            AppError::NotFound(format!("未找到产物 {}: {}", full_path.display(), error))
        })?;

        let content_type = if path.ends_with(".md") {
            "text/markdown; charset=utf-8"
        } else if path.ends_with(".json") {
            "application/json; charset=utf-8"
        } else {
            "application/octet-stream"
        };

        Ok(StoredObject {
            content_type: content_type.to_string(),
            bytes,
        })
    }

    async fn delete(&self, path: &str) -> AppResult<()> {
        let full_path = self.root.join(path);
        match fs::remove_file(&full_path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AppError::Io(format!(
                "删除本地产物失败: path={}, error={error}",
                full_path.display()
            ))),
        }
    }
}

pub struct R2ArtifactStore {
    client: S3Client,
    bucket: String,
    semaphore: Arc<Semaphore>,
}

impl R2ArtifactStore {
    pub async fn new(config: &AppConfig) -> AppResult<Self> {
        if config.r2_bucket.is_empty()
            || config.r2_endpoint.is_empty()
            || config.r2_access_key_id.is_empty()
            || config.r2_secret_access_key.is_empty()
        {
            return Err(AppError::Config(
                "启用 R2 模式时，必须配置 CLASSFLOW_R2_BUCKET / ENDPOINT / ACCESS_KEY_ID / SECRET_ACCESS_KEY"
                    .to_string(),
            ));
        }

        let credentials = Credentials::new(
            config.r2_access_key_id.clone(),
            config.r2_secret_access_key.clone(),
            None,
            None,
            "classflow",
        );

        let shared_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(config.r2_region.clone()))
            .credentials_provider(credentials)
            .load()
            .await;

        let client = S3Client::from_conf(
            aws_sdk_s3::config::Builder::from(&shared_config)
                .endpoint_url(config.r2_endpoint.clone())
                .build(),
        );

        Ok(Self {
            client,
            bucket: config.r2_bucket.clone(),
            semaphore: Arc::new(Semaphore::new(config.r2_concurrency.max(1))),
        })
    }
}

#[async_trait]
impl ArtifactStore for R2ArtifactStore {
    async fn put_bytes(&self, path: &str, content_type: &str, bytes: Vec<u8>) -> AppResult<()> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|error| AppError::Internal(format!("R2 信号量获取失败: {error}")))?;

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(path)
            .content_type(content_type)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|error| AppError::External(format!("R2 上传失败: {error}")))?;

        Ok(())
    }

    async fn get_bytes(&self, path: &str) -> AppResult<StoredObject> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|error| AppError::Internal(format!("R2 信号量获取失败: {error}")))?;

        let output = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await
            .map_err(|error| AppError::NotFound(format!("R2 产物不存在或读取失败: {error}")))?;

        let content_type = output
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = output
            .body
            .collect()
            .await
            .map_err(|error| AppError::External(format!("R2 响应读取失败: {error}")))?
            .into_bytes()
            .to_vec();

        Ok(StoredObject {
            content_type,
            bytes,
        })
    }

    async fn delete(&self, path: &str) -> AppResult<()> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|error| AppError::Internal(format!("R2 信号量获取失败: {error}")))?;

        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await
            .map_err(|error| AppError::External(format!("R2 删除失败: {error}")))?;

        Ok(())
    }
}

pub struct WorkerArtifactStore {
    client: HttpClient,
    base_url: Url,
    token: String,
    retry_attempts: u32,
    retry_wait: Duration,
}

impl WorkerArtifactStore {
    pub fn new(config: &AppConfig) -> AppResult<Self> {
        if config.artifact_proxy_base_url.trim().is_empty()
            || config.artifact_proxy_token.trim().is_empty()
        {
            return Err(AppError::Config(
                "启用 Worker 产物模式时，必须配置 CLASSFLOW_ARTIFACT_PROXY_BASE_URL / CLASSFLOW_ARTIFACT_PROXY_TOKEN"
                    .to_string(),
            ));
        }

        let base_url = Url::parse(&config.artifact_proxy_base_url).map_err(|error| {
            AppError::Config(format!(
                "CLASSFLOW_ARTIFACT_PROXY_BASE_URL 非法: {} ({error})",
                config.artifact_proxy_base_url
            ))
        })?;

        Ok(Self {
            client: HttpClient::builder()
                .connect_timeout(Duration::from_secs_f64(
                    config.artifact_proxy_connect_timeout_secs.max(1.0),
                ))
                .timeout(Duration::from_secs_f64(
                    config.artifact_proxy_timeout_secs.max(1.0),
                ))
                .build()
                .unwrap_or_else(|_| HttpClient::new()),
            base_url,
            token: config.artifact_proxy_token.clone(),
            retry_attempts: config.artifact_proxy_retry_attempts.max(1),
            retry_wait: Duration::from_secs_f64(config.artifact_proxy_retry_wait_secs.max(0.0)),
        })
    }

    fn build_object_url(&self, path: &str) -> AppResult<Url> {
        let mut url = self.base_url.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| AppError::Internal("Worker 产物 URL 不能作为路径前缀".to_string()))?;
            segments.pop_if_empty();
            segments.push("__classflow");
            segments.push("artifacts");
            for segment in path.split('/') {
                if segment.is_empty() {
                    continue;
                }
                segments.push(segment);
            }
        }
        Ok(url)
    }

    async fn send_with_retry<Builder>(
        &self,
        operation: &str,
        path: &str,
        mut build_request: Builder,
    ) -> AppResult<reqwest::Response>
    where
        Builder: FnMut(Url) -> reqwest::RequestBuilder,
    {
        let url = self.build_object_url(path)?;

        for attempt in 1..=self.retry_attempts {
            match build_request(url.clone()).send().await {
                Ok(response) => {
                    if !should_retry_status(response.status()) || attempt == self.retry_attempts {
                        return Ok(response);
                    }

                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    warn!(
                        operation,
                        path,
                        attempt,
                        total_attempts = self.retry_attempts,
                        status = %status,
                        body,
                        "Worker 产物请求收到可重试响应，准备重试"
                    );
                }
                Err(error) => {
                    let detail = format_reqwest_error(operation, url.as_str(), &error);
                    if !should_retry_reqwest_error(&error) || attempt == self.retry_attempts {
                        return Err(AppError::External(detail));
                    }

                    warn!(
                        operation,
                        path,
                        attempt,
                        total_attempts = self.retry_attempts,
                        error = detail,
                        "Worker 产物请求失败，准备重试"
                    );
                }
            }

            if !self.retry_wait.is_zero() {
                tokio::time::sleep(self.retry_wait).await;
            }
        }

        Err(AppError::Internal(format!(
            "Worker 产物 {operation} 进入了不可能到达的重试分支"
        )))
    }
}

#[async_trait]
impl ArtifactStore for WorkerArtifactStore {
    async fn put_bytes(&self, path: &str, content_type: &str, bytes: Vec<u8>) -> AppResult<()> {
        let response = self
            .send_with_retry("写入", path, |url| {
                self.client
                    .put(url)
                    .bearer_auth(&self.token)
                    .header("Content-Type", content_type)
                    .body(bytes.clone())
            })
            .await?;

        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(AppError::External(format!(
                "Worker 产物写入失败，HTTP={status}，响应={body}"
            )));
        }

        Ok(())
    }

    async fn get_bytes(&self, path: &str) -> AppResult<StoredObject> {
        let response = self
            .send_with_retry("读取", path, |url| {
                self.client.get(url).bearer_auth(&self.token)
            })
            .await?;

        let status = response.status().as_u16();
        if status == 404 {
            return Err(AppError::NotFound(format!("Worker 产物不存在: {path}")));
        }

        if !(200..300).contains(&status) {
            let body = response.text().await.unwrap_or_default();
            return Err(AppError::External(format!(
                "Worker 产物读取失败，HTTP={status}，响应={body}"
            )));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = response.bytes().await?.to_vec();

        Ok(StoredObject {
            content_type,
            bytes,
        })
    }

    async fn delete(&self, path: &str) -> AppResult<()> {
        let response = self
            .send_with_retry("删除", path, |url| {
                self.client.delete(url).bearer_auth(&self.token)
            })
            .await?;

        let status = response.status().as_u16();
        if matches!(status, 200 | 202 | 204 | 404) {
            return Ok(());
        }

        let body = response.text().await.unwrap_or_default();
        Err(AppError::External(format!(
            "Worker 产物删除失败，HTTP={status}，响应={body}"
        )))
    }
}

fn should_retry_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429) || status.is_server_error()
}

fn should_retry_reqwest_error(error: &reqwest::Error) -> bool {
    !error.is_builder() && !error.is_redirect()
}

fn format_reqwest_error(operation: &str, url: &str, error: &reqwest::Error) -> String {
    let reason = if error.is_timeout() {
        "请求超时"
    } else if error.is_connect() {
        "连接失败"
    } else if error.is_request() {
        "请求发送失败"
    } else if error.is_body() {
        "请求体读取失败"
    } else if error.is_decode() {
        "响应解码失败"
    } else {
        "未知请求错误"
    };

    format!("Worker 产物{operation}失败: url={url}, reason={reason}, error={error}")
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Router,
        body::Bytes,
        extract::{Path, State},
        http::{HeaderMap, StatusCode},
        routing::put,
    };
    use tokio::net::TcpListener;

    use super::*;
    use crate::config::ArtifactStoreMode;

    fn test_config(base_url: &str) -> AppConfig {
        AppConfig {
            bind_addr: "127.0.0.1:0".parse().expect("测试地址应合法"),
            db_url: "sqlite://./tmp/test.db?mode=rwc".to_string(),
            bearer_token: "test-token".to_string(),
            default_semester: "2025-2026-2".to_string(),
            temp_root: PathBuf::from("./tmp"),
            local_artifact_root: PathBuf::from("./data/artifacts"),
            task_worker_count: 1,
            download_concurrency: 1,
            upload_concurrency: 1,
            transcribe_concurrency: 1,
            r2_concurrency: 1,
            cleanup_hours: 24,
            artifact_store_mode: ArtifactStoreMode::Worker,
            aria2_bin: "aria2c".to_string(),
            download_retry_attempts: 1,
            download_retry_wait_secs: 0.0,
            download_connect_timeout_secs: 5.0,
            download_timeout_secs: 30.0,
            download_split: 2,
            download_connections_per_server: 2,
            download_lowest_speed_limit_bytes: 0,
            dashscope_api_key: String::new(),
            dashscope_model: "fun-asr-mtl".to_string(),
            dashscope_submit_url: String::new(),
            dashscope_task_url_template: String::new(),
            dashscope_upload_policy_url: String::new(),
            dashscope_request_timeout_secs: 30.0,
            dashscope_request_retry_attempts: 1,
            dashscope_request_retry_wait_secs: 0.0,
            upload_retry_attempts: 1,
            upload_retry_wait_secs: 0.0,
            upload_timeout_secs: 30.0,
            dashscope_poll_interval_secs: 1.0,
            dashscope_poll_timeout_secs: 30.0,
            r2_bucket: String::new(),
            r2_endpoint: String::new(),
            r2_access_key_id: String::new(),
            r2_secret_access_key: String::new(),
            r2_region: "auto".to_string(),
            artifact_proxy_base_url: base_url.to_string(),
            artifact_proxy_token: "proxy-token".to_string(),
            artifact_proxy_connect_timeout_secs: 5.0,
            artifact_proxy_timeout_secs: 5.0,
            artifact_proxy_retry_attempts: 2,
            artifact_proxy_retry_wait_secs: 0.0,
            task_event_retention_days: 30,
            task_event_retention_rows_per_task: 200,
        }
    }

    #[test]
    fn worker_artifact_url_should_normalize_leading_and_duplicate_slashes() {
        let store = WorkerArtifactStore::new(&test_config("https://example.com/base/"))
            .expect("Worker store 应构造成功");

        let url = store
            .build_object_url("/semester//course///segment.json")
            .expect("产物 URL 应构造成功");

        assert_eq!(
            url.as_str(),
            "https://example.com/base/__classflow/artifacts/semester/course/segment.json"
        );
    }

    #[tokio::test]
    async fn worker_artifact_put_should_retry_transient_http_failure() {
        #[derive(Clone)]
        struct MockState {
            put_calls: Arc<AtomicUsize>,
        }

        async fn handle_put(
            State(state): State<MockState>,
            Path(path): Path<String>,
            headers: HeaderMap,
            body: Bytes,
        ) -> (StatusCode, &'static str) {
            assert_eq!(path, "semester/course/segment.md");
            assert_eq!(
                headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer proxy-token")
            );
            assert_eq!(
                headers
                    .get("content-type")
                    .and_then(|value| value.to_str().ok()),
                Some("text/markdown; charset=utf-8")
            );
            assert_eq!(body.as_ref(), b"hello worker");

            let call_index = state.put_calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                (StatusCode::BAD_GATEWAY, "transient failure")
            } else {
                (StatusCode::CREATED, "ok")
            }
        }

        let state = MockState {
            put_calls: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/__classflow/artifacts/{*path}", put(handle_put))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("测试监听器应启动成功");
        let addr = listener.local_addr().expect("监听地址应能读取成功");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("测试 HTTP 服务应运行成功");
        });

        let mut config = test_config(&format!("http://{addr}/"));
        config.artifact_proxy_timeout_secs = Duration::from_secs(5).as_secs_f64();
        let store = WorkerArtifactStore::new(&config).expect("Worker store 应构造成功");

        store
            .put_bytes(
                "semester/course/segment.md",
                "text/markdown; charset=utf-8",
                b"hello worker".to_vec(),
            )
            .await
            .expect("第二次重试后应成功写入");

        assert_eq!(
            state.put_calls.load(Ordering::SeqCst),
            2,
            "首次 502 后应自动重试一次"
        );

        server.abort();
    }
}
