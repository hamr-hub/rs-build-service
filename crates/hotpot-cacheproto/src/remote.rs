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
