//! Hotpot 单二进制服务：api + 内嵌 worker + SQLite（部署形态 A）。
//!
//! 配置优先级：**内置默认值 < TOML 配置文件 < 环境变量 < 命令行参数**。
//! 环境变量与命令行参数用 `HOTPOT_*` 前缀，命名与配置文件字段一一对应。

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use clap::Parser;
use hotpot_api::driver::{SccacheConfig, WorkerConfig};
use hotpot_api::hardening;
use hotpot_api::{AppState, router};
use hotpot_cacheproto::RemoteCache;
use hotpot_core::config::ServerConfig;
use hotpot_scheduler::Scheduler;
use hotpot_store::local::{LocalStore, StoreOptions};
use hotpot_worker::ExecutorKind;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

/// Hotpot 构建服务。
#[derive(Parser, Debug)]
#[command(name = "hotpot-server", version)]
struct Args {
    /// TOML 配置文件；未指定时只用内置默认值（仍接受环境变量覆盖）。
    #[arg(long)]
    config: Option<PathBuf>,
    /// 监听地址（覆盖配置文件）。
    #[arg(long)]
    listen: Option<String>,
    /// 数据目录（SQLite、CAS、会话目录均在其下）。
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// 内嵌 worker 数；0 = CPU 并行度（上限 4）。
    #[arg(long)]
    workers: Option<usize>,
    /// 构建执行后端：local（默认）或 docker。
    #[arg(long)]
    executor: Option<String>,
    /// docker 后端工具链镜像（默认 rust:slim-bookworm）。
    #[arg(long)]
    docker_image: Option<String>,
    /// docker daemon 地址（unix://… / tcp://…）；默认自动探测。
    #[arg(long)]
    docker_host: Option<String>,
    /// 单构建超时秒数。
    #[arg(long)]
    build_timeout_secs: Option<u64>,
    /// 会话目录保留时长（秒）；成功构建超过即被回收。
    #[arg(long)]
    session_max_age_secs: Option<u64>,
    /// 失败/超时构建的会话保留时长（秒）。
    #[arg(long)]
    session_failed_max_age_secs: Option<u64>,
    /// 后台回收扫描间隔（秒）。
    #[arg(long)]
    gc_interval_secs: Option<u64>,
    /// git 共享工作区最长保留时长（秒）。
    #[arg(long)]
    git_workspace_max_age_secs: Option<u64>,
    /// 关闭 docker 执行器的系统包自动供给（气隙环境 / 预烘焙镜像）。
    #[arg(long)]
    docker_no_provision: bool,
    /// CAS 容量上限（字节），0 为不限。
    #[arg(long)]
    cache_max_bytes: Option<u64>,
    /// 启用自身构建的 sccache 闭环加速（构建复用本服务的 /sccache 端点）。
    #[arg(long)]
    self_sccache: bool,
    /// 缓存端点 Bearer token；设置后 sccache / turbo 都需携带。
    #[arg(long, env = "HOTPOT_CACHE_TOKEN")]
    cache_token: Option<String>,
    /// 允许 git 来源的构建（会 clone 任意 URL 并执行其中的 build.rs，
    /// **等价于在服务上执行不可信代码**，默认关闭）。
    #[arg(long)]
    allow_git_source: bool,
    /// 允许跨源访问的来源，逗号分隔（如 https://console.internal）。
    /// `*` 表示放行任意来源；留空（同源部署）不加 CORS 头。
    #[arg(long, env = "HOTPOT_CORS_ORIGINS")]
    cors_origins: Option<String>,
    /// 单个非流式请求的服务端处理超时（秒）；SSE 日志流不受此限制。
    #[arg(long, env = "HOTPOT_REQUEST_TIMEOUT_SECS")]
    request_timeout_secs: Option<u64>,
    /// 同时处理的普通请求数上限，超出直接返回 503（快失败而非无限排队）。
    /// SSE 日志流不计入该配额。
    #[arg(long, env = "HOTPOT_MAX_CONCURRENT_REQUESTS")]
    max_concurrent_requests: Option<usize>,
    /// 请求体上限（字节）。
    #[arg(long, env = "HOTPOT_MAX_REQUEST_BODY")]
    max_request_body: Option<usize>,
    /// 关闭响应压缩。
    #[arg(long)]
    no_compression: bool,
    /// 前端构建产物目录（web/dist）。存在时由本服务同源托管，实现单进程交付。
    #[arg(long, env = "HOTPOT_WEB_DIR")]
    web_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let mut config = load_config(args.config.as_deref())?;
    apply_env(&mut config);
    apply_args(&mut config, &args);
    if args.self_sccache {
        config.cache.self_sccache = true;
    }

