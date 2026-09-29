//! Docker 执行器真实构建测试（需要 docker daemon；不可达时自动 skip）。
//!
//! 覆盖：容器内构建成功（产物经挂载点回到宿主机）、编译失败显式失败、
//! cancel 杀掉容器（build.rs 无限循环）。

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use hotpot_worker::{BuildPlan, ExecutorKind, run_build};
use tempfile::TempDir;
use tokio::sync::Mutex;

const IMAGE: &str = "rust:1.85-slim-bookworm";
/// 单个用例的构建超时。**刻意保持温和**：用例失败时要尽快给出结论，
/// 而不是让整个 docker 测试组被一个卡住的用例拖住。
const BUILD_TIMEOUT: Duration = Duration::from_secs(240);

/// 容器内能否访问 apt 源。
///
/// 自动安装 build-essential 需要外网。在受限网络里，这个探测让相关用例
/// **秒级跳过**，而不是跑满超时才失败——后者还会把同一测试组里的其它用例
/// 一起拖慢。探测本身用 `getent`（镜像自带，不产生 apt 副作用）。
async fn apt_reachable() -> bool {
    use bollard::container::{CreateContainerOptions, RemoveContainerOptions};
    use bollard::image::CreateImageOptions;
    use futures::StreamExt;

    let Ok(docker) = bollard::Docker::connect_with_local_defaults() else {
        return false;
    };
    if docker.ping().await.is_err() {
        return false;
    }
    // bollard 的 create_image 直接返回流（错误通过流里的 item 报告）。
    let mut stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: IMAGE.to_string(),
            ..Default::default()
        }),
        None,
        None,
    );
    while let Some(_item) = stream.next().await {}

    let config = bollard::container::Config {
        image: Some(IMAGE.to_string()),
        // 15s 足够解析 DNS；不通就立刻放弃。
        cmd: Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "timeout 15 getent hosts deb.debian.org".to_string(),
        ]),
        ..Default::default()
    };
    let Ok(created) = docker
        .create_container(
            Some(CreateContainerOptions {
                name: String::new(),
                platform: None,
            }),
            config,
        )
        .await
    else {
        return false;
    };
    let id = created.id;
    let started = docker.start_container::<String>(&id, None).await.is_ok();
    let result = if started {
        let mut wait = docker.wait_container::<String>(&id, None);
        match tokio::time::timeout(Duration::from_secs(20), wait.next()).await {
            Ok(Some(item)) => item.ok().map(|out| out.status_code == 0).unwrap_or(false),
            // wait 流提前结束或探测超时：保守视为不可达。
            Ok(None) | Err(_) => false,
        }
    } else {
        false
    };
    let _ = docker
        .remove_container(
            &id,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
    result
}

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

