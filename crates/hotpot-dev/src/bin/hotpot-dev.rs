//! `hotpot-dev` 命令行：本地监听 — 增量构建 — 热重启。

use std::collections::BTreeMap;

use clap::Parser;
use hotpot_dev::builder::BuildArgs;
use hotpot_dev::dev::{self, DevOptions};

/// 监听一个 Rust 项目，源码变更后增量重建并优雅重启；构建失败时保留旧进程。
#[derive(Parser, Debug)]
#[command(name = "hotpot-dev", version)]
struct Cli {
    /// 项目目录（包含 Cargo.toml）。
    #[arg(default_value = ".")]
    project: String,

    /// release 模式构建。
    #[arg(long)]
    release: bool,

    /// 启用的 cargo feature（可重复或逗号分隔）。
    #[arg(long, value_delimiter = ',')]
    features: Vec<String>,

    /// 公共监听地址（如 127.0.0.1:8080）；启用 socket keeper，重启窗口不拒连。
    /// 子进程通过 HOTPOT_BIND_ADDR 获知后端绑定地址。
    #[arg(long)]
    addr: Option<String>,

    /// 注入子进程的环境变量，格式 KEY=VALUE（可重复）。
    #[arg(long = "env", value_name = "KEY=VALUE")]
    envs: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    let mut extra_env: BTreeMap<String, String> = BTreeMap::new();
    for item in cli.envs {
        let Some((key, value)) = item.split_once('=') else {
            anyhow::bail!("--env 需要 KEY=VALUE 格式，得到 {item:?}");
        };
        extra_env.insert(key.to_string(), value.to_string());
    }

    let options = DevOptions {
        project: std::path::PathBuf::from(&cli.project),
        build: BuildArgs {
            release: cli.release,
            features: cli.features,
        },
        extra_env,
        public_addr: cli.addr,
    };

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    dev::run(options, shutdown).await
}
