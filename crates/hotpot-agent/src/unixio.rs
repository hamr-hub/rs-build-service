//! 底层 Unix fd/HTTP 辅助（可移植：Linux 与 macOS）。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::RawFd;
use std::time::Duration;

/// 创建一对 AF_UNIX 双向流：supervisor 一端，子进程 fd5 一端。
pub fn socketpair() -> std::io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    for fd in fds {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    }
    Ok((fds[0], fds[1]))
}

/// 把 fd 复制到 ≥100 的临时位置（带 CLOEXEC），供 pre_exec 内安全搬运。
pub fn fcntl_dupfd(fd: RawFd) -> std::io::Result<RawFd> {
    let new_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 100) };
    if new_fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(new_fd)
    }
}

/// 请求探针 /healthz：200=已 warm，503=预热中，其余=错误。
pub fn probe_health(port: u16) -> anyhow::Result<bool> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.write_all(b"GET /healthz HTTP/1.0\r\nConnection: close\r\n\r\n")?;
    // Half-close 写端，服务端排空可立即读到 EOF，避免其带未读数据关闭发 RST。
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let status = response
        .iter()
        .position(|b| *b == b' ')
        .map(|i| String::from_utf8_lossy(&response[i + 1..i + 4]).to_string())
        .unwrap_or_default();
    match status.as_str() {
        "200" => Ok(true),
        "503" => Ok(false),
        other => anyhow::bail!("unexpected probe status: {other}"),
    }
}

/// 给 AF_UNIX socket 设置接收超时（File 没有 set_read_timeout）。
pub fn set_socket_read_timeout(fd: RawFd, timeout: Duration) -> std::io::Result<()> {
    let tv = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&tv as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// 复制进来的二进制补回可执行位。
pub fn make_executable(path: &std::path::Path) -> std::io::Result<()> {
    let cstr = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::other("invalid path"))?;
    let rc = unsafe { libc::chmod(cstr.as_ptr(), 0o755) };
    if rc != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// 部署 id：时间戳 + pid（避免引 uuid 依赖）。
pub fn deploy_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("dpl-{ms}-{}", std::process::id())
}
