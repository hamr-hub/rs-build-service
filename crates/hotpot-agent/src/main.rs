//! hotpot-agent CLI：run（supervisor 常驻）/ deploy / rollback / status。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use hotpot_agent::release::Release;
use hotpot_agent::supervisor::Supervisor;

/// 零停机部署 agent。
#[derive(Parser, Debug)]
#[command(name = "hotpot-agent", version)]
struct Cli {
    /// agent 数据目录（控制 socket、状态、版本副本均在其下）。
    #[arg(long, default_value = "./.hotpot-agent", global = true)]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 常驻运行：首次可用 --app 指定初始版本。
    Run(RunArgs),
    /// 部署新版本。
    Deploy(DeployArgs),
    /// 回滚到上一版本。
    Rollback,
    /// 查询当前部署状态。
    Status,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// 监听地址（agent 绑定一次，永久持有）。
    #[arg(long, default_value = "127.0.0.1:9000")]
    listen: String,
    /// 初始二进制（仅数据目录为空时使用）。
    #[arg(long)]
    app: Option<PathBuf>,
    /// 初始版本名。
    #[arg(long, default_value = "base")]
    version: String,
    /// 子进程环境变量，格式 KEY=VAL，可重复。
    #[arg(long = "env", value_parser = parse_kv)]
    envs: Vec<(String, String)>,
}

#[derive(Args, Debug)]
struct DeployArgs {
    /// 新二进制路径（会复制到 versions/ 固化）。
    #[arg(long)]
    app: PathBuf,
    /// 版本名。
    #[arg(long)]
    version: String,
    /// 子进程环境变量，格式 KEY=VAL，可重复。
    #[arg(long = "env", value_parser = parse_kv)]
    envs: Vec<(String, String)>,
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VAL, got {s}"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let socket_path = cli.data_dir.join("agent.sock");

    match cli.command {
        Cmd::Run(args) => run(cli.data_dir, args),
        Cmd::Deploy(args) => {
            let env: std::collections::BTreeMap<_, _> = args.envs.into_iter().collect();
            send(
                &socket_path,
                &serde_json::json!({
                    "op": "deploy",
                    "path": args.app,
                    "version": args.version,
                    "env": env,
                }),
            )
        }
        Cmd::Rollback => send(&socket_path, &serde_json::json!({ "op": "rollback" })),
        Cmd::Status => send(&socket_path, &serde_json::json!({ "op": "status" })),
    }
}

fn run(data_dir: PathBuf, args: RunArgs) -> ExitCode {
    let result = run_impl(data_dir, args);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hotpot-agent: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_impl(data_dir: PathBuf, args: RunArgs) -> anyhow::Result<()> {
    install_agent_signals();
    let mut supervisor = Supervisor::new(&data_dir, &args.listen)?;
    // 控制面先就位，再引导接流量，消除 agent.sock 启动空窗。
    let control_listener = hotpot_agent::control::bind(&data_dir.join("agent.sock"))?;

    // 首次初始化：固化并登记初始版本（不启动），随后由 bootstrap 统一拉起，
    // 绝不先 deploy 再 bootstrap，否则初始版本会被 spawn 两次。
    if supervisor.state().current.is_none() {
        if let Some(app) = args.app {
            let release = Release {
                version: args.version,
                path: app,
                env: args.envs.into_iter().collect(),
            };
            supervisor.install_initial(release)?;
        }
    }
    supervisor.bootstrap()?;
    hotpot_agent::control::serve(&mut supervisor, control_listener)?;
    supervisor.shutdown_child();
    Ok(())
}

/// 发送控制命令并打印应答；exit code 镜像 ok。
fn send(socket_path: &std::path::Path, command: &serde_json::Value) -> ExitCode {
    let result = (|| -> anyhow::Result<bool> {
        let mut conn = std::os::unix::net::UnixStream::connect(socket_path)?;
        conn.write_all(&serde_json::to_vec(command)?)?;
        conn.shutdown(std::net::Shutdown::Write)?;
        let mut bytes = Vec::new();
        conn.read_to_end(&mut bytes)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        println!("{value}");
        Ok(value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
    })();
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("hotpot-agent: {e}");
            ExitCode::FAILURE
        }
    }
}

/// agent 自身的 SIGTERM/SIGINT：控制循环轮询到后优雅退出。
fn install_agent_signals() {
    extern "C" fn handler(_: libc::c_int) {
        hotpot_agent::supervisor::request_agent_shutdown();
    }
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    let handler: extern "C" fn(libc::c_int) = handler;
    sa.sa_sigaction = handler as usize;
    unsafe {
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}
