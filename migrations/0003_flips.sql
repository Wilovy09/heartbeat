-- Heartbeat 0.3.2: status changes are marked when a check is stored, so the dashboard's
-- recent events (and startup, which rebuilds them for every app) read a handful of rows
-- from an index instead of walking each app's history back to its last flips.

-- 1 when this check's status differs from the one stored right before it for the same
-- app (or it's the app's first). Kept by `store::insert`.
ALTER TABLE heartbeats ADD COLUMN flip INTEGER NOT NULL DEFAULT 0;

UPDATE heartbeats SET flip = 1
WHERE (slug, at) IN (
  SELECT slug, at FROM (
    SELECT slug, at, status, lag(status) OVER (PARTITION BY slug ORDER BY at) AS previous
    FROM heartbeats
  )
  WHERE previous IS NULL OR previous != status
);

CREATE INDEX heartbeats_flips ON heartbeats (slug, at) WHERE flip = 1;
