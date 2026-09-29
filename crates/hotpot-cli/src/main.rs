//! hotpot：Rust 构建服务命令行客户端。

mod sse;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use hotpot_core::model::{BuildRecord, BuildStatus};
use reqwest::Client;
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(name = "hotpot", version, about = "Hotpot Rust build service client")]
struct Cli {
    /// 服务地址（也可用环境变量 HOTPOT_SERVER）。
    #[arg(long, global = true, env = "HOTPOT_SERVER")]
    server: Option<String>,

    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand, Debug)]
enum CommandKind {
    /// 提交构建并（默认）实时跟踪日志。
    Build {
        /// Cargo 项目根目录（省略时用 --git-url 提交远端仓库构建）。
        #[arg(long, short = 'p')]
        project: Option<PathBuf>,
        /// release 构建。
        #[arg(long)]
        release: bool,
        /// 启用的 features（逗号分隔）。
        #[arg(long)]
        features: Option<String>,
        /// 关闭默认 features。
        #[arg(long)]
        no_default_features: bool,
        /// 目标三元组（如 aarch64-unknown-linux-gnu）。
        #[arg(long)]
        target: Option<String>,
        /// Rust 工具链：stable / beta / nightly / nightly-2026-01-15 / 1.98 / 1.98.0。
        #[arg(long)]
        toolchain: Option<String>,
        /// 追加的原生 cargo 参数（可重复，如 --cargo-flag "--offline"）。
        #[arg(long = "cargo-flag")]
        cargo_flags: Vec<String>,
        /// git 仓库地址（需要服务端 --allow-git-source）。
        #[arg(long)]
        git_url: Option<String>,
        /// git 引用（分支/标签，默认 HEAD）。
        #[arg(long, default_value = "HEAD")]
        git_ref: String,
        /// 锁定到具体 commit sha。
        #[arg(long)]
        git_sha: Option<String>,
        /// 只提交不跟踪日志。
        #[arg(long)]
        no_follow: bool,
    },
    /// 列出构建（按创建时间倒序）。
    List {
        /// 按状态过滤：queued/dispatched/running/succeeded/failed/canceled/timeout。
        #[arg(long)]
        status: Option<String>,
        /// 返回条数（1..=200）。
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// 偏移。
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// 列出服务端可用的 Rust 工具链与 docker 工具链镜像。
    Toolchains,
    /// 附加到构建日志（SSE）。
    Logs {
        /// 构建 ID。
        id: String,
        /// 从该 seq 开始（含）。
        #[arg(long)]
        since: Option<u64>,
    },
    /// 取消构建。
    Cancel { id: String },
    /// 查询构建状态。
    Status { id: String },
    /// 列出构建产物。
    Artifacts { id: String },
    /// 下载构建的全部产物。
    Download {
        id: String,
        /// 输出目录，默认 ./hotpot-out。
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
    },
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

struct Api {
    base: String,
    http: Client,
}

impl Api {
    fn new(base: String) -> Self {
        Self {
            base,
            http: Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn error_for_status(resp: reqwest::Response) -> Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let msg = resp
            .json::<ErrorBody>()
            .await
            .map(|b| b.error)
            .unwrap_or_else(|_| status.to_string());
        bail!("{status}: {msg}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let base = cli
        .server
        .unwrap_or_else(|| "http://127.0.0.1:7878".to_string());
    let api = Api::new(base);

    match cli.command {
        CommandKind::Build {
            project,
            release,
            features,
            no_default_features,
            target,
            toolchain,
            cargo_flags,
            git_url,
            git_ref,
            git_sha,
            no_follow,
        } => {
            cmd_build(
                &api,
                BuildArgs {
                    project,
                    release,
                    features,
                    no_default_features,
                    target,
                    toolchain,
                    cargo_flags,
                    git_url,
                    git_ref,
                    git_sha,
                    no_follow,
                },
            )
            .await?;
        }
        CommandKind::List {
            status,
            limit,
            offset,
        } => {
            cmd_list(&api, status.as_deref(), limit, offset).await?;
        }
        CommandKind::Toolchains => {
            cmd_toolchains(&api).await?;
        }
        CommandKind::Logs { id, since } => {
            let terminal = follow_logs(&api, &id, since).await?;
            if terminal == BuildStatus::Failed {
                bail!("build failed");
            }
        }
        CommandKind::Cancel { id } => {
            let resp = api
                .http
                .post(api.url(&format!("/v1/builds/{id}/cancel")))
                .send()
                .await?;
            let rec: BuildRecord = Api::error_for_status(resp).await?.json().await?;
            println!("{} -> {}", rec.id, status_str(rec.status));
        }
        CommandKind::Status { id } => {
            let resp = api
                .http
                .get(api.url(&format!("/v1/builds/{id}")))
                .send()
                .await?;
            let rec: BuildRecord = Api::error_for_status(resp).await?.json().await?;
            print_record(&rec);
        }
        CommandKind::Artifacts { id } => {
            let resp = api
                .http
                .get(api.url(&format!("/v1/builds/{id}/artifacts")))
                .send()
                .await?;
            let artifacts: Vec<hotpot_core::ArtifactMeta> =
                Api::error_for_status(resp).await?.json().await?;
            for a in artifacts {
                println!("{:>12}  {}  {}", a.size, a.digest, a.name);
            }
        }
        CommandKind::Download { id, out } => {
            cmd_download(
                &api,
                &id,
                out.unwrap_or_else(|| PathBuf::from("hotpot-out")),
            )
            .await?;
        }
    }
    Ok(())
}

/// `build` 子命令参数。
struct BuildArgs {
    project: Option<PathBuf>,
    release: bool,
    features: Option<String>,
    no_default_features: bool,
    target: Option<String>,
    toolchain: Option<String>,
    cargo_flags: Vec<String>,
    git_url: Option<String>,
    git_ref: String,
    git_sha: Option<String>,
    no_follow: bool,
}

async fn cmd_build(api: &Api, args: BuildArgs) -> Result<()> {
    // source 与 profile 在本地先组装好；服务端会在系统边界再校验一次
    // （工具链写法、target 形状、列表规模），这里只是让错误更早出现在 CLI。
    let source = match (&args.project, &args.git_url) {
        (Some(project), None) => serde_json::json!({
            "kind": "local",
            "path": project.canonicalize().context("project path")?.display().to_string(),
        }),
        (None, Some(url)) => serde_json::json!({
            "kind": "git",
            "url": url,
            "ref_name": args.git_ref,
            "sha": args.git_sha,
        }),
        (Some(_), Some(_)) => bail!("--project 与 --git-url 只能二选一"),
        (None, None) => bail!("需要 --project <dir> 或 --git-url <url>"),
    };
    let profile = serde_json::json!({
        "mode": if args.release { "release" } else { "debug" },
        "features": args.features
            .map(|f| f.split(',').map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default(),
        "no_default_features": args.no_default_features,
        "target": args.target,
        "toolchain": args.toolchain,
        "cargo_flags": args.cargo_flags,
    });
    let body = serde_json::json!({ "source": source, "profile": profile });

    let resp = api
        .http
        .post(api.url("/v1/builds"))
        .json(&body)
        .send()
        .await?;
    let rec: BuildRecord = Api::error_for_status(resp).await?.json().await?;
    println!("submitted build {} ({})", rec.id, status_str(rec.status));

    if !args.no_follow {
        let status = follow_logs(api, &rec.id.to_string(), None).await?;
        if status != BuildStatus::Succeeded {
            bail!("build ended with status {}", status_str(status));
        }
    }
    Ok(())
}

/// 流式打印日志，返回构建最终状态。
async fn follow_logs(api: &Api, id: &str, since: Option<u64>) -> Result<BuildStatus> {
    let query = since.map(|s| format!("?since={s}")).unwrap_or_default();
    let resp = api
        .http
        .get(api.url(&format!("/v1/builds/{id}/logs/stream{query}")))
        .send()
        .await?;
    let resp = Api::error_for_status(resp).await?;

    let mut frames = Box::pin(sse::frames(resp));
    while let Some(frame) = frames.next().await {
        let frame = frame?;
        match frame.event.as_str() {
            "end" => break,
            "error" => bail!("server error: {}", frame.data),
            _ => print_frame(&frame),
        }
    }

    let resp = api
        .http
        .get(api.url(&format!("/v1/builds/{id}")))
        .send()
        .await?;
    let rec: BuildRecord = Api::error_for_status(resp).await?.json().await?;
    Ok(rec.status)
}

fn print_frame(frame: &sse::Frame) {
    if let Ok(event) = serde_json::from_str::<hotpot_core::BuildEvent>(&frame.data) {
        match event.kind {
            hotpot_core::EventKind::Stderr => {
                eprintln!("[{:>4}] ! {}", event.seq, event.payload)
            }
            hotpot_core::EventKind::Phase => println!("---- {}", event.payload),
            _ => println!("[{:>4}] {}", event.seq, event.payload),
        }
    }
}

async fn cmd_download(api: &Api, id: &str, out_dir: PathBuf) -> Result<()> {
    let resp = api
        .http
        .get(api.url(&format!("/v1/builds/{id}/artifacts")))
        .send()
        .await?;
    let artifacts: Vec<hotpot_core::ArtifactMeta> =
        Api::error_for_status(resp).await?.json().await?;
    std::fs::create_dir_all(&out_dir)?;

    for artifact in artifacts {
        let resp = api
            .http
            .get(api.url(&format!(
                "/v1/artifacts/{}?filename={}",
                artifact.digest, artifact.name
            )))
            .send()
            .await?;
        let resp = Api::error_for_status(resp).await?;
        let path = out_dir.join(&artifact.name);
        let bytes = resp.bytes().await?;
        std::fs::write(&path, bytes)?;

        #[cfg(unix)]
        if artifact.attrs.contains_key("executable") {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            std::fs::set_permissions(&path, perms)?;
        }
        println!("downloaded {} -> {}", artifact.name, path.display());
    }
    Ok(())
}

async fn cmd_list(api: &Api, status: Option<&str>, limit: u32, offset: u32) -> Result<()> {
    let mut query = format!("?limit={limit}&offset={offset}");
    if let Some(status) = status {
        // 服务端按领域枚举的 JSON 形式匹配，值原样透传即可。
        query.push_str(&format!("&status={status}"));
    }
    let resp = api
        .http
        .get(api.url(&format!("/v1/builds{query}")))
        .send()
        .await?;
    let records: Vec<BuildRecord> = Api::error_for_status(resp).await?.json().await?;
    if records.is_empty() {
        println!("no builds matched");
        return Ok(());
    }
    println!(
        "{:<38} {:<11} {:>9} {:>9}  SOURCE",
        "BUILD", "STATUS", "BUILD_MS", "TOTAL_MS"
    );
    for rec in records {
        println!(
            "{:<38} {:<11} {:>9} {:>9}  {}",
            rec.id,
            status_str(rec.status),
            rec.timings.build_ms,
            rec.timings.total_ms,
            source_label(&rec),
        );
    }
    Ok(())
}

/// 列表里用一行概括构建来源。
fn source_label(rec: &BuildRecord) -> String {
    match &rec.source {
        hotpot_core::model::SourceSpec::Local { path } => {
            let trimmed = path.trim_end_matches('/');
            let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
            format!("local:{name}")
        }
        hotpot_core::model::SourceSpec::Git { url, ref_name, .. } => {
            let name = url.trim_end_matches('/').rsplit('/').next().unwrap_or(url);
            format!("git:{name}@{ref_name}")
        }
        hotpot_core::model::SourceSpec::Upload { upload_id, .. } => format!("upload:{upload_id}"),
    }
}

#[derive(Deserialize)]
struct ToolchainEntry {
    spec: String,
    rustc: String,
}

#[derive(Deserialize)]
struct ToolchainInventory {
    default_rustc: Option<String>,
    local: Vec<ToolchainEntry>,
    docker_images: Vec<String>,
    warnings: Vec<String>,
}

async fn cmd_toolchains(api: &Api) -> Result<()> {
    let resp = api.http.get(api.url("/v1/toolchains")).send().await?;
    let inv: ToolchainInventory = Api::error_for_status(resp).await?.json().await?;
    println!(
        "default: {}",
        inv.default_rustc.as_deref().unwrap_or("(unknown)")
    );
    if inv.local.is_empty() {
        println!("local toolchains: (none reported)");
    } else {
        println!("local toolchains:");
        for t in inv.local {
            println!("  {:<24} {}", t.spec, t.rustc);
        }
    }
    if inv.docker_images.is_empty() {
        println!("docker images: (none reported)");
    } else {
        println!("docker images:");
        for image in inv.docker_images {
            println!("  {image}");
        }
    }
    // 软失败语义：盘点不全不是错误，但必须让用户看见，否则会以为「没有工具链」。
    for warning in inv.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

fn print_record(rec: &BuildRecord) {
    println!("build:  {}", rec.id);
    println!("status: {}", status_str(rec.status));
    println!(
        "timings: build_ms={} total_ms={}",
        rec.timings.build_ms, rec.timings.total_ms
    );
    if let Some(error) = &rec.error {
        println!("error:  {error}");
    }
}

fn status_str(status: BuildStatus) -> &'static str {
    match status {
        BuildStatus::Queued => "queued",
        BuildStatus::Dispatched => "dispatched",
        BuildStatus::Running => "running",
        BuildStatus::Succeeded => "succeeded",
        BuildStatus::Failed => "failed",
        BuildStatus::Canceled => "canceled",
        BuildStatus::Timeout => "timeout",
    }
}
