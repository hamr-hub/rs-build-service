//! 内嵌 worker：认领任务 → 就位源码 → 执行 cargo → 采集产物入 CAS → 回写终态。

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hotpot_core::model::{BuildRecord, BuildStatus, EventKind, SourceSpec};
use hotpot_core::{ArtifactMeta, ContentDigest};
use hotpot_store::BlobStore;
use hotpot_worker::{BuildPlan, EndReason, ExecutorKind, run_build};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::source::{PhaseSink, prepare_git};
use crate::state::AppState;

const LEASE: Duration = Duration::from_secs(60);
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 构建失败时写进 `error` 字段的 stderr 尾部行数。
const ERROR_TAIL_LINES: usize = 12;

/// 自身构建的 sccache 闭环配置：让 Hotpot 的构建复用 Hotpot 自己的
/// sccache WebDAV 端点（crate 级缓存），而不只是对外提供协议。
#[derive(Debug, Clone)]
pub struct SccacheConfig {
    /// 宿主 sccache 目录（local 模式使用）。
    pub host_dir: PathBuf,
    /// sccache WebDAV 端点（Hotpot 自身 `/sccache`）。None = 只用本地盘缓存。
    pub webdav_url: Option<String>,
    /// docker 模式：预取 Linux sccache 的落地目录。
    pub tools_dir: PathBuf,
}

/// worker 运行期配置。
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub data_root: PathBuf,
    pub executor: ExecutorKind,
    /// None 表示不启用编译缓存。
    pub sccache: Option<SccacheConfig>,
    /// 单构建超时。
    pub timeout: Duration,
    /// 是否接受 git 来源构建（会执行不可信代码，默认关闭）。
    pub allow_git_source: bool,
}

impl WorkerConfig {
    pub fn new(data_root: PathBuf, executor: ExecutorKind) -> Self {
        Self {
            data_root,
            executor,
            sccache: None,
            timeout: Duration::from_secs(1800),
            allow_git_source: false,
        }
    }

    pub fn with_sccache(mut self, sccache: SccacheConfig) -> Self {
        self.sccache = Some(sccache);
        self
    }

    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }
}

/// sccache 可用性在 worker 启动时确定一次，避免每个构建重复探测。
enum SccacheBootstrap {
    /// 不可用：构建不带编译缓存（降级而非失败）。
    Disabled,
    /// 宿主 PATH 中的 sccache 可执行文件。
    Host(PathBuf),
    /// 容器内使用：缓存目录按构建隔离，二进制由 docker 执行器按 daemon
    /// 架构解析（单文件挂载 + 失败回退镜像 PATH）。
    Container,
}

