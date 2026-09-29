//! 源码监听：notify 递归监听 + 静默窗口防抖，仅保留与构建相关的变更。
//!
//! 后台线程做纯逻辑防抖，向前台 async 循环投递 [`ChangeSet`]；watcher 句柄
//! 随 [`Watch`] 持有，drop 即停止监听。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use tokio::sync::mpsc;

/// 默认防抖静默窗口：批量保存/一次 cargo 写入只产生一个变更集。
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(200);

/// 一次防抖窗口内聚合的相关变更路径。
#[derive(Debug, Clone)]
pub struct ChangeSet {
    pub paths: Vec<PathBuf>,
}

/// 活动监听句柄；drop 后监听与后台线程结束。
pub struct Watch {
    pub rx: mpsc::Receiver<ChangeSet>,
    _watcher: RecommendedWatcher,
}

/// 递归监听 `root`，以 `debounce` 静默窗口聚合变更。
pub fn watch(root: impl AsRef<Path>, debounce: Duration) -> anyhow::Result<Watch> {
    let root = root.as_ref().to_path_buf();
    let (notify_tx, notify_rx) = channel();
    let mut watcher = recommended_watcher(move |res| {
        let _ = notify_tx.send(res);
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    let (tx, rx) = mpsc::channel(64);
    std::thread::spawn(move || debounce_loop(notify_rx, tx, debounce));

    Ok(Watch {
        rx,
        _watcher: watcher,
    })
}

fn debounce_loop(
    rx: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    tx: mpsc::Sender<ChangeSet>,
    debounce: Duration,
) {
    let mut pending: HashSet<PathBuf> = HashSet::new();
    loop {
        match rx.recv_timeout(debounce) {
            Ok(Ok(event)) => {
                for path in event.paths {
                    if is_relevant(&path) {
                        pending.insert(path);
                    }
                }
            }
            // 单个事件错误不致命，继续监听。
            Ok(Err(e)) => tracing::warn!("watch event error: {e}"),
            Err(RecvTimeoutError::Timeout) => {
                if pending.is_empty() {
                    continue;
                }
                let mut paths: Vec<PathBuf> = pending.drain().collect();
                paths.sort();
                if tx.blocking_send(ChangeSet { paths }).is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// 是否为应触发重建的路径：Rust 源或清单文件，且不在 target/.git 下。
pub fn is_relevant(path: &Path) -> bool {
    let in_ignored_dir = path
        .components()
        .any(|c| c.as_os_str() == "target" || c.as_os_str() == ".git");
    if in_ignored_dir {
        return false;
    }
    let is_rs = path.extension().is_some_and(|e| e == "rs");
    let is_manifest = path
        .file_name()
        .is_some_and(|n| n == "Cargo.toml" || n == "Cargo.lock");
    is_rs || is_manifest
}
