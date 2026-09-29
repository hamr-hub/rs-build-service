//! 内嵌 worker：认领任务 → 就位源码 → 执行 cargo → 采集产物入 CAS → 回写终态。

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use hotpot_core::model::{BuildRecord, BuildStatus, EventKind, SourceSpec};
use hotpot_core::{ArtifactMeta, ContentDigest};
use hotpot_store::BlobStore;
use hotpot_worker::{BuildPlan, EndReason, ExecutorKind, run_build};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::source::{PhaseSink, prepare_git_workspace};
use crate::state::AppState;

const LEASE: Duration = Duration::from_secs(60);
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(15);

/// 续租任务句柄：离开作用域即中止。
///
/// 续租原本写成「末尾一句 `renew_task.abort()`」，而 `handle_build` 中间有
/// 多个 `?`（`mark_running`、`run_build` 的 join……）。任何一处提前返回都会
/// 把续租任务留在后台：它每 15s 续一次租约，而构建再也不会被推进。
/// 租约机制的本意是「worker 失活后任务能被接管」，这条路径反而让任务
/// 永远停在 `dispatched` 且不可接管——一个瞬时 DB 错误变成永久卡死。
///
/// RAII 让「函数怎么退出，续租都一定停」成为类型层面的保证。
struct LeaseRenewer(tokio::task::JoinHandle<()>);

impl Drop for LeaseRenewer {
    fn drop(&mut self) {
        self.0.abort();
    }
}
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 构建失败时写进 `error` 字段的 stderr 尾部行数。
const ERROR_TAIL_LINES: usize = 12;

/// 按项目的共享工作区锁注册表（每个唯一 URL 泄漏一个 static Mutex，
/// 开销可忽略；换来 'static 守卫，可跨整个构建 await 持有）。
static GIT_LOCKS: OnceLock<Mutex<HashMap<String, &'static Mutex<()>>>> = OnceLock::new();

/// 获取某 git 项目的全构建周期锁：同一项目（含不同 ref）的构建在同一进程
/// 内串行，避免一个构建正在编译时另一个构建强制 checkout 覆盖源码。
/// 不同项目互不阻塞。
async fn git_project_lock(url: &str) -> &'static Mutex<()> {
    let registry =
        GIT_LOCKS.get_or_init(|| Mutex::new(HashMap::<String, &'static Mutex<()>>::new()));
    let key = url.to_string();
    {
        let guard = registry.lock().await;
        if let Some(lock) = guard.get(&key) {
            return *lock;
        }
    }
    let mut guard = registry.lock().await;
    *guard
        .entry(key)
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
}

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
    /// docker 执行器是否自动安装系统包（气隙/预烘焙镜像可关闭）。
    pub provision_system_packages: bool,
}

