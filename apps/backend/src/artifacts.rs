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

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, primitives::ByteStream};
use reqwest::{Client as HttpClient, Url};
use tokio::{fs, sync::Semaphore};

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

        let mut base_url = Url::parse(&config.artifact_proxy_base_url).map_err(|error| {
            AppError::Config(format!(
                "CLASSFLOW_ARTIFACT_PROXY_BASE_URL 非法: {} ({error})",
                config.artifact_proxy_base_url
            ))
        })?;

        if !base_url.path().ends_with('/') {
            let next_path = format!("{}/", base_url.path().trim_end_matches('/'));
            base_url.set_path(&next_path);
        }

        Ok(Self {
            client: HttpClient::new(),
            base_url,
            token: config.artifact_proxy_token.clone(),
        })
    }

    fn build_object_url(&self, path: &str) -> AppResult<Url> {
        let mut url = self
            .base_url
            .join("__classflow/artifacts/")
            .map_err(|error| AppError::Internal(format!("拼接 Worker 产物前缀失败: {error}")))?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| AppError::Internal("Worker 产物 URL 不能作为路径前缀".to_string()))?;
            for segment in path.split('/') {
                if segment.is_empty() {
                    continue;
                }
                segments.push(segment);
            }
        }
        Ok(url)
    }
}

#[async_trait]
impl ArtifactStore for WorkerArtifactStore {
    async fn put_bytes(&self, path: &str, content_type: &str, bytes: Vec<u8>) -> AppResult<()> {
        let response = self
            .client
            .put(self.build_object_url(path)?)
            .bearer_auth(&self.token)
            .header("Content-Type", content_type)
            .body(bytes)
            .send()
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
            .client
            .get(self.build_object_url(path)?)
            .bearer_auth(&self.token)
            .send()
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
            .client
            .delete(self.build_object_url(path)?)
            .bearer_auth(&self.token)
            .send()
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
