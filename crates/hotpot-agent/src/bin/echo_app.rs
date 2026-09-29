//! 接入示例：支持 fd 激活的最小 HTTP 服务。
//!
//! 环境变量：
//! - APP_VERSION：版本标识（响应体/健康检查中返回）
//! - PREWARM_MS：预热耗时（此期间探针 503）
//! - HANDLER_MS：每请求处理耗时（用于制造在飞连接）
//! - ADDR：未被激活时的自绑地址（默认 127.0.0.1:8080）

use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hotpot_agent::activate::{self, reply};

fn main() {
    let version = std::env::var("APP_VERSION").unwrap_or_else(|_| "dev".into());
    let prewarm_ms: u64 = std::env::var("PREWARM_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let handler_ms: u64 = std::env::var("HANDLER_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    match activate::take() {
        Some(activation) => run_activated(activation, version, prewarm_ms, handler_ms),
        None => run_standalone(version, handler_ms),
    }
}

fn make_handler(version: String, handler_ms: u64) -> impl Fn(&TcpStream, &str) + Send + Sync {
    move |stream: &TcpStream, path: &str| {
        if handler_ms > 0 {
            std::thread::sleep(Duration::from_millis(handler_ms));
        }
        let mut stream = stream.try_clone().expect("clone stream");
        if path == "/healthz" {
            let body = format!("{{\"status\":\"warm\",\"version\":\"{version}\"}}");
            reply(&mut stream, 200, "OK", "application/json", &body);
        } else {
            let body = format!("version={version} pid={}\n", std::process::id());
            reply(&mut stream, 200, "OK", "text/plain", &body);
        }
    }
}

fn run_activated(
    activation: activate::Activation,
    version: String,
    prewarm_ms: u64,
    handler_ms: u64,
) {
    let warm = Arc::new(AtomicBool::new(false));
    let armed = Arc::new(AtomicBool::new(false));

    activation.serve_probe(warm.clone(), make_handler(version.clone(), handler_ms));
    activation.serve_listener(armed.clone(), make_handler(version, handler_ms));

    // 预热：完成后探针才返回 200。
    std::thread::sleep(Duration::from_millis(prewarm_ms));
    warm.store(true, Ordering::SeqCst);

    activation.wait_arm().expect("arm");
    activation.watch_commit();
    armed.store(true, Ordering::SeqCst);

    while !activation.shutdown.load() {
        std::thread::sleep(Duration::from_millis(100));
    }
    activation.await_drained(Duration::from_secs(7));
}

fn run_standalone(version: String, handler_ms: u64) {
    let addr = std::env::var("ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = std::net::TcpListener::bind(&addr).expect("bind");
    let handler = make_handler(version, handler_ms);
    loop {
        let (stream, _) = listener.accept().expect("accept");
        let mut line = [0u8; 512];
        let _ = stream.peek(&mut line);
        let text = String::from_utf8_lossy(&line);
        let path = text.split_whitespace().nth(1).unwrap_or("/");
        handler(&stream, path);
    }
}