/// 持久化工具目录（apt 归档/lists、sccache 均在此）：测试间共享，
/// cold apt update 只发生一次。位于 $HOME 下以保证 colima 已挂载。
fn shared_tools_dir() -> PathBuf {
    let root = match std::env::var("HOTPOT_TEST_TMP") {
        Ok(root) => root,
        Err(_) => format!("{}/.hotpot-tmp", std::env::var("HOME").unwrap_or_default()),
    };
    let dir = PathBuf::from(root).join("live-tools");
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// live 测试串行化：colima VM 资源有限，并行容器同时冷 apt/编译会互相
/// 拖死（实测 4 路并行全部超 600s）。
static LIVE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn live_lock() -> &'static Mutex<()> {
    LIVE_LOCK.get_or_init(|| Mutex::new(()))
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
    // 所有 live 测试串行执行，避免在 colima 上争用资源（见 LIVE_LOCK）。
    let _guard = live_lock().lock().await;

    let tmp = test_tempdir();
    let project = tmp.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    fixture(&project, "fn main() { println!(\"hi from container\"); }\n");
    fs::write(project.join("build.rs"), "fn main() {}\n").unwrap();

    let session = tmp.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.tools_dir = Some(shared_tools_dir());
    plan.timeout = BUILD_TIMEOUT;

    let (events, handle) = run_build(plan, &docker_executor()).await;
    let logs = tokio::task::spawn_blocking(|| drain(events)).await.unwrap();
    let result = tokio::time::timeout(BUILD_TIMEOUT, handle)
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
    // 所有 live 测试串行执行，避免在 colima 上争用资源（见 LIVE_LOCK）。
    let _guard = live_lock().lock().await;

    let tmp = test_tempdir();
    let project = tmp.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    fixture(&project, "fn main() { broken rust\n");
    fs::write(project.join("build.rs"), "fn main() {}\n").unwrap();

    let session = tmp.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.tools_dir = Some(shared_tools_dir());
    plan.timeout = BUILD_TIMEOUT;

    let (events, handle) = run_build(plan, &docker_executor()).await;
    let logs = tokio::task::spawn_blocking(|| drain(events)).await.unwrap();
    let result = tokio::time::timeout(BUILD_TIMEOUT, handle)
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
    // 所有 live 测试串行执行，避免在 colima 上争用资源（见 LIVE_LOCK）。
    let _guard = live_lock().lock().await;

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
    plan.tools_dir = Some(shared_tools_dir());
    plan.timeout = BUILD_TIMEOUT;
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
    // 所有 live 测试串行执行，避免在 colima 上争用资源（见 LIVE_LOCK）。
    let _guard = live_lock().lock().await;

    let tmp = test_tempdir();
    let project = tmp.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    fixture(&project, "fn main() {}\n");
    fs::write(project.join("build.rs"), "fn main() {}\n").unwrap();

    let session = tmp.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.tools_dir = Some(shared_tools_dir());
    plan.timeout = BUILD_TIMEOUT;
    // 部分版本号 spec：镜像里预装的是 1.85.x-<host>，容器内 cargo 不带 +spec，
    // 直接使用镜像 default 工具链（spec 的作用仅在选镜像）。
    plan.profile.toolchain = Some("1.85".to_string());

    let (events, handle) = run_build(plan, &docker_executor()).await;
    let logs = tokio::task::spawn_blocking(|| drain(events)).await.unwrap();
    let result = tokio::time::timeout(BUILD_TIMEOUT, handle)
        .await
        .expect("join timed out")
        .unwrap();

    assert!(result.success, "构建应成功；events:\n{}", logs.join("\n"));
    // 回归：不得出现 rustup 联网同步工具链（镜像预热必须被复用）。
    assert!(
        !logs.iter().any(|l| l.contains("syncing channel updates")),
        "不应触发 rustup 下载工具链；events:\n{}",
        logs.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docker_slim_image_auto_installs_build_essential() {
    if !docker_available().await {
        eprintln!("skip: docker daemon unreachable");
        return;
    }
    if !apt_reachable().await {
        eprintln!("skip: container cannot reach deb.debian.org (no apt provisioning possible)");
        return;
    }
    // 所有 live 测试串行执行，避免在 colima 上争用资源（见 LIVE_LOCK）。
    let _guard = live_lock().lock().await;

    let tmp = test_tempdir();
    let project = tmp.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    fixture(&project, "fn main() {}\n");
    // 构建脚本直接调用 cc 编译一段 C：slim 镜像默认没有 cc，
    // 必须由执行器自动安装 build-essential 后才能成功（fd 的 jemalloc-sys
    // 即因此失败——真实回归来源）。
    fs::write(
        project.join("build.rs"),
        r#"fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/add.c"), "int add(int a, int b) { return a + b; }\n").unwrap();
    let status = std::process::Command::new("cc")
        .args(["-c", &format!("{out}/add.c"), "-o", &format!("{out}/add.o")])
        .status()
        .expect("cc should exist after build-essential provisioning");
    assert!(status.success(), "cc compilation failed");
}
"#,
    )
    .unwrap();

    let session = tmp.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.tools_dir = Some(shared_tools_dir());
    plan.timeout = BUILD_TIMEOUT;

    let (events, handle) = run_build(plan, &docker_executor()).await;
    let logs = tokio::task::spawn_blocking(|| drain(events)).await.unwrap();
    let result = tokio::time::timeout(BUILD_TIMEOUT, handle)
        .await
        .expect("join timed out")
        .unwrap();

    assert!(
        result.success,
        "slim 镜像应自动装好 cc 后构建成功；events:\n{}",
        logs.join("\n")
    );
    // 预处理确实发生过（apt 安装日志可见）。
    assert!(
        logs.iter().any(|l| l.contains("build-essential")),
        "事件中应出现 build-essential 安装；events:\n{}",
        logs.join("\n")
    );
}

/// same-arch musl：自动装 musl-tools，产物静态链接（无 glibc 依赖，
/// 可直接放进 scratch/distroless 镜像）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docker_same_arch_musl_builds_static_binary() {
    if !docker_available().await {
        eprintln!("skip: docker daemon unreachable");
        return;
    }
    let _guard = live_lock().lock().await;

    let tmp = test_tempdir();
    let project = tmp.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    fixture(&project, "fn main() { println!(\"musl\"); }\n");
    fs::write(project.join("build.rs"), "fn main() {}\n").unwrap();

    let session = tmp.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.tools_dir = Some(shared_tools_dir());
    plan.timeout = BUILD_TIMEOUT;
    let target = format!("{}-unknown-linux-musl", std::env::consts::ARCH);
    plan.profile.target = Some(target.clone());

    let (events, handle) = run_build(plan, &docker_executor()).await;
    let logs = tokio::task::spawn_blocking(|| drain(events)).await.unwrap();
    let result = tokio::time::timeout(BUILD_TIMEOUT, handle)
        .await
        .expect("join timed out")
        .unwrap();

    assert!(
        result.success,
        "musl 构建应成功（自动供给 musl-tools）；events:\n{}",
        logs.join("\n")
    );
    let bin = session
        .join("target")
        .join(&target)
        .join("debug")
        .join("fixture");
    assert!(bin.exists(), "musl 二进制应存在: {}", bin.display());
    // musl 默认静态链接；用 file(1) 验证（macOS/Linux 均自带）。
    let desc = std::process::Command::new("file")
        .arg(&bin)
        .output()
        .expect("file(1) available");
    let desc = String::from_utf8_lossy(&desc.stdout);
    assert!(
        desc.contains("statically linked"),
        "musl 产物应静态链接；file 输出: {desc}"
    );
}

