//! 远程缓存：对外协议不透明字节按 client key 索引，载荷内容寻址入 CAS。

use std::path::{Path, PathBuf};
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
}

/// 远程缓存索引（CAS 在 LocalStore，索引在 SQLite）。
#[derive(Clone)]
pub struct RemoteCache {
    store: LocalStore,
    pool: SqlitePool,
}

const SCHEMA: &str = include_str!("schema.sql");

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
        Ok(Self { store, pool })
    }

    /// 写入条目。相同 (namespace, tenant, key) 已存在时幂等返回，不重复存。
    pub async fn put(
        &self,
        ns: Namespace,
        key: &str,
        tenant: &str,
        bytes: &[u8],
        tag: Option<String>,
    ) -> Result<ContentDigest> {
        if let Some(existing) = self.lookup(ns, key, tenant).await? {
            return Ok(existing);
        }
        let digest = self.store.put(bytes)?;
        sqlx::query(
            "INSERT INTO kv_entries (namespace, tenant, cache_key, digest, size, tag, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(ns.as_str())
        .bind(tenant)
        .bind(key)
        .bind(digest.to_hex())
        .bind(bytes.len() as i64)
        .bind(tag)
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Other(format!("insert kv entry: {e}")))?;
        Ok(digest)
    }

    /// 读取条目（未命中 None）。
    pub async fn get(&self, ns: Namespace, key: &str, tenant: &str) -> Result<Option<Entry>> {
        let Some(digest) = self.lookup(ns, key, tenant).await? else {
            return Ok(None);
        };
        let Some(bytes) = self.store.get(&digest)? else {
            // 索引指向的 CAS 对象丢失：清除悬挂索引并按未命中处理。
            self.forget(ns, key, tenant).await?;
            return Ok(None);
        };
        let tag = sqlx::query(
            "SELECT tag FROM kv_entries WHERE namespace = ?1 AND tenant = ?2 AND cache_key = ?3",
        )
        .bind(ns.as_str())
        .bind(tenant)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Other(format!("lookup tag: {e}")))?
        .and_then(|row| {
            use sqlx::Row;
            row.try_get::<Option<String>, _>("tag").ok().flatten()
        });
        Ok(Some(Entry { bytes, tag }))
    }

    /// 条目是否存在。
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
