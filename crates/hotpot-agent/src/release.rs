//! 发布版本描述与部署状态机的持久化状态。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 一个不可变的发布版本：二进制副本路径 + 固定的环境变量表。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    pub path: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// 部署状态机相位（与 architecture.md §9.1 一致）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    #[default]
    Idle,
    Preflighting,
    Armed,
    Draining,
    Committed,
    Failed,
}

/// 落盘状态；agent 崩溃后据此恢复（重新拉起 current）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub listen: String,
    pub current: Option<Release>,
    #[serde(default)]
    pub previous: Option<Release>,
    #[serde(default)]
    pub phase: Phase,
    #[serde(default)]
    pub deploy_id: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

impl AgentState {
    pub fn new(listen: String) -> Self {
        Self {
            listen,
            current: None,
            previous: None,
            phase: Phase::Idle,
            deploy_id: None,
            last_error: None,
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
