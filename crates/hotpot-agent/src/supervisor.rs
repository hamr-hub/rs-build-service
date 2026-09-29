//! 部署 supervisor：永久持有监听 socket，驱动 Fetch→Preflight→Arm→
//! Attach→Drain→Commit 状态机，point-of-no-return（Drain）前失败自动回退。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::release::{AgentState, Phase, Release};
use crate::unixio;

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(15);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(8);
const CONTROL_READ_TIMEOUT: Duration = Duration::from_millis(500);
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

static AGENT_SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// 请求 agent 自身退出（控制循环轮询）。
pub fn request_agent_shutdown() {
    AGENT_SHUTDOWN.store(true, Ordering::SeqCst);
}

/// 一个运行中的子进程及其交接资源。
struct ChildProc {
    child: Child,
    release: Release,
    control: std::fs::File,
    probe_port: u16,
}

impl ChildProc {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn signal(&self, sig: libc::c_int) {
        unsafe { libc::kill(self.pid() as libc::pid_t, sig) };
    }

    /// 等待退出，超时后 SIGKILL 兜底。
    fn wait_or_kill(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let start = Instant::now();
        loop {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            if start.elapsed() > timeout {
                self.signal(libc::SIGKILL);
                self.child.wait()?;
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn send(&mut self, msg: &[u8]) -> std::io::Result<()> {
        self.control.write_all(msg)?;
        self.control.flush()
    }

    fn recv_line(&mut self) -> std::io::Result<String> {
        unixio::set_socket_read_timeout(self.control.as_raw_fd(), CONTROL_READ_TIMEOUT)?;
        let mut line = String::new();
        let mut byte = [0u8; 1];
        loop {
            match self.control.read(&mut byte) {
                Ok(0) => return Err(std::io::Error::other("control closed")),
                Ok(_) if byte[0] == b'\n' => return Ok(line),
                Ok(_) => line.push(byte[0] as char),
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    if !self.is_alive() {
                        return Err(std::io::Error::other("child exited"));
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

pub struct Supervisor {
    listener: TcpListener,
    data_dir: PathBuf,
    state_path: PathBuf,
    state: AgentState,
    active: Option<ChildProc>,
}

impl Supervisor {
    pub fn new(data_dir: &Path, listen: &str) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir.join("versions"))?;
        let listener = TcpListener::bind(listen)?;
        let state_path = data_dir.join("state.json");
        let state =
            AgentState::load(&state_path)?.unwrap_or_else(|| AgentState::new(listen.into()));
        Ok(Self {
            listener,
            data_dir: data_dir.to_path_buf(),
            state_path,
            state,
            active: None,
        })
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    fn save(&self) -> anyhow::Result<()> {
        self.state.save(&self.state_path)
    }

    /// 首次安装：固化二进制并写入 current，但不启动——启动统一由
    /// [`Supervisor::bootstrap`] 完成，避免初始版本被重复 spawn 出
    /// 无人管理、仍在接流的幽灵进程。
    pub fn install_initial(&mut self, mut release: Release) -> anyhow::Result<()> {
        let staged = self.stage(&release)?;
        release.path = staged;
        self.state.current = Some(release);
        self.state.phase = Phase::Idle;
        self.save()
    }

    /// 启动/恢复当前版本（无旧版本可保护，直接 Preflight→Arm→Commit）。
    pub fn bootstrap(&mut self) -> anyhow::Result<()> {
        let Some(release) = self.state.current.clone() else {
            return Ok(());
        };
        let mut child = self.spawn_child(&release)?;
        self.wait_warm(&mut child)?;
        self.arm(&mut child)?;
        child.send(b"COMMIT\n")?;
        self.active = Some(child);
        self.state.phase = Phase::Idle;
        self.save()
    }

    /// 部署新版本；Drain 前任何失败都终止新子进程，旧版本继续服务。
    pub fn deploy(&mut self, release: Release) -> anyhow::Result<()> {
        // Fetch：复制不可变副本；已在 versions/ 下（如 rollback）则原样使用。
        let staged = if release.path.starts_with(&self.data_dir) {
            release.path.clone()
        } else {
            self.stage(&release)?
        };
        let release = Release {
            path: staged,
            ..release
        };

        self.state.phase = Phase::Preflighting;
        self.state.deploy_id = Some(unixio::deploy_id());
        self.state.last_error = None;
        self.save()?;

        let mut new_child = self.spawn_child(&release)?;
        if let Err(e) = self.wait_warm(&mut new_child) {
            // Preflight 失败：point-of-no-return 之前，自动回退（杀新保旧）。
            new_child.signal(libc::SIGKILL);
            let _ = new_child.child.wait();
            self.state.phase = Phase::Failed;
            self.state.last_error = Some(e.to_string());
            self.save()?;
            return Err(e);
        }

        // Arm：打开真实端口接流。
        if let Err(e) = self.arm(&mut new_child) {
            new_child.signal(libc::SIGKILL);
            let _ = new_child.child.wait();
            self.state.phase = Phase::Failed;
            self.state.last_error = Some(e.to_string());
            self.save()?;
            return Err(e);
        }
        self.state.phase = Phase::Armed;
        self.save()?;

        // Attach 后再确认一次健康，然后 Drain 旧版本（point-of-no-return）。
        if let Err(e) = self.wait_warm(&mut new_child) {
            self.state.phase = Phase::Failed;
            self.state.last_error = Some(e.to_string());
            self.save()?;
            return Err(e);
        }

        self.state.phase = Phase::Draining;
        self.save()?;
        let old_release = self.active.as_ref().map(|c| c.release.clone());
        if let Some(mut old) = self.active.take() {
            old.signal(libc::SIGTERM);
            old.wait_or_kill(DRAIN_TIMEOUT)?;
        }

        // Drain 期间新版本异常 → 尽力回滚到旧版本并报告。
        if !new_child.is_alive() {
            self.state.phase = Phase::Failed;
            self.state.last_error = Some("new release died during drain".into());
            self.save()?;
            if let Some(old) = old_release {
                let _ = self.deploy(old);
            }
            anyhow::bail!("new release died during drain");
        }

        // Commit：切换 current/previous，关闭新版本探针。
        new_child.send(b"COMMIT\n")?;
        self.state.previous = old_release;
        self.state.current = Some(release);
        self.state.phase = Phase::Committed;
        self.save()?;
        self.state.phase = Phase::Idle;
        self.save()?;
        self.active = Some(new_child);
        Ok(())
    }

    /// Rollback = 对 previous 复用同一部署状态机。
    pub fn rollback(&mut self) -> anyhow::Result<()> {
        let previous = self
            .state
            .previous
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no previous release to roll back to"))?;
        self.deploy(previous)
    }

    /// 优雅停止当前子进程（agent 退出时）。
    pub fn shutdown_child(&mut self) {
        if let Some(mut child) = self.active.take() {
            child.signal(libc::SIGTERM);
            let _ = child.wait_or_kill(DRAIN_TIMEOUT);
        }
    }

    pub fn agent_shutting_down(&self) -> bool {
        AGENT_SHUTDOWN.load(Ordering::SeqCst)
    }

    // ---------- 内部步骤 ----------

    /// Fetch：把二进制复制到 versions/<name>。
    fn stage(&self, release: &Release) -> anyhow::Result<PathBuf> {
        let dest = self
            .data_dir
            .join("versions")
            .join(release.version.replace('/', "_"));
        std::fs::copy(&release.path, &dest)?;
        unixio::make_executable(&dest)?;
        Ok(dest)
    }

    /// 启动子进程并 dup2 注入 fd3/4/5。
    fn spawn_child(&self, release: &Release) -> anyhow::Result<ChildProc> {
        let probe = TcpListener::bind("127.0.0.1:0")?;
        let probe_port = probe.local_addr()?.port();
        let (parent_fd, child_fd) = unixio::socketpair()?;

        let listen_fd = self.listener.as_raw_fd();
        let probe_fd = probe.as_raw_fd();

        let mut cmd = Command::new(&release.path);
        cmd.env("HOTPOT_LISTEN_FDS", "1")
            .envs(release.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        unsafe {
            cmd.pre_exec(move || {
                // 先复制到高位 fd，避免源 fd 与目标 3/4/5 重叠。
                let l = unixio::fcntl_dupfd(listen_fd)?;
                let p = unixio::fcntl_dupfd(probe_fd)?;
                let cw = unixio::fcntl_dupfd(child_fd)?;
                libc::dup2(l, crate::activate::FD_LISTEN);
                libc::dup2(p, crate::activate::FD_PROBE);
                libc::dup2(cw, crate::activate::FD_CONTROL);
                for fd in [l, p, cw] {
                    libc::close(fd);
                }
                for fd in [
                    crate::activate::FD_LISTEN,
                    crate::activate::FD_PROBE,
                    crate::activate::FD_CONTROL,
                ] {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;

        // 父端 socket：关闭子端，持有的一端包装为 File。
        unsafe { libc::close(child_fd) };
        let control = unsafe { std::fs::File::from_raw_fd(parent_fd) };

        Ok(ChildProc {
            child,
            release: release.clone(),
            control,
            probe_port,
        })
    }

    /// Preflight：轮询探针 /healthz，503=预热中，200=warm。
    fn wait_warm(&self, child: &mut ChildProc) -> anyhow::Result<()> {
        let deadline = Instant::now() + PREFLIGHT_TIMEOUT;
        loop {
            if unixio::probe_health(child.probe_port).unwrap_or(false) {
                return Ok(());
            }
            if !child.is_alive() {
                anyhow::bail!("child exited during preflight");
            }
            if Instant::now() > deadline {
                anyhow::bail!("preflight timed out after {PREFLIGHT_TIMEOUT:?}");
            }
            std::thread::sleep(PROBE_INTERVAL);
        }
    }

    /// Arm：发 ARM，等子进程应答 ARMED。
    fn arm(&self, child: &mut ChildProc) -> anyhow::Result<()> {
        child.send(b"ARM\n")?;
        let reply = child.recv_line()?;
        if reply != "ARMED" {
            anyhow::bail!("unexpected arm reply: {reply}");
        }
        Ok(())
    }
}
