//! SQLite 支持的构建队列：租约、FIFO 派发、事件与产物持久化。

use std::path::Path;
use std::time::Duration;

use hotpot_core::model::{ArtifactMeta, BuildEvent, BuildRecord, BuildStatus};
use hotpot_core::{BuildId, Error, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteRow};
use sqlx::{Row, SqlitePool};
use tracing::debug;

/// 构建队列（一个 SQLite 连接池）。
#[derive(Clone)]
pub struct Scheduler {
    pool: SqlitePool,
}

const SCHEMA: &str = include_str!("schema.sql");

impl Scheduler {
    /// 打开（必要时创建）`root/hotpot.db`。
    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        let path = root.as_ref().join("hotpot.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePool::connect_with(options)
            .await
            .map_err(|e| Error::Other(format!("open sqlite: {e}")))?;
        let sched = Self { pool };
        sqlx::query(SCHEMA)
            .execute(&sched.pool)
            .await
            .map_err(|e| Error::Other(format!("init schema: {e}")))?;
        Ok(sched)
    }

    /// 入队。相同 (source, profile) 已有排队任务时直接返回该任务（上下文合并）。
    pub async fn enqueue(&self, record: BuildRecord) -> Result<BuildRecord> {
        let source_json = serde_json::to_string(&record.source).map_err(serde_err)?;
        let profile_json = serde_json::to_string(&record.profile).map_err(serde_err)?;

        let existing = sqlx::query(
            "SELECT * FROM builds WHERE status = '\"queued\"' AND source_json = ?1 \
             AND profile_json = ?2 ORDER BY created_at_ms DESC LIMIT 1",
        )
        .bind(&source_json)
        .bind(&profile_json)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_err)?;
        if let Some(row) = existing {
            debug!("coalesced into existing queued build");
            return row_to_build(row);
        }

        sqlx::query(
            "INSERT INTO builds (id, project_id, source_json, profile_json, status, \
             timings_json, created_at_ms, priority) \
             VALUES (?1, ?2, ?3, ?4, '\"queued\"', '{}', ?5, 0)",
        )
        .bind(record.id.to_string())
        .bind(record.project_id.map(|p| p.to_string()))
        .bind(&source_json)
        .bind(&profile_json)
        .bind(record.created_at_ms)
        .execute(&self.pool)
        .await
        .map_err(sqlx_err)?;
        Ok(record)
    }

    /// 认领最老的可执行任务：queued，或租约已过期的 dispatched（失活接管）。
    /// 成功则写入 worker 租约并返回记录；无任务返回 None。
    pub async fn claim_next(&self, worker: &str, lease: Duration) -> Result<Option<BuildRecord>> {
        let now = now_ms();
        let until = now + lease.as_millis() as i64;
        let row = sqlx::query(
            "UPDATE builds SET status = '\"dispatched\"', leased_by = ?1, leased_until_ms = ?2 \
             WHERE id = ( \
               SELECT id FROM builds \
               WHERE status = '\"queued\"' \
                  OR (status = '\"dispatched\"' AND leased_until_ms < ?3) \
               ORDER BY priority DESC, created_at_ms ASC LIMIT 1) \
             RETURNING *",
        )
        .bind(worker)
        .bind(until)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_err)?;
        match row {
            Some(row) => Ok(Some(row_to_build(row)?)),
            None => Ok(None),
        }
    }

    /// 续租（长构建期间定期调用）。
    pub async fn renew_lease(&self, id: BuildId, worker: &str, lease: Duration) -> Result<()> {
        let until = now_ms() + lease.as_millis() as i64;
        sqlx::query("UPDATE builds SET leased_until_ms = ?1 WHERE id = ?2 AND leased_by = ?3")
            .bind(until)
            .bind(id.to_string())
            .bind(worker)
            .execute(&self.pool)
            .await
            .map_err(sqlx_err)?;
        Ok(())
    }

    /// 追加单个事件。
    pub async fn append_event(&self, event: &BuildEvent) -> Result<()> {
        self.append_events(std::slice::from_ref(event)).await
    }

    /// 批量追加事件（seq 已由执行器保证连续）。
    pub async fn append_events(&self, events: &[BuildEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let kind_json = |k: &hotpot_core::EventKind| {
            serde_json::to_string(k).unwrap_or_else(|_| "\"stdout\"".to_string())
        };
        let mut tx = self.pool.begin().await.map_err(sqlx_err)?;
        for e in events {
            sqlx::query(
                "INSERT OR REPLACE INTO build_events \
                 (build_id, seq, timestamp_ms, kind, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(e.build_id.to_string())
            .bind(e.seq as i64)
            .bind(e.timestamp_ms)
            .bind(kind_json(&e.kind))
            .bind(&e.payload)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        }
        tx.commit().await.map_err(sqlx_err)?;
        Ok(())
    }

    /// 读取某构建 seq > after_seq 的事件（SSE 轮询/续传）。
    pub async fn list_events(
        &self,
        id: BuildId,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<BuildEvent>> {
        let rows = sqlx::query(
            "SELECT build_id, seq, timestamp_ms, kind, payload FROM build_events \
             WHERE build_id = ?1 AND seq >= ?2 ORDER BY seq ASC LIMIT ?3",
        )
        .bind(id.to_string())
        .bind(after_seq as i64)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_err)?;

        rows.into_iter()
            .map(|row| {
                let kind: hotpot_core::EventKind =
                    serde_json::from_str(&rg::<String>(&row, "kind")?)
                        .unwrap_or(hotpot_core::EventKind::Stdout);
                Ok(BuildEvent {
                    build_id: parse_id(&rg::<String>(&row, "build_id")?)?,
                    seq: rg::<i64>(&row, "seq")? as u64,
                    timestamp_ms: rg::<i64>(&row, "timestamp_ms")?,
                    kind,
                    payload: rg(&row, "payload")?,
                })
            })
            .collect()
    }

    /// 获取构建记录。
    pub async fn get_build(&self, id: BuildId) -> Result<Option<BuildRecord>> {
        let row = sqlx::query("SELECT * FROM builds WHERE id = ?1")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_err)?;
        match row {
            Some(row) => Ok(Some(row_to_build(row)?)),
            None => Ok(None),
        }
    }

    /// 构建结束：写终态、耗时、错误信息并释放租约。
    pub async fn finish_build(
        &self,
        id: BuildId,
        status: BuildStatus,
        timings: hotpot_core::BuildTimings,
        error: Option<String>,
    ) -> Result<()> {
        let status_json = serde_json::to_string(&status).map_err(serde_err)?;
        let timings_json = serde_json::to_string(&timings).map_err(serde_err)?;
        sqlx::query(
            "UPDATE builds SET status = ?1, timings_json = ?2, error = ?3, \
             finished_at_ms = ?4, leased_by = NULL, leased_until_ms = NULL WHERE id = ?5",
        )
        .bind(status_json)
        .bind(timings_json)
        .bind(error)
        .bind(now_ms())
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }

    /// 取消排队中的构建（直接置 canceled）。
    /// 返回 Ok(true) 表示已取消；Ok(false) 表示任务不在排队状态，需由运行时发信号。
    pub async fn cancel_queued(&self, id: BuildId) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE builds SET status = '\"canceled\"', finished_at_ms = ?1 \
             WHERE id = ?2 AND status = '\"queued\"'",
        )
        .bind(now_ms())
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(sqlx_err)?;
        Ok(result.rows_affected() > 0)
    }

    /// 登记产物。
    pub async fn add_artifacts(&self, id: BuildId, artifacts: &[ArtifactMeta]) -> Result<()> {
        if artifacts.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(sqlx_err)?;
        for a in artifacts {
            sqlx::query(
                "INSERT OR REPLACE INTO artifacts (build_id, name, digest, size, attrs_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(id.to_string())
            .bind(&a.name)
            .bind(&a.digest)
            .bind(a.size as i64)
            .bind(serde_json::to_string(&a.attrs).map_err(serde_err)?)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        }
        tx.commit().await.map_err(sqlx_err)?;
        Ok(())
    }

    /// 列出某构建的产物。
    pub async fn list_artifacts(&self, id: BuildId) -> Result<Vec<ArtifactMeta>> {
        let rows =
            sqlx::query("SELECT name, digest, size, attrs_json FROM artifacts WHERE build_id = ?1")
                .bind(id.to_string())
                .fetch_all(&self.pool)
                .await
                .map_err(sqlx_err)?;

        rows.into_iter()
            .map(|row| {
                let attrs_json: String = rg(&row, "attrs_json")?;
                Ok(ArtifactMeta {
                    name: rg(&row, "name")?,
                    digest: rg(&row, "digest")?,
                    size: rg::<i64>(&row, "size")? as u64,
                    attrs: serde_json::from_str(&attrs_json).unwrap_or_default(),
                })
            })
            .collect()
    }
}

