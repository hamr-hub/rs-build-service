//! hotpot-dev：本地开发的监听—增量构建—热重启循环。
//!
//! - [`watcher`]：notify 监听 + 防抖；
//! - [`builder`]：增量 cargo build（复用既有 target，失败保留旧进程）；
//! - [`process`]：子进程监管（优雅停机 + 强杀兜底）；
//! - [`keeper`]：socket keeper 代理，重启窗口公共端口不拒连；
//! - [`dev`]：把上述组件编排成开发循环。

pub mod builder;
pub mod dev;
pub mod keeper;
pub mod process;
pub mod watcher;
