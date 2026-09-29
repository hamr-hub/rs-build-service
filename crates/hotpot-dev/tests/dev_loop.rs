//! M6 验收：改函数体 → 增量重建 + 热重启；构建失败保留旧进程；
//! socket keeper 让公共端口在重启窗口不拒连。
//!
//! 被测项目是一个零依赖纯 std 的 HTTP 服务（离线可构建），绑定
//! HOTPOT_BIND_ADDR（dev 经 keeper 模式注入）。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hotpot_dev::builder::BuildArgs;
use hotpot_dev::dev;
use hotpot_dev::dev::DevOptions;
use tempfile::TempDir;

/// 生成零依赖 HTTP 服务；`rev` 决定响应体；`broken` 时写入编译错误。
fn write_main(project: &Path, rev: &str, broken: bool) {
    let src = if broken {
        "this is not valid rust {{{\n".to_string()
    } else {
        format!(
            r#"use std::io::{{Read, Write}};
use std::net::TcpListener;

const REV: &str = "{rev}";

/// 业务函数：改函数体后应被重新编译并生效。
fn message() -> String {{
    let mut out = String::from(REV);
    out.push_str(";fn-body-v1");
    out
}}

fn main() {{
    let addr = std::env::var("HOTPOT_BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let listener = TcpListener::bind(addr).expect("bind");
    for stream in listener.incoming() {{
        let mut stream = match stream {{
            Ok(s) => s,
            Err(_) => continue,
        }};
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let body = message();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {{}}\r\nConnection: close\r\n\r\n{{}}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
    }}
}}
"#
        )
    };
    std::fs::write(project.join("src").join("main.rs"), src).unwrap();
}

fn scaffold(project: &Path) {
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"devapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    write_main(project, "rev1", false);
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// 发 HTTP 请求并返回 body（去掉响应头）。
fn http_get(addr: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string()
}

/// 轮询直到 body 包含 `expected` 或超时。
fn wait_for_body(addr: &str, expected: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    loop {
        if let Ok(body) = std::panic::catch_unwind(|| http_get(addr)) {
            last = body.clone();
            if body.contains(expected) {
                return body;
            }
        }
        assert!(
            Instant::now() < deadline,
            "等待 {expected:?} 超时，最后 body={last:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// 多线程 runtime：测试主线程跑同步轮询循环时，spawn 的 dev 任务仍能在
// 其他 worker 上推进（current-thread runtime 会被同步轮询饿死）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dev_watch_rebuild_restart() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let tmp = TempDir::new().unwrap();
    let project: PathBuf = tmp.path().join("proj");
    scaffold(&project);

    let port = free_port();
    let public = format!("127.0.0.1:{port}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let run_project = project.clone();
    let run_public = public.clone();
    let handle = tokio::spawn(async move {
        let options = DevOptions {
            project: run_project,
            build: BuildArgs::default(),
            extra_env: BTreeMap::new(),
            public_addr: Some(run_public),
        };
        dev::run(options, async move {
            let _ = shutdown_rx.await;
        })
        .await
        .expect("dev run")
    });

    // 1) 初始构建并接流量（首次编译，给足时间）。
    let body = wait_for_body(&public, "rev1", Duration::from_secs(120));
    assert!(body.contains("fn-body-v1"), "初始 body={body:?}");

    // 2) 改函数体：重建期间公共端口必须始终可连（keeper 常驻 listen socket）。
    write_main(&project, "rev2", false);
    let probe_deadline = Instant::now() + Duration::from_secs(4);
    let mut refused = 0usize;
    while Instant::now() < probe_deadline {
        if TcpStream::connect(&public).is_err() {
            refused += 1;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(refused, 0, "重启窗口公共端口出现拒连 {refused} 次");

    // 3) 新版本生效。
    let body = wait_for_body(&public, "rev2", Duration::from_secs(60));
    assert!(body.contains("fn-body-v1"), "rev2 body={body:?}");

    // 4) 构建失败：旧进程继续服务，不被杀掉。
    write_main(&project, "rev3", true);
    std::thread::sleep(Duration::from_secs(6));
    for _ in 0..3 {
        assert!(
            http_get(&public).contains("rev2"),
            "构建失败期间旧版本应继续服务"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // 5) 修复后新版本生效。
    write_main(&project, "rev4", false);
    let body = wait_for_body(&public, "rev4", Duration::from_secs(60));
    assert!(body.contains("fn-body-v1"), "rev4 body={body:?}");

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(15), handle)
        .await
        .expect("dev 未在超时内退出")
        .unwrap();
}
