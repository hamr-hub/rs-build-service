//! 资源生命周期：会话目录回收 + CAS 容量兜底。
//!
//! # 为什么必须有
//!
//! 构建服务最典型的死法不是崩溃，而是**磁盘被自己写满**。两个来源：
//!
//! 1. **会话目录**：`sessions/<build-id>/target` 是完整的 cargo target 目录，
//!    release 构建动辄几百 MB 到数 GB。产物在采集时已经进了 CAS，
//!    会话目录**只在构建进行中**和**失败后短窗口内**有用。
//! 2. **CAS 对象**：若不回收，容量上限形同虚设。
//!
//! 实测（demo-webapp，4 次构建）：遗留 1.1 GB 会话目录；
//! 且此前 `StoreOptions::max_bytes` 从未被设置、`gc()` 从未被调用。
//!
//! # 安全不变量
//!
//! **绝不删除非终态构建的会话目录**。判定依据是调度器里的构建状态，
//! 而不是目录 mtime——后者在「刚被认领、target 还没落盘」时会误导。
//! 失活 worker 留下的 `dispatched` 构建会被重新认领重跑，删掉它的
//! 工作目录会让重跑从零开始，甚至读到写了一半的 target。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use hotpot_core::BuildId;
use hotpot_core::model::BuildStatus;
use hotpot_store::LocalStore;
use tracing::{debug, info, warn};

/// 回收器的累计计数（`/metrics` 暴露）。
///
/// 用原子计数而不是「每次现算」：扫描本身有成本（要 stat 每个目录），
/// 把最近一次的结果缓存起来，让 `/metrics` 保持廉价。
#[derive(Debug, Default)]
pub struct GcStats {
    /// 扫描轮次。
    pub sweeps: std::sync::atomic::AtomicU64,
    /// 删除的会话目录数。
    pub sessions_removed: std::sync::atomic::AtomicU64,
    /// 因构建非终态而跳过的目录数（安全不变量生效的证据）。
    pub sessions_skipped_active: std::sync::atomic::AtomicU64,
    /// 会话目录释放的字节数。
    pub session_bytes_freed: std::sync::atomic::AtomicU64,
    /// CAS 容量回收轮次。
    pub cas_gc_runs: std::sync::atomic::AtomicU64,
    /// CAS 回收释放的字节数。
    pub cas_bytes_freed: std::sync::atomic::AtomicU64,
    /// 最近一轮扫描的会话目录总数。
    pub sessions_present: std::sync::atomic::AtomicU64,
    /// 累计删除的 git 共享工作区数。
    pub workspaces_removed: std::sync::atomic::AtomicU64,
    /// 累计由工作区回收释放的字节数。
    pub workspace_bytes_freed: std::sync::atomic::AtomicU64,
}

impl GcStats {
    /// 记录一轮会话扫描的结果。
    pub fn record_sweep(&self, report: &SweepReport) {
        use std::sync::atomic::Ordering::Relaxed;
        self.sweeps.fetch_add(1, Relaxed);
        self.sessions_removed
            .fetch_add(report.removed as u64, Relaxed);
        self.sessions_skipped_active
            .fetch_add(report.skipped_active as u64, Relaxed);
        self.session_bytes_freed
            .fetch_add(report.freed_bytes, Relaxed);
        self.sessions_present.store(report.scanned as u64, Relaxed);
    }

    /// 记录一轮 git 工作区回收。
    pub fn record_workspace_sweep(&self, removed: usize, freed: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.workspaces_removed.fetch_add(removed as u64, Relaxed);
        self.workspace_bytes_freed.fetch_add(freed, Relaxed);
    }

    /// 记录一轮 CAS 容量回收。
    pub fn record_cas_gc(&self, freed: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.cas_gc_runs.fetch_add(1, Relaxed);
        self.cas_bytes_freed.fetch_add(freed, Relaxed);
    }
}

/// 会话目录保留策略。
#[derive(Debug, Clone)]
pub struct SessionPolicy {
    /// 超过该时长的**成功**构建会话目录被删除。
    pub max_age: Duration,
    /// 超过该时长的**失败**构建会话目录被删除（默认更长，便于排查）。
    pub failed_max_age: Duration,
    /// 额外保留最近 N 个终态构建的会话目录（0 = 不按数量保留）。
    pub keep_last: usize,
    /// 后台扫描间隔。
    pub interval: Duration,
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            // 成功构建的产物已在 CAS 里，会话目录只需留很短的重现窗口。
            max_age: Duration::from_secs(3600),
            // 失败构建留久一点：这是唯一能看到「构建当时长什么样」的地方。
            failed_max_age: Duration::from_secs(24 * 3600),
            keep_last: 0,
            interval: Duration::from_secs(600),
        }
    }
}

