//! `GET /v1/toolchains`：工具链发现。
//!
//! 构建请求可以指定 `profile.toolchain`，但用户往往不知道**这台机器上**
//! 到底有什么可用：宿主装了哪些 rustup 工具链、docker daemon 上缓存了哪些
//! 官方 rust 镜像。这个端点把两者列出来，让「换一个工具链构建」不需要
//! 猜版本号。
//!
//! 刻意做成**软失败**：daemon 不可达、rustup 不存在等只体现为字段缺失与
//! `warnings`，绝不让整个请求 500——发现接口不可用不该阻断构建。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hotpot_worker::toolchains::InstalledToolchain;
use serde::Serialize;

/// 工具链清单响应。
#[derive(Debug, Serialize)]
pub struct ToolchainInventory {
    /// 宿主默认 `rustc --version` 输出。
    pub default_rustc: Option<String>,
    /// 宿主 rustup 已安装的工具链。
    pub local: Vec<InstalledToolchain>,
    /// docker daemon 已缓存的官方 `rust:` 镜像标签。
    pub docker_images: Vec<String>,
    /// docker daemon 报告的 CPU 架构（决定容器内工具链/工具的架构）。
    pub docker_arch: Option<String>,
    /// 非致命问题（工具缺失、daemon 不可达等）。
    pub warnings: Vec<String>,
}

/// 工具链路由状态。
#[derive(Clone)]
pub struct ToolchainState {
    /// docker daemon 地址；None 走自动探测。
    pub docker_host: Option<String>,
}

pub async fn inventory(State(state): State<ToolchainState>) -> Response {
    let mut warnings = Vec::new();

    let default_rustc = match hotpot_worker::toolchains::default_rustc().await {
        Ok(v) => Some(v),
        Err(e) => {
            warnings.push(format!("读取默认 rustc 版本失败: {e}"));
            None
        }
    };

    let local = match hotpot_worker::toolchains::local_inventory().await {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("盘点 rustup 工具链失败: {e}"));
            Vec::new()
        }
    };

    // daemon 架构决定容器内工具链/工具该用哪个架构的资产，一并报出来，
    // 免得用户选了工具链才发现镜像架构不对。
    let docker_arch = hotpot_worker::docker::daemon_arch(state.docker_host.as_deref())
        .await
        .ok();

    let docker_images =
        match hotpot_worker::toolchains::docker_inventory(state.docker_host.as_deref()).await {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("盘点 docker 镜像失败: {e}"));
                Vec::new()
            }
        };

    (
        StatusCode::OK,
        Json(ToolchainInventory {
            default_rustc,
            local,
            docker_images,
            docker_arch,
            warnings,
        }),
    )
        .into_response()
}
