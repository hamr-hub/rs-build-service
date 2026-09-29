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
    /// 提交本地项目构建并（默认）实时跟踪日志。
    Build {
        /// Cargo 项目根目录。
        #[arg(long, short = 'p')]
        project: PathBuf,
        /// release 构建。
        #[arg(long)]
        release: bool,
        /// 启用的 features（逗号分隔）。
        #[arg(long)]
        features: Option<String>,
        /// 只提交不跟踪日志。
        #[arg(long)]
        no_follow: bool,
    },
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
            no_follow,
        } => {
            cmd_build(&api, project, release, features, no_follow).await?;
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

async fn cmd_build(
    api: &Api,
    project: PathBuf,
    release: bool,
    features: Option<String>,
    no_follow: bool,
) -> Result<()> {
    let profile = serde_json::json!({
        "mode": if release { "release" } else { "debug" },
        "features": features.map(|f| f.split(',').map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default(),
    });
    let body = serde_json::json!({
        "source": {
            "kind": "local",
            "path": project.canonicalize().context("project path")?.display().to_string(),
        },
        "profile": profile,
    });

    let resp = api
        .http
        .post(api.url("/v1/builds"))
        .json(&body)
        .send()
        .await?;
    let rec: BuildRecord = Api::error_for_status(resp).await?.json().await?;
    println!("submitted build {} ({})", rec.id, status_str(rec.status));

    if !no_follow {
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
