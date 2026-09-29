//! 远程缓存：对外协议不透明字节按 client key 索引，载荷内容寻址入 CAS。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hotpot_core::{ContentDigest, Error, Result};
use hotpot_store::BlobStore;
use hotpot_store::LocalStore;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

/// 协议命名空间。
#[derive(Debug, Clone, Copy)]
pub enum Namespace {
    /// sccache WebDAV 兼容缓存。
    Sccache,
    /// Turborepo v8 远程缓存。
    Turbo,
}

impl Namespace {
    fn as_str(self) -> &'static str {
        match self {
            Namespace::Sccache => "sccache",
            Namespace::Turbo => "turbo",
        }
    }
}

/// 命中条目。
pub struct Entry {
    pub bytes: Vec<u8>,
    /// 上传时携带的 x-artifact-tag（turbo 签名透传）。
    pub tag: Option<String>,
    /// 逻辑字节数（与 `bytes.len()` 一致，供 `Content-Length` 免二次计算）。
    pub size: u64,
    /// Turborepo 观测元数据（缺失时 turbo 把 time saved 记 0，不影响正确性）。
    pub meta: ArtifactMeta,
}

/// 一次缓存写入的元数据。
///
/// 与 body 分开成值对象：写入路径上的参数已经够多了。把「大小 + 签名 tag
/// + turbo 观测头」收成一个结构体，调用点读起来就是一句话的事，也省得每加
///   一个 header 就动一遍函数签名。
#[derive(Debug, Clone, Default)]
pub struct PutMeta {
    /// 请求声明的 `Content-Length`（可能与实际不符，仅作索引参考）。
    pub size: Option<i64>,
    /// 上传时携带的 `x-artifact-tag`（turbo 签名，原样存取）。
    pub tag: Option<String>,
    /// Turborepo 观测元数据。
    pub meta: ArtifactMeta,
}

/// Turborepo v8 随 artifact 携带的元数据。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ArtifactMeta {
    /// `x-artifact-duration`：任务耗时毫秒。
    pub duration_ms: Option<i64>,
    /// `x-artifact-sha`：源码 commit。
    pub sha: Option<String>,
    /// `x-artifact-dirty-hash`：脏工作区摘要。
    pub dirty_hash: Option<String>,
}

/// 远程缓存索引（CAS 在 LocalStore，索引在 SQLite）。
#[derive(Clone)]
pub struct RemoteCache {
    store: LocalStore,
    pool: SqlitePool,
    /// 只读计数器（`/metrics` 暴露）：进程内单调递增，重启归零。
    stats: Arc<CacheCounters>,
}

/// 缓存命中/未命中/写入计数。按命名空间分开统计。
#[derive(Debug, Default)]
pub struct CacheCounters {
    pub sccache_hits: AtomicU64,
    pub sccache_misses: AtomicU64,
    pub sccache_puts: AtomicU64,
    pub turbo_hits: AtomicU64,
    pub turbo_misses: AtomicU64,
    pub turbo_puts: AtomicU64,
}

/// 某个命名空间的计数快照。
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct NamespaceStats {
    pub hits: u64,
    pub misses: u64,
    pub puts: u64,
}

