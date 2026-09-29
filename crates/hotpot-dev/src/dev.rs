//! 开发循环编排：初始构建并启动 → 监听变更 → 增量重建 → 优雅重启；
//! 构建失败保留旧进程；可选 socket keeper 消除重启窗口拒连。

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::builder::{self, BuildArgs};
use crate::keeper;
use crate::process::{self, ManagedProcess};
use crate::watcher::{self, ChangeSet, DEFAULT_DEBOUNCE};

/// `hotpot dev` 参数。
#[derive(Debug, Clone)]
pub struct DevOptions {
    pub project: PathBuf,
    pub build: BuildArgs,
    pub extra_env: BTreeMap<String, String>,
    /// Some(公共地址)：启用 socket keeper；子进程经 HOTPOT_BIND_ADDR 绑后端。
    pub public_addr: Option<String>,
}

/// 运行开发循环直到 `shutdown` 完成。
pub async fn run<F>(options: DevOptions, shutdown: F) -> anyhow::Result<()>
where
    F: std::future::Future<Output = ()>,
{
    tokio::pin!(shutdown);
    let project = options.project.clone();

    // 先注册监听：FSEvents/kqueue 的底层注册是异步完成的，初始构建耗时足以
    // 覆盖注册窗口；若在构建之后才创建 watcher，紧随其后的一次编辑可能正落在
    // 注册空窗里，事件永久丢失。
    let mut watch = watcher::watch(&project, DEFAULT_DEBOUNCE)?;

    // keeper 与后端地址（启用时）。
    let mut env = options.extra_env.clone();
    let backend: Option<SocketAddr> = match &options.public_addr {
        Some(public) => {
            let port = keeper::pick_backend_port()?;
            let backend: SocketAddr = format!("127.0.0.1:{port}").parse()?;
            env.insert("HOTPOT_BIND_ADDR".into(), backend.to_string());
            let public = public.clone();
            tokio::spawn(async move { keeper::run(&public, backend).await });
            Some(backend)
        }
        None => None,
    };

    // 初始构建（失败直接终止：无可用产物）。
    let first = builder::build(&project, &options.build).await?;
    if !first.success {
        anyhow::bail!("initial build failed:\n{}", first.stderr_tail);
    }
    let binary = first
        .binary
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no binary after initial build"))?;
    let mut active = process::spawn(&binary, &env)?;
    if let Some(backend) = backend {
        wait_backend(backend).await;
    }
    tracing::info!(
        "dev serving {} (pid {:?}); watching for changes…",
        binary.display(),
        active.pid()
    );

    // 收敛初始构建期间发生的编辑：cargo 可能漏采构建过程中的写入，
    // 有变更就补一次重建，保证启动即与磁盘一致。
    if let Some(change) = drain_pending(&mut watch.rx) {
        restart_if_built(&project, &options.build, &env, backend, &mut active, change).await;
    }

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            change = watch.rx.recv() => match change {
                Some(change) => {
                    restart_if_built(
                        &project, &options.build, &env, backend, &mut active, change,
                    ).await;
                }
                None => break,
            },
        }
    }

    let _ = active.stop(process::DEFAULT_STOP_TIMEOUT).await;
    Ok(())
}

/// 非阻塞排空已聚合的变更集并合并为一个；无变更返回 None。
fn drain_pending(rx: &mut tokio::sync::mpsc::Receiver<ChangeSet>) -> Option<ChangeSet> {
    let mut merged: BTreeSet<PathBuf> = BTreeSet::new();
    while let Ok(set) = rx.try_recv() {
        merged.extend(set.paths);
    }
    if merged.is_empty() {
        None
    } else {
        Some(ChangeSet {
            paths: merged.into_iter().collect(),
        })
    }
}

/// 重建并（成功时）热重启；失败保留旧进程。
async fn restart_if_built(
    project: &Path,
    build_args: &BuildArgs,
    env: &BTreeMap<String, String>,
    backend: Option<SocketAddr>,
    active: &mut ManagedProcess,
    change: ChangeSet,
) {
    let count = change.paths.len();
    tracing::info!("change detected ({count} path); rebuilding…");
    let outcome = builder::build(project, build_args).await;
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            tracing::error!("build invocation failed: {e}; keeping current process");
            return;
        }
    };

    if !outcome.success {
        tracing::warn!(
            "build failed ({}ms); kept old process",
            outcome.duration.as_millis()
        );
        for line in outcome.stderr_tail.lines() {
            eprintln!("    {line}");
        }
        return;
    }

    let Some(binary) = outcome.binary.clone() else {
        tracing::warn!("build succeeded but binary unresolved; kept old process");
        return;
    };

    let _ = active.stop(process::DEFAULT_STOP_TIMEOUT).await;
    match process::spawn(&binary, env) {
        Ok(new) => {
            *active = new;
            if let Some(backend) = backend {
                wait_backend(backend).await;
            }
            tracing::info!(
                "restarted {} in {}ms",
                binary.display(),
                outcome.duration.as_millis()
            );
        }
        Err(e) => tracing::error!("failed to start new binary: {e}"),
    }
}

/// 等待子进程在后端端口就绪（仅 keeper 模式）。
async fn wait_backend(backend: SocketAddr) {
    let start = tokio::time::Instant::now();
    let window = crate::keeper::READY_WINDOW;
    loop {
        if tokio::net::TcpStream::connect(backend).await.is_ok() {
            return;
        }
        if start.elapsed() > window {
            tracing::warn!("backend {backend} not ready within {window:?}");
            return;
        }
        tokio::time::sleep(crate::keeper::RETRY_INTERVAL).await;
    }
}
