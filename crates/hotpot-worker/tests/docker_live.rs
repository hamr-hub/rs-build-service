//! Docker 执行器真实构建测试（需要 docker daemon；不可达时自动 skip）。
//!
//! 覆盖：容器内构建成功（产物经挂载点回到宿主机）、编译失败显式失败、
//! cancel 杀掉容器（build.rs 无限循环）。

use std::fs;
use std::time::Duration;

use hotpot_worker::{BuildPlan, ExecutorKind, run_build};
use tempfile::TempDir;

const IMAGE: &str = "rust:1.85-slim-bookworm";

fn fixture(dir: &std::path::Path, main_rs: &str) {
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\nbuild = \"build.rs\"\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src").join("main.rs"), main_rs).unwrap();
}

/// 临时目录：HOTPOT_TEST_TMP 优先；macOS 上 colima 默认只挂载 /Users，
/// /var/folders 下的 bind source 会被 daemon 静默建成空目录，因此未显式
/// 指定时在 macOS 也回落到 HOME 下；Linux 上系统临时目录（/tmp）通常已挂载。
fn test_tempdir() -> TempDir {
    let root = match std::env::var("HOTPOT_TEST_TMP") {
        Ok(root) => root,
        Err(_) if cfg!(target_os = "macos") => {
            format!("{}/.hotpot-tmp", std::env::var("HOME").unwrap_or_default())
        }
        Err(_) => std::env::temp_dir().to_string_lossy().into_owned(),
    };
    fs::create_dir_all(&root).unwrap();
    TempDir::new_in(root).unwrap()
}

fn docker_executor() -> ExecutorKind {
    ExecutorKind::Docker {
        image: IMAGE.to_string(),
        docker_host: None,
    }
}

/// 探测 daemon 是否可达：DOCKER_HOST → 默认 socket → 常见位置（含 colima）。
async fn docker_available() -> bool {
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        if let Ok(d) =
            bollard::Docker::connect_with_local(&host, PING_TIMEOUT, bollard::API_DEFAULT_VERSION)
            && d.ping().await.is_ok()
        {
            return true;
        }
    }
    if let Ok(d) = bollard::Docker::connect_with_local_defaults()
        && d.ping().await.is_ok()
    {
        return true;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    for socket in [
        "/var/run/docker.sock",
        &format!("{home}/.colima/default/docker.sock"),
    ] {
        if !std::path::Path::new(socket).exists() {
            continue;
        }
        if let Ok(d) =
            bollard::Docker::connect_with_unix(socket, PING_TIMEOUT, bollard::API_DEFAULT_VERSION)
            && d.ping().await.is_ok()
        {
            return true;
        }
    }
    false
}

const PING_TIMEOUT: u64 = 5;

fn drain(mut events: tokio::sync::mpsc::Receiver<hotpot_worker::BuildEvent>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(event) = events.blocking_recv() {
        out.push(format!("{:?}: {}", event.kind, event.payload));
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docker_build_success() {
    if !docker_available().await {
        eprintln!("skip: docker daemon unreachable");
        return;
    }

    let tmp = test_tempdir();
    let project = tmp.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    fixture(&project, "fn main() { println!(\"hi from container\"); }\n");
    fs::write(project.join("build.rs"), "fn main() {}\n").unwrap();

    let session = tmp.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.timeout = Duration::from_secs(300);

    let (events, handle) = run_build(plan, &docker_executor()).await;
    let logs = tokio::task::spawn_blocking(|| drain(events)).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(300), handle)
        .await
        .expect("join timed out")
        .unwrap();

    assert!(
        result.success,
        "容器内构建应成功；events:\n{}",
        logs.join("\n")
    );
    // 产物经 target 挂载点回到宿主机。
    let bin = session.join("target").join("debug").join("fixture");
    assert!(
        bin.exists(),
        "二进制应出现在宿主机挂载目录: {}",
        bin.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docker_build_failure() {
    if !docker_available().await {
        eprintln!("skip: docker daemon unreachable");
        return;
    }

    let tmp = test_tempdir();
    let project = tmp.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    fixture(&project, "fn main() { broken rust\n");
    fs::write(project.join("build.rs"), "fn main() {}\n").unwrap();

    let session = tmp.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.timeout = Duration::from_secs(300);

    let (events, handle) = run_build(plan, &docker_executor()).await;
    let logs = tokio::task::spawn_blocking(|| drain(events)).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(300), handle)
        .await
        .expect("join timed out")
        .unwrap();

    assert!(!result.success, "编译失败应显式失败");
    // 必须是真正的编译诊断，而非挂载空目录导致的 "could not find Cargo.toml"。
    assert!(
        logs.iter()
            .any(|l| l.contains("error[E") || l.contains("could not compile")),
        "事件中应包含 rustc 编译诊断；events:\n{}",
        logs.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docker_build_cancel() {
    if !docker_available().await {
        eprintln!("skip: docker daemon unreachable");
        return;
    }

    let tmp = test_tempdir();
    let project = tmp.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    fixture(&project, "fn main() {}\n");
    // build.rs 无限循环：cargo 永远卡在编译前。
    fs::write(
        project.join("build.rs"),
        "fn main() { loop { std::thread::sleep(std::time::Duration::from_millis(10)); } }\n",
    )
    .unwrap();

    let session = tmp.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.timeout = Duration::from_secs(300);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    plan.cancel = Some(cancel_rx);

    let (events, handle) = run_build(plan, &docker_executor()).await;
    // 给容器启动并进入 build.rs 的时间。
    tokio::time::sleep(Duration::from_secs(15)).await;
    cancel_tx.send(true).unwrap();

    let _logs = tokio::task::spawn_blocking(move || drain(events))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(60), handle)
        .await
        .expect("join timed out")
        .unwrap();

    use hotpot_worker::EndReason;
    assert_eq!(
        result.end_reason,
        EndReason::Canceled,
        "cancel 后容器应被杀掉并返回 Canceled"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docker_partial_toolchain_uses_preinstalled_image_toolchain() {
    if !docker_available().await {
        eprintln!("skip: docker daemon unreachable");
        return;
    }

    let tmp = test_tempdir();
    let project = tmp.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    fixture(&project, "fn main() {}\n");

    let session = tmp.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.timeout = Duration::from_secs(300);
    // 部分版本号 spec：镜像里预装的是 1.85.x-<host>，`+1.85` 若不链接会触发下载。
    plan.profile.toolchain = Some("1.85".to_string());

    let (events, handle) = run_build(plan, &docker_executor()).await;
    let logs = tokio::task::spawn_blocking(|| drain(events)).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(300), handle)
        .await
        .expect("join timed out")
        .unwrap();

    assert!(result.success, "构建应成功；events:\n{}", logs.join("\n"));
    // 回归：不得出现 rustup 联网同步工具链（镜像预热必须被复用）。
    assert!(
        logs.iter().none(|l| l.contains("syncing channel updates")),
        "不应触发 rustup 下载工具链；events:\n{}",
        logs.join("\n")
    );
}
