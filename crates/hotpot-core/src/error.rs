//! Hotpot 统一错误类型。

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("content digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },

    #[error("storage error: {0}")]
    Storage(String),

    #[error("build not found: {0}")]
    BuildNotFound(String),

    #[error("invalid request: {0}")]
    Invalid(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("other: {0}")]
    Other(String),
}
