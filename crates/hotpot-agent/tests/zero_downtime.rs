//! M5 验收：部署期间持续压测，0 失败请求，且新旧版本都服务过流量；
//! Preflight 失败时自动回退，旧版本继续可用；rollback 命令可用。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const LOAD_THREADS: usize = 8;
const HANDLER_MS: u64 = 15;

struct Agent {
    child: Child,
    port: u16,
    data: tempfile::TempDir,
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn start_agent(version: &str, prewarm_ms: u64) -> Agent {
    let data = tempfile::tempdir().unwrap();
    let port = free_port();
    let bin = env!("CARGO_BIN_EXE_hotpot-agent");
    let app = env!("CARGO_BIN_EXE_echo_app");
    let child = Command::new(bin)
        .args(["--data-dir"])
        .arg(data.path())
        .args(["run", "--listen"])
        .arg(format!("127.0.0.1:{port}"))
        .args(["--app", app, "--version", version,
            "--env", &format!("APP_VERSION={version}"),
            "--env", &format!("PREWARM_MS={prewarm_ms}"),
            "--env", &format!("HANDLER_MS={HANDLER_MS}")])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        // agent 自成进程组：停止时整组杀掉，避免 SIGKILL agent 泄漏 echo_app。
        .process_group(0)
        .spawn()
        .unwrap();

    let agent = Agent { child, port, data };
    agent.wait_ready(Duration::from_secs(8));
    agent
}

impl Agent {
    fn wait_ready(&self, timeout: Duration) {
        let start = std::time::Instant::now();
        loop {
            if self.request("/version").is_some() {
                return;
            }
            if start.elapsed() > timeout {
                panic!("agent did not become ready on port {}", self.port);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn request(&self, path: &str) -> Option<(u16, String)> {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", self.port)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
        let req = format!("GET {path} HTTP/1.0\r\n\r\n");
        stream.write_all(req.as_bytes()).ok()?;
        stream.shutdown(std::net::Shutdown::Write).ok()?;
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        let status = text.split_whitespace().nth(1)?.parse().ok()?;
        let body = text.split("\r\n\r\n").last().unwrap_or("").to_string();
        Some((status, body))
    }

    fn control(&self, value: serde_json::Value) -> (bool, Option<String>) {
        let mut conn =
            std::os::unix::net::UnixStream::connect(self.data.path().join("agent.sock")).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        conn.write_all(&serde_json::to_vec(&value).unwrap())
            .unwrap();
        conn.shutdown(std::net::Shutdown::Write).unwrap();
        let mut bytes = Vec::new();
        conn.read_to_end(&mut bytes).unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (
            reply.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            reply
                .get("error")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        )
    }

    fn deploy(&self, version: &str, prewarm_ms: u64) -> bool {
        let mut env = BTreeMap::new();
        env.insert("APP_VERSION".to_string(), version.to_string());
        env.insert("PREWARM_MS".to_string(), prewarm_ms.to_string());
        env.insert("HANDLER_MS".to_string(), HANDLER_MS.to_string());
        let app: PathBuf = env!("CARGO_BIN_EXE_echo_app").into();
        let (ok, error) = self.control(serde_json::json!({
            "op": "deploy",
            "path": app,
            "version": version,
            "env": env,
        }));
        if let Some(error) = error {
            eprintln!("deploy {version}: {error}");
        }
        ok
    }

    fn stop(&mut self) {
        // 杀整个进程组（agent + echo_app），负号 = pgid。
        unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
        let _ = self.child.wait();
        // 兜底：按数据目录清掉任何未随组退出的版本进程。
        let _ = Command::new("/usr/bin/pkill")
            .arg("-9")
            .arg("-f")
            .arg(format!("{}/versions/", self.data.path().display()))
            .output();
    }
}

/// panic 也不泄漏 agent：否则孤儿进程持有管道会让 cargo test 永久挂起。
impl Drop for Agent {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn zero_failed_requests_during_deploy() {
    let mut agent = start_agent("v1", 300);
    let port = agent.port;

    let errors = Arc::new(Mutex::new(0u64));
    let versions = Arc::new(Mutex::new(Vec::<String>::new()));
    // 负载持续到部署提交之后：固定次数会在 v2 接管前跑完，无法验证两版本。
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut handles = Vec::new();
    for _ in 0..LOAD_THREADS {
        let errors = errors.clone();
        let versions = versions.clone();
        let stop = stop.clone();
        handles.push(thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                match one_request(port) {
                    Some((200, body)) => {
                        let version = body
                            .split_whitespace()
                            .next()
                            .and_then(|kv| kv.strip_prefix("version="))
                            .unwrap_or("?")
                            .to_string();
                        versions.lock().unwrap().push(version);
                    }
                    Some((status, _)) => {
                        eprintln!("non-200: {status}");
                        *errors.lock().unwrap() += 1;
                    }
                    None => {
                        *errors.lock().unwrap() += 1;
                    }
                }
            }
        }));
    }

    // 压测开始 0.4s 后（在飞请求持续期间）触发部署；deploy 阻塞至提交完成。
    thread::sleep(Duration::from_millis(400));
    assert!(agent.deploy("v2", 400), "deploy v2 failed");
    // v2 已提交、v1 已 drain；再续跑一段，确保新版本确实接过流量。
    thread::sleep(Duration::from_millis(300));
    stop.store(true, std::sync::atomic::Ordering::SeqCst);

    for handle in handles {
        handle.join().unwrap();
    }

    let served = versions.lock().unwrap();
    let v1_count = served.iter().filter(|v| v.as_str() == "v1").count();
    let v2_count = served.iter().filter(|v| v.as_str() == "v2").count();
    let error_count = *errors.lock().unwrap();

    println!("served v1={v1_count} v2={v2_count} errors={error_count}");
    assert_eq!(error_count, 0, "requests failed during deploy");
    assert!(
        v1_count > 0 && v2_count > 0,
        "both versions must serve traffic"
    );

    // 部署完成后新请求全部落在 v2。
    let (status, body) = agent.request("/version").unwrap();
    assert!(
        body.contains("version=v2"),
        "post-deploy request status={status} body={body:?}"
    );

    agent.stop();
}

#[test]
fn failed_preflight_keeps_old_version() {
    let mut agent = start_agent("v1", 200);

    // /usr/bin/false 启动即退出：Preflight 失败，point-of-no-return 前自动回退。
    let (ok, _) = agent.control(serde_json::json!({
        "op": "deploy",
        "path": "/usr/bin/false",
        "version": "broken",
        "env": {},
    }));
    assert!(!ok, "broken deploy must fail");

    let (status, body) = agent.request("/version").unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("version=v1"), "old version must keep serving");

    agent.stop();
}

#[test]
fn rollback_returns_previous_version() {
    let mut agent = start_agent("v1", 200);
    assert!(agent.deploy("v2", 200));
    let (_, body) = agent.request("/version").unwrap();
    assert!(body.contains("version=v2"));

    let (ok, _) = agent.control(serde_json::json!({ "op": "rollback" }));
    assert!(ok, "rollback failed");

    let (status, body) = agent.request("/version").unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("version=v1"));

    agent.stop();
}

fn one_request(port: u16) -> Option<(u16, String)> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream.write_all(b"GET /version HTTP/1.0\r\n\r\n").ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    let body = text
        .split("\r\n\r\n")
        .last()
        .unwrap_or("")
        .trim()
        .to_string();
    Some((status, body))
}
