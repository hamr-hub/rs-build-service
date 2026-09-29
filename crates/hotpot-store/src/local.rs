//! 本地磁盘内容寻址存储。

use std::fs;
use std::path::{Path, PathBuf};

use hotpot_core::{ContentDigest, Error, Result};
use tracing::debug;

use crate::BlobStore;

/// 存储文件格式首字节：载荷是否为 zstd 压缩。
const FLAG_RAW: u8 = 0;
const FLAG_ZSTD: u8 = 1;

/// 小于该字节数不压缩（压缩开销不划算）。
const COMPRESS_MIN: usize = 128;

/// 本地存储配置。
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// 容量上限（字节），0 为不限；超限触发 LRU 回收。
    pub max_bytes: u64,
    /// 是否允许 zstd 压缩。
    pub compression: bool,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            max_bytes: 0,
            compression: true,
        }
    }
}

/// 本地磁盘 blob 存储。
#[derive(Clone)]
pub struct LocalStore {
    root: PathBuf,
    objects_dir: PathBuf,
    temp_dir: PathBuf,
    opts: StoreOptions,
}

impl LocalStore {
    /// 在 `root/objects` 下打开（必要时创建）存储。
    pub fn open(root: impl AsRef<Path>, opts: StoreOptions) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let objects_dir = root.join("objects");
        let temp_dir = root.join("tmp");
        fs::create_dir_all(&objects_dir)?;
        fs::create_dir_all(&temp_dir)?;
        Ok(Self {
            root,
            objects_dir,
            temp_dir,
            opts,
        })
    }

    fn object_path(&self, digest: &ContentDigest) -> PathBuf {
        let hex = digest.to_hex();
        self.objects_dir.join(&hex[..2]).join(&hex[2..])
    }

    fn encode(&self, data: &[u8]) -> Vec<u8> {
        if self.opts.compression && data.len() >= COMPRESS_MIN && !is_already_compressed(data) {
            let mut out = Vec::with_capacity(data.len() / 2);
            out.push(FLAG_ZSTD);
            zstd::stream::copy_encode(data, &mut out, 3).expect("in-memory zstd encode");
            out
        } else {
            let mut out = Vec::with_capacity(data.len() + 1);
            out.push(FLAG_RAW);
            out.extend_from_slice(data);
            out
        }
    }

    fn decode(&self, stored: &[u8]) -> Result<Vec<u8>> {
        let (&flag, payload) = stored
            .split_first()
            .ok_or_else(|| Error::Storage("empty object file".into()))?;
        match flag {
            FLAG_RAW => Ok(payload.to_vec()),
            FLAG_ZSTD => {
                let mut out = Vec::new();
                zstd::stream::copy_decode(payload, &mut out)
                    .map_err(|e| Error::Storage(format!("zstd decode: {e}")))?;
                Ok(out)
            }
            other => Err(Error::Storage(format!("unknown object flag: {other}"))),
        }
    }

    /// 当前已存储对象占用字节（存储文件口径）。
    pub fn stored_bytes(&self) -> Result<u64> {
        let mut total = 0;
        for entry in walkdir::WalkDir::new(&self.objects_dir) {
            let entry = entry.map_err(std::io::Error::other)?;
            if entry.file_type().is_file() {
                total += entry.metadata().map_err(std::io::Error::other)?.len();
            }
        }
        Ok(total)
    }

    /// LRU 回收：删除最久未访问的对象直到容量不超过上限。
    /// 返回释放的字节数。
    pub fn gc(&self) -> Result<u64> {
        if self.opts.max_bytes == 0 {
            return Ok(0);
        }
        let mut total = self.stored_bytes()?;
        if total <= self.opts.max_bytes {
            return Ok(0);
        }

        let mut files: Vec<(u64, std::time::SystemTime, PathBuf)> = Vec::new();
        for entry in walkdir::WalkDir::new(&self.objects_dir) {
            let entry = entry.map_err(std::io::Error::other)?;
            if !entry.file_type().is_file() {
                continue;
            }
            let meta = entry.metadata().map_err(std::io::Error::other)?;
            // APFS/常见 Linux 的 atime 有 lazy/relatime 语义，实测顺序会反转，
            // 不能作为 LRU 依据。用创建时间（rename 保留，纳秒精度），
            // 退化到 mtime。
            let when = meta.created().or_else(|_| meta.modified())?;
            files.push((meta.len(), when, entry.into_path()));
        }
        // LRU 相同时按路径确定性打破平局（语义上同级，只保证 GC 结果可复现）。
        files.sort_by(|(_, a_when, a_path), (_, b_when, b_path)| {
            a_when.cmp(b_when).then_with(|| a_path.cmp(b_path))
        });

        let mut freed = 0;
        for (size, _, path) in files {
            if total <= self.opts.max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total -= size;
                freed += size;
            }
        }
        debug!("LRU gc freed {} bytes", freed);
        Ok(freed)
    }

    /// 数据根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl BlobStore for LocalStore {
    fn put(&self, data: &[u8]) -> Result<ContentDigest> {
        let digest = ContentDigest::of_bytes(data);
        let target = self.object_path(&digest);
        if target.exists() {
            return Ok(digest);
        }

        let encoded = self.encode(data);
        fs::create_dir_all(target.parent().expect("object has parent"))?;

        // 临时文件 + 原子 rename，杜绝读到写了一半的对象。
        fs::create_dir_all(&self.temp_dir)?;
        let temp = self.temp_dir.join(format!("{}.tmp", digest.to_hex()));
        fs::write(&temp, &encoded)?;
        fs::rename(&temp, &target)?;

        if self.opts.max_bytes > 0 {
            self.gc()?;
        }
        Ok(digest)
    }

    fn get(&self, digest: &ContentDigest) -> Result<Option<Vec<u8>>> {
        let path = self.object_path(digest);
        match fs::read(&path) {
            Ok(stored) => {
                let data = self.decode(&stored)?;
                let actual = ContentDigest::of_bytes(&data);
                if actual != *digest {
                    return Err(Error::DigestMismatch {
                        expected: digest.to_hex(),
                        actual: actual.to_hex(),
                    });
                }
                Ok(Some(data))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn contains(&self, digest: &ContentDigest) -> Result<bool> {
        Ok(self.object_path(digest).exists())
    }

    fn delete(&self, digest: &ContentDigest) -> Result<bool> {
        match fs::remove_file(self.object_path(digest)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// 魔数启发式：已压缩/已编码内容不再 zstd（收益微小甚至变大）。
fn is_already_compressed(data: &[u8]) -> bool {
    matches!(
        data.get(..4),
        Some([0x1f, 0x8b, ..])           // gzip
        | Some([0x28, 0xb5, 0x2f, 0xfd]) // zstd
        | Some([0x50, 0x4b, 0x03, 0x04]) // zip
        | Some([0x89, 0x50, 0x4e, 0x47]) // png
        | Some([0x4d, 0x53, 0x43, 0x46]) // CAB
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> LocalStore {
        LocalStore::open(tempfile::tempdir().unwrap(), StoreOptions::default()).unwrap()
    }

    #[test]
    fn put_get_roundtrip() {
        let s = store();
        let d = s.put(b"hello hotpot").unwrap();
        assert!(s.contains(&d).unwrap());
        assert_eq!(s.get(&d).unwrap().unwrap(), b"hello hotpot");
    }

    #[test]
    fn dedups_same_content() {
        let s = store();
        let d1 = s.put(b"same").unwrap();
        let d2 = s.put(b"same").unwrap();
        assert_eq!(d1, d2);
        s.delete(&d1).unwrap();
        assert!(!s.contains(&d2).unwrap());
    }

    #[test]
    fn missing_is_none() {
        let s = store();
        let d = ContentDigest::of_bytes(b"nope");
        assert!(s.get(&d).unwrap().is_none());
    }

    #[test]
    fn compresses_large_payload() {
        let s = LocalStore::open(
            tempfile::tempdir().unwrap(),
            StoreOptions {
                max_bytes: 0,
                compression: true,
            },
        )
        .unwrap();
        let payload = vec![b'a'; 4096];
        let d = s.put(&payload).unwrap();
        assert_eq!(s.get(&d).unwrap().unwrap(), payload);
        // 压缩实际生效：存储文件显著小于原文。
        assert!(s.stored_bytes().unwrap() < 1024);
    }

    #[test]
    fn lru_eviction_respects_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let s = LocalStore::open(
            &dir,
            StoreOptions {
                max_bytes: 300,
                compression: false,
            },
        )
        .unwrap();
        let first = s.put(&vec![b'1'; 200][..]).unwrap();
        // 确保两个对象 LRU 排名不同（不依赖文件系统时间戳精度）。
        std::thread::sleep(std::time::Duration::from_millis(10));
        // 第二次写入触发 GC；容量只容得下后写入对象。
        let second = s.put(&vec![b'2'; 200][..]).unwrap();
        assert!(!s.contains(&first).unwrap());
        assert!(s.contains(&second).unwrap());
    }
}
