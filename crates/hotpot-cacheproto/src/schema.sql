-- 远程缓存索引（client key → CAS digest）。

CREATE TABLE IF NOT EXISTS kv_entries (
  namespace     TEXT NOT NULL,
  tenant        TEXT NOT NULL DEFAULT '',
  cache_key     TEXT NOT NULL,
  digest        TEXT NOT NULL,
  size          INTEGER NOT NULL,
  tag           TEXT,
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY (namespace, tenant, cache_key)
);

CREATE INDEX IF NOT EXISTS idx_kv_lookup
  ON kv_entries (namespace, tenant, cache_key);
