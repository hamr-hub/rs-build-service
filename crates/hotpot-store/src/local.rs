//! 本地磁盘内容寻址存储。

use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

use hotpot_core::{ContentDigest, Error, Hasher, Result};
use tracing::debug;

use crate::BlobStore;

/// 存储文件格式首字节：载荷是否为 zstd 压缩。
const FLAG_RAW: u8 = 0;
const FLAG_ZSTD: u8 = 1;

/// 流式写入的临时文件序号（配合 pid，保证并发写不互相覆盖）。
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

    /// 流式写入：边读边算摘要、（按需）压缩，**不在内存里拼整个对象**。
    ///
    /// 为什么需要它：非流式 `put(&[u8])` 在写入路径上会同时持有
    /// 「原始字节 + 压缩后字节」两份拷贝。对 GB 级 artifact 来说，
    /// 一个上传请求就能吃掉数倍于自身大小的内存——这正是缓存端点
    /// 在加上 body 上限之前的老问题。
    ///
    /// 正确性：摘要在数据落盘**之前**就算完，只有校验通过才 rename 到
    /// 正式路径，因此**不存在「写了一半却被读到」或「内容与 key 不符」的
    /// 可见状态**。临时文件名含 pid + 计数器，避免并发写互相覆盖。
    ///
    /// 压缩决策与 `encode()` 保持一致（`>= COMPRESS_MIN` 且非已压缩格式），
    /// 代价是最多多缓冲 `COMPRESS_MIN` 字节——有界，且与既有格式兼容。
    pub fn put_reader<R: Read>(&self, mut reader: R) -> Result<ContentDigest> {
        use std::io::{BufWriter, Write};

        fs::create_dir_all(&self.temp_dir)?;
        let temp = self.temp_dir.join(format!(
            "{}.{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut hasher = ContentDigest::hasher();

        let outcome = (|| -> Result<ContentDigest> {
            let file = fs::File::create(&temp)?;
            let mut writer = BufWriter::with_capacity(128 * 1024, file);

            // 先读满 COMPRESS_MIN 字节用于压缩决策（有界缓冲，堆上分配：
            // blocking 任务的栈比 async 任务小，没必要压 64 KiB 进去）。
            let mut prefix = Vec::with_capacity(COMPRESS_MIN);
            // 刻意保留堆缓冲（见上行注释）；数组会把 64 KiB 压进 blocking 栈。
            #[allow(clippy::useless_vec)]
            let mut buf = vec![0u8; COMPRESS_MIN];
            while prefix.len() < COMPRESS_MIN {
                let n = reader.read(&mut buf[..COMPRESS_MIN - prefix.len()])?;
                if n == 0 {
                    break;
                }
                prefix.extend_from_slice(&buf[..n]);
            }
            hasher.update(&prefix);

            let compress = self.opts.compression
                && prefix.len() >= COMPRESS_MIN
                && !is_already_compressed(&prefix);
            writer.write_all(&[if compress { FLAG_ZSTD } else { FLAG_RAW }])?;

            if compress {
                let mut encoder = zstd::stream::Encoder::new(writer, 3)
                    .map_err(|e| Error::Storage(format!("zstd encoder: {e}")))?;
                encoder.write_all(&prefix)?;
                Self::pump(&mut reader, &mut hasher, |chunk| encoder.write_all(chunk))?;
                let mut writer = encoder
                    .finish()
                    .map_err(|e| Error::Storage(format!("zstd finish: {e}")))?;
                writer
                    .flush()
                    .map_err(|e| Error::Storage(format!("flush: {e}")))?;
            } else {
                writer.write_all(&prefix)?;
                Self::pump(&mut reader, &mut hasher, |chunk| writer.write_all(chunk))?;
                writer
                    .flush()
                    .map_err(|e| Error::Storage(format!("flush: {e}")))?;
            }

            let digest = hasher.finalize();
            let target = self.object_path(&digest);
            if target.exists() {
                // 相同内容已存在：丢弃临时文件即可（内容寻址天然去重）。
                let _ = fs::remove_file(&temp);
                return Ok(digest);
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&temp, &target)?;
            Ok(digest)
        })();

        if outcome.is_err() {
            let _ = fs::remove_file(&temp);
        }
        let digest = outcome?;
        if self.opts.max_bytes > 0 {
            self.gc()?;
        }
        Ok(digest)
    }

    /// 把 reader 剩余内容读干净：同一块数据同时喂给摘要器与 sink。
    fn pump<R: Read, W: FnMut(&[u8]) -> std::io::Result<()>>(
        reader: &mut R,
        hasher: &mut Hasher,
        mut sink: W,
    ) -> Result<()> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf).map_err(Error::Io)?;
            if n == 0 {
                return Ok(());
            }
            hasher.update(&buf[..n]);
            sink(&buf[..n])?;
        }
    }

    /// 对象的**未压缩**逻辑大小；对象不存在返回 None。
    ///
    /// 索引里的 `size` 来自请求声明，不可靠；这个值是实际落盘对象的解压后
    /// 长度，用于校正 `Content-Length`。
    pub fn object_size(&self, digest: &ContentDigest) -> Option<u64> {
        let mut reader = self.reader(digest).ok()??;
        let mut buf = [0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) => return Some(total),
                Ok(n) => total += n as u64,
                Err(_) => return None,
            }
        }
    }

    /// 打开对象读取器：流式解压，并在读到 EOF 时校验摘要。
    ///
    /// 与 `get()` 的差别：内存占用 O(1) 而非 O(对象大小)。代价是
    /// **摘要在 EOF 时才能确认**——若发现不匹配，已经交出去的字节无法收回，
    /// 调用方必须能处理「中途失败」。缓存协议的两端对此都是安全的：
    /// sccache 会把读坏的对象当作 cache miss，turbo 的签名校验会失败。
    pub fn reader(&self, digest: &ContentDigest) -> Result<Option<ObjectReader>> {
        let path = self.object_path(digest);
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(ObjectReader::open(file, *digest)?))
    }
}

