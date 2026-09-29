//! API 错误到 HTTP 响应的映射。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hotpot_core::Error as CoreError;
use serde_json::json;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Invalid(msg) => Self::bad_request(msg),
            CoreError::BuildNotFound(msg) => Self::not_found(msg),
            CoreError::Unauthorized => Self {
                status: StatusCode::UNAUTHORIZED,
                message: "unauthorized".into(),
            },
            other => Self::internal(other.to_string()),
        }
    }
}

/// 解析路径参数中的 BuildId（兼容裸 uuid 与 `bld_<uuid>` 两种形式）。
pub fn parse_build_id(s: &str) -> Result<hotpot_core::BuildId, ApiError> {
    let uuid = s.split_once('_').map_or(s, |(_, u)| u);
    uuid::Uuid::parse_str(uuid)
        .map(hotpot_core::BuildId)
        .map_err(|e| ApiError::bad_request(format!("invalid build id {s}: {e}")))
}
