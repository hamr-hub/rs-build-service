//! 服务配置：TOML 文件 + 环境变量覆盖（环境变量覆盖在各入口处理）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Hotpot 服务端配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// HTTP 监听地址。
    pub listen: String,
    /// 数据根目录（元数据库、缓存、会话工作区）。
    pub data_dir: PathBuf,
    /// 同进程内启动的 worker 数（0 = 按 CPU 核数）。
    pub workers: u32,
    /// 单构建默认超时（秒）。
    pub build_timeout_secs: u64,
    /// 成功构建的会话目录保留时长（秒）；0 = 用默认值。
    pub session_max_age_secs: u64,
    /// 失败/超时构建的会话目录保留时长（秒）；0 = 用默认值。
    pub session_failed_max_age_secs: u64,
    /// 额外保留最近 N 个终态构建的会话目录；0 = 不按数量保留。
    pub session_keep_last: u64,
    /// 后台资源回收（会话目录 + git 工作区 + CAS 容量）扫描间隔（秒）；0 = 用默认值。
    pub gc_interval_secs: u64,
    /// git 共享工作区最长保留时长（秒）；0 = 用默认值。
    pub git_workspace_max_age_secs: u64,
    pub cache: CacheConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7878".to_string(),
            data_dir: PathBuf::from("./hotpot-data"),
            workers: 0,
            build_timeout_secs: 1800,
            session_max_age_secs: 0,
            session_failed_max_age_secs: 0,
            session_keep_last: 0,
            gc_interval_secs: 0,
            git_workspace_max_age_secs: 0,
            cache: CacheConfig::default(),
        }
    }
}

/// 缓存配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// 本地 CAS 最大容量（字节），0 为不限。
    pub max_bytes: u64,
    /// 写入时是否以 zstd 压缩（已压缩内容自动跳过由存储层判断）。
    pub compression: bool,
    /// 远程对象存储 endpoint（S3 兼容），None 时仅本地。
    pub remote_endpoint: Option<String>,
    pub remote_bucket: Option<String>,
    /// 自身构建是否通过 sccache WebDAV 协议复用 crate 级缓存（闭环加速）。
    pub self_sccache: bool,
    /// sccache 远端缓存地址（含 `/sccache` 前缀）。None 时按监听地址推导。
    pub sccache_webdav_url: Option<String>,
    /// sccache 本地目录（宿主）；None 时用 `<data_dir>/sccache`。
    pub sccache_dir: Option<PathBuf>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: 10 * 1024 * 1024 * 1024,
            compression: true,
            remote_endpoint: None,
            remote_bucket: None,
            self_sccache: false,
            sccache_webdav_url: None,
            sccache_dir: None,
        }
    }
}

impl ServerConfig {
    /// 自身构建使用的 sccache 远端地址：显式配置优先，否则由监听地址推导。
    ///
    /// 推导规则：`http://<listen>/sccache`。仅监听 `127.0.0.1` 时，
    /// docker 执行器内的构建容器通常**无法**访问该地址，需显式配置
    /// （如 `http://host.docker.internal:7878/sccache`）。
    pub fn sccache_webdav_url(&self) -> String {
        self.cache
            .sccache_webdav_url
            .clone()
            .unwrap_or_else(|| format!("http://{}/sccache", self.listen))
            .trim_end_matches('/')
            .to_string()
    }

    /// sccache 本地目录。
    pub fn sccache_dir(&self) -> PathBuf {
        self.cache
            .sccache_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("sccache"))
    }
}

/// 0 视为「未设置」，回落到 `fallback`。
pub fn secs_or(value: u64, fallback: std::time::Duration) -> std::time::Duration {
    if value == 0 {
        fallback
    } else {
        std::time::Duration::from_secs(value)
    }
}
