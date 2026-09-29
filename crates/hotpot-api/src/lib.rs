//! Hotpot HTTP API：构建提交、状态、SSE 日志、取消与产物下载。

pub mod driver;
pub mod error;
pub mod routes;
pub mod state;

pub use routes::router;
pub use state::AppState;
