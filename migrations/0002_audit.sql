-- Heartbeat 0.3.1: who did what, from the web UI, Slack, Discord or Heartbeat itself.

-- Sessions now remember who logged in; sessions from before this migration have none.
ALTER TABLE sessions ADD COLUMN email TEXT NOT NULL DEFAULT '';

CREATE TABLE audit_log (
  id      INTEGER PRIMARY KEY,
  -- Unix seconds.
  at      INTEGER NOT NULL,
  source  TEXT NOT NULL CHECK (source IN ('web', 'slack', 'discord', 'system')),
  -- An email (web), a Slack or Discord user ID, or 'system'. Empty for a web session
  -- started before this migration.
  actor   TEXT NOT NULL,
  -- 'app.update', 'chat.pause', 'theme.save'... (see `audit::Action`).
  action  TEXT NOT NULL,
  -- An app's slug, a notice's ID, a login's email.
  target  TEXT,
  outcome TEXT NOT NULL CHECK (outcome IN ('ok', 'denied', 'error')),
  -- JSON with the action's specifics (the fields an edit changed, a command's text...).
  -- Never secrets: tokens, passwords and header values are left out or masked.
  detail  TEXT,
  ip      TEXT
) STRICT;
CREATE INDEX audit_at ON audit_log (at);
CREATE INDEX audit_target ON audit_log (target, at);
