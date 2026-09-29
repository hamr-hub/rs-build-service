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
    pub cache: CacheConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7878".to_string(),
            data_dir: PathBuf::from("./hotpot-data"),
            workers: 0,
            build_timeout_secs: 1800,
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
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: 10 * 1024 * 1024 * 1024,
            compression: true,
            remote_endpoint: None,
            remote_bucket: None,
        }
    }
}