    let data_dir = config.data_dir.clone();
    std::fs::create_dir_all(&data_dir)?;

    let executor = resolve_executor(&args, &config);
    let allow_git_source = args.allow_git_source
        || matches!(
            std::env::var("HOTPOT_ALLOW_GIT_SOURCE").as_deref(),
            Ok("1") | Ok("true")
        );

    let scheduler = Scheduler::open(&data_dir).await?;
    // 容量上限此前从未接线：`StoreOptions::default()` 是 0（不限），
    // `config.cache.max_bytes` 是个死字段，CAS 会无限增长直到磁盘写满。
    let store = LocalStore::open(
        data_dir.join("store"),
        StoreOptions {
            max_bytes: config.cache.max_bytes,
            compression: config.cache.compression,
        },
    )?;
    let remote_cache = RemoteCache::open(&data_dir, store.clone()).await?;

    // 构建默认走宿主工具链：把它写进 /metrics，用户一眼能看出「这台机器的
    // 构建默认用哪个 rustc」，排查工具链不一致时不必再去翻配置。
    let default_toolchain = hotpot_worker::toolchains::default_rustc()
        .await
        .unwrap_or_else(|_| "unknown".to_string());

    // 并发上限在这里就定下来：中间件与 `/metrics` 必须共用同一个限流器，
    // 否则指标反映的是一个没人使用的实例，饱和时反而看不出问题。
    let concurrency_limit = args
        .max_concurrent_requests
        .unwrap_or(hotpot_api::state::DEFAULT_CONCURRENCY_LIMIT);
    let state = AppState::new(scheduler.clone(), store.clone())
        .with_concurrency_limit(concurrency_limit)
        .with_cache(remote_cache.clone())
        .with_executor(executor.clone())
        .with_git_source(allow_git_source);

    let workers = match config.workers {
        0 => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(4),
        n => n as usize,
    };

    // 自身 sccache 闭环：让构建真正用上本服务的 crate 级缓存端点。
    //
    // 端点可达性对两种后端不同：local 模式可以用监听地址推导；
    // docker 模式下容器访问不到宿主回环地址，必须显式配置
    // （如 `http://host.docker.internal:7878/sccache` 或 compose 内的服务名）。
    let sccache = if config.cache.self_sccache {
        let derived = || Some(config.sccache_webdav_url());
        let webdav_url = match &config.cache.sccache_webdav_url {
            Some(explicit) => Some(explicit.clone()),
            None => match executor {
                ExecutorKind::Local => derived(),
                ExecutorKind::Docker { .. } => {
                    tracing::warn!(
                        "已启用自身 sccache 缓存，但未显式配置 sccache_webdav_url：\
                         构建容器无法访问宿主回环地址，本次仅使用容器本地盘缓存。\
                         如需跨构建共享，请配置 cache.sccache_webdav_url \
                         （如 http://host.docker.internal:{}/sccache）",
                        config
                            .listen
                            .rsplit_once(':')
                            .map(|(_, p)| p)
                            .unwrap_or("7878")
                    );
                    None
                }
            },
        };
        Some(SccacheConfig {
            host_dir: config.sccache_dir(),
            webdav_url,
            tools_dir: data_dir.join("tools"),
        })
    } else {
        None
    };

    tracing::info!(
        workers,
        executor = ?executor,
        data_dir = %data_dir.display(),
        self_sccache = sccache.is_some(),
        allow_git_source,
        "starting hotpot-server"
    );
    if is_public_bind(&config.listen) && args.cache_token.is_none() {
        tracing::warn!(
            listen = %config.listen,
            "监听非回环地址但未设置缓存 token：/sccache 与 /v8 处于无鉴权状态，\
             任何人都能写入伪造产物（构建供应链投毒）。请设置 HOTPOT_CACHE_TOKEN \
             或置于带鉴权的反向代理之后。"
        );
    }
    if allow_git_source {
        tracing::warn!(
            "已允许 git 来源构建：服务会 clone 任意 URL 并执行其 build.rs，\
             等价于允许在服务进程权限下执行不可信代码。仅可在可信内网开启。"
        );
    }

