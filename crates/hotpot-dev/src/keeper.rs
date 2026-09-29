//! Socket keeper：常驻代理公共端口，重启子进程期间端口不消失、不拒连。
//!
//! 子进程经 `HOTPOT_BIND_ADDR` 绑定到固定 loopback 后端端口；keeper 接受
//! 公共连接后转发到后端。后端短暂不可用时持连重试，就绪后续传，超过窗口
//! 回 503（面向 HTTP 开发服务）。

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};

/// 后端就绪等待窗口：覆盖一次增量重启的典型时长。
pub const READY_WINDOW: Duration = Duration::from_secs(10);
pub const RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// 选取一个空闲 loopback 端口作为子进程后端地址（dev 本地，竞争可忽略）。
pub fn pick_backend_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// 在 `public` 上运行代理直到任务被 abort。
pub async fn run(public: &str, backend: SocketAddr) -> anyhow::Result<()> {
    let listener = TcpListener::bind(public).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = forward(stream, backend).await {
                tracing::debug!("keeper forward ended: {e}");
            }
        });
    }
}

/// 连接后端（带就绪窗口），成功后双向透传。
async fn forward(mut public: TcpStream, backend: SocketAddr) -> anyhow::Result<()> {
    let backend = match connect_ready(backend).await {
        Ok(stream) => stream,
        Err(()) => {
            let _ = public.try_write(RETRY_503);
            anyhow::bail!("backend not ready within window");
        }
    };
    let mut backend = backend;
    tokio::io::copy_bidirectional(&mut public, &mut backend).await?;
    Ok(())
}

/// 在就绪窗口内反复尝试连接后端。
async fn connect_ready(backend: SocketAddr) -> Result<TcpStream, ()> {
    let start = tokio::time::Instant::now();
    loop {
        if let Ok(stream) = TcpStream::connect(backend).await {
            return Ok(stream);
        }
        if start.elapsed() > READY_WINDOW {
            return Err(());
        }
        tokio::time::sleep(RETRY_INTERVAL).await;
    }
}

const RETRY_503: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
