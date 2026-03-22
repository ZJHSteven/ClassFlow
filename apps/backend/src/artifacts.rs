/*!
该模块负责“产物存取”。

当前实现提供两种后端：

1. `LocalArtifactStore`：开发与测试使用，直接把文件写到本地目录。
2. `R2ArtifactStore`：部署时使用 Cloudflare R2（通过 S3 兼容 API）。

这样分层之后：

1. 课程 Markdown / JSON 的业务逻辑不需要知道文件最终去了哪里。
2. 测试可以稳定落在本地临时目录，不依赖真实云存储。
*/

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, primitives::ByteStream};
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
}

pub async fn build_artifact_store(config: &AppConfig) -> AppResult<Arc<dyn ArtifactStore>> {
    match config.artifact_store_mode {
        ArtifactStoreMode::Local => Ok(Arc::new(LocalArtifactStore {
            root: config.local_artifact_root.clone(),
        })),
        ArtifactStoreMode::R2 => Ok(Arc::new(R2ArtifactStore::new(config).await?)),
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
        let bytes = fs::read(&full_path)
            .await
            .map_err(|error| AppError::NotFound(format!("未找到产物 {}: {}", full_path.display(), error)))?;

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
}