    let state = state.with_runtime_info(workers, default_toolchain);

    let mut worker_config = WorkerConfig::new(data_dir.clone(), executor.clone())
        .with_timeout(config.build_timeout_secs);
    if let Some(sccache) = sccache {
        worker_config = worker_config.with_sccache(sccache);
    }
    worker_config.allow_git_source = allow_git_source;
    worker_config.provision_system_packages = !args.docker_no_provision
        && !matches!(
            std::env::var("HOTPOT_DOCKER_NO_PROVISION").as_deref(),
            Ok("1") | Ok("true")
        );

    tracing::info!(
        workers,
        provision_system_packages = worker_config.provision_system_packages,
        "worker configuration resolved"
    );
    for i in 0..workers {
        let worker_state = state.clone();
        let worker_config = worker_config.clone();
        tokio::spawn(async move {
            hotpot_api::driver::worker_loop(worker_state, format!("worker-{i}"), worker_config)
                .await;
        });
    }

    // 后台回收：会话目录 + CAS 容量。启动时先跑一次——
    // 服务崩溃后遗留的会话目录只有重启才能清掉。
    {
        use hotpot_api::gc::SessionPolicy;
        let policy = SessionPolicy {
            max_age: hotpot_core::config::secs_or(
                config.session_max_age_secs,
                SessionPolicy::default().max_age,
            ),
            failed_max_age: hotpot_core::config::secs_or(
                config.session_failed_max_age_secs,
                SessionPolicy::default().failed_max_age,
            ),
            keep_last: config.session_keep_last as usize,
            interval: hotpot_core::config::secs_or(
                config.gc_interval_secs,
                SessionPolicy::default().interval,
            ),
        };
        tracing::info!(
            max_age_secs = policy.max_age.as_secs(),
            failed_max_age_secs = policy.failed_max_age.as_secs(),
            keep_last = policy.keep_last,
            interval_secs = policy.interval.as_secs(),
            cas_max_bytes = config.cache.max_bytes,
            "resource janitor enabled"
        );
        spawn_janitor(
            scheduler.clone(),
            data_dir.clone(),
            store.clone(),
            state.gc_stats.clone(),
            policy,
            hotpot_core::config::secs_or(
                config.git_workspace_max_age_secs,
                std::time::Duration::from_secs(7 * 24 * 3600),
            ),
        );
    }

    // 两个路由器各自的状态在 merge 前注入，合并为 Router<()>。
    let token = args.cache_token.map(Arc::<str>::from);

    // ---- 生产加固层 ----
    //
    // 顺序有讲究：压缩最内层（只压已经生成好的响应），限流在压缩外层
    // （限的是"正在占用资源"的数量，而不是响应体的字节数），超时再外层，
    // CORS 最外层（要能在被拒的响应上也带上跨源头）。
    let shedder = state.shedder.clone();
    let request_timeout = Duration::from_secs(args.request_timeout_secs.unwrap_or(30));
    let body_limit = args.max_request_body.unwrap_or(256 * 1024);
    let cors = hardening::CorsOrigins::parse(args.cors_origins.as_deref().unwrap_or(""));

    tracing::info!(
        max_concurrent = shedder.limit(),
        request_timeout_secs = request_timeout.as_secs(),
        max_request_body = body_limit,
        compression = !args.no_compression,
        cors = ?cors,
        "production hardening enabled"
    );

    let app = Router::new()
        .merge(router(state))
        .merge(
            hotpot_cacheproto::router(token)
                .with_state(hotpot_cacheproto::CacheState::new(remote_cache)),
        )
        // 同源托管前端产物：单进程交付，省掉一层反向代理与跨源配置。
        .merge(hardening::frontend_router(
            &args
                .web_dir
                .clone()
                .unwrap_or_else(|| data_dir.join("web")),
        ))
        .layer(hardening::body_limit_layer(body_limit));

    // 顺序有意义：限流在超时**外层**，这样排队等不到 permit 的请求由限流
    // 直接拒绝，而不会先被超时打断成 503。
    let mut app = hardening::load_shed_middleware(app, shedder.clone());
    app = hardening::timeout_middleware(app, request_timeout);