fn row_to_build(row: SqliteRow) -> Result<BuildRecord> {
    let source_json: String = rg(&row, "source_json")?;
    let profile_json: String = rg(&row, "profile_json")?;
    let status_json: String = rg(&row, "status")?;
    let timings_json: String = rg(&row, "timings_json")?;
    Ok(BuildRecord {
        id: parse_id(&rg::<String>(&row, "id")?)?,
        project_id: None,
        source: serde_json::from_str(&source_json).map_err(serde_err)?,
        profile: serde_json::from_str(&profile_json).map_err(serde_err)?,
        status: serde_json::from_str(&status_json).map_err(serde_err)?,
        timings: serde_json::from_str(&timings_json).unwrap_or_default(),
        created_at_ms: rg(&row, "created_at_ms")?,
        started_at_ms: rg(&row, "started_at_ms")?,
        finished_at_ms: rg(&row, "finished_at_ms")?,
        error: rg(&row, "error")?,
    })
}

/// BuildId 的 Display 为 `bld_<uuid>`，去掉前缀还原 uuid。
fn parse_id(s: &str) -> Result<BuildId> {
    let uuid = s.split_once('_').map_or(s, |(_, u)| u);
    Ok(BuildId(uuid::Uuid::parse_str(uuid).map_err(|e| {
        Error::Invalid(format!("bad build id {s}: {e}"))
    })?))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 读取列并把 sqlx 错误映射为 hotpot 错误。
fn rg<'r, T>(row: &'r SqliteRow, col: &str) -> Result<T>
where
    T: sqlx::Decode<'r, sqlx::sqlite::Sqlite> + sqlx::Type<sqlx::sqlite::Sqlite>,
{
    row.try_get(col).map_err(sqlx_err)
}

fn serde_err(e: serde_json::Error) -> Error {
    Error::Other(format!("serde: {e}"))
}

fn sqlx_err(e: sqlx::Error) -> Error {
    Error::Other(format!("sqlite: {e}"))
}
