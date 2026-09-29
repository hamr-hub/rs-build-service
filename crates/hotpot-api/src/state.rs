//! HTTP 层共享状态。

use std::collections::HashMap;
use std::sync::Arc;

use hotpot_core::BuildId;
use hotpot_scheduler::Scheduler;
use hotpot_store::LocalStore;
use tokio::sync::{Mutex, watch};

/// 应用共享状态。
#[derive(Clone)]
pub struct AppState {
    pub scheduler: Scheduler,
    pub store: Arc<LocalStore>,
    /// 运行中构建的取消信号通道（构建结束即移除）。
    cancels: Arc<Mutex<HashMap<BuildId, watch::Sender<bool>>>>,
}

impl AppState {
    pub fn new(scheduler: Scheduler, store: LocalStore) -> Self {
        Self {
            scheduler,
            store: Arc::new(store),
            cancels: Arc::new(Mutex::new(HashMap::new())),
        }
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