/// 一次回收的结果。
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct SweepReport {
    /// 扫描到的会话目录数。
    pub scanned: usize,
    /// 删除的会话目录数。
    pub removed: usize,
    /// 释放的字节数。
    pub freed_bytes: u64,
    /// 因构建非终态而**跳过**的目录数（安全不变量生效的证据）。
    pub skipped_active: usize,
    /// 删除的 git 共享工作区数。
    pub workspaces_removed: usize,
    /// 删除的 git 共享工作区释放的字节数。
    pub workspace_bytes_freed: u64,
}

/// 一个会话目录的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// 删除。
    Remove,
    /// 保留：构建仍在进行（安全不变量）。
    Active,
    /// 保留：还在保留窗口内，或命中 keep_last。
    Retained,
}

/// 判定单个会话目录是否可删（纯函数，便于穷举测试）。
///
/// `status` 为 None 表示数据库里没有这条构建（孤儿目录，例如库被重置）。
fn decide(
    status: Option<BuildStatus>,
    age: Duration,
    is_failed_window: bool,
    keep_last_hit: bool,
    policy: &SessionPolicy,
) -> Verdict {
    if keep_last_hit {
        return Verdict::Retained;
    }
    match status {
        // 安全不变量：非终态构建的工作目录绝不能删。
        Some(s) if !s.is_terminal() => Verdict::Active,
        _ => {
            let limit = if is_failed_window {
                policy.failed_max_age
            } else {
                policy.max_age
            };
            if age >= limit {
                Verdict::Remove
            } else {
                Verdict::Retained
            }
        }
    }
}

/// 回收会话目录。
pub async fn sweep_sessions(
    scheduler: &hotpot_scheduler::Scheduler,
    data_root: &Path,
    policy: &SessionPolicy,
) -> hotpot_core::Result<SweepReport> {
    let sessions_dir = data_root.join("sessions");
    let mut entries = match tokio::fs::read_dir(&sessions_dir).await {
        Ok(entries) => entries,
        // 还没有任何构建：不是错误。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SweepReport::default()),
        Err(e) => return Err(std::io::Error::other(e).into()),
    };

    struct Candidate {
        path: PathBuf,
        status: Option<BuildStatus>,
        finished_at_ms: Option<i64>,
        mtime: SystemTime,
        size: u64,
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(std::io::Error::other)? {
        let path = entry.path();
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let (status, finished_at_ms) = match parse_build_id(name) {
            Some(id) => match scheduler.get_build(id).await? {
                Some(rec) => (Some(rec.status), rec.finished_at_ms),
                // 孤儿目录：库里没有这条构建。
                None => (None, None),
            },
            None => (None, None),
        };
        candidates.push(Candidate {
            path,
            status,
            finished_at_ms,
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: dir_size(&entry.path()),
        });
    }

    // keep_last：按结束时间倒序，前 N 个终态构建一律保留。
    // 用下标集合而非路径引用，避免与随后的消费循环产生借用冲突。
    let mut protected: std::collections::HashSet<usize> = std::collections::HashSet::new();
    if policy.keep_last > 0 {
        let mut finished: Vec<(usize, i64)> = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.status.is_some_and(|s| s.is_terminal()))
            .map(|(i, c)| (i, c.finished_at_ms.unwrap_or(0)))
            .collect();
        finished.sort_by_key(|(_, finished_at)| std::cmp::Reverse(*finished_at));
        protected.extend(finished.into_iter().take(policy.keep_last).map(|(i, _)| i));
    }

    let now_ms = hotpot_core::model::now_ms();
    let mut report = SweepReport {
        scanned: candidates.len(),
        ..Default::default()
    };
    for (index, c) in candidates.into_iter().enumerate() {
        let keep_last_hit = protected.contains(&index);
        // 结束时间优先；孤儿目录回退到目录 mtime。
        let age = match c.finished_at_ms {
            Some(f) => Duration::from_millis((now_ms - f).max(0) as u64),
            None => SystemTime::now()
                .duration_since(c.mtime)
                .unwrap_or(Duration::ZERO),
        };
        let is_failed = matches!(c.status, Some(BuildStatus::Failed | BuildStatus::Timeout));
        match decide(c.status, age, is_failed, keep_last_hit, policy) {
            Verdict::Active => report.skipped_active += 1,
            Verdict::Retained => {}
            Verdict::Remove => match remove_dir_all(&c.path).await {
                Ok(()) => {
                    report.removed += 1;
                    report.freed_bytes += c.size;
                    debug!("swept session {}", c.path.display());
                }
                // 并发删除（另一个 sweep 或人工清理）不算失败。
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!("failed to remove session {}: {e}", c.path.display()),
            },
        }
    }

    if report.removed > 0 {
        info!(
            scanned = report.scanned,
            removed = report.removed,
            freed_bytes = report.freed_bytes,
            "session sweep reclaimed disk"
        );
    }
    Ok(report)
}