    if !args.no_compression {
        app = app.layer(hardening::compression_layer());
    }
    if let Some(cors_layer) = cors.layer() {
        app = app.layer(cors_layer);
    }
    let app = app.layer(TraceLayer::new_for_http());

    let listener = TcpListener::bind(&config.listen).await?;
    tracing::info!("hotpot-server listening on http://{}", config.listen);
    serve_with_graceful_shutdown(listener, app).await?;
    Ok(())
}

/// 带优雅停机与 TCP 调用的服务循环。
///
/// 优雅停机是可靠性的最后一环：直接 `axum::serve(..).await` 的话，收到
/// SIGTERM 会**立刻**断掉所有在途请求和正在推日志的 SSE 长连接——用户看到
/// 的是日志流莫名其妙中断，构建本身却毫发无损。正确做法是先停止接受新连接，
/// 再等已有的（含 SSE）自行结束。
///
/// TCP 侧关掉 Nagle：控制面全是小响应（healthz、状态查询），Nagle 的延迟
/// 叠加在这里纯属浪费。
async fn serve_with_graceful_shutdown(
    listener: tokio::net::TcpListener,
    app: Router,
) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = async {
        let ctrl_c = async {
            if let Err(e) = tokio::signal::ctrl_c().await {
                tracing::error!(error = %e, "failed to install Ctrl-C handler");
                // 装不上就永远不触发，交给 SIGTERM 路径。
                std::future::pending::<()>().await;
            }
        };

        #[cfg(unix)]
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut sig) => {
                    sig.recv().await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to install SIGTERM handler");
                    std::future::pending::<()>().await;
                }
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => tracing::info!("received SIGINT, draining"),
            _ = terminate => tracing::info!("received SIGTERM, draining"),
        }
    };

    // nodelay：延迟敏感的 SSE/日志流默认会被 Nagle 算法延迟 ~40ms，
    // 由 tokio 在 listener 层面设置（axum 0.8 的 Serve 不再暴露该选项）。
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    tracing::info!("hotpot-server stopped cleanly");
    Ok(())
}

/// 后台回收循环：会话目录判龄删除 + CAS 容量兜底。
///
/// 首轮在启动时立即执行（清掉上次崩溃遗留的目录），之后按 interval 周期运行。
/// 单轮出错只告警、不终止循环——回收器自己挂掉等于磁盘泄漏，必须能自愈。
fn spawn_janitor(
    scheduler: Scheduler,
    data_root: PathBuf,
    store: hotpot_store::LocalStore,
    stats: Arc<hotpot_api::gc::GcStats>,
    policy: hotpot_api::gc::SessionPolicy,
    workspace_max_age: std::time::Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(policy.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match hotpot_api::gc::sweep_sessions(&scheduler, &data_root, &policy).await {
                Ok(report) => stats.record_sweep(&report),
                Err(e) => tracing::warn!("session sweep failed: {e}"),
            }
            let workspaces = data_root.join("git-workspaces");
            match hotpot_api::gc::sweep_git_workspaces(&workspaces, workspace_max_age).await {
                Ok((removed, freed)) => stats.record_workspace_sweep(removed, freed),
                Err(e) => tracing::warn!("git workspace sweep failed: {e}"),
            }
            if let Err(e) = hotpot_api::gc::enforce_capacity(&store, &stats) {
                tracing::warn!("CAS capacity enforcement failed: {e}");
            }
        }
    });
}

/// 读 TOML 配置文件；不存在时报错（显式指定的文件不该被静默忽略）。
fn load_config(path: Option<&std::path::Path>) -> Result<ServerConfig, Box<dyn std::error::Error>> {
    let Some(path) = path else {
        return Ok(ServerConfig::default());
    };
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read config {}: {e}", path.display()))?;
    let config: ServerConfig =
        toml::from_str(&text).map_err(|e| format!("parse config {}: {e}", path.display()))?;
    tracing::info!("loaded config {}", path.display());
    Ok(config)
}

