//! 进程级 cargo 构建执行（宿主机本地）。

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use hotpot_core::BuildId;
use hotpot_core::model::{BuildEvent, EventKind};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::debug;

use super::executor::{BuildPlan, BuildResult, EndReason, cargo_invocation};

/// 执行本地构建；构建事件通过返回的接收器流式送出。
pub fn run_local(
    plan: BuildPlan,
) -> (
    mpsc::Receiver<BuildEvent>,
    tokio::task::JoinHandle<BuildResult>,
) {
    let (tx, rx) = mpsc::channel(256);
    let handle = tokio::spawn(run_inner(plan, tx));
    (rx, handle)
}

async fn run_inner(plan: BuildPlan, tx: mpsc::Sender<BuildEvent>) -> BuildResult {
    let seq = Arc::new(AtomicU64::new(plan.first_seq));
    let started = Instant::now();

    // 工具链解析失败不应 panic：API 边界已校验，但 worker 也可被直接调用。
    let toolchain = match plan.profile.toolchain.as_deref() {
        Some(raw) => match hotpot_core::parse_toolchain(raw) {
            Ok(tc) => Some(tc),
            Err(e) => {
                emit_event(
                    &tx,
                    &seq,
                    &plan.build_id,
                    EventKind::Stderr,
                    format!("invalid toolchain '{raw}': {e}"),
                )
                .await;
                return BuildResult {
                    success: false,
                    exit_code: None,
                    end_reason: EndReason::SpawnFailed,
                    timings: build_timings(&started),
                };
            }
        },
        None => None,
    };
    let (args, extra_env) = cargo_invocation(
        toolchain.as_ref(),
        &plan.profile,
        plan.sccache_dir.as_deref(),
        plan.sccache_bin.as_deref(),
    );

    let mut cmd = Command::new(cargo_bin());
    cmd.current_dir(&plan.project_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CARGO_TARGET_DIR", &plan.target_dir)
        .envs(extra_env)
        .envs(plan.extra_env.clone());

    emit_phase(&tx, &seq, &plan.build_id, "build started").await;
    debug!(?plan, "spawning cargo");

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            emit_event(
                &tx,
                &seq,
                &plan.build_id,
                EventKind::Stderr,
                format!("failed to spawn cargo: {e}"),
            )
            .await;
            return BuildResult {
                success: false,
                exit_code: None,
                end_reason: EndReason::SpawnFailed,
                timings: build_timings(&started),
            };
        }
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let tx_out = tx.clone();
    let seq_out = seq.clone();
    let id = plan.build_id;
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            emit_event(&tx_out, &seq_out, &id, EventKind::Stdout, line).await;
        }
    });
    let tx_err = tx.clone();
    let seq_err = seq.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            emit_event(&tx_err, &seq_err, &id, EventKind::Stderr, line).await;
        }
    });

    let timed_wait = tokio::time::timeout(plan.timeout, child.wait());
    let mut cancel_rx = plan.cancel;

    let (end_reason, exit_code, success) = tokio::select! {
        res = timed_wait => match res {
            Ok(Ok(status)) => (EndReason::Completed, status.code(), status.success()),
            Ok(Err(e)) => {
                emit_event(
                    &tx, &seq, &plan.build_id, EventKind::Stderr,
                    format!("failed to wait on cargo: {e}"),
                ).await;
                (EndReason::SpawnFailed, None, false)
            }
            Err(_) => (EndReason::TimedOut, None, false),
        },
        _ = wait_cancel(&mut cancel_rx) => (EndReason::Canceled, None, false),
    };

    // 超时/取消时 cargo 可能仍在运行：杀掉并回收，避免会话目录泄漏进程。
    if end_reason != EndReason::Completed {
        let _ = child.start_kill();
        let _ = child.wait().await;
        let msg = match end_reason {
            EndReason::TimedOut => format!("build timed out after {}s", plan.timeout.as_secs()),
            EndReason::Canceled => "build canceled".to_string(),
            EndReason::SpawnFailed => "cargo process failed".to_string(),
            EndReason::Completed => String::new(),
        };
        emit_phase(&tx, &seq, &plan.build_id, &msg).await;
    }

    stdout_task.await.ok();
    stderr_task.await.ok();

    BuildResult {
        success,
        exit_code,
        end_reason,
        timings: build_timings(&started),
    }
}

fn build_timings(started: &Instant) -> hotpot_core::BuildTimings {
    let build_ms = started.elapsed().as_millis() as u64;
    hotpot_core::BuildTimings {
        build_ms,
        total_ms: build_ms,
        ..Default::default()
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

fn cargo_bin() -> String {
    std::env::var("HOTPOT_CARGO_BIN").unwrap_or_else(|_| "cargo".to_string())
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
