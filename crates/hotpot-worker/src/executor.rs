//! 构建执行：统一的 [`BuildPlan`] / [`BuildResult`] 类型，与本地/容器两种后端。
//!
//! - [`local`]：直接 spawn 本机 cargo；
//! - [`docker`]：bollard 在容器内跑 cargo，项目与 target 目录 bind mount，
//!   产物经挂载点回到宿主机，采集路径与本地模式完全一致。

use std::path::PathBuf;
use std::time::Duration;

use hotpot_core::model::BuildProfile;
use hotpot_core::toolchain::ToolchainRequest;
use hotpot_core::{BuildId, BuildTimings};
use tokio::sync::mpsc;

pub use super::local::run_local;

/// 单次构建的执行计划。
#[derive(Debug, Clone)]
pub struct BuildPlan {
    pub build_id: BuildId,
    /// Cargo 项目根目录（含 Cargo.toml）。
    pub project_dir: PathBuf,
    /// 会话级 CARGO_TARGET_DIR（绝不跨项目共享）。
    pub target_dir: PathBuf,
    pub profile: BuildProfile,
    /// 构建超时。
    pub timeout: Duration,
    /// sccache 设置：None 不使用；Some(dir) 使用 sccache 并指定 SCCACHE_DIR。
    pub sccache_dir: Option<PathBuf>,
    /// sccache 可执行文件路径：本地模式为宿主机路径，容器模式为镜像内命令名。
    pub sccache_bin: Option<PathBuf>,
    /// Docker 模式：跨构建共享的容器 CARGO_HOME（registry/git 缓存）。
    /// 必须是**容器平台专用**目录，不能指向宿主 CARGO_HOME。
    pub cargo_home: Option<PathBuf>,
    /// Docker 模式：预取工具目录（宿主下载好的 Linux sccache 等），挂载到容器 PATH 首位。
    pub tools_dir: Option<PathBuf>,
    /// Docker 模式：是否自动安装系统包（slim 镜像的 build-essential、交叉工具链）。
    ///
    /// 官方 `rust:*-slim` 镜像不带 `cc`，任何含 C 代码的依赖（jemalloc/ring/
    /// openssl…）都会在 build script 阶段失败，因此默认开启自动供给。
    /// 但它需要容器能访问 apt 源——**气隙环境**或**自带预烘焙镜像**的用户
    /// 必须能关掉它，否则每次构建都卡在网络上。
    pub provision_system_packages: bool,
    /// 项目的稳定标识（git URL / 本地绝对路径）：设置后在 Docker 模式复用
    /// 持久化 warm target 卷（按镜像 × triple × mode 分桶）。
    pub warm_identity: Option<String>,
    /// 透传到构建进程/容器的额外环境变量（如 SCCACHE_WEBDAV_ENDPOINT）。
    pub extra_env: Vec<(String, String)>,
    /// 执行器事件的起始 seq（fetch 等前置阶段可能已占用编号）。
    /// 事件 seq 起始值。源码获取阶段（git clone 等）已占用若干 seq，
    /// 执行器从这里续接，保证单构建内 seq 全局单调。
    pub first_seq: u64,
    /// 取消信号：值变为 true 时杀掉 cargo 并提前结束。
    pub cancel: Option<tokio::sync::watch::Receiver<bool>>,
}

/// 构建结束原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Completed,
    TimedOut,
    Canceled,
    SpawnFailed,
}

/// 执行后端选择。
#[derive(Debug, Clone, Default)]
pub enum ExecutorKind {
    /// 宿主机直接执行。
    #[default]
    Local,
    /// Docker 容器执行（镜像须含 cargo + 链接器）。
    Docker {
        /// 工具链镜像，如 rust:slim-bookworm。
        image: String,
        /// Docker daemon 地址；None 走 bollard 本地默认（含 DOCKER_HOST）。
        docker_host: Option<String>,
    },
}

/// 构建结果。
#[derive(Debug)]
pub struct BuildResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub end_reason: EndReason,
    pub timings: BuildTimings,
}

/// 执行构建；构建事件通过返回的接收器流式送出。
pub async fn run_build(
    plan: BuildPlan,
    executor: &ExecutorKind,
) -> (
    mpsc::Receiver<BuildEvent>,
    tokio::task::JoinHandle<BuildResult>,
) {
    match executor {
        ExecutorKind::Local => run_local(plan),
        ExecutorKind::Docker { image, docker_host } => {
            super::docker::run_docker(plan, image, docker_host.as_deref())
        }
    }
}

impl BuildPlan {
    /// M1 便捷构造：本地项目 + 独立临时 target 目录。
    pub fn local(project_dir: impl Into<PathBuf>, session_dir: impl Into<PathBuf>) -> Self {
        let target_dir = session_dir.into().join("target");
        Self {
            build_id: BuildId::new(),
            project_dir: project_dir.into(),
            target_dir,
            profile: BuildProfile::default(),
            timeout: Duration::from_secs(1800),
            sccache_dir: None,
            sccache_bin: None,
            cargo_home: None,
            tools_dir: None,
            provision_system_packages: true,
            warm_identity: None,
            extra_env: Vec::new(),
            first_seq: 0,
            cancel: None,
        }
    }

    pub fn release(mut self) -> Self {
        self.profile.mode = hotpot_core::model::BuildMode::Release;
        self
    }
}

/// 由计划生成 cargo 参数与环境变量；本地与容器后端共用，保证行为一致。
/// sccache 场景下 `rustc_wrapper` 为对应的 wrapper 命令（宿主路径或容器内命令名）。
pub(crate) fn cargo_invocation(
    toolchain: Option<&ToolchainRequest>,
    profile: &BuildProfile,
    sccache_dir: Option<&std::path::Path>,
    sccache_bin: Option<&std::path::Path>,
) -> (Vec<String>, Vec<(String, String)>) {
    let mut args = Vec::new();
    if let Some(tc) = toolchain {
        args.push(tc.cargo_plus_arg());
    }
    args.push("build".to_string());
    match profile.mode {
        hotpot_core::model::BuildMode::Release => args.push("--release".to_string()),
        hotpot_core::model::BuildMode::Debug => {}
    }
    if profile.no_default_features {
        args.push("--no-default-features".to_string());
    }
    if !profile.features.is_empty() {
        args.push("--features".to_string());
        args.push(profile.features.join(","));
    }
    if let Some(target) = &profile.target {
        args.push("--target".to_string());
        args.push(target.clone());
    }
    args.extend(profile.cargo_flags.iter().cloned());

    // sccache 与增量互斥；sccache 场景强制关闭增量。
    let mut env = vec![
        ("CARGO_INCREMENTAL".to_string(), "0".to_string()),
        ("CARGO_TERM_COLOR".to_string(), "never".to_string()),
    ];
    if let Some(cache_dir) = sccache_dir {
        let bin = sccache_bin
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("sccache"));
        env.push(("RUSTC_WRAPPER".to_string(), bin.display().to_string()));
        env.push(("SCCACHE_DIR".to_string(), cache_dir.display().to_string()));
    }
    (args, env)
}

// 仅用于模块内类型引用（见 hotpot_core::model::BuildEvent）。
pub use hotpot_core::model::BuildEvent;