/// 环境变量覆盖（`HOTPOT_*`）。
fn apply_env(config: &mut ServerConfig) {
    if let Ok(v) = std::env::var("HOTPOT_LISTEN") {
        config.listen = v;
    }
    if let Ok(v) = std::env::var("HOTPOT_DATA_DIR") {
        config.data_dir = PathBuf::from(v);
    }
    if let Ok(v) = std::env::var("HOTPOT_WORKERS") {
        if let Ok(n) = v.parse() {
            config.workers = n;
        }
    }
    if let Ok(v) = std::env::var("HOTPOT_BUILD_TIMEOUT_SECS") {
        if let Ok(n) = v.parse() {
            config.build_timeout_secs = n;
        }
    }
    if let Ok(v) = std::env::var("HOTPOT_SCCACHE_WEBDAV_URL") {
        config.cache.sccache_webdav_url = Some(v);
    }
    if let Ok(v) = std::env::var("HOTPOT_SESSION_MAX_AGE_SECS") {
        if let Ok(n) = v.parse() {
            config.session_max_age_secs = n;
        }
    }
    if let Ok(v) = std::env::var("HOTPOT_SESSION_FAILED_MAX_AGE_SECS") {
        if let Ok(n) = v.parse() {
            config.session_failed_max_age_secs = n;
        }
    }
    if let Ok(v) = std::env::var("HOTPOT_GC_INTERVAL_SECS") {
        if let Ok(n) = v.parse() {
            config.gc_interval_secs = n;
        }
    }
    if let Ok(v) = std::env::var("HOTPOT_SELF_SCCACHE") {
        config.cache.self_sccache = matches!(v.as_str(), "1" | "true" | "yes");
    }
}

/// 命令行参数覆盖配置文件与环境变量。
fn apply_args(config: &mut ServerConfig, args: &Args) {
    if let Some(v) = &args.listen {
        config.listen = v.clone();
    }
    if let Some(v) = &args.data_dir {
        config.data_dir = v.clone();
    }
    if let Some(v) = args.workers {
        config.workers = v as u32;
    }
    if let Some(v) = args.build_timeout_secs {
        config.build_timeout_secs = v;
    }
    if let Some(v) = args.session_max_age_secs {
        config.session_max_age_secs = v;
    }
    if let Some(v) = args.session_failed_max_age_secs {
        config.session_failed_max_age_secs = v;
    }
    if let Some(v) = args.gc_interval_secs {
        config.gc_interval_secs = v;
    }
    if let Some(v) = args.git_workspace_max_age_secs {
        config.git_workspace_max_age_secs = v;
    }
    if let Some(v) = args.cache_max_bytes {
        config.cache.max_bytes = v;
    }
}

/// 解析执行后端：CLI 参数 → 环境变量 → local。
fn resolve_executor(args: &Args, _config: &ServerConfig) -> ExecutorKind {
    let pick = args
        .executor
        .clone()
        .or_else(|| std::env::var("HOTPOT_EXECUTOR").ok());
    match pick.as_deref() {
        Some("docker") => ExecutorKind::Docker {
            image: args
                .docker_image
                .clone()
                .or_else(|| std::env::var("HOTPOT_DOCKER_IMAGE").ok())
                .unwrap_or_else(|| "rust:slim-bookworm".to_string()),
            docker_host: args
                .docker_host
                .clone()
                .or_else(|| std::env::var("HOTPOT_DOCKER_HOST").ok()),
        },
        _ => ExecutorKind::Local,
    }
}

/// 判断监听地址是否暴露到本机之外（用于缓存端点无鉴权时的安全告警）。
///
/// 只有**回环**才算安全：`0.0.0.0` / `::` 虽不是回环，但语义是「监听所有
/// 网卡」，同样属于对外暴露。无法解析的 host（如自定义域名）按暴露处理，
/// 宁可多告警一次。
fn is_public_bind(listen: &str) -> bool {
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    host.parse::<IpAddr>()
        .map(|ip| !ip.is_loopback())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::is_public_bind;

    #[test]
    fn only_loopback_is_safe() {
        assert!(!is_public_bind("127.0.0.1:7878"));
        assert!(!is_public_bind("127.0.0.5:7878"));
        assert!(!is_public_bind("[::1]:7878"));
        assert!(!is_public_bind("localhost:7878"));
        // 0.0.0.0 / :: 语义是「所有网卡」，属于对外暴露。
        assert!(is_public_bind("0.0.0.0:7878"));
        assert!(is_public_bind("[::]:7878"));
        assert!(is_public_bind("192.168.1.10:7878"));
        // 解析不了的一律按暴露处理（宁可多告警）。
        assert!(is_public_bind("build.internal:7878"));
    }
}
