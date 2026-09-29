//! Docker（bollard）构建执行：容器内跑 cargo，项目/target/sccache 经
//! bind mount 与宿主机共享。target 目录是挂载点，产物采集路径与本地一致。

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bollard::API_DEFAULT_VERSION;
use bollard::container::{
    AttachContainerOptions, AttachContainerResults, Config, CreateContainerOptions,
    KillContainerOptions, RemoveContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::models::HostConfig;
use bollard::{Docker, container::LogOutput};
use futures::StreamExt;
use hotpot_core::BuildId;
use hotpot_core::model::{BuildEvent, EventKind};
use tokio::sync::mpsc;

use super::executor::{BuildPlan, BuildResult, EndReason, cargo_invocation};

/// 容器内挂载点。
const WORKSPACE: &str = "/workspace";
const TARGET_MOUNT: &str = "/target";
const SCCACHE_MOUNT: &str = "/sccache";

const CONNECT_TIMEOUT_SECS: u64 = 30;
/// 停止后等待容器真正退出的回收窗口。
const RECLAIM_TIMEOUT: Duration = Duration::from_secs(10);

/// 执行 docker 构建；构建事件通过返回的接收器流式送出。
pub fn run_docker(
    plan: BuildPlan,
    image: &str,
    docker_host: Option<&str>,
) -> (
    mpsc::Receiver<BuildEvent>,
    tokio::task::JoinHandle<BuildResult>,
) {
    let (tx, rx) = mpsc::channel(256);
    let image = image.to_string();
    let docker_host = docker_host.map(str::to_string);
    let handle = tokio::spawn(async move { run_inner(plan, image, docker_host, tx).await });
    (rx, handle)
}

async fn run_inner(
    plan: BuildPlan,
    image: String,
    docker_host: Option<String>,
    tx: mpsc::Sender<BuildEvent>,
) -> BuildResult {
    let seq = Arc::new(AtomicU64::new(0));
    let started = Instant::now();

    let docker = match connect(docker_host.as_deref()).await {
        Ok(d) => d,
        Err(e) => {
            emit_event(
                &tx,
                &seq,
                &plan.build_id,
                EventKind::Stderr,
                format!("connect docker daemon failed: {e}"),
            )
            .await;
            return failed(&started, EndReason::SpawnFailed, None);
        }
    };

    if let Err(e) = ensure_image(&docker, &image, &tx, &seq, &plan.build_id).await {
        emit_event(
            &tx,
            &seq,
            &plan.build_id,
            EventKind::Stderr,
            format!("pull image {image} failed: {e}"),
        )
        .await;
        return failed(&started, EndReason::SpawnFailed, None);
    }

    // 挂载源路径（必须绝对路径；macOS colima 下 /tmp、/Users 在 VM 内可见）。
    let project = match plan.project_dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            emit_event(
                &tx,
                &seq,
                &plan.build_id,
                EventKind::Stderr,
                format!("canonicalize project failed: {e}"),
            )
            .await;
            return failed(&started, EndReason::SpawnFailed, None);
        }
    };
    std::fs::create_dir_all(&plan.target_dir).ok();
    let target = match plan.target_dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            emit_event(
                &tx,
                &seq,
                &plan.build_id,
                EventKind::Stderr,
                format!("canonicalize target dir failed: {e}"),
            )
            .await;
            return failed(&started, EndReason::SpawnFailed, None);
        }
    };

    let sccache = plan
        .sccache_dir
        .as_ref()
        .and_then(|d| d.canonicalize().ok());

    // 容器内 sccache 命令名固定为 sccache（镜像须提供）；缓存目录指向挂载点。
    let (args, extra_env) = {
        let cache_dir = sccache.as_ref().map(|_| Path::new(SCCACHE_MOUNT));
        let bin = plan
            .sccache_bin
            .as_deref()
            .or_else(|| cache_dir.map(|_| Path::new("sccache")));
        cargo_invocation(&plan.profile, cache_dir, bin)
    };

    let mut binds = vec![
        format!("{}:{WORKSPACE}", project.display()),
        format!("{}:{TARGET_MOUNT}", target.display()),
    ];
    if let Some(dir) = &sccache {
        binds.push(format!("{}:{SCCACHE_MOUNT}", dir.display()));
    }

    let env = {
        let mut vars = vec![format!("CARGO_TARGET_DIR={TARGET_MOUNT}")];
        vars.extend(extra_env.iter().map(|(k, v)| format!("{k}={v}")));
        vars
    };

    let name = format!(
        "hotpot-build-{}-{}",
        plan.build_id,
        started.elapsed().as_nanos()
    );
    let config = Config {
        image: Some(image.clone()),
        working_dir: Some(WORKSPACE.to_string()),
        cmd: Some(std::iter::once("cargo".to_string()).chain(args).collect()),
        env: Some(env),
        host_config: Some(HostConfig {
            binds: Some(binds),
            ..Default::default()
        }),
        ..Default::default()
    };

    let created = match docker
        .create_container(
            Some(CreateContainerOptions {
                name: name.clone(),
                platform: None,
            }),
            config,
        )
        .await
    {
        Ok(c) => c,
        Err(e) => {
            emit_event(
                &tx,
                &seq,
                &plan.build_id,
                EventKind::Stderr,
                format!("create container failed: {e}"),
            )
            .await;
            return failed(&started, EndReason::SpawnFailed, None);
        }
    };
    let id = created.id;

    // 先 attach 再 start，避免漏掉启动瞬间的输出。
    let AttachContainerResults { output, .. } = match docker
        .attach_container(
            &id,
            Some(AttachContainerOptions::<String> {
                stdout: Some(true),
                stderr: Some(true),
                stdin: Some(false),
                stream: Some(true),
                ..Default::default()
            }),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            emit_event(
                &tx,
                &seq,
                &plan.build_id,
                EventKind::Stderr,
                format!("attach container failed: {e}"),
            )
            .await;
            remove_container(&docker, &id).await;
            return failed(&started, EndReason::SpawnFailed, None);
        }
    };

    // 日志泵：把 cargo 输出逐行转为构建事件。
    let pump = tokio::spawn({
        let tx = tx.clone();
        let seq = seq.clone();
        let build_id = plan.build_id;
        async move { pump_logs(output, &tx, &seq, &build_id).await }
    });

    if let Err(e) = docker.start_container::<&str>(&id, None).await {
        emit_event(
            &tx,
            &seq,
            &plan.build_id,
            EventKind::Stderr,
            format!("start container failed: {e}"),
        )
        .await;
        pump.await.ok();
        remove_container(&docker, &id).await;
        return failed(&started, EndReason::SpawnFailed, None);
    }
    emit_phase(&tx, &seq, &plan.build_id, "build started (docker)").await;

    // 等待退出 / 超时 / 取消。
    let mut wait_stream = docker.wait_container::<&str>(&id, None);
    let mut cancel_rx = plan.cancel.clone();
    let end_reason;
    let exit_code;

    tokio::select! {
        item = wait_stream.next() => {
            match item {
                Some(Ok(resp)) => {
                    let code = resp.status_code as i32;
                    end_reason = EndReason::Completed;
                    exit_code = Some(code);
                }
                Some(Err(e)) => {
                    emit_event(
                        &tx, &seq, &plan.build_id, EventKind::Stderr,
                        format!("wait container failed: {e}"),
                    ).await;
                    end_reason = EndReason::SpawnFailed;
                    exit_code = None;
                }
                None => {
                    end_reason = EndReason::SpawnFailed;
                    exit_code = None;
                }
            }
        }
        _ = tokio::time::timeout(plan.timeout, std::future::pending::<()>()) => {
            end_reason = EndReason::TimedOut;
            exit_code = None;
        }
        _ = wait_cancel(&mut cancel_rx) => {
            end_reason = EndReason::Canceled;
            exit_code = None;
        }
    }

    if end_reason != EndReason::Completed {
        kill_and_reclaim(&docker, &id, &tx, &seq, &plan.build_id).await;
        let msg = match end_reason {
            EndReason::TimedOut => format!("build timed out after {}s", plan.timeout.as_secs()),
            EndReason::Canceled => "build canceled".to_string(),
            EndReason::SpawnFailed => "container failed".to_string(),
            EndReason::Completed => String::new(),
        };
        emit_phase(&tx, &seq, &plan.build_id, &msg).await;
    }

    // 容器退出后 attach 流结束，回收日志泵。
    let _ = tokio::time::timeout(Duration::from_secs(5), pump).await;
    let success = exit_code == Some(0);
    remove_container(&docker, &id).await;

    let build_ms = started.elapsed().as_millis() as u64;
    BuildResult {
        success,
        exit_code,
        end_reason,
        timings: hotpot_core::BuildTimings {
            build_ms,
            total_ms: build_ms,
            ..Default::default()
        },
    }
}