impl NamespaceStats {
    /// 命中率（无任何查询时为 0）。
    pub fn hit_ratio(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

impl CacheCounters {
    /// 取某命名空间计数。
    pub fn get(&self, ns: Namespace) -> NamespaceStats {
        let (hits, misses, puts) = match ns {
            Namespace::Sccache => (&self.sccache_hits, &self.sccache_misses, &self.sccache_puts),
            Namespace::Turbo => (&self.turbo_hits, &self.turbo_misses, &self.turbo_puts),
        };
        NamespaceStats {
            hits: hits.load(Ordering::Relaxed),
            misses: misses.load(Ordering::Relaxed),
            puts: puts.load(Ordering::Relaxed),
        }
    }

    fn count(&self, ns: Namespace, kind: CountKind) {
        let (hits, misses, puts) = match ns {
            Namespace::Sccache => (&self.sccache_hits, &self.sccache_misses, &self.sccache_puts),
            Namespace::Turbo => (&self.turbo_hits, &self.turbo_misses, &self.turbo_puts),
        };
        let cell = match kind {
            CountKind::Hit => hits,
            CountKind::Miss => misses,
            CountKind::Put => puts,
        };
        cell.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
enum CountKind {
    Hit,
    Miss,
    Put,
}

const SCHEMA: &str = include_str!("schema.sql");

/// 读取字符串列。
fn rg_string(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<String> {
    use sqlx::Row;
    row.try_get::<String, _>(column)
        .map_err(|e| Error::Other(format!("read column {column}: {e}")))
}

/// 幂等迁移：为已存在的库补齐后加的列（SQLite 无 `ADD COLUMN IF NOT EXISTS`，
/// 重复执行会报 duplicate column，按错误信息忽略即可）。
async fn migrate(pool: &SqlitePool) -> Result<()> {
    for column in ["duration_ms INTEGER", "sha TEXT", "dirty_hash TEXT"] {
        if let Err(e) = sqlx::query(&format!("ALTER TABLE kv_entries ADD COLUMN {column}"))
            .execute(pool)
            .await
        {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(Error::Other(format!(
                    "migrate kv_entries ({column}): {msg}"
                )));
            }
        }
    }
    Ok(())
}

impl RemoteCache {
    /// 打开索引：`root/cacheproto.db`。
    pub async fn open(root: impl AsRef<Path>, store: LocalStore) -> Result<Self> {
        let path: PathBuf = root.as_ref().join("cacheproto.db");
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePool::connect_with(options)
            .await
            .map_err(|e| Error::Other(format!("open cacheproto db: {e}")))?;
        sqlx::query(SCHEMA)
            .execute(&pool)
            .await
            .map_err(|e| Error::Other(format!("init cacheproto schema: {e}")))?;
        migrate(&pool).await?;
        Ok(Self {
            store,
            pool,
            stats: Arc::new(CacheCounters::default()),
        })
    }

    /// 某命名空间的命中/未命中/写入计数（供 `/metrics` 暴露）。
    pub fn stats(&self, ns: Namespace) -> NamespaceStats {
        self.stats.get(ns)
    }

    /// 索引中某命名空间的条目数与逻辑字节数。
    pub async fn footprint(&self, ns: Namespace) -> Result<(u64, u64)> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS n, COALESCE(SUM(size), 0) AS bytes FROM kv_entries \
             WHERE namespace = ?1",
        )
        .bind(ns.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| Error::Other(format!("kv footprint: {e}")))?;
        use sqlx::Row;
        let n: i64 = row.try_get("n").unwrap_or(0);
        let bytes: i64 = row.try_get("bytes").unwrap_or(0);
        Ok((n.max(0) as u64, bytes.max(0) as u64))
    }

    /// 写入条目。相同 (namespace, tenant, key) 已存在时**不覆盖 body**，但会
    /// 补齐缺失的 tag / 元数据。
    ///
    /// 这是**按 key 幂等**而非 WebDAV 覆盖语义：同 key 二次写入的**不同内容**会被
    /// 丢弃。对 sccache / turbo 的 body 无害（key 本身就是内容哈希），但补齐
    /// tag 是必需的——否则「先无签名上传、后启用签名」的客户端在下载时会因
    /// `ArtifactTagMissing` **硬错误**（而非 cache miss）。
    ///
    /// 不变量：body 与 tag 必须配对落库，GET 必须回显与所服务 body 配对的 tag。
    /// 这里遵守它：body 首次写入即固定，tag 只在缺失时补齐，永不与 body 错配。
    pub async fn put(
        &self,
        ns: Namespace,
        key: &str,
        tenant: &str,
        bytes: &[u8],
        tag: Option<String>,
        meta: ArtifactMeta,
    ) -> Result<ContentDigest> {
        if let Some(existing) = self.lookup(ns, key, tenant).await? {
            self.backfill_meta(ns, key, tenant, tag, meta).await?;
            return Ok(existing);
        }
        self.stats.count(ns, CountKind::Put);
        let digest = self.store.put(bytes)?;
        sqlx::query(
            "INSERT INTO kv_entries (namespace, tenant, cache_key, digest, size, tag, \
             duration_ms, sha, dirty_hash, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(ns.as_str())
        .bind(tenant)
        .bind(key)
        .bind(digest.to_hex())
        .bind(bytes.len() as i64)
        .bind(tag)
        .bind(meta.duration_ms)
        .bind(meta.sha)
        .bind(meta.dirty_hash)
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Other(format!("insert kv entry: {e}")))?;
        Ok(digest)
    }

    /// 流式写入：body 从 reader 读入，内容寻址入 CAS 后再登记索引。
    ///
    /// 为什么需要它：缓冲版 `put` 要求调用方先把整个 body 读进内存，
    /// 而一次上传会同时持有「请求缓冲 + CAS 编码缓冲」，GB 级 artifact
    /// 的内存峰值可达自身大小的数倍。缓存端点的体积上限只防住了
    /// 「无限大」，没防住「几倍大」。
    ///
    /// 幂等语义与 `put` 一致：同 key 已存在时不覆盖 body，只补齐 tag/元数据。
    pub async fn put_reader<R: std::io::Read + Send + 'static>(
        &self,
        ns: Namespace,
        key: &str,
        tenant: &str,
        reader: R,
        put: PutMeta,
    ) -> Result<ContentDigest> {
        // 两条分支都要「消费 body」，其中一条是丢弃；用 Option 表达这一点，
        // 而不是靠分支里的 move 体操。
        let mut reader = Some(reader);
        let PutMeta { size, tag, meta } = put;
        // 已有条目：body 不动，只补齐缺失的 tag / 元数据。
        if let Some(existing) = self.lookup(ns, key, tenant).await? {
            // **必须先把请求体消费完**再应答。提前返回会让客户端在仍在上传时
            // 收到响应并判定为传输失败：curl 报传输错误，而 turbo 会在连接错误
            // 时**重试 PUT**，于是每次重试都再触发一次「提前应答」——把一次幂等
            // 命中放大成持续的失败流量。
            //
            // 丢弃也要放到 blocking 线程里：`BodyReader::read` 内部是
            // `blocking_recv`，在 async 线程上调用会 panic。
            if let Some(reader) = reader.take() {
                let _ = tokio::task::spawn_blocking(move || {
                    let mut reader = reader;
                    if let Err(e) = std::io::copy(&mut reader, &mut std::io::sink()) {
                        tracing::debug!("drain request body for existing key failed: {e}");
                    }
                })
                .await;
            }
            self.backfill_meta(ns, key, tenant, tag, meta).await?;
            return Ok(existing);
        }
        let reader = reader
            .take()
            .expect("reader is consumed on exactly one path");

        // 大对象在 spawn_blocking 里写：zstd + 磁盘 IO 都是同步的，
        // 绝不能占住 async worker 线程。
        let store = self.store.clone();
        let digest = tokio::task::spawn_blocking(move || store.put_reader(reader))
            .await
            .map_err(|e| Error::Other(format!("put_reader task panicked: {e}")))??;

        self.stats.count(ns, CountKind::Put);
        let logical_size =
            size.unwrap_or_else(|| self.store.object_size(&digest).unwrap_or(0) as i64);
        sqlx::query(
            "INSERT OR REPLACE INTO kv_entries (namespace, tenant, cache_key, digest, size, tag, \
             duration_ms, sha, dirty_hash, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(ns.as_str())
        .bind(tenant)
        .bind(key)
        .bind(digest.to_hex())
        .bind(logical_size)
        .bind(tag)
        .bind(meta.duration_ms)
        .bind(meta.sha)
        .bind(meta.dirty_hash)
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Other(format!("insert kv entry: {e}")))?;
        Ok(digest)
    }

    /// 只查索引拿到条目的逻辑大小（未命中 None），不读对象内容。
    ///
    /// 用于 HEAD：需要给出真实 `Content-Length`，但没必要把对象读进内存。
    pub async fn peek(&self, ns: Namespace, key: &str, tenant: &str) -> Result<Option<u64>> {
        let row = sqlx::query(
            "SELECT size FROM kv_entries WHERE namespace = ?1 AND tenant = ?2 AND cache_key = ?3",
        )
        .bind(ns.as_str())
        .bind(tenant)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Other(format!("peek kv: {e}")))?;
        let Some(row) = row else {
            return Ok(None);
        };
        use sqlx::Row;
        let size: i64 = row.try_get("size").unwrap_or(0);
        Ok(Some(size.max(0) as u64))
    }

