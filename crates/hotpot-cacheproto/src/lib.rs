//! 缓存协议适配：sccache WebDAV 兼容端点 + Turborepo v8 兼容端点。

pub mod remote;
pub mod routes;
pub mod state;

pub use remote::{Namespace, RemoteCache};
pub use routes::router;
pub use state::CacheState;
