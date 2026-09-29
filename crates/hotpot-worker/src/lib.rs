//! Hotpot 构建执行器：在隔离的会话目录中以约定的缓存环境运行 cargo，
//! 采集带序号的构建事件与阶段耗时。支持本地与 docker（bollard）两种后端。

pub mod docker;
pub mod executor;
pub mod local;

pub use executor::{BuildEvent, BuildPlan, BuildResult, EndReason, ExecutorKind, run_build};