impl WorkerConfig {
    pub fn new(data_root: PathBuf, executor: ExecutorKind) -> Self {
        Self {
            data_root,
            executor,
            sccache: None,
            timeout: Duration::from_secs(1800),
            allow_git_source: false,
            provision_system_packages: true,
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
    let mut sink = PhaseSink::new(state, rec.id);
    // 共享 git 工作区需要全构建周期持锁（见 git_project_lock）；guard 在
    // 函数结束时自动释放。会话级 clone 无需加锁。
    // 只需要守卫在函数结束时 drop 释放锁，无需读取；下划线前缀保留 Drop。
    let mut _git_guard: Option<tokio::sync::MutexGuard<'static, ()>> = None;
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
            // 复用**稳定路径**的共享工作区：只有工作区路径稳定，cargo 的
            // target fingerprint 才稳定，第二次构建才能命中增量与 sccache
            // （每构建一个会话目录 = 永远冷构建）。工作区根与 sccache 无关，
            // 放在数据目录下由回收器统一清理。
            //
            // 同一项目的构建全周期持锁，避免一个构建正在编译时被另一个的
            // checkout 覆盖源码；不同项目互不阻塞。
            let lock = git_project_lock(url).await;
            _git_guard = Some(lock.lock().await);
            prepare_git_workspace(
                &data_root.join("git-workspaces"),
                url,
                ref_name,
                sha.as_deref(),
                &mut sink,
            )
            .await
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
    plan.provision_system_packages = config.provision_system_packages;
    plan.cancel = Some(state.register_cancel(rec.id).await);
    plan.first_seq = sink.event_count();
    // 项目稳定标识：git 用 URL（跨 tag 同一项目），本地用规范化绝对路径。
    plan.warm_identity = match &rec.source {
        SourceSpec::Git { url, .. } => Some(url.clone()),
        SourceSpec::Local { path } => Some(
            Path::new(path)
                .canonicalize()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string()),
        ),
        SourceSpec::Upload { .. } => None,
    };
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
    // `LeaseRenewer` 在任何返回路径（含 `?` 与 panic 展开）上都会中止续租。
    let _renewer = LeaseRenewer(tokio::spawn(async move {
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
    }));

    let queue_ms = millis_since(rec.created_at_ms);
    state.scheduler.mark_running(rec.id).await?;
    state.publish(crate::state::StreamSignal::Status(rec.id));
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
        } else {
            // 写库成功才广播：否则订阅者会被唤醒去读一个还不存在的 seq。
            state.publish(crate::state::StreamSignal::Log(event.clone()));
        }
    }
    let result = job
        .await
        .map_err(|e| hotpot_core::Error::Other(e.to_string()))?;
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
        match collect_artifacts(&warm_target_root(&rec, config), &rec, state) {
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
    // 终态必须唤醒订阅者，否则它们会一直等着（没有新事件可推送）。
    state.publish(crate::state::StreamSignal::Status(rec.id));
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

/// 本次构建产物所在的 target 根目录：启用 warm target 时是 tools 下的持久化
/// 卷（与 docker 执行器挂载的目录必须一致），否则是会话目录内的 target。
fn warm_target_root(rec: &BuildRecord, config: &WorkerConfig) -> PathBuf {
    // 与 plan.warm_identity 保持一致：本地路径先 canonicalize。
    let identity_local;
    let identity: Option<&str> = match &rec.source {
        SourceSpec::Git { url, .. } => Some(url),
        SourceSpec::Local { path } => {
            identity_local = Path::new(path)
                .canonicalize()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string());
            Some(&identity_local)
        }
        SourceSpec::Upload { .. } => None,
    };
    // 镜像 tag 必须与 docker 执行器**解析后**的镜像一致：profile 指定工具链
    // 时官方镜像会被改写版本段（如 1.98-slim + toolchain 1.85 → 1.85-slim），
    // key 用错版本会把 warm 卷挂错工具链。
    let image_tag: String = match &config.executor {
        ExecutorKind::Docker { image, .. } => {
            let resolved = match &rec.profile.toolchain {
                Some(spec) => hotpot_core::parse_toolchain(spec)
                    .map(|tc| hotpot_core::resolve_rust_image(image, &tc).unwrap_or(image.clone()))
                    .unwrap_or_else(|_| image.clone()),
                None => image.clone(),
            };
            resolved
                .split_once(':')
                .map(|(_, t)| t.to_string())
                .unwrap_or_default()
        }
        ExecutorKind::Local => "local".to_string(),
    };
    if let (Some(identity), true) = (
        identity,
        matches!(config.executor, ExecutorKind::Docker { .. }),
    ) {
        let triple = rec.profile.target.as_deref().unwrap_or("host");
        let mode = match rec.profile.mode {
            hotpot_core::BuildMode::Debug => "debug",
            hotpot_core::BuildMode::Release => "release",
        };
        if let Some(sccache) = &config.sccache {
            let key = hotpot_worker::warmcache::key(identity, &image_tag, triple, mode);
            return hotpot_worker::warmcache::dir(&sccache.tools_dir, &key);
        }
    }
    let data_root = config.data_root.as_path();
    data_root
        .join("sessions")
        .join(rec.id.to_string())
        .join("target")
}

/// 收集 profile 目录顶层的可执行/库文件，逐一内容寻址入 CAS。
fn collect_artifacts(
    target_root: &Path,
    rec: &BuildRecord,
    state: &AppState,
) -> hotpot_core::Result<Vec<ArtifactMeta>> {
    let mut profile_dir = target_root.to_path_buf();
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