/// 回收长期未使用的 git 共享工作区。
///
/// 共享工作区是**故意稳定**的——路径变了 cargo 的 target fingerprint 就变，
/// 增量与 sccache 全部失效。但稳定不等于永不过期：长期不用的仓库会一直
/// 占着磁盘（一个中型仓库连 target 可以到 GB 级）。
///
/// 策略：只按「最后修改时间超过 `max_age`」删除。不追踪活跃度是刻意的
/// 取舍——工作区上的 mtime 会被构建活动刷新，所以「很久没被动过」等价于
/// 「很久没人构建它」。
pub async fn sweep_git_workspaces(
    workspaces_root: &Path,
    max_age: Duration,
) -> hotpot_core::Result<(usize, u64)> {
    let mut entries = match tokio::fs::read_dir(workspaces_root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(std::io::Error::other(e).into()),
    };
    let mut removed = 0usize;
    let mut freed = 0u64;
    while let Some(entry) = entries.next_entry().await.map_err(std::io::Error::other)? {
        let path = entry.path();
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let age = meta
            .modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .unwrap_or(Duration::ZERO);
        if age < max_age {
            continue;
        }
        let size = dir_size(&path);
        if remove_dir_all(&path).await.is_ok() {
            removed += 1;
            freed += size;
            debug!("swept git workspace {}", path.display());
        }
    }
    if removed > 0 {
        info!(
            removed,
            freed_bytes = freed,
            "git workspace sweep reclaimed disk"
        );
    }
    Ok((removed, freed))
}

/// 目录树字节数（构建产物目录动辄数万文件；个别文件 stat 失败按 0 计）。
fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// 递归删除；目录不存在视为成功。
async fn remove_dir_all(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 解析目录名为 BuildId（目录名可能是 `bld_<uuid>` 或裸 uuid）。
fn parse_build_id(name: &str) -> Option<BuildId> {
    uuid::Uuid::parse_str(name.split_once('_').map_or(name, |(_, u)| u))
        .ok()
        .map(BuildId)
}

/// CAS 容量兜底：超限时回收最旧对象。
///
/// 注意：回收可能删掉仍被缓存协议索引引用的对象，`RemoteCache::get`
/// 会自愈（删悬挂索引并按未命中处理）。代价是缓存命中率，**不是**正确性。
/// 构建产物同理——CAS 是缓存，产物丢了重跑即可。
pub fn enforce_capacity(store: &LocalStore, stats: &GcStats) -> hotpot_core::Result<u64> {
    let freed = store.gc()?;
    stats.record_cas_gc(freed);
    if freed > 0 {
        info!(freed_bytes = freed, "CAS gc reclaimed space");
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotpot_core::BuildStatus::*;

    fn policy() -> SessionPolicy {
        SessionPolicy {
            max_age: Duration::from_secs(100),
            failed_max_age: Duration::from_secs(1000),
            keep_last: 0,
            interval: Duration::from_secs(10),
        }
    }

    #[test]
    fn never_removes_active_builds() {
        // 无论多老，进行中的构建都必须保留——这是本模块的核心不变量。
        for status in [Queued, Dispatched, Running] {
            let v = decide(
                Some(status),
                Duration::from_secs(999_999),
                false,
                false,
                &policy(),
            );
            assert_eq!(v, Verdict::Active, "{status:?} must be treated as active");
        }
    }

    #[test]
    fn removes_expired_terminal_builds() {
        for status in [Succeeded, Failed, Canceled, Timeout] {
            let v = decide(
                Some(status),
                Duration::from_secs(200),
                false,
                false,
                &policy(),
            );
            assert_eq!(v, Verdict::Remove, "{status:?} should expire");
        }
    }

    #[test]
    fn failed_builds_get_a_longer_window() {
        let age = Duration::from_secs(500);
        // 500s：成功构建已过期，失败构建仍在窗口内。
        assert_eq!(
            decide(Some(Succeeded), age, false, false, &policy()),
            Verdict::Remove
        );
        assert_eq!(
            decide(Some(Failed), age, true, false, &policy()),
            Verdict::Retained
        );
    }

    #[test]
    fn keep_last_overrides_expiry() {
        assert_eq!(
            decide(
                Some(Succeeded),
                Duration::from_secs(9999),
                false,
                true,
                &policy()
            ),
            Verdict::Retained
        );
    }

    #[test]
    fn orphan_sessions_expire_by_mtime() {
        // 数据库里没有记录（status=None）时按普通窗口判龄，不视为活动。
        assert_eq!(
            decide(None, Duration::from_secs(500), false, false, &policy()),
            Verdict::Remove
        );
        assert_eq!(
            decide(None, Duration::from_secs(10), false, false, &policy()),
            Verdict::Retained
        );
    }
}
