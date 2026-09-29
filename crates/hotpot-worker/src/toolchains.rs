//! 工具链盘点：列出本机已安装的 Rust 工具链，以及 docker 侧已缓存的
//! 官方 rust 镜像。供 `GET /v1/toolchains` 使用。

use std::path::Path;

use bollard::API_DEFAULT_VERSION;
use bollard::{Docker, image::ListImagesOptions};
use tokio::process::Command;

/// 一条已安装工具链信息。
#[derive(Debug, Clone, serde::Serialize)]
pub struct InstalledToolchain {
    /// rustup 工具链规格（去掉宿主三元组后缀），如 `stable`、`1.98.0`。
    pub spec: String,
    /// `rustc --version` 输出。
    pub rustc: String,
}

/// 默认 rustc 版本（本机）。
pub async fn default_rustc() -> Result<String, String> {
    let out = Command::new("rustc")
        .arg("--version")
        .output()
        .await
        .map_err(|e| format!("spawn rustc: {e}"))?;
    if !out.status.success() {
        return Err("rustc --version failed".to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 盘点本机 rustup 工具链。
pub async fn local_inventory() -> Result<Vec<InstalledToolchain>, String> {
    let host = host_triple().await?;
    let out = Command::new("rustup")
        .args(["toolchain", "list", "-v"])
        .output()
        .await
        .map_err(|e| format!("spawn rustup: {e}"))?;
    if !out.status.success() {
        return Err("rustup toolchain list failed".to_string());
    }

    let mut toolchains = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // -v 输出中路径行以空白或 '/' 开头；工具链名行以名称开头。
        if line.starts_with(char::is_whitespace) || line.starts_with('/') {
            continue;
        }
        let full_name = line.split_whitespace().next().unwrap_or_default();
        if full_name.is_empty() {
            continue;
        }
        let spec = match full_name.strip_suffix(&format!("-{host}")) {
            Some(s) => s.to_string(),
            None => full_name.to_string(),
        };
        let rustc = rustc_version(full_name).await?;
        toolchains.push(InstalledToolchain { spec, rustc });
    }
    Ok(toolchains)
}

/// 查询某个 rustup 工具链的 rustc 版本。
async fn rustc_version(full_name: &str) -> Result<String, String> {
    let out = Command::new("rustup")
        .args(["run", full_name, "rustc", "--version"])
        .output()
        .await
        .map_err(|e| format!("spawn rustup run: {e}"))?;
    if !out.status.success() {
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 通过 `rustc -vV` 取本机宿主三元组。
async fn host_triple() -> Result<String, String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .await
        .map_err(|e| format!("spawn rustc: {e}"))?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(triple) = line.strip_prefix("host: ") {
            return Ok(triple.trim().to_string());
        }
    }
    Err("could not determine rustc host triple".to_string())
}

/// 盘点 daemon 上已缓存的官方 rust 镜像标签。
pub async fn docker_inventory(docker_host: Option<&str>) -> Result<Vec<String>, String> {
    let docker = connect(docker_host).await?;
    let images = docker
        .list_images(Some(ListImagesOptions::<String> {
            filters: [("reference".to_string(), vec!["rust".to_string()])]
                .into_iter()
                .collect(),
            ..Default::default()
        }))
        .await
        .map_err(|e| format!("list images: {e}"))?;

    let mut tags: Vec<String> = images
        .into_iter()
        .flat_map(|img| img.repo_tags)
        // 忽略悬空镜像 ID 占位。
        .filter(|tag| !tag.starts_with('<'))
        .collect();
    tags.sort();
    tags.dedup();
    Ok(tags)
}

async fn connect(docker_host: Option<&str>) -> Result<Docker, String> {
    if let Some(host) = docker_host {
        return super::docker::connect_uri(host).await;
    }
    if let Ok(d) = Docker::connect_with_local_defaults() {
        if d.ping().await.is_ok() {
            return Ok(d);
        }
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        "/var/run/docker.sock",
        &format!("{home}/.docker/run/docker.sock"),
        &format!("{home}/.colima/default/docker.sock"),
        &format!("{home}/.docker/desktop/docker.sock"),
    ];
    for socket in candidates {
        if !Path::new(socket).exists() {
            continue;
        }
        if let Ok(d) = bollard::Docker::connect_with_unix(
            socket,
            super::docker::CONNECT_TIMEOUT_SECS,
            API_DEFAULT_VERSION,
        ) {
            if d.ping().await.is_ok() {
                return Ok(d);
            }
        }
    }
    Err("no reachable docker daemon".to_string())
}
