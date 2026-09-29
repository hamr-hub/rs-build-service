//! AF_UNIX 控制通道：deploy / rollback / status / shutdown（单行 JSON）。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::release::{AgentState, Release};
use crate::supervisor::Supervisor;

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Command {
    Deploy {
        path: PathBuf,
        version: String,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Rollback,
    Status,
    Shutdown,
}

#[derive(Debug, Serialize)]
struct Reply {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<AgentState>,
}

impl Reply {
    fn ok(state: AgentState) -> Self {
        Self {
            ok: true,
            error: None,
            state: Some(state),
        }
    }

    fn err(error: String) -> Self {
        Self {
            ok: false,
            error: Some(error),
            state: None,
        }
    }
}

/// 绑定控制 socket（须在 bootstrap 接流量之前完成，避免控制面空窗竞态）。
pub fn bind(socket_path: &Path) -> anyhow::Result<UnixListener> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// 运行控制循环直到 agent 自身被要求关闭。
pub fn serve(supervisor: &mut Supervisor, listener: UnixListener) -> anyhow::Result<()> {
    while !supervisor.agent_shutting_down() {
        match listener.accept() {
            Ok((mut conn, _)) => {
                let reply = handle_conn(supervisor, &mut conn);
                let bytes = serde_json::to_vec(&reply)?;
                let _ = conn.write_all(&bytes);
                let _ = conn.write_all(b"\n");
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn handle_conn(supervisor: &mut Supervisor, conn: &mut std::os::unix::net::UnixStream) -> Reply {
    conn.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let mut bytes = Vec::new();
    match conn.read_to_end(&mut bytes) {
        Ok(_) => {}
        Err(e) => return Reply::err(e.to_string()),
    }
    let command: Command = match serde_json::from_slice(&bytes) {
        Ok(command) => command,
        Err(e) => return Reply::err(e.to_string()),
    };

    let result = match command {
        Command::Deploy { path, version, env } => supervisor.deploy(Release { version, path, env }),
        Command::Rollback => supervisor.rollback(),
        Command::Status => Ok(()),
        Command::Shutdown => {
            crate::supervisor::request_agent_shutdown();
            Ok(())
        }
    };
    match result {
        Ok(()) => Reply::ok(supervisor.state().clone()),
        Err(e) => Reply::err(e.to_string()),
    }
}