/// 单个 worker 的认领循环。
pub async fn worker_loop(state: AppState, worker_id: String, config: WorkerConfig) {
    // sccache 预取只做一次；任何一步失败都降级为「不带编译缓存」，
    // 加速是可选项，构建正确性不能依赖它。
    let bootstrap = prepare_sccache(config.sccache.as_ref(), &config.executor).await;
    loop {
        let claimed = state.scheduler.claim_next(&worker_id, LEASE).await;
        match claimed {
            Ok(Some(rec)) => {
                if let Err(e) = handle_build(&state, &worker_id, rec, &config, &bootstrap).await {
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
    config: &WorkerConfig,
    bootstrap: &SccacheBootstrap,
) -> hotpot_core::Result<()> {
    let data_root = config.data_root.as_path();
    let executor = &config.executor;
    let session_dir = data_root.join("sessions").join(rec.id.to_string());
    fs::create_dir_all(&session_dir)?;

    // ---- 源码就位（fetch 阶段；Local 无 IO，Git 在会话目录内 clone）----
    let fetch_started = Instant::now();
    let mut sink = PhaseSink::new(&state.scheduler, rec.id);
    let prepared = match &rec.source {
        SourceSpec::Local { path } => Ok(PathBuf::from(path)),
        SourceSpec::Git { url, ref_name, sha } => {
            if !config.allow_git_source {
                return fail(
                    state,
                    rec.id,
                    "git source builds are disabled on this server \
                     (start the server with --allow-git-source to enable)"
                        .to_string(),
                )
                .await;
            }
            let src = session_dir.join("src");
            prepare_git(url, ref_name, sha.as_deref(), &src, &mut sink).await
        }
        SourceSpec::Upload { .. } => Err(hotpot_core::Error::Other(
            "upload source kind is not supported yet".to_string(),
        )),
    };
    let project_path = match prepared {
        Ok(path) => path,
        Err(e) => {
            return fail(state, rec.id, format!("prepare source: {e}")).await;
        }
    };
    let fetch_ms = fetch_started.elapsed().as_millis() as u64;

    let mut plan = BuildPlan::local(&project_path, &session_dir);
    plan.build_id = rec.id;
    plan.profile = rec.profile.clone();
    plan.timeout = config.timeout;
    plan.cancel = Some(state.register_cancel(rec.id).await);
    plan.first_seq = sink.event_count();
    apply_sccache(
        &mut plan,
        config.sccache.as_ref(),
        bootstrap,
        &session_dir,
        &rec,
    );

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

    let queue_ms = millis_since(rec.created_at_ms);
    state.scheduler.mark_running(rec.id).await?;
    info!(build = %rec.id.short(), mode = ?rec.profile.mode, "running build");
    let (mut events, job) = run_build(plan, executor).await;
    // 保留最近若干条 stderr：构建失败时直接写进 error 字段，
    // 否则用户只能自己翻 SSE 日志才知道是「依赖拉不到」还是「代码写错」。
    let mut tail: Vec<String> = Vec::new();
    while let Some(event) = events.recv().await {
        if event.kind == EventKind::Stderr && !event.payload.trim().is_empty() {
            if tail.len() == ERROR_TAIL_LINES {
                tail.remove(0);
            }
            tail.push(event.payload.trim().to_string());
        }
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

    // ---- 产物采集（upload 阶段；编译成功但打包失败必须显式失败）----
    let upload_started = Instant::now();
    let mut artifacts = Vec::new();
    if status == BuildStatus::Succeeded {
        match collect_artifacts(&session_dir, &rec, state) {
            Ok(found) => artifacts = found,
            Err(e) => {
                status = BuildStatus::Failed;
                error = Some(format!("collect artifacts: {e}"));
            }
        }
    }
    let upload_ms = upload_started.elapsed().as_millis() as u64;
    if status == BuildStatus::Failed {
        error = error.or_else(|| {
            // 采集失败已有具体原因；这里只补编译/执行失败的真实尾部输出。
            Some(if tail.is_empty() {
                describe_end_reason(result.end_reason).to_string()
            } else {
                tail.join("\n")
            })
        });
    }

    state.scheduler.add_artifacts(rec.id, &artifacts).await?;
    let timings = hotpot_core::BuildTimings {
        queue_ms,
        fetch_ms,
        build_ms: result.timings.build_ms,
        upload_ms,
        total_ms: millis_since(rec.created_at_ms),
        ..Default::default()
    };
    state
        .scheduler
        .finish_build(rec.id, status, timings, error)
        .await?;
    info!(build = %rec.id.short(), ?status, artifact_cnt = artifacts.len(), "build finished");
    Ok(())
}

/// 一次性确定 sccache 可用性：
/// - 远端端点不可达 → 告警并继续（本地盘缓存仍有价值）；
/// - local 模式要求宿主 PATH 有 sccache；
/// - docker 模式只校验可达性，二进制解析交给执行器。
async fn prepare_sccache(cfg: Option<&SccacheConfig>, executor: &ExecutorKind) -> SccacheBootstrap {
    let Some(cfg) = cfg else {
        return SccacheBootstrap::Disabled;
    };
    if let Some(url) = &cfg.webdav_url
        && let Err(e) = probe_http(url).await
    {
        // 注意 sccache 自身对不可达端点只会退化为本地缓存并打一行 warning，
        // 用户极易忽略；这里显式告警，让「远端没生效」在服务日志里可见。
        warn!("sccache 远端端点不可达（{e}）：{url}，本次仅使用本地盘缓存");
    }
    match executor {
        ExecutorKind::Local => match which("sccache") {
            Some(bin) => {
                info!(
                    "sccache 已启用：{} → {}",
                    bin.display(),
                    cfg.webdav_url.as_deref().unwrap_or("(仅本地盘)")
                );
                SccacheBootstrap::Host(bin)
            }
            None => {
                warn!("宿主 PATH 中没有 sccache，本次运行不带编译缓存");
                SccacheBootstrap::Disabled
            }
        },
        ExecutorKind::Docker { .. } => {
            // 不在这里预取：二进制由 docker 执行器在建容器时按 daemon 架构
            // 解析（单文件挂载，失败回退镜像 PATH），两处预取会分叉。
            info!(
                "容器内 sccache 已启用：{}",
                cfg.webdav_url.as_deref().unwrap_or("(仅容器本地盘)")
            );
            SccacheBootstrap::Container
        }
    }
}

/// 把 sccache 设置注入构建计划。
fn apply_sccache(
    plan: &mut BuildPlan,
    cfg: Option<&SccacheConfig>,
    bootstrap: &SccacheBootstrap,
    session_dir: &Path,
    rec: &BuildRecord,
) {
    let (Some(cfg), enabled) = (cfg, bootstrap) else {
        return;
    };
    match enabled {
        SccacheBootstrap::Disabled => return,
        SccacheBootstrap::Host(bin) => {
            if fs::create_dir_all(&cfg.host_dir).is_err() {
                warn!("创建 sccache 目录失败：{}", cfg.host_dir.display());
                return;
            }
            plan.sccache_dir = Some(cfg.host_dir.clone());
            plan.sccache_bin = Some(bin.clone());
        }
        SccacheBootstrap::Container => {
            // 容器内缓存目录按构建隔离：sccache 的本地盘里同时存 server 状态
            // 与对象，多个构建容器并发共享同一挂载目录容易互相干扰。
            let dir = session_dir.join("sccache");
            let _ = fs::create_dir_all(&dir);
            plan.sccache_dir = Some(dir);
            // 预取目录交给执行器解析二进制；同时给容器专用 CARGO_HOME，
            // 让 registry/git 缓存跨构建复用（容器平台固定，可安全共享；
            // 绝不能复用宿主 CARGO_HOME——那是宿主平台的）。
            plan.tools_dir = Some(cfg.tools_dir.clone());
            plan.cargo_home = Some(cfg.tools_dir.join("cargo-home"));
        }
    }
    if let Some(url) = &cfg.webdav_url {
        // 权威变量名是 SCCACHE_WEBDAV_ENDPOINT（不存在 SCCACHE_WEBDAV_URL）；
        // 等价写法是 SCCACHE_REMOTE_STORAGE=webdav+<url>。
        plan.extra_env
            .push(("SCCACHE_WEBDAV_ENDPOINT".to_string(), url.clone()));
    }
    // 后台 server 不因空闲退出，避免每次构建都重新握手远端。
    plan.extra_env
        .push(("SCCACHE_IDLE_TIMEOUT".to_string(), "0".to_string()));
    debug!(build = %rec.id.short(), "build uses sccache");
}

/// 极简 HTTP 探测：确认端点所在服务能应答，避免为一次启动检查引入 HTTP 客户端依赖。
/// `url` 形如 `http://host:port/sccache`。
async fn probe_http(url: &str) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("仅支持 http:// 端点：{url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let mut stream = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect(authority),
    )
    .await
    .map_err(|_| format!("连接 {authority} 超时"))?
    .map_err(|e| format!("连接 {authority} 失败: {e}"))?;
    let request = format!("HEAD {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("写探测请求失败: {e}"))?;
    let mut buf = [0u8; 64];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| format!("读探测响应失败: {e}"))?;
    let head = String::from_utf8_lossy(&buf[..n]);
    // 404/405 也说明服务在应答：探测的是「有没有服务」，不是「路径对不对」。
    if head.contains(" 200") || head.contains(" 404") || head.contains(" 405") {
        Ok(())
    } else {
        Err(format!(
            "{authority} 响应异常：{}",
            head.lines().next().unwrap_or("")
        ))
    }
}

/// 在 PATH 中查找可执行文件（不引入 which crate）。
fn which(exe: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    let file = format!("{exe}{}", env::consts::EXE_SUFFIX);
    env::split_paths(&path)
        .map(|dir| dir.join(&file))
        .find(|p| p.is_file())
}

/// 结束原因的人类可读描述（没有 stderr 可用时的兜底错误文案）。
fn describe_end_reason(reason: EndReason) -> &'static str {
    match reason {
        EndReason::Completed => "cargo build failed",
        EndReason::TimedOut => "build timed out",
        EndReason::Canceled => "build canceled",
        EndReason::SpawnFailed => "failed to spawn cargo",
    }
}

/// 把构建直接置失败并落终态。
async fn fail(
    state: &AppState,
    id: hotpot_core::BuildId,
    message: String,
) -> hotpot_core::Result<()> {
    warn!(build = %id.short(), "{message}");
    state
        .scheduler
        .finish_build(id, BuildStatus::Failed, Default::default(), Some(message))
        .await
}

/// 当前 UTC 与某 Unix 毫秒时间戳的差值（毫秒，不为负）。
fn millis_since(ts_ms: i64) -> u64 {
    (hotpot_core::model::now_ms() - ts_ms).max(0) as u64
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
