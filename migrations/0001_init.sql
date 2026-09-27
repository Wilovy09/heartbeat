-- Heartbeat 0.3.0: everything that used to live in data/*.json and data/uptime/*.jsonl.
-- Times are unix seconds. Applied once by src/db.rs, which then sets user_version = 1.

CREATE TABLE apps (
  slug          TEXT PRIMARY KEY,
  -- Registration order: the dashboard lists apps the way the old apps.json did.
  position      INTEGER NOT NULL,
  name          TEXT NOT NULL,
  health_url    TEXT,
  logs_url      TEXT,
  embed_token   TEXT NOT NULL,
  public        INTEGER NOT NULL DEFAULT 0,
  paused        INTEGER NOT NULL DEFAULT 0,
  paused_at     INTEGER,
  paused_until  INTEGER,
  -- What's only ever read together with the app (thresholds, headers, webhooks,
  -- mentions...), as JSON; validated by registry.rs before it's saved.
  settings      TEXT NOT NULL DEFAULT '{}'
) STRICT;

-- One row per check. The primary key is the table (WITHOUT ROWID), ordered by app and
-- time, so a window of one app's history is a single range scan.
CREATE TABLE heartbeats (
  slug        TEXT NOT NULL REFERENCES apps(slug) ON DELETE CASCADE,
  at          INTEGER NOT NULL,
  -- 0 up, 1 degraded, 2 down.
  status      INTEGER NOT NULL CHECK (status BETWEEN 0 AND 2),
  -- NULL: the request never got a response (DNS, connect, timeout).
  latency_ms  INTEGER,
  message     TEXT,
  PRIMARY KEY (slug, at)
) STRICT, WITHOUT ROWID;

-- Checks counted per UTC day, updated in the same transaction as each check. Kept much
-- longer than the checks themselves: the status page and the 30-day uptime read it.
CREATE TABLE daily_uptime (
  slug      TEXT NOT NULL REFERENCES apps(slug) ON DELETE CASCADE,
  -- 00:00 UTC of the day.
  day       INTEGER NOT NULL,
  up        INTEGER NOT NULL DEFAULT 0,
  degraded  INTEGER NOT NULL DEFAULT 0,
  down      INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (slug, day)
) STRICT, WITHOUT ROWID;

CREATE TABLE sessions (
  -- Random 256-bit id, the only thing the cookie carries.
  id              TEXT PRIMARY KEY,
  upstream_token  TEXT NOT NULL,
  role            TEXT NOT NULL CHECK (role IN ('admin', 'viewer')),
  expires_at      INTEGER NOT NULL
) STRICT;
CREATE INDEX sessions_expiry ON sessions (expires_at);

CREATE TABLE notices (
  id           TEXT PRIMARY KEY,
  title        TEXT NOT NULL,
  body         TEXT NOT NULL,
  state        TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  resolved_at  INTEGER
) STRICT;

-- The apps a notice is about; none = every app. No foreign key to apps: a notice outlives
-- the apps it mentioned, as it always has.
CREATE TABLE notice_apps (
  notice_id  TEXT NOT NULL REFERENCES notices(id) ON DELETE CASCADE,
  slug       TEXT NOT NULL,
  PRIMARY KEY (notice_id, slug)
) STRICT, WITHOUT ROWID;

-- Alert texts edited in Settings; a missing kind uses the default for APP_LANG.
CREATE TABLE alert_templates (
  kind  TEXT PRIMARY KEY,
  text  TEXT NOT NULL
) STRICT;

-- Single values: the custom theme's CSS, and whatever comes next.
CREATE TABLE kv (
  key    TEXT PRIMARY KEY,
  value  TEXT NOT NULL
) STRICT;
