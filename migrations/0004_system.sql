-- Heartbeat 0.3.2: CPU, memory and disk of each app's server (from its system URL) and of
-- Heartbeat's own, one row per sample, kept SYSTEM_RETENTION_DAYS. The full readings
-- (every disk, the busiest processes) are only kept in memory, for the latest sample.
CREATE TABLE system_samples (
  -- An app's slug, or '@heartbeat' for Heartbeat's own host (never a slug: slugs are
  -- letters, digits and dashes). Removed with the app by `AppRegistry::remove`.
  source        TEXT NOT NULL,
  -- Unix seconds.
  at            INTEGER NOT NULL,
  cpu_pct       REAL NOT NULL,
  memory_used   INTEGER NOT NULL,
  memory_total  INTEGER NOT NULL,
  -- The fullest disk reported; NULL when none was.
  disk_used     INTEGER,
  disk_total    INTEGER,
  load1         REAL,
  PRIMARY KEY (source, at)
) STRICT, WITHOUT ROWID;
