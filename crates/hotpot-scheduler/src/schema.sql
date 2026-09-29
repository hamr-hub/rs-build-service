-- Hotpot 构建队列 schema（幂等）。

CREATE TABLE IF NOT EXISTS builds (
  id               TEXT PRIMARY KEY,
  project_id       TEXT,
  source_json      TEXT NOT NULL,
  profile_json     TEXT NOT NULL,
  status           TEXT NOT NULL,
  timings_json     TEXT NOT NULL DEFAULT '{}',
  created_at_ms    INTEGER NOT NULL,
  started_at_ms    INTEGER,
  finished_at_ms   INTEGER,
  error            TEXT,
  leased_by        TEXT,
  leased_until_ms  INTEGER,
  priority         INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS build_events (
  build_id     TEXT NOT NULL,
  seq          INTEGER NOT NULL,
  timestamp_ms INTEGER NOT NULL,
  kind         TEXT NOT NULL,
  payload      TEXT NOT NULL,
  PRIMARY KEY (build_id, seq)
);

CREATE TABLE IF NOT EXISTS artifacts (
  build_id   TEXT NOT NULL,
  name       TEXT NOT NULL,
  digest     TEXT NOT NULL,
  size       INTEGER NOT NULL,
  attrs_json TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (build_id, name)
);

CREATE INDEX IF NOT EXISTS idx_builds_claim
  ON builds (status, priority, created_at_ms);
CREATE INDEX IF NOT EXISTS idx_build_events_build
  ON build_events (build_id, seq);
