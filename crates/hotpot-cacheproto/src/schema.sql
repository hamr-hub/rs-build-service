-- 远程缓存索引（client key → CAS digest）。

CREATE TABLE IF NOT EXISTS kv_entries (
  namespace     TEXT NOT NULL,
  tenant        TEXT NOT NULL DEFAULT '',
  cache_key     TEXT NOT NULL,
  digest        TEXT NOT NULL,
  size          INTEGER NOT NULL,
  tag           TEXT,
  -- Turborepo v8 元数据（客户端观测用；缺失不影响正确性）
  duration_ms   INTEGER,
  sha           TEXT,
  dirty_hash    TEXT,
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY (namespace, tenant, cache_key)
);
