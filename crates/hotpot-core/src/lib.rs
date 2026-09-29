//! Hotpot 核心领域模型：标识符、内容摘要、错误类型与构建规格。

pub mod config;
pub mod digest;
pub mod error;
pub mod ids;
pub mod model;
pub mod toolchain;

pub use digest::ContentDigest;
pub use error::{Error, Result};
pub use ids::{BuildId, ProjectId};
pub use model::{
    ArtifactMeta, BuildEvent, BuildMode, BuildProfile, BuildRecord, BuildStatus, BuildTimings,
    EventKind, SourceSpec,
};
pub use toolchain::{ToolchainChannel, ToolchainRequest, parse_toolchain};
