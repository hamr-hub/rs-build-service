//! 内嵌 worker：认领任务 → 执行 cargo → 采集产物入 CAS → 回写终态。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hotpot_core::model::{BuildRecord, BuildStatus, SourceSpec};
use hotpot_core::{ArtifactMeta, ContentDigest};
use hotpot_store::BlobStore;
use hotpot_worker::{BuildPlan, EndReason, ExecutorKind, run_build};
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::state::AppState;

const LEASE: Duration = Duration::from_secs(60);
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 单个 worker 的认领循环。
pub async fn worker_loop(
    state: AppState,
    worker_id: String,
    data_root: PathBuf,
    executor: ExecutorKind,
) {
    loop {
        let claimed = state.scheduler.claim_next(&worker_id, LEASE).await;
        match claimed {
            Ok(Some(rec)) => {
                if let Err(e) = handle_build(&state, &worker_id, rec, &data_root, &executor).await {
                    warn!("worker {worker_id} handle failed: {e}");
                }
            }
            Ok(None) => tokio::time::sleep(POLL_INTERVAL).await,
            Err(e) => {
                warn!("worker {worker_id} claim failed: {e}");
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}

async fn handle_build(
    state: &AppState,
    worker_id: &str,
    rec: BuildRecord,
    data_root: &Path,
    executor: &ExecutorKind,
) -> hotpot_core::Result<()> {
    let project_path = match &rec.source {
        SourceSpec::Local { path } => PathBuf::from(path),
        other => {
            let msg = format!("unsupported source: {other:?}");
            state
                .scheduler
                .finish_build(rec.id, BuildStatus::Failed, Default::default(), Some(msg))
                .await?;
            return Ok(());
        }
    };

    let session_dir = data_root.join("sessions").join(rec.id.to_string());
    fs::create_dir_all(&session_dir)?;

    let mut plan = BuildPlan::local(&project_path, &session_dir);
    plan.build_id = rec.id;
    plan.profile = rec.profile.clone();
    plan.cancel = Some(state.register_cancel(rec.id).await);

    // 长构建期间续租，防止失活接管触发重复执行。
    let renew_state = state.clone();
    let renew_worker = worker_id.to_string();
    let renew_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(LEASE_RENEW_INTERVAL).await;
            if let Err(e) = renew_state
                .scheduler
                .renew_lease(rec.id, &renew_worker, LEASE)
                .await
            {
                warn!("renew lease failed: {e}");
            }
        }
    });

    info!(build = %rec.id.short(), mode = ?rec.profile.mode, "running build");
    let (mut events, job) = run_build(plan, executor).await;
    while let Some(event) = events.recv().await {
        if let Err(e) = state.scheduler.append_event(&event).await {
            warn!("persist event failed: {e}");
        }
    }
    let result = job
        .await
        .map_err(|e| hotpot_core::Error::Other(e.to_string()))?;
    renew_task.abort();
    state.remove_cancel(rec.id).await;

    let mut status = match result.end_reason {
        EndReason::Completed if result.success => BuildStatus::Succeeded,
        EndReason::Canceled => BuildStatus::Canceled,
        EndReason::TimedOut => BuildStatus::Timeout,
        _ => BuildStatus::Failed,
    };
    let mut error = None;

    let mut artifacts = Vec::new();
    if status == BuildStatus::Succeeded {
        match collect_artifacts(&session_dir, &rec, state) {
            Ok(found) => artifacts = found,
            Err(e) => {
                // 编译成功但产物打包失败必须显式失败，不能报成功。
                status = BuildStatus::Failed;
                error = Some(format!("collect artifacts: {e}"));
            }
        }
    }
    if status == BuildStatus::Failed {
        error = error.or_else(|| Some("cargo build failed".to_string()));
    }

    state.scheduler.add_artifacts(rec.id, &artifacts).await?;
    state
        .scheduler
        .finish_build(rec.id, status, result.timings, error)
        .await?;
    info!(build = %rec.id.short(), ?status, artifact_cnt = artifacts.len(), "build finished");
    Ok(())
}

/// 收集 profile 目录顶层的可执行/库文件，逐一内容寻址入 CAS。
fn collect_artifacts(
    session_dir: &Path,
    rec: &BuildRecord,
    state: &AppState,
) -> hotpot_core::Result<Vec<ArtifactMeta>> {
    let mut profile_dir = session_dir.join("target");
    if let Some(triple) = &rec.profile.target {
        profile_dir = profile_dir.join(triple);
    }
    profile_dir = profile_dir.join(match rec.profile.mode {
        hotpot_core::BuildMode::Debug => "debug",
        hotpot_core::BuildMode::Release => "release",
    });

    let mut artifacts = Vec::new();
    for entry in WalkDir::new(&profile_dir).min_depth(1).max_depth(1) {
        let entry = entry.map_err(std::io::Error::other)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // 仅采集产物本身：跳过 .d 依赖文件与隐藏文件。
        if name.ends_with(".d") || name.starts_with('.') {
            continue;
        }

        let bytes = fs::read(entry.path())?;
        let digest = ContentDigest::of_bytes(&bytes);
        state.store.put(&bytes)?;

        let mut attrs = HashMap::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = entry
                .metadata()
                .map_err(std::io::Error::other)?
                .permissions()
                .mode();
            if mode & 0o111 != 0 {
                attrs.insert("executable".to_string(), "true".to_string());
            }
        }
        artifacts.push(ArtifactMeta {
            name,
            digest: digest.to_hex(),
            size: bytes.len() as u64,
            attrs,
        });
    }
    Ok(artifacts)
}
