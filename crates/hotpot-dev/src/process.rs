//! 子进程监管：启动目标二进制（stdio 直通终端），重启时优雅停机并强杀兜底。

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::process::{Child, Command};

/// 默认优雅停机窗口：超时则 SIGKILL。
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// 一个受监管的子进程。
pub struct ManagedProcess {
    child: Child,
}

/// 启动 `binary`，注入 `envs`；stdout/stderr 直通当前终端。
pub fn spawn(binary: &Path, envs: &BTreeMap<String, String>) -> io::Result<ManagedProcess> {
    let mut cmd = Command::new(binary);
    cmd.envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let child = cmd.spawn()?;
    Ok(ManagedProcess { child })
}

impl ManagedProcess {
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// SIGTERM 优雅停机；超时未退则 SIGKILL。
    pub async fn stop(&mut self, timeout: Duration) -> io::Result<()> {
        if let Some(pid) = self.child.id() {
            // 进程可能已退出：ESRCH 可忽略，wait 会立刻回收。
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(result) => result.map(|_| ()),
            Err(_) => {
                let _ = self.child.start_kill();
                self.child.wait().await?;
                Ok(())
            }
        }
    }

    /// 进程是否仍在运行（顺带回收）。
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}