/// 解析/拉取镜像：本地存在则直接复用。
async fn ensure_image(
    docker: &Docker,
    image: &str,
    tx: &mpsc::Sender<BuildEvent>,
    seq: &AtomicU64,
    build_id: &BuildId,
) -> Result<(), bollard::errors::Error> {
    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }
    emit_phase(tx, seq, build_id, &format!("pulling image {image}…")).await;

    let mut stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: image.to_string(),
            ..Default::default()
        }),
        None,
        None,
    );
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(item) = stream.next().await {
        let info = item?;
        if let Some(err) = info.error {
            emit_event(tx, seq, build_id, EventKind::Stderr, err).await;
        } else if let Some(status) = info.status {
            // 去重状态行，避免刷屏。
            if seen.insert(status.clone()) {
                emit_event(tx, seq, build_id, EventKind::Phase, status).await;
            }
        }
    }
    Ok(())
}

/// attach 日志流 → 逐行构建事件。
async fn pump_logs(
    mut output: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<LogOutput, bollard::errors::Error>> + Send>,
    >,
    tx: &mpsc::Sender<BuildEvent>,
    seq: &AtomicU64,
    build_id: &BuildId,
) {
    while let Some(item) = output.next().await {
        let Ok(item) = item else { continue };
        let (kind, bytes) = match item {
            LogOutput::StdOut { message } | LogOutput::Console { message } => {
                (EventKind::Stdout, message)
            }
            LogOutput::StdErr { message } => (EventKind::Stderr, message),
            LogOutput::StdIn { message } => (EventKind::Stdout, message),
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            emit_event(tx, seq, build_id, kind, line.to_string()).await;
        }
    }
}