/// 流式对象读取器：按需解压 + 边读边校验摘要。
pub struct ObjectReader {
    inner: ReaderKind,
    hasher: Hasher,
    expected: ContentDigest,
    verified: bool,
}

/// 底层读取器：原始字节或 zstd 解压流。
enum ReaderKind {
    Raw(BufReader<std::fs::File>),
    Zstd(Box<zstd::stream::read::Decoder<'static, BufReader<std::fs::File>>>),
}

impl ObjectReader {
    fn open(mut file: std::fs::File, expected: ContentDigest) -> Result<Self> {
        // 标志字节直接从 File 读（1 次 syscall）：若先用 BufReader 读它再把
        // BufReader 交给解码器，已预读进缓冲的字节会被丢掉。
        let mut flag = [0u8; 1];
        file.read_exact(&mut flag)
            .map_err(|e| Error::Storage(format!("read object flag: {e}")))?;
        let inner = match flag[0] {
            FLAG_RAW => ReaderKind::Raw(BufReader::with_capacity(128 * 1024, file)),
            FLAG_ZSTD => {
                // zstd 解码器自带缓冲，不需要再包一层。
                let decoder = zstd::stream::read::Decoder::new(file)
                    .map_err(|e| Error::Storage(format!("zstd decoder: {e}")))?;
                ReaderKind::Zstd(Box::new(decoder))
            }
            other => return Err(Error::Storage(format!("unknown object flag: {other}"))),
        };
        Ok(Self {
            inner,
            hasher: ContentDigest::hasher(),
            expected,
            verified: false,
        })
    }
}

impl Read for ObjectReader {
    /// 读到 EOF 时才校验摘要——流式读取无法在交出字节后收回它们。
    /// 因此不匹配以 `io::Error`（InvalidData）返回，调用方据此中断响应。
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.verified {
            return Ok(0);
        }
        let n = match &mut self.inner {
            ReaderKind::Raw(r) => r.read(buf)?,
            ReaderKind::Zstd(r) => r.read(buf)?,
        };
        if n == 0 {
            let actual = self.hasher.finalize();
            if actual != self.expected {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "content digest mismatch: expected {}, got {}",
                        self.expected, actual
                    ),
                ));
            }
            self.verified = true;
        } else {
            self.hasher.update(&buf[..n]);
        }
        Ok(n)
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
    use std::io::Read;

    /// 流式写入必须与缓冲写入产生**逐字节相同**的对象，否则同一份内容
    /// 会因为写入路径不同而算出不同摘要——那等于把 CAS 变成两个世界。
    fn assert_stream_matches_buffered(data: &[u8]) {
        let dir = tempfile::tempdir().unwrap();
        for opts in [
            StoreOptions {
                max_bytes: 0,
                compression: true,
            },
            StoreOptions {
                max_bytes: 0,
                compression: false,
            },
        ] {
            let a = LocalStore::open(dir.path().join("a"), opts.clone()).unwrap();
            let b = LocalStore::open(dir.path().join("b"), opts).unwrap();
            let via_slice = a.put(data).unwrap();
            let via_stream = b.put_reader(std::io::Cursor::new(data)).unwrap();
            assert_eq!(
                via_slice,
                via_stream,
                "digest mismatch (compress={})",
                opts_flag(&a)
            );
            // 落盘字节也应一致（编码方式相同）。
            assert_eq!(
                fs::read(a.object_path(&via_slice)).unwrap(),
                fs::read(b.object_path(&via_stream)).unwrap()
            );
        }
    }

    fn opts_flag(s: &LocalStore) -> bool {
        s.opts.compression
    }

    #[test]
    fn stream_put_matches_buffered_put() {
        // 覆盖三种编码分支：小于 COMPRESS_MIN（RAW）、可压缩、已压缩格式。
        assert_stream_matches_buffered(b"");
        assert_stream_matches_buffered(b"tiny");
        assert_stream_matches_buffered(&vec![b'x'; 5000]);
        assert_stream_matches_buffered(&b"\x1f\x8b compressed-looking payload".repeat(200));
    }

    #[test]
    fn stream_put_is_deduplicated() {
        let store =
            LocalStore::open(tempfile::tempdir().unwrap(), StoreOptions::default()).unwrap();
        let data = vec![b'y'; 4096];
        let d1 = store.put_reader(std::io::Cursor::new(&data)).unwrap();
        let d2 = store.put_reader(std::io::Cursor::new(&data)).unwrap();
        assert_eq!(d1, d2);
        assert_eq!(d1, ContentDigest::of_bytes(&data));
    }

    #[test]
    fn object_reader_roundtrips_and_verifies() {
        let store =
            LocalStore::open(tempfile::tempdir().unwrap(), StoreOptions::default()).unwrap();
        for data in [b"short".to_vec(), vec![b'z'; 200_000]] {
            let d = store.put(&data).unwrap();
            let mut reader = store.reader(&d).unwrap().expect("object exists");
            let mut out = Vec::new();
            reader.read_to_end(&mut out).unwrap();
            assert_eq!(out, data);
        }
        // 缺失对象返回 None（不是错误）。
        let missing = ContentDigest::of_bytes(b"never stored");
        assert!(store.reader(&missing).unwrap().is_none());
    }

    #[test]
    fn object_reader_detects_corruption_in_raw_objects() {
        let dir = tempfile::tempdir().unwrap();
        // 关掉压缩 → 落盘为 RAW，损坏不会被 zstd 帧校验拦下，
        // 必须由摘要校验兜住。
        let store = LocalStore::open(
            dir.path(),
            StoreOptions {
                max_bytes: 0,
                compression: false,
            },
        )
        .unwrap();
        let data = b"uncompressed payload that will be corrupted".to_vec();
        let d = store.put(&data).unwrap();
        let path = store.object_path(&d);
        let mut raw = fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        fs::write(&path, &raw).unwrap();

        let mut reader = store.reader(&d).unwrap().unwrap();
        let mut out = Vec::new();
        let err = reader.read_to_end(&mut out).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().contains("digest mismatch"), "{err}");
    }

    #[test]
    fn object_reader_reports_compressed_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::open(dir.path(), StoreOptions::default()).unwrap();
        let data = vec![b'q'; 5000];
        let d = store.put(&data).unwrap();
        let path = store.object_path(&d);
        let mut raw = fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        fs::write(&path, &raw).unwrap();

        // zstd 帧自带校验，会先一步发现损坏，并用自己的 io::ErrorKind 报告
        // （不是 InvalidData）。这里只断言**必须报错**——绝不能静默返回坏数据。
        let mut reader = store.reader(&d).unwrap().unwrap();
        let mut out = Vec::new();
        let err = reader
            .read_to_end(&mut out)
            .expect_err("损坏的对象必须报错，不能静默返回");
        assert!(!err.to_string().is_empty());
    }

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
