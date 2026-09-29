//! Hotpot 单二进制服务：api + 内嵌 worker + SQLite（部署形态 A）。

use std::path::PathBuf;

use axum::Router;
use clap::Parser;
use hotpot_api::{AppState, router};
use hotpot_cacheproto::{CacheState, RemoteCache};
use hotpot_scheduler::Scheduler;
use hotpot_store::local::{LocalStore, StoreOptions};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

/// Hotpot 构建服务。
#[derive(Parser, Debug)]
#[command(name = "hotpot-server", version)]
struct Args {
    /// 监听地址。
    #[arg(long, default_value = "127.0.0.1:7878")]
    listen: String,
    /// 数据目录（SQLite、CAS、会话目录均在其下）。
    #[arg(long, default_value = "./hotpot-data")]
    data_dir: PathBuf,
    /// 内嵌 worker 数；0 = CPU 并行度（上限 4）。
    #[arg(long, default_value_t = 0)]
    workers: usize,
    /// 构建执行后端：local（默认）或 docker。
    #[arg(long)]
    executor: Option<String>,
    /// docker 后端工具链镜像（默认 rust:slim-bookworm）。
    #[arg(long)]
    docker_image: Option<String>,
    /// docker daemon 地址（unix://… / tcp://…）；默认自动探测。
    #[arg(long)]
    docker_host: Option<String>,
}

/// 解析执行后端：CLI 参数 → HOTPOT_EXECUTOR 环境变量 → 本地。
fn resolve_executor(args: &Args) -> hotpot_worker::ExecutorKind {
    let pick = args
        .executor
        .clone()
        .or_else(|| std::env::var("HOTPOT_EXECUTOR").ok());
    match pick.as_deref() {
        Some("docker") => hotpot_worker::ExecutorKind::Docker {
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
        _ => hotpot_worker::ExecutorKind::Local,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    std::fs::create_dir_all(&args.data_dir)?;

    let scheduler = Scheduler::open(&args.data_dir).await?;
    let store = LocalStore::open(args.data_dir.join("store"), StoreOptions::default())?;
    let state = AppState::new(scheduler, store.clone());
    let remote_cache = RemoteCache::open(&args.data_dir, store).await?;

    let workers = match args.workers {
        0 => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(4),
        n => n,
    };
    let executor = resolve_executor(&args);
    tracing::info!("spawning {workers} embedded worker(s), executor={executor:?}");
    for i in 0..workers {
        let worker_state = state.clone();
        let data_root = args.data_dir.clone();
        let executor = executor.clone();
        tokio::spawn(async move {
            hotpot_api::driver::worker_loop(
                worker_state,
                format!("worker-{i}"),
                data_root,
                executor,
            )
            .await;
        });
    }

    // 两个路由器各自的状态在 merge 前注入，合并为 Router<()>。
    let app = Router::new()
        .merge(router(state))
        .merge(hotpot_cacheproto::router().with_state(CacheState::new(remote_cache)))
        .layer(TraceLayer::new_for_http());
    let listener = TcpListener::bind(&args.listen).await?;
    tracing::info!("hotpot-server listening on http://{}", args.listen);
    axum::serve(listener, app).await?;
    Ok(())
}
