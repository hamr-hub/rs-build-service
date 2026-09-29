//! 增量构建：在既有 target 上跑 cargo build，失败时返回诊断并保留旧进程。
//!
//! 产物路径通过 `cargo metadata` 解析（首个 bin target + 目标目录），
//! 不依赖固定项目布局。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use tokio::process::Command;

/// 一次构建的结果。
#[derive(Debug)]
pub struct BuildOutcome {
    pub success: bool,
    pub duration: std::time::Duration,
    /// 失败时的 cargo 诊断尾部，用于控制台展示。
    pub stderr_tail: String,
    /// 成功时的主二进制路径。
    pub binary: Option<PathBuf>,
}

/// 构建参数。
#[derive(Debug, Clone, Default)]
pub struct BuildArgs {
    pub release: bool,
    pub features: Vec<String>,
}

/// 增量构建 `project`。
pub async fn build(project: &Path, args: &BuildArgs) -> anyhow::Result<BuildOutcome> {
    let started = Instant::now();
    let mut cmd = Command::new(cargo_bin());
    cmd.current_dir(project)
        .arg("build")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if args.release {
        cmd.arg("--release");
    }
    if !args.features.is_empty() {
        cmd.arg("--features").arg(args.features.join(","));
    }

    let output = cmd.output().await?;
    let success = output.status.success();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stderr_tail = tail_lines(&stderr, STDERR_TAIL_LINES);

    let binary = if success {
        resolve_binary(project, args.release).ok()
    } else {
        None
    };

    Ok(BuildOutcome {
        success,
        duration: started.elapsed(),
        stderr_tail,
        binary,
    })
}

const STDERR_TAIL_LINES: usize = 40;

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// 经 cargo metadata 定位首个 bin target 的产物路径。
fn resolve_binary(project: &Path, release: bool) -> anyhow::Result<PathBuf> {
    let output = std::process::Command::new(cargo_bin())
        .current_dir(project)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()?;
    let meta: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    let target_dir = meta["target_directory"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("metadata missing target_directory"))?;
    let profile = if release { "release" } else { "debug" };

    // 取首个含 bin kind 的 target；优先 main.rs 对应的主 bin。
    let name = meta["packages"]
        .as_array()
        .and_then(|pkgs| {
            pkgs.iter().find_map(|pkg| {
                pkg["targets"].as_array().and_then(|targets| {
                    targets.iter().find(|t| {
                        t["kind"]
                            .as_array()
                            .is_some_and(|k| k.iter().any(|x| x == "bin"))
                    })
                })
            })
        })
        .and_then(|t| t["name"].as_str())
        .ok_or_else(|| anyhow::anyhow!("no binary target found"))?;

    Ok(Path::new(target_dir).join(profile).join(name))
}

fn cargo_bin() -> String {
    std::env::var("HOTPOT_CARGO_BIN").unwrap_or_else(|_| "cargo".to_string())
}
