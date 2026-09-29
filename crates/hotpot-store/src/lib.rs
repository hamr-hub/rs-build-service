//! 内容寻址 blob 存储。
//!
//! M0 提供本地磁盘实现：256 桶目录、写入临时文件后原子 rename、读取时校验
//! 内容摘要、可选 zstd 压缩与基于容量的 LRU 回收。远程（S3/object_store）
//! 分层在后续里程碑接入。

pub mod local;

pub use local::LocalStore;

use hotpot_core::{ContentDigest, Result};

/// Blob 存储抽象。身份是内容摘要，同一内容天然去重；对象不可变。
pub trait BlobStore {
    /// 写入内容，返回其摘要；相同内容只落一份。
    fn put(&self, data: &[u8]) -> Result<ContentDigest>;

    /// 按摘要读取；不存在返回 None；读取时校验内容摘要。
    fn get(&self, digest: &ContentDigest) -> Result<Option<Vec<u8>>>;

    /// 对象是否存在。
    fn contains(&self, digest: &ContentDigest) -> Result<bool>;

    /// 删除对象，返回此前是否存在。
    fn delete(&self, digest: &ContentDigest) -> Result<bool>;
}