/// wasm32-unknown-unknown：无需系统工具链（rust-lld 随 rustup 组件），
/// rustlib 经 rustlib-cache 持久化，产物为合法 wasm 模块。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docker_wasm32_builds_valid_wasm_module() {
    if !docker_available().await {
        eprintln!("skip: docker daemon unreachable");
        return;
    }
    let _guard = live_lock().lock().await;

    let tmp = test_tempdir();
    let project = tmp.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    fixture(&project, "fn main() {}\n");
    fs::write(project.join("build.rs"), "fn main() {}\n").unwrap();

    let session = tmp.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.tools_dir = Some(shared_tools_dir());
    plan.timeout = BUILD_TIMEOUT;
    let target = "wasm32-unknown-unknown";
    plan.profile.target = Some(target.to_string());

    let (events, handle) = run_build(plan, &docker_executor()).await;
    let logs = tokio::task::spawn_blocking(|| drain(events)).await.unwrap();
    let result = tokio::time::timeout(BUILD_TIMEOUT, handle)
        .await
        .expect("join timed out")
        .unwrap();

    assert!(
        result.success,
        "wasm32 构建应成功；events:\n{}",
        logs.join("\n")
    );
    let wasm = session
        .join("target")
        .join(target)
        .join("debug")
        .join("fixture.wasm");
    assert!(wasm.exists(), "wasm 产物应存在: {}", wasm.display());
    // wasm 魔数 \0asm。
    assert_eq!(&fs::read(&wasm).unwrap()[..4], b"\0asm", "非法 wasm 魔数");
}
