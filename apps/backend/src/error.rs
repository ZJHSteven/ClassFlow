/*!
统一错误定义。

这样做有两个直接好处：

1. 业务层可以专注返回“语义明确”的错误，而不是手工拼 HTTP 响应。
2. axum 层只需要把 `AppError` 转成 JSON，就能保证接口错误格式统一。
*/

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("配置错误: {0}")]
    Config(String),
    #[error("未授权")]
    Unauthorized,
    #[error("资源不存在: {0}")]
    NotFound(String),
    #[error("请求参数错误: {0}")]
    BadRequest(String),
    #[error("外部服务错误: {0}")]
    External(String),
    #[error("数据库错误: {0}")]
    Database(String),
    #[error("IO 错误: {0}")]
    Io(String),
    #[error("内部错误: {0}")]
    Internal(String),
}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(error: reqwest::Error) -> Self {
        Self::External(error.to_string())
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) | Self::Config(_) => StatusCode::BAD_REQUEST,
            Self::External(_) => StatusCode::BAD_GATEWAY,
            Self::Database(_) | Self::Io(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = ErrorBody {
            error: self.to_string(),
        };

        (status, Json(body)).into_response()
    }
}
