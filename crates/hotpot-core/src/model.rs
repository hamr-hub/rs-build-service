//! 构建领域模型（核心字段；更完整的 API 类型在 hotpot-api 中定义）。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::ids::{BuildId, ProjectId};

/// 构建源码来源。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceSpec {
    /// Git 仓库与引用；sha 用于校验与内容寻址。
    Git {
        url: String,
        ref_name: String,
        sha: Option<String>,
    },
    /// 已上传的源码包（upload_id 由上传接口返回）。
    Upload {
        upload_id: String,
        root: Option<String>,
    },
    /// 主机上已有的项目目录（M1/本地 CLI 直跑用）。
    Local { path: String },
}

/// 构建状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    Queued,
    Dispatched,
    Running,
    Succeeded,
    Failed,
    Canceled,
    Timeout,
}

impl BuildStatus {
    /// 是否终态。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            BuildStatus::Succeeded
                | BuildStatus::Failed
                | BuildStatus::Canceled
                | BuildStatus::Timeout
        )
    }
}

/// 构建各阶段耗时（毫秒）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildTimings {
    pub queue_ms: u64,
    pub fetch_ms: u64,
    pub build_ms: u64,
    pub link_ms: Option<u64>,
    pub upload_ms: u64,
    pub total_ms: u64,
}

/// 构建 profile / 编译选项（显式可配档位）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildProfile {
    /// 工具链 channel（如 `1.98.0`），None 用 worker 默认。
    pub toolchain: Option<String>,
    pub mode: BuildMode,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub target: Option<String>,
    pub cargo_flags: Vec<String>,
}

impl Default for BuildProfile {
    fn default() -> Self {
        Self {
            toolchain: None,
            mode: BuildMode::Debug,
            features: Vec::new(),
            no_default_features: false,
            target: None,
            cargo_flags: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BuildMode {
    Debug,
    Release,
}

/// 构建事件（worker → 服务端 → SSE），`seq` 在单构建内单调递增。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildEvent {
    pub build_id: BuildId,
    pub seq: u64,
    pub timestamp_ms: i64,
    pub kind: EventKind,
    pub payload: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Stdout,
    Stderr,
    Phase,
    Status,
}

/// 产物元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub name: String,
    pub digest: String,
    pub size: u64,
    pub attrs: HashMap<String, String>,
}

/// 一条构建记录（队列表的内存映射）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRecord {
    pub id: BuildId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub source: SourceSpec,
    #[serde(default)]
    pub profile: BuildProfile,
    pub status: BuildStatus,
    #[serde(default)]
    pub timings: BuildTimings,
    /// Unix 毫秒。
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl BuildRecord {
    /// 新建排队记录。
    pub fn queued(source: SourceSpec, profile: BuildProfile) -> Self {
        Self {
            id: BuildId::new(),
            project_id: None,
            source,
            profile,
            status: BuildStatus::Queued,
            timings: BuildTimings::default(),
            created_at_ms: now_ms(),
            started_at_ms: None,
            finished_at_ms: None,
            error: None,
        }
    }
}

/// 当前 UTC Unix 毫秒。
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
