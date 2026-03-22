/*!
该模块负责把环境变量转成强类型配置。

设计取舍：

1. 采用“代码内置默认值 + 环境变量覆盖”的方式，方便本地开发快速启动。
2. 需要保密的参数统一走环境变量，不允许写死到源码。
3. 与部署直接相关的参数尽量显式列出来，避免把行为藏在魔法常量里。
*/

use std::{env, net::SocketAddr, path::PathBuf, str::FromStr};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactStoreMode {
    Local,
    R2,
}

impl FromStr for ArtifactStoreMode {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "r2" => Ok(Self::R2),
            other => Err(AppError::Config(format!(
                "不支持的 CLASSFLOW_ARTIFACT_STORE_MODE: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind_addr: SocketAddr,
    pub db_url: String,
    pub bearer_token: String,
    pub default_semester: String,
    pub temp_root: PathBuf,
    pub local_artifact_root: PathBuf,
    pub task_worker_count: usize,
    pub download_concurrency: usize,
    pub dashscope_concurrency: usize,
    pub r2_concurrency: usize,
    pub cleanup_hours: u64,
    pub artifact_store_mode: ArtifactStoreMode,
    pub dashscope_api_key: String,
    pub dashscope_model: String,
    pub dashscope_submit_url: String,
    pub dashscope_task_url_template: String,
    pub dashscope_upload_policy_url: String,
    pub dashscope_poll_interval_secs: f64,
    pub dashscope_poll_timeout_secs: f64,
    pub r2_bucket: String,
    pub r2_endpoint: String,
    pub r2_access_key_id: String,
    pub r2_secret_access_key: String,
    pub r2_region: String,
}

impl AppConfig {
    pub fn from_env() -> AppResult<Self> {
        let bind_addr = env_or("CLASSFLOW_BIND_ADDR", "0.0.0.0:8787")
            .parse::<SocketAddr>()
            .map_err(|error| AppError::Config(format!("CLASSFLOW_BIND_ADDR 非法: {error}")))?;

        Ok(Self {
            bind_addr,
            db_url: env_or("CLASSFLOW_DB_URL", "sqlite://./data/classflow.db?mode=rwc"),
            bearer_token: env_or("CLASSFLOW_BEARER_TOKEN", "classflow-dev-token"),
            default_semester: env_or("CLASSFLOW_DEFAULT_SEMESTER", "2025-2026-2"),
            temp_root: PathBuf::from(env_or("CLASSFLOW_TEMP_ROOT", "./tmp")),
            local_artifact_root: PathBuf::from(env_or(
                "CLASSFLOW_LOCAL_ARTIFACT_ROOT",
                "./data/artifacts",
            )),
            task_worker_count: env_or_parse("CLASSFLOW_TASK_WORKER_COUNT", 4)?,
            download_concurrency: env_or_parse("CLASSFLOW_DOWNLOAD_CONCURRENCY", 2)?,
            dashscope_concurrency: env_or_parse("CLASSFLOW_DASHSCOPE_CONCURRENCY", 8)?,
            r2_concurrency: env_or_parse("CLASSFLOW_R2_CONCURRENCY", 4)?,
            cleanup_hours: env_or_parse("CLASSFLOW_TMP_CLEANUP_HOURS", 24)?,
            artifact_store_mode: env_or("CLASSFLOW_ARTIFACT_STORE_MODE", "local")
                .parse::<ArtifactStoreMode>()?,
            dashscope_api_key: env::var("DASHSCOPE_API_KEY").unwrap_or_default(),
            dashscope_model: env_or("CLASSFLOW_DASHSCOPE_MODEL", "fun-asr"),
            dashscope_submit_url: env_or(
                "CLASSFLOW_DASHSCOPE_SUBMIT_URL",
                "https://dashscope.aliyuncs.com/api/v1/services/audio/asr/transcription",
            ),
            dashscope_task_url_template: env_or(
                "CLASSFLOW_DASHSCOPE_TASK_URL_TEMPLATE",
                "https://dashscope.aliyuncs.com/api/v1/tasks/{task_id}",
            ),
            dashscope_upload_policy_url: env_or(
                "CLASSFLOW_DASHSCOPE_UPLOAD_POLICY_URL",
                "https://dashscope.aliyuncs.com/api/v1/uploads",
            ),
            dashscope_poll_interval_secs: env_or_parse(
                "CLASSFLOW_DASHSCOPE_POLL_INTERVAL_SECS",
                1.0,
            )?,
            dashscope_poll_timeout_secs: env_or_parse(
                "CLASSFLOW_DASHSCOPE_POLL_TIMEOUT_SECS",
                900.0,
            )?,
            r2_bucket: env::var("CLASSFLOW_R2_BUCKET").unwrap_or_default(),
            r2_endpoint: env::var("CLASSFLOW_R2_ENDPOINT").unwrap_or_default(),
            r2_access_key_id: env::var("CLASSFLOW_R2_ACCESS_KEY_ID").unwrap_or_default(),
            r2_secret_access_key: env::var("CLASSFLOW_R2_SECRET_ACCESS_KEY").unwrap_or_default(),
            r2_region: env_or("CLASSFLOW_R2_REGION", "auto"),
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_or_parse<T>(key: &str, default: T) -> AppResult<T>
where
    T: FromStr + ToString,
    <T as FromStr>::Err: std::fmt::Display,
{
    let raw = env::var(key).unwrap_or_else(|_| default.to_string());
    raw.parse::<T>()
        .map_err(|error| AppError::Config(format!("{key} 解析失败: {error}")))
}
