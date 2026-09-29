//! 缓存协议适配：sccache WebDAV 兼容端点 + Turborepo v8 兼容端点。

pub mod remote;
pub mod routes;
pub mod state;

pub use remote::{CacheCounters, Namespace, NamespaceStats, RemoteCache};
pub use routes::router;
pub use state::{AuthState, CacheState, require_token};
