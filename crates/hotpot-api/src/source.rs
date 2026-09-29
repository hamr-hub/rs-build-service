//! 构建源码就位：本地路径直接引用；Git 仓库在会话目录内 clone 到指定
//! 引用/sha。Git 输出逐行转成构建事件，失败显式报错（不允许带着空目录
//! 进入编译阶段）。

use std::path::{Path, PathBuf};

use hotpot_core::model::{BuildEvent, EventKind};
use hotpot_core::{BuildId, Result};
use tokio::process::Command;

/// 阶段事件发射器：保持 seq 在 fetch 阶段与后续执行器之间连续。
pub(super) struct PhaseSink<'a> {
    scheduler: &'a hotpot_scheduler::Scheduler,
    build_id: BuildId,
    next_seq: u64,
}

impl<'a> PhaseSink<'a> {
    pub(super) fn new(scheduler: &'a hotpot_scheduler::Scheduler, build_id: BuildId) -> Self {
        Self {
            scheduler,
            build_id,
            next_seq: 0,
        }
    }

    /// 已发射事件数；执行器以此作为起始 seq。
    pub(super) fn event_count(&self) -> u64 {
        self.next_seq
    }

    pub(super) async fn phase(&mut self, msg: impl Into<String>) -> Result<()> {
        self.emit(EventKind::Phase, msg.into()).await
    }

    /// 把外部命令的 stderr 逐行作为事件落库。
    pub(super) async fn stderr_lines(&mut self, text: &str) -> Result<()> {
        for line in text.lines() {
            self.emit(EventKind::Stderr, line.to_string()).await?;
        }
        Ok(())
    }

    async fn emit(&mut self, kind: EventKind, payload: String) -> Result<()> {
        let event = BuildEvent {
            build_id: self.build_id,
            seq: self.next_seq,
            timestamp_ms: hotpot_core::model::now_ms(),
            kind,
            payload,
        };
        self.next_seq += 1;
        self.scheduler.append_event(&event).await
    }
}

/// git 命令执行结果。
struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// 准备 Git 源码：clone/fetch 到 `dest` 并检出目标引用。
pub(super) async fn prepare_git(
    url: &str,
    ref_name: &str,
    sha: Option<&str>,
    dest: &Path,
    sink: &mut PhaseSink<'_>,
) -> Result<PathBuf> {
    sink.phase(format!("git clone --depth 1 --branch {ref_name} {url}"))
        .await?;

    // 优先浅克隆指定引用（最快路径）；服务端不支持时回退 init+fetch。
    let clone = run_git(
        &[
            "clone", "--quiet", "--depth", "1", "--branch", ref_name, url,
        ],
        &[dest.as_os_str()],
        // clone 时目标目录尚不存在，以父目录为 cwd。
        dest.parent().unwrap_or_else(|| Path::new(".")),
    )
    .await?;
    if !clone.success {
        sink.stderr_lines(&clone.stderr).await?;
        sink.phase("shallow clone unsupported; fetching full ref")
            .await?;
        run_git(&["init", "--quiet"], &[], dest).await?.ok()?;
        run_git(&["remote", "add", "origin", url], &[], dest)
            .await?
            .ok()?;
        run_git(&["fetch", "--quiet", "origin", ref_name], &[], dest)
            .await?
            .ok()?;
        run_git(
            &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
            &[],
            dest,
        )
        .await?
        .ok()?;
    }

    // 指定 sha：浅克隆的分支头可能不是该 sha（如固定到 PR 内提交），按需补取。
    if let Some(sha) = sha {
        let head = run_git(&["rev-parse", "HEAD"], &[], dest).await?;
        if head.stdout.trim() != sha {
            sink.phase(format!("git fetch origin {sha}")).await?;
            let fetched = run_git(&["fetch", "--quiet", "origin", sha], &[], dest).await?;
            if !fetched.success {
                sink.stderr_lines(&fetched.stderr).await?;
                return Err(hotpot_core::Error::Other(format!(
                    "could not fetch pinned sha {sha} from {url}"
                )));
            }
        }
        run_git(&["checkout", "--quiet", "--detach", sha], &[], dest)
            .await?
            .ok()?;
        let head = run_git(&["rev-parse", "HEAD"], &[], dest).await?;
        if head.stdout.trim() != sha {
            return Err(hotpot_core::Error::Other(format!(
                "git checkout {sha} produced {}",
                head.stdout.trim()
            )));
        }
    }
    Ok(dest.to_path_buf())
}

impl GitOutput {
    /// 成功时返回 Ok(())，失败时把 git 诊断包成错误。
    fn ok(&self) -> Result<()> {
        if self.success {
            Ok(())
        } else {
            Err(hotpot_core::Error::Other(format!(
                "git command failed: {}",
                self.stderr.trim()
            )))
        }
    }
}

/// 运行 git 子命令，`extra_args` 插在子命令参数之后。
async fn run_git(args: &[&str], extra_args: &[&std::ffi::OsStr], cwd: &Path) -> Result<GitOutput> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .args(extra_args.iter().copied())
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| hotpot_core::Error::Other(format!("spawn git: {e}")))?;
    Ok(GitOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}
