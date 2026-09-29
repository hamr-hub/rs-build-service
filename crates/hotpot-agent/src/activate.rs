//! 子进程侧的激活契约（应用接入库）。
//!
//! 固定 fd 布局（supervisor 经 pre_exec dup2 注入）：
//! - fd 3：真实监听 socket（systemd 风格 LISTEN_FDS）
//! - fd 4：预热探针 socket（127.0.0.1 随机端口，仅 Preflight 阶段使用）
//! - fd 5：控制管道（supervisor 写 ARM/COMMIT，子进程回写 ARMED）
//!
//! 时序：初始化/预热 → 在探针上应答 warm → 收到 ARM 后开始 accept
//! 真实端口 → COMMIT 后关闭探针；SIGTERM 后停止 accept、排空在飞请求再退出。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub const FD_LISTEN: RawFd = 3;
pub const FD_PROBE: RawFd = 4;
pub const FD_CONTROL: RawFd = 5;

/// 接管 supervisor 注入的资源；未被激活时返回 None（应用可按普通方式自绑端口）。
pub fn take() -> Option<Activation> {
    if std::env::var("HOTPOT_LISTEN_FDS").as_deref() != Ok("1") {
        return None;
    }
    // SAFETY: supervisor 保证三个 fd 已 dup2 就位且对应正确的 socket 类型。
    let listener = unsafe { TcpListener::from_raw_fd(FD_LISTEN) };
    let probe = unsafe { TcpListener::from_raw_fd(FD_PROBE) };
    let control = unsafe { std::fs::File::from_raw_fd(FD_CONTROL) };
    listener.set_nonblocking(true).ok()?;
    probe.set_nonblocking(true).ok()?;
    Some(Activation {
        listener,
        probe,
        control: RwLock::new(control),
        shutdown: install_signals(),
        in_flight: Arc::new(AtomicUsize::new(0)),
        probe_off: Arc::new(AtomicBool::new(false)),
    })
}

/// 信号标志：SIGTERM/SIGINT handler 唯一操作就是置位（lock-free，安全）。
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// 关闭标志的只读代理，读取进程级静态。
#[derive(Debug, Clone, Copy)]
pub struct Shutdown;

impl Shutdown {
    pub fn load(&self) -> bool {
        SHUTDOWN.load(Ordering::SeqCst)
    }
}

pub struct Activation {
    pub listener: TcpListener,
    pub probe: TcpListener,
    control: RwLock<std::fs::File>,
    pub shutdown: Shutdown,
    pub in_flight: Arc<AtomicUsize>,
    probe_off: Arc<AtomicBool>,
}

impl Activation {
    /// 阻塞直到 supervisor 发来 ARM；应答 ARMED 后应用方可开始接流。
    pub fn wait_arm(&self) -> std::io::Result<()> {
        let mut control = self.control.write().expect("control lock");
        let mut byte = [0u8; 1];
        let mut line = String::new();
        loop {
            control.read_exact(&mut byte)?;
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0] as char);
        }
        assert_eq!(line, "ARM", "unexpected control message: {line}");
        control.write_all(b"ARMED\n")?;
        control.flush()
    }

    /// 后台监听 COMMIT（关闭探针）。
    pub fn watch_commit(&self) {
        let probe_off = self.probe_off.clone();
        let control_fd = self
            .control
            .read()
            .expect("control lock")
            .try_clone()
            .expect("clone control");
        std::thread::spawn(move || {
            let mut control = control_fd;
            let mut buf = [0u8; 64];
            if let Ok(n) = control.read(&mut buf) {
                if n > 0 {
                    probe_off.store(true, Ordering::SeqCst);
                }
            }
        });
    }

    /// 探针服务循环：warm 之后应答 /healthz；COMMIT 后退出。
    pub fn serve_probe<F>(&self, warm: Arc<AtomicBool>, handler: F)
    where
        F: Fn(&TcpStream, &str) + Send + Sync + 'static,
    {
        let probe_off = self.probe_off.clone();
        let probe = self.probe.try_clone().expect("clone probe");
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            loop {
                if probe_off.load(Ordering::SeqCst) {
                    break;
                }
                match probe.accept() {
                    Ok((stream, _)) => {
                        let warm = warm.clone();
                        let handler = handler.clone();
                        std::thread::spawn(move || {
                            let handler: &F = &handler;
                            handle_one(stream, warm, handler);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
        });
    }

    /// 真实端口 accept 循环；armed 之前不接流，shutdown 后停止 accept。
    pub fn serve_listener<F>(&self, armed: Arc<AtomicBool>, handler: F)
    where
        F: Fn(&TcpStream, &str) + Send + Sync + 'static,
    {
        let listener = self.listener.try_clone().expect("clone listener");
        let shutdown = self.shutdown;
        let in_flight = self.in_flight.clone();
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            loop {
                if shutdown.load() {
                    break;
                }
                if !armed.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    continue;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        in_flight.fetch_add(1, Ordering::SeqCst);
                        let guard = InFlight(in_flight.clone());
                        let warm = Arc::new(AtomicBool::new(true));
                        let handler = handler.clone();
                        std::thread::spawn(move || {
                            let handler: &F = &handler;
                            handle_one(stream, warm, handler);
                            drop(guard);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
    }

    /// 排空在飞请求（SIGTERM 之后调用），超时则放弃等待。
    pub fn await_drained(&self, timeout: std::time::Duration) {
        let start = std::time::Instant::now();
        while self.in_flight.load(Ordering::SeqCst) > 0 && start.elapsed() < timeout {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

struct InFlight(Arc<AtomicUsize>);
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 读一个请求的首行，按路径分发；未 warm 时探针一律 503。
fn handle_one<F>(mut stream: TcpStream, warm: Arc<AtomicBool>, handler: &F)
where
    F: Fn(&TcpStream, &str),
{
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut byte = [0u8; 1];
    let mut line = String::new();
    while line.len() < 4096 {
        match stream.read(&mut byte) {
            Ok(0) => return,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0] as char);
            }
            Err(_) => return,
        }
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    if !warm.load(Ordering::SeqCst) {
        let body = "{\"status\":\"starting\"}";
        let resp = format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        drain_before_close(&mut stream);
        return;
    }
    handler(&stream, path);
    drain_before_close(&mut stream);
}

/// 响应后排空剩余请求再关闭：接收缓冲残留未读数据时，
/// macOS/BSD 关闭 socket 会直接发 RST 而非 FIN，客户端读到一半即报错。
fn drain_before_close(stream: &mut TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf = [0u8; 512];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => continue,
            Err(_) => return,
        }
    }
}

/// 拼装一个简单 HTTP 响应（Connection: close）。
pub fn reply(stream: &mut TcpStream, status: u16, reason: &str, content_type: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// 安装 SIGTERM/SIGINT 处理器：唯一动作是置位全局标志。
fn install_signals() -> Shutdown {
    extern "C" fn handler(_: libc::c_int) {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }

    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    // 具名函数指针 → sa_sigaction 整数字段。
    let handler: extern "C" fn(libc::c_int) = handler;
    sa.sa_sigaction = handler as usize;
    unsafe {
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
    Shutdown
}
