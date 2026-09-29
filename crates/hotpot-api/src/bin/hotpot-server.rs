//! Hotpot 单二进制服务：api + 内嵌 worker + SQLite（部署形态 A）。
//!
//! 配置优先级：**内置默认值 < TOML 配置文件 < 环境变量 < 命令行参数**。
//! 环境变量与命令行参数用 `HOTPOT_*` 前缀，命名与配置文件字段一一对应。

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use clap::Parser;
use hotpot_api::driver::{SccacheConfig, WorkerConfig};
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
    let store = LocalStore::open(data_dir.join("store"), StoreOptions::default())?;
    let remote_cache = RemoteCache::open(&data_dir, store.clone()).await?;

    // 构建默认走宿主工具链：把它写进 /metrics，用户一眼能看出「这台机器的
    // 构建默认用哪个 rustc」，排查工具链不一致时不必再去翻配置。
    let default_toolchain = hotpot_worker::toolchains::default_rustc()
        .await
        .unwrap_or_else(|_| "unknown".to_string());

    let state = AppState::new(scheduler.clone(), store)
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

    for i in 0..workers {
        let worker_state = state.clone();
        let worker_config = worker_config.clone();
        tokio::spawn(async move {
            hotpot_api::driver::worker_loop(worker_state, format!("worker-{i}"), worker_config)
                .await;
        });
    }

    // 两个路由器各自的状态在 merge 前注入，合并为 Router<()>。
    let token = args.cache_token.map(Arc::<str>::from);
    let app = Router::new()
        .merge(router(state))
        .merge(
            hotpot_cacheproto::router(token)
                .with_state(hotpot_cacheproto::CacheState::new(remote_cache)),
        )
        .layer(TraceLayer::new_for_http());
    let listener = TcpListener::bind(&config.listen).await?;
    tracing::info!("hotpot-server listening on http://{}", config.listen);
    axum::serve(listener, app).await?;
    Ok(())
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
