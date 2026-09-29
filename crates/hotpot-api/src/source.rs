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
    /// 用于把事件广播给 SSE 订阅者（拉源码阶段的事件也要实时可见）。
    state: &'a crate::AppState,
    build_id: BuildId,
    next_seq: u64,
}

impl<'a> PhaseSink<'a> {
    pub(super) fn new(state: &'a crate::AppState, build_id: BuildId) -> Self {
        Self {
            scheduler: &state.scheduler,
            state,
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
        self.scheduler.append_event(&event).await?;
        // 与 driver 同一套广播，否则"拉源码"阶段的事件不会实时到达前端。
        self.state.publish(crate::state::StreamSignal::Log(event));
        Ok(())
    }
}

/// 准备**共享** Git 工作区：同一项目（URL）的所有构建复用 tools_dir 下的
/// 同一份仓库与检出路径，配合 warm target 卷让 warm 构建成为 cargo 意义上
/// 的真正 no-op——否则每次构建源码路径都变（sessions/<id>/src），cargo
/// fingerprint 会重编译项目自身的全部 crate。
///
/// 首次按 `--filter=blob:none` 做部分克隆（历史树可达、blob 按需拉取，
/// 避免大仓库一次性全量下载）；每次构建 fetch 指定 ref/sha 后强制检出。
/// 并发安全由调用方的按项目锁保证（见 driver 中 git_workspace_lock）。
pub(super) async fn prepare_git_workspace(
    workspaces_root: &Path,
    url: &str,
    ref_name: &str,
    sha: Option<&str>,
    sink: &mut PhaseSink<'_>,
) -> Result<PathBuf> {
    use hotpot_core::ContentDigest;
    // 短 id 只是为了目录名可读；真正的隔离靠 `git_project_lock` 按完整 URL 加锁。
    let id = ContentDigest::of_bytes(url.as_bytes()).to_hex();
    let id = &id[..16];
    let root = workspaces_root.join(id);
    let repo = root.join("repo");
    std::fs::create_dir_all(&root)?;

    if !repo.join(".git").exists() {
        sink.phase(format!("git clone --filter=blob:none {url}"))
            .await?;
        let clone = run_git(
            &["clone", "--quiet", "--filter=blob:none", url],
            &[repo.as_os_str()],
            &root,
        )
        .await?;
        if !clone.success {
            sink.stderr_lines(&clone.stderr).await?;
            // 服务端不支持 partial clone filter：回退普通浅克隆（功能等价）。
            sink.phase("partial clone unsupported; falling back to shallow clone")
                .await?;
            run_git(
                &["clone", "--quiet", "--depth", "1", url],
                &[repo.as_os_str()],
                &root,
            )
            .await?
            .ok()?;
        }
    }

    // 浅回退克隆可能不含目标 ref：按需补 fetch；固定 sha 时直接 fetch sha。
    let target = sha.unwrap_or(ref_name);
    sink.phase(format!("git fetch origin {target}")).await?;
    let fetched = run_git(&["fetch", "--quiet", "origin", target], &[], &repo).await?;
    if !fetched.success {
        sink.stderr_lines(&fetched.stderr).await?;
        return Err(hotpot_core::Error::Other(format!(
            "could not fetch {target} from {url}"
        )));
    }
    // 强制覆盖检出：工作区里没有用户修改，--force 保证每次构建起点确定。
    run_git(
        &["checkout", "--quiet", "--force", "--detach", target],
        &[],
        &repo,
    )
    .await?
    .ok()?;

    // **必须校验实际检出的 commit**。指定 sha 时若 HEAD 与之不符，说明
    // fetch 到的是别的东西；此时若放行，产物会被贴上「按请求的 sha 构建」
    // 的假标签——缓存与产物 provenance 全部失真。宁可构建失败。
    let head = run_git(&["rev-parse", "HEAD"], &[], &repo).await?;
    let head = head.stdout.trim();
    if let Some(want) = sha
        && !head.eq_ignore_ascii_case(want)
    {
        return Err(hotpot_core::Error::Other(format!(
            "git checkout {want} landed on {head} for {url}; refusing to build a different commit"
        )));
    }
    // 记录实际构建的 commit：既方便排查，也让产物有可追溯的源码标识。
    sink.phase(format!("building {url} at {head}")).await?;
    Ok(repo)
}

/// git 命令执行结果。
struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
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
