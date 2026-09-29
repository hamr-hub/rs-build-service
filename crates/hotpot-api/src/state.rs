//! HTTP 层共享状态。

use std::collections::HashMap;
use std::sync::Arc;

use hotpot_cacheproto::RemoteCache;
use hotpot_core::BuildId;
use hotpot_scheduler::Scheduler;
use hotpot_store::LocalStore;
use hotpot_worker::ExecutorKind;
use tokio::sync::{Mutex, watch};

/// 应用共享状态。
#[derive(Clone)]
pub struct AppState {
    pub scheduler: Scheduler,
    pub store: Arc<LocalStore>,
    /// 远程缓存（`/metrics` 统计命中率用；未挂载协议层时为 None）。
    pub cache: Option<RemoteCache>,
    /// 执行后端（供 `/metrics` 与 `/v1/toolchains` 标注）。
    pub executor: Arc<ExecutorKind>,
    /// 是否接受 git 来源构建。默认 false：git 来源会执行不可信仓库里的
    /// `build.rs`/proc-macro，等价于允许在服务进程权限下执行任意代码。
    pub allow_git_source: bool,
    /// 内嵌 worker 数（`/metrics` 暴露）。
    pub workers: usize,
    /// 资源回收器的累计计数（`/metrics` 暴露）。
    pub gc_stats: Arc<crate::gc::GcStats>,
    /// 构建默认使用的工具链描述（`/metrics` 的 `hotpot_info` 标签）。
    pub default_toolchain: String,
    /// 运行中构建的取消信号通道（构建结束即移除）。
    cancels: Arc<Mutex<HashMap<BuildId, watch::Sender<bool>>>>,
}

impl AppState {
    pub fn new(scheduler: Scheduler, store: LocalStore) -> Self {
        Self {
            scheduler,
            store: Arc::new(store),
            cache: None,
            executor: Arc::new(ExecutorKind::default()),
            allow_git_source: false,
            workers: 0,
            gc_stats: Arc::new(crate::gc::GcStats::default()),
            default_toolchain: "default".to_string(),
            cancels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 挂载远程缓存（协议层与指标共享同一实例）。
    pub fn with_cache(mut self, cache: RemoteCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// 记录执行后端。
    pub fn with_executor(mut self, executor: ExecutorKind) -> Self {
        self.executor = Arc::new(executor);
        self
    }

    /// 允许 git 来源构建（安全敏感，调用方需显式开启）。
    pub fn with_git_source(mut self, allow: bool) -> Self {
        self.allow_git_source = allow;
        self
    }

    /// 记录 worker 数与默认工具链（供 `/metrics` 标注）。
    pub fn with_runtime_info(
        mut self,
        workers: usize,
        default_toolchain: impl Into<String>,
    ) -> Self {
        self.workers = workers;
        self.default_toolchain = default_toolchain.into();
        self
    }

    /// 为构建注册取消通道，返回接收端。
    pub async fn register_cancel(&self, id: BuildId) -> watch::Receiver<bool> {
        let (tx, rx) = watch::channel(false);
        self.cancels.lock().await.insert(id, tx);
        rx
    }

    /// 移除取消通道。
    pub async fn remove_cancel(&self, id: BuildId) {
        self.cancels.lock().await.remove(&id);
    }

    /// 给运行中的构建发取消信号；没有对应构建（未在运行）返回 false。
    pub async fn signal_cancel(&self, id: BuildId) -> bool {
        match self.cancels.lock().await.get(&id) {
            Some(tx) => tx.send(true).is_ok(),
            None => false,
        }
    }
}