/// kill 容器并在回收窗口内等待其退出（避免泄漏与 remove 卡住）。
async fn kill_and_reclaim(
    docker: &Docker,
    id: &str,
    tx: &mpsc::Sender<BuildEvent>,
    seq: &AtomicU64,
    build_id: &BuildId,
) {
    let _ = docker
        .kill_container(
            id,
            Some(KillContainerOptions {
                signal: "SIGKILL".to_string(),
            }),
        )
        .await;
    let reclaimed = async {
        let mut stream = docker.wait_container::<&str>(id, None);
        stream.next().await
    };
    if tokio::time::timeout(RECLAIM_TIMEOUT, reclaimed)
        .await
        .is_err()
    {
        emit_event(
            tx,
            seq,
            build_id,
            EventKind::Stderr,
            "container did not exit after SIGKILL".to_string(),
        )
        .await;
    }
}

async fn remove_container(docker: &Docker, id: &str) {
    let _ = docker
        .remove_container(
            id,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
}

/// 连接 daemon：显式 host 优先；否则探测常见 socket（含 colima）。
async fn connect(docker_host: Option<&str>) -> Result<Docker, String> {
    if let Some(host) = docker_host {
        return connect_uri(host).await;
    }
    if let Ok(d) = Docker::connect_with_local_defaults() {
        if d.ping().await.is_ok() {
            return Ok(d);
        }
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        "/var/run/docker.sock",
        &format!("{home}/.docker/run/docker.sock"),
        &format!("{home}/.colima/default/docker.sock"),
        &format!("{home}/.docker/desktop/docker.sock"),
    ];
    for socket in candidates {
        if !Path::new(socket).exists() {
            continue;
        }
        if let Ok(d) = Docker::connect_with_unix(socket, CONNECT_TIMEOUT_SECS, API_DEFAULT_VERSION)
        {
            if d.ping().await.is_ok() {
                return Ok(d);
            }
        }
    }
    Err("no reachable docker daemon (tried defaults and common sockets)".to_string())
}

async fn connect_uri(host: &str) -> Result<Docker, String> {
    let d = if let Some(socket) = host.strip_prefix("unix://") {
        Docker::connect_with_unix(socket, CONNECT_TIMEOUT_SECS, API_DEFAULT_VERSION)
    } else if host.starts_with("http://") || host.starts_with("https://") {
        Docker::connect_with_http(host, CONNECT_TIMEOUT_SECS, API_DEFAULT_VERSION)
    } else {
        Docker::connect_with_local(host, CONNECT_TIMEOUT_SECS, API_DEFAULT_VERSION)
    }
    .map_err(|e| e.to_string())?;
    d.ping().await.map_err(|e| e.to_string())?;
    Ok(d)
}

fn failed(started: &Instant, end_reason: EndReason, exit_code: Option<i32>) -> BuildResult {
    let total_ms = started.elapsed().as_millis() as u64;
    BuildResult {
        success: false,
        exit_code,
        end_reason,
        timings: hotpot_core::BuildTimings {
            total_ms,
            ..Default::default()
        },
    }
}

/// 阻塞直到取消信号变为 true；无信号通道或发送端消失时永不返回。
async fn wait_cancel(rx: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    if let Some(rx) = rx.as_mut() {
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    } else {
        std::future::pending().await
    }
}

async fn emit_phase(tx: &mpsc::Sender<BuildEvent>, seq: &AtomicU64, build_id: &BuildId, msg: &str) {
    emit_event(tx, seq, build_id, EventKind::Phase, msg.to_string()).await;
}

async fn emit_event(
    tx: &mpsc::Sender<BuildEvent>,
    seq: &AtomicU64,
    build_id: &BuildId,
    kind: EventKind,
    payload: String,
) {
    let event = BuildEvent {
        build_id: *build_id,
        seq: seq.fetch_add(1, Ordering::Relaxed),
        timestamp_ms: chrono::Utc::now().timestamp_millis(),
        kind,
        payload,
    };
    // 接收方消失则丢弃（构建已无人关注）。
    let _ = tx.send(event).await;
}
