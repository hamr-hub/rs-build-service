//! Hotpot HTTP API：构建提交、状态、SSE 日志、取消与产物下载。

pub mod driver;
pub mod error;
pub mod metrics;
pub mod routes;
pub mod source;
pub mod state;
pub mod toolchains;

pub use routes::router;
pub use state::AppState;
