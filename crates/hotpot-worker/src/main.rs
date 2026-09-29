//! hotpot-worker 命令行：对本地 Cargo 项目执行一次隔离构建，
//! 把带序号的构建事件打到 stdout，结尾输出阶段耗时。

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use hotpot_core::model::EventKind;
use hotpot_worker::{BuildPlan, ExecutorKind, run_build};

/// 在隔离会话目录中构建一个本地 Cargo 项目。
#[derive(Parser, Debug)]
#[command(name = "hotpot-worker", version)]
struct Args {
    /// Cargo 项目根目录（含 Cargo.toml）。
    #[arg(long, short = 'p')]
    project: PathBuf,
    /// 会话目录（CARGO_TARGET_DIR 位于其下）；默认系统临时目录。
    #[arg(long, short = 's')]
    session: Option<PathBuf>,
    /// release 构建。
    #[arg(long)]
    release: bool,
    /// 启用 sccache（可执行文件须在 PATH）。
    #[arg(long)]
    sccache: bool,
    /// sccache 缓存目录（--sccache 时生效，默认会话目录/sccache）。
    #[arg(long)]
    sccache_dir: Option<PathBuf>,
    /// 构建超时秒数。
    #[arg(long, default_value_t = 1800)]
    timeout: u64,
    /// 执行后端：local 或 docker（或经 HOTPOT_EXECUTOR 环境变量）。
    #[arg(long)]
    executor: Option<String>,
    /// docker 执行器工具链镜像。
    #[arg(long)]
    docker_image: Option<String>,
    /// docker daemon 地址（如 unix:///… 或 tcp://…）。
    #[arg(long)]
    docker_host: Option<String>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();

    let session = match args.session {
        Some(dir) => dir,
        None => std::env::temp_dir().join(format!("hotpot-session-{}", std::process::id())),
    };
    std::fs::create_dir_all(&session).expect("create session dir");

    let mut plan = BuildPlan::local(&args.project, &session);
    plan.timeout = Duration::from_secs(args.timeout);
    if args.release {
        plan = plan.release();
    }
    if args.sccache {
        plan.sccache_dir = Some(args.sccache_dir.unwrap_or_else(|| session.join("sccache")));
    }

    println!(
        "== build {} project={}",
        plan.build_id,
        plan.project_dir.display()
    );
    let executor = executor_kind(args.executor, args.docker_image, args.docker_host);
    let (mut events, handle) = run_build(plan.clone(), &executor).await;

    while let Some(event) = events.recv().await {
        match event.kind {
            EventKind::Stdout => println!("[{:>4}] {}", event.seq, event.payload),
            EventKind::Stderr => eprintln!("[{:>4}] ! {}", event.seq, event.payload),
            EventKind::Phase => println!("---- phase: {}", event.payload),
            EventKind::Status => println!("---- status: {}", event.payload),
        }
    }

    let result = handle.await.expect("worker task panicked");
    println!(
        "== done success={} exit_code={:?} build_ms={} total_ms={}",
        result.success, result.exit_code, result.timings.build_ms, result.timings.total_ms
    );

    if result.success {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

/// 解析执行后端：CLI 参数优先，其次 HOTPOT_EXECUTOR 环境变量，默认本地。
fn executor_kind(
    executor: Option<String>,
    image: Option<String>,
    docker_host: Option<String>,
) -> ExecutorKind {
    let pick = executor.or_else(|| std::env::var("HOTPOT_EXECUTOR").ok());
    match pick.as_deref() {
        Some("docker") => ExecutorKind::Docker {
            image: image
                .or_else(|| std::env::var("HOTPOT_DOCKER_IMAGE").ok())
                .unwrap_or_else(|| "rust:slim-bookworm".to_string()),
            docker_host: docker_host.or_else(|| std::env::var("HOTPOT_DOCKER_HOST").ok()),
        },
        _ => ExecutorKind::Local,
    }
}