    /// 打开条目用于流式读取（未命中 None）。
    ///
    /// 调用方拿到的是 store 层的 `ObjectReader`：内存 O(1)，
    /// 摘要在 EOF 校验。`size` 来自索引，用于直接填 `Content-Length`，
    /// 无需把对象读进内存就能算出来。
    pub async fn open_entry(
        &self,
        ns: Namespace,
        key: &str,
        tenant: &str,
    ) -> Result<
        Option<(
            Box<dyn std::io::Read + Send>,
            u64,
            Option<String>,
            ArtifactMeta,
        )>,
    > {
        let row = sqlx::query(
            "SELECT digest, size, tag, duration_ms, sha, dirty_hash FROM kv_entries \
             WHERE namespace = ?1 AND tenant = ?2 AND cache_key = ?3",
        )
        .bind(ns.as_str())
        .bind(tenant)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Other(format!("lookup entry: {e}")))?;
        let Some(row) = row else {
            self.stats.count(ns, CountKind::Miss);
            return Ok(None);
        };
        use sqlx::Row;
        let digest = ContentDigest::from_hex(&rg_string(&row, "digest")?)
            .map_err(|e| Error::Other(format!("bad digest in index: {e}")))?;
        let size: i64 = row.try_get("size").unwrap_or(0);
        let tag: Option<String> = row.try_get("tag").unwrap_or(None);
        let meta = ArtifactMeta {
            duration_ms: row.try_get("duration_ms").unwrap_or(None),
            sha: row.try_get("sha").unwrap_or(None),
            dirty_hash: row.try_get("dirty_hash").unwrap_or(None),
        };
        match self.store.reader(&digest)? {
            Some(reader) => {
                self.stats.count(ns, CountKind::Hit);
                Ok(Some((Box::new(reader), size.max(0) as u64, tag, meta)))
            }
            // 索引指向的 CAS 对象丢失：清悬挂索引并按未命中处理。
            None => {
                self.forget(ns, key, tenant).await?;
                self.stats.count(ns, CountKind::Miss);
                Ok(None)
            }
        }
    }

    /// 已有条目：仅用 COALESCE 补齐缺失的 tag / 元数据，不改动 body 与 digest。
    async fn backfill_meta(
        &self,
        ns: Namespace,
        key: &str,
        tenant: &str,
        tag: Option<String>,
        meta: ArtifactMeta,
    ) -> Result<()> {
        if tag.is_none()
            && meta.duration_ms.is_none()
            && meta.sha.is_none()
            && meta.dirty_hash.is_none()
        {
            return Ok(());
        }
        sqlx::query(
            "UPDATE kv_entries SET \
               tag = COALESCE(tag, ?4), \
               duration_ms = COALESCE(duration_ms, ?5), \
               sha = COALESCE(sha, ?6), \
               dirty_hash = COALESCE(dirty_hash, ?7) \
             WHERE namespace = ?1 AND tenant = ?2 AND cache_key = ?3",
        )
        .bind(ns.as_str())
        .bind(tenant)
        .bind(key)
        .bind(tag)
        .bind(meta.duration_ms)
        .bind(meta.sha)
        .bind(meta.dirty_hash)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Other(format!("backfill kv meta: {e}")))?;
        Ok(())
    }

    /// 读取条目（未命中 None）。
    pub async fn get(&self, ns: Namespace, key: &str, tenant: &str) -> Result<Option<Entry>> {
        let Some(digest) = self.lookup(ns, key, tenant).await? else {
            self.stats.count(ns, CountKind::Miss);
            return Ok(None);
        };
        let Some(bytes) = self.store.get(&digest)? else {
            // 索引指向的 CAS 对象丢失：清除悬挂索引并按未命中处理。
            self.forget(ns, key, tenant).await?;
            self.stats.count(ns, CountKind::Miss);
            return Ok(None);
        };
        self.stats.count(ns, CountKind::Hit);
        let row = sqlx::query(
            "SELECT tag, size, duration_ms, sha, dirty_hash FROM kv_entries \
             WHERE namespace = ?1 AND tenant = ?2 AND cache_key = ?3",
        )
        .bind(ns.as_str())
        .bind(tenant)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Other(format!("lookup meta: {e}")))?;
        let Some(row) = row else {
            // 索引行在 CAS 读取后消失（并发 forget）：按未命中处理。
            self.stats.count(ns, CountKind::Miss);
            return Ok(None);
        };
        use sqlx::Row;
        let size: i64 = row.try_get("size").unwrap_or(0);
        let meta = ArtifactMeta {
            duration_ms: row.try_get("duration_ms").unwrap_or(None),
            sha: row.try_get("sha").unwrap_or(None),
            dirty_hash: row.try_get("dirty_hash").unwrap_or(None),
        };
        let tag: Option<String> = row.try_get("tag").unwrap_or(None);
        Ok(Some(Entry {
            size: size.max(0) as u64,
            bytes,
            tag,
            meta,
        }))
    }

    /// 条目是否存在（HEAD 探测；sccache 读路径不走这里，故不计命中）。
    pub async fn contains(&self, ns: Namespace, key: &str, tenant: &str) -> Result<bool> {
        Ok(self.lookup(ns, key, tenant).await?.is_some())
    }

    async fn lookup(
        &self,
        ns: Namespace,
        key: &str,
        tenant: &str,
    ) -> Result<Option<ContentDigest>> {
        let row = sqlx::query(
            "SELECT digest FROM kv_entries WHERE namespace = ?1 AND tenant = ?2 AND cache_key = ?3",
        )
        .bind(ns.as_str())
        .bind(tenant)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Other(format!("lookup kv: {e}")))?;
        match row {
            Some(row) => {
                use sqlx::Row;
                let hex: String = row
                    .try_get("digest")
                    .map_err(|e| Error::Other(format!("read digest column: {e}")))?;
                Ok(Some(ContentDigest::from_hex(&hex).map_err(|e| {
                    Error::Other(format!("bad digest in index: {e}"))
                })?))
            }
            None => Ok(None),
        }
    }

    async fn forget(&self, ns: Namespace, key: &str, tenant: &str) -> Result<()> {
        sqlx::query(
            "DELETE FROM kv_entries WHERE namespace = ?1 AND tenant = ?2 AND cache_key = ?3",
        )
        .bind(ns.as_str())
        .bind(tenant)
        .bind(key)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Other(format!("forget kv: {e}")))?;
        Ok(())
    }
}
