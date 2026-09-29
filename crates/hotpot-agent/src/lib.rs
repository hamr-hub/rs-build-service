//! hotpot-agent：零停机部署 supervisor。
//!
//! - supervisor 永久持有监听 socket（中立托管，等价 systemd 角色），
//!   不依赖 systemd，Linux/macOS 均可运行；
//! - 子进程经固定 fd（3 监听 / 4 探针 / 5 控制）激活，应用侧见 [`activate`]。

pub mod activate;
pub mod control;
pub mod release;
pub mod supervisor;
pub mod unixio;
