//! The uptime history in SQLite: `heartbeats` (one row per check, kept for
//! `UPTIME_RETENTION_DAYS`) and `daily_uptime` (checks counted per UTC day, kept much
//! longer). Plain functions over a connection, called from `UptimeMonitor` through
//! `Db::read`/`Db::write`.

use rusqlite::types::{FromSqlError, Type};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use super::{DAY_SECS, DayUptime, Heartbeat, Status};
use crate::db::DbError;

const COLUMNS: &str = "at, status, latency_ms, message";

fn sql_int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn status_code(status: Status) -> i64 {
    i64::from(status.severity())
}

fn beat_from_row(row: &Row<'_>) -> rusqlite::Result<Heartbeat> {
    let status = match row.get::<_, i64>(1)? {
        0 => Status::Up,
        1 => Status::Degraded,
        2 => Status::Down,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                1,
                Type::Integer,
                FromSqlError::OutOfRange(other).into(),
            ));
        }
    };
    Ok(Heartbeat {
        at: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
        status,
        latency_ms: row
            .get::<_, Option<i64>>(2)?
            .and_then(|ms| u32::try_from(ms).ok()),
        message: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
    })
}

/// Stores one check and counts it in its day. `false` if the app already had a check at
/// that exact second (it's then left as it was, and not counted twice).
///
/// Also keeps `flip` (the status changed from the check before) right: for this check,
/// and for the next one when this one lands in the middle of the history (an import).
pub fn insert(tx: &Transaction<'_>, slug: &str, beat: &Heartbeat) -> Result<bool, DbError> {
    let at = sql_int(beat.at);
    let status = status_code(beat.status);
    let previous: Option<i64> = tx
        .prepare_cached(
            "SELECT status FROM heartbeats WHERE slug = ?1 AND at < ?2 ORDER BY at DESC LIMIT 1",
        )?
        .query_row(params![slug, at], |row| row.get(0))
        .optional()?;
    let inserted = tx
        .prepare_cached(
            "INSERT INTO heartbeats (slug, at, status, latency_ms, message, flip) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (slug, at) DO NOTHING",
        )?
        .execute(params![
            slug,
            at,
            status,
            beat.latency_ms,
            Some(&beat.message).filter(|m| !m.is_empty()),
            previous != Some(status),
        ])?;
    if inserted == 0 {
        return Ok(false);
    }
    // A no-op for a round of checks, which always lands after the newest one.
    tx.prepare_cached(
        "UPDATE heartbeats SET flip = (status != ?3) WHERE slug = ?1 AND at = \
         (SELECT at FROM heartbeats WHERE slug = ?1 AND at > ?2 ORDER BY at LIMIT 1)",
    )?
    .execute(params![slug, at, status])?;
    count_day(tx, slug, beat)?;
    Ok(true)
}

/// Counts a check in its day's uptime without storing the check itself: how history
/// older than the retention is imported.
pub fn count_day(tx: &Transaction<'_>, slug: &str, beat: &Heartbeat) -> Result<(), DbError> {
    // The column comes from the enum, never from input.
    let column = match beat.status {
        Status::Up => "up",
        Status::Degraded => "degraded",
        Status::Down => "down",
    };
    tx.prepare_cached(&format!(
        "INSERT INTO daily_uptime (slug, day, {column}) VALUES (?1, ?2, 1) \
         ON CONFLICT (slug, day) DO UPDATE SET {column} = {column} + 1"
    ))?
    .execute(params![slug, sql_int(beat.at / DAY_SECS * DAY_SECS)])?;
    Ok(())
}

/// Every registered app's slug, in registration order.
pub fn slugs(conn: &Connection) -> Result<Vec<String>, DbError> {
    let mut stmt = conn.prepare_cached("SELECT slug FROM apps ORDER BY position")?;
    let slugs = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(slugs)
}

/// The newest `n` checks, oldest first.
pub fn recent(conn: &Connection, slug: &str, n: usize) -> Result<Vec<Heartbeat>, DbError> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM heartbeats WHERE slug = ?1 ORDER BY at DESC LIMIT ?2"
    ))?;
    let mut beats = stmt
        .query_map(params![slug, sql_int(n as u64)], beat_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    beats.reverse();
    Ok(beats)
}

/// Every check since `cutoff`, oldest first.
pub fn since(conn: &Connection, slug: &str, cutoff: u64) -> Result<Vec<Heartbeat>, DbError> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM heartbeats WHERE slug = ?1 AND at >= ?2 ORDER BY at"
    ))?;
    let beats = stmt
        .query_map(params![slug, sql_int(cutoff)], beat_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(beats)
}

/// Whether there are more than `n` checks since `cutoff` -- counting stops at `n + 1`.
pub fn more_than(conn: &Connection, slug: &str, cutoff: u64, n: usize) -> Result<bool, DbError> {
    let found: i64 = conn
        .prepare_cached(
            "SELECT count(*) FROM (SELECT 1 FROM heartbeats WHERE slug = ?1 AND at >= ?2 LIMIT ?3)",
        )?
        .query_row(
            params![slug, sql_int(cutoff), sql_int(n as u64 + 1)],
            |row| row.get(0),
        )?;
    Ok(usize::try_from(found).unwrap_or(0) > n)
}

/// The checks since `cutoff` aggregated into `bucket_secs` buckets, oldest first: each
/// one placed mid-bucket, with its worst status (and the message of the first check that
/// had it) and the average latency of its available checks. The same points as
/// `History::downsampled`, without loading every check: the rows are read as three
/// integers each, in index order, and only the chosen checks' messages are looked up.
pub fn buckets(
    conn: &Connection,
    slug: &str,
    cutoff: u64,
    bucket_secs: u64,
) -> Result<Vec<Heartbeat>, DbError> {
    /// One bucket while it's being filled: its worst check so far and the latency sum.
    struct Bucket {
        index: u64,
        worst_at: i64,
        worst: i64,
        sum: u64,
        n: u64,
    }
    let bucket_secs = bucket_secs.max(1);
    let mut rows = conn.prepare_cached(
        "SELECT at, status, latency_ms FROM heartbeats WHERE slug = ?1 AND at >= ?2 ORDER BY at",
    )?;
    let mut rows = rows.query(params![slug, sql_int(cutoff)])?;
    let mut done: Vec<Bucket> = Vec::new();
    let mut open: Option<Bucket> = None;
    while let Some(row) = rows.next()? {
        let (at, status, latency): (i64, i64, Option<i64>) =
            (row.get(0)?, row.get(1)?, row.get(2)?);
        let index = u64::try_from(at).unwrap_or(0).saturating_sub(cutoff) / bucket_secs;
        // Only available checks count toward the latency, as in `History::downsampled`.
        let latency = latency
            .filter(|_| status != 2)
            .and_then(|ms| u64::try_from(ms).ok());
        match open.as_mut() {
            Some(b) if b.index == index => {
                if status > b.worst {
                    (b.worst, b.worst_at) = (status, at);
                }
                if let Some(ms) = latency {
                    b.sum += ms;
                    b.n += 1;
                }
            }
            _ => {
                done.extend(open.take());
                open = Some(Bucket {
                    index,
                    worst_at: at,
                    worst: status,
                    sum: latency.unwrap_or(0),
                    n: u64::from(latency.is_some()),
                });
            }
        }
    }
    done.extend(open);
    let mut message =
        conn.prepare_cached("SELECT message FROM heartbeats WHERE slug = ?1 AND at = ?2")?;
    done.iter()
        .map(|b| {
            let text: Option<String> =
                message.query_row(params![slug, b.worst_at], |row| row.get(0))?;
            Ok(Heartbeat {
                at: cutoff + b.index * bucket_secs + bucket_secs / 2,
                status: match b.worst {
                    0 => Status::Up,
                    1 => Status::Degraded,
                    _ => Status::Down,
                },
                latency_ms: (b.n > 0).then(|| u32::try_from(b.sum / b.n).unwrap_or(u32::MAX)),
                message: text.unwrap_or_default(),
            })
        })
        .collect()
}

/// What `IncidentReport::build` needs from the checks since `from`, oldest first: every
/// down check, and the flips (among them, the first check after each outage). The checks
/// in between change nothing for it.
pub fn incident_beats(conn: &Connection, slug: &str, from: u64) -> Result<Vec<Heartbeat>, DbError> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM heartbeats WHERE slug = ?1 AND at >= ?2 \
         AND (status = 2 OR flip = 1) ORDER BY at"
    ))?;
    let beats = stmt
        .query_map(params![slug, sql_int(from)], beat_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(beats)
}

/// Where a history read for the window starting at `cutoff` should begin so an outage
/// already going on at `cutoff` is seen from its first down check: the last check before
/// `cutoff` that wasn't down (or the beginning of the history).
pub fn window_start(conn: &Connection, slug: &str, cutoff: u64) -> Result<u64, DbError> {
    let start: Option<i64> = conn
        .prepare_cached(
            "SELECT at FROM heartbeats WHERE slug = ?1 AND at < ?2 AND status != 2 \
             ORDER BY at DESC LIMIT 1",
        )?
        .query_row(params![slug, sql_int(cutoff)], |row| row.get(0))
        .optional()?;
    Ok(start.map_or(0, |at| u64::try_from(at).unwrap_or(0)))
}

/// When the current run of down checks began; `None` if the last check wasn't down.
pub fn down_since(conn: &Connection, slug: &str) -> Result<Option<u64>, DbError> {
    let start: Option<i64> = conn
        .prepare_cached(
            "SELECT min(at) FROM heartbeats WHERE slug = ?1 AND status = 2 AND at > \
             coalesce((SELECT at FROM heartbeats WHERE slug = ?1 AND status != 2 \
             ORDER BY at DESC LIMIT 1), -1)",
        )?
        .query_row(params![slug], |row| row.get(0))?;
    Ok(start.and_then(|at| u64::try_from(at).ok()))
}

/// Checks where the status flipped, newest first, at most `max`. The oldest retained
/// check counts as one too (its run starts there as far as the history goes), once the
/// flips run out.
///
/// Read from the partial index on `flip`: a few rows per app, however long its history.
pub fn events(conn: &Connection, slug: &str, max: usize) -> Result<Vec<Heartbeat>, DbError> {
    // `INDEXED BY`: without planner statistics (a new or just migrated database, until
    // the hourly `PRAGMA optimize`), SQLite picks the primary key and filters `flip` row
    // by row, reading the whole history it's meant to skip.
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM heartbeats INDEXED BY heartbeats_flips \
         WHERE slug = ?1 AND flip = 1 ORDER BY at DESC LIMIT ?2"
    ))?;
    let mut events = stmt
        .query_map(params![slug, sql_int(max as u64)], beat_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if events.len() < max {
        // After pruning, the oldest check left may not be a flip any more than it was.
        let oldest = conn
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM heartbeats WHERE slug = ?1 ORDER BY at LIMIT 1"
            ))?
            .query_map([slug], beat_from_row)?
            .next()
            .transpose()?;
        if let Some(oldest) = oldest.filter(|o| events.last().is_none_or(|e| e.at != o.at)) {
            events.push(oldest);
        }
    }
    Ok(events)
}

#[cfg(test)]
/// `events` the way it was found before `flip` existed, walking each run back to its
/// start: the reference the index is checked against (tests only).
///
/// Checks where the status flipped (the oldest one counts), newest first, at most `max`.
///
/// Jumps from run to run instead of reading every check: for the run the newest check
/// belongs to, SQLite finds the last earlier check with another status (the scan stays
/// inside SQLite), and the check right after it is where the run began -- an event.
/// A steady app costs two queries, not a pass over its whole history in Rust.
pub fn events_by_walking(
    conn: &Connection,
    slug: &str,
    max: usize,
) -> Result<Vec<Heartbeat>, DbError> {
    let mut latest = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM heartbeats WHERE slug = ?1 ORDER BY at DESC LIMIT 1"
    ))?;
    // `ORDER BY ... LIMIT 1`, not `max(at)`: with the status filter SQLite can't take
    // max() from the index, and would read the whole history before `at` every time;
    // walking the index backwards stops at the run's edge.
    let mut boundary = conn.prepare_cached(
        "SELECT at FROM heartbeats WHERE slug = ?1 AND at < ?2 AND status != ?3 \
         ORDER BY at DESC LIMIT 1",
    )?;
    let mut first_after = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM heartbeats WHERE slug = ?1 AND at > ?2 ORDER BY at LIMIT 1"
    ))?;
    let mut at = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM heartbeats WHERE slug = ?1 AND at = ?2"
    ))?;

    let mut events = Vec::new();
    let Some(mut current) = latest
        .query_map([slug], beat_from_row)?
        .next()
        .transpose()?
    else {
        return Ok(events);
    };
    while events.len() < max {
        let before: Option<i64> = boundary
            .query_row(
                params![slug, sql_int(current.at), status_code(current.status)],
                |row| row.get(0),
            )
            .optional()?;
        let start = first_after.query_row(params![slug, before.unwrap_or(-1)], beat_from_row)?;
        events.push(start);
        let Some(before) = before else {
            break; // that run is the oldest retained one
        };
        current = at.query_row(params![slug, before], beat_from_row)?;
    }
    Ok(events)
}

/// Uptime and average latency over the last 24 hours, and uptime over the last 30 days
/// (today included), from the daily counts.
pub fn stats(conn: &Connection, slug: &str, now: u64) -> Result<Stats, DbError> {
    // Counted by SQLite rather than loaded as checks: this runs for every app at startup
    // and after every round. Same figures as `History::uptime_pct` / `avg_latency_ms`.
    let (day_total, day_available, latency_sum, latency_n): (i64, Option<i64>, Option<i64>, i64) =
        conn.prepare_cached(
            "SELECT count(*), sum(status != 2), \
             sum(CASE WHEN status != 2 THEN latency_ms END), \
             count(CASE WHEN status != 2 AND latency_ms IS NOT NULL THEN 1 END) \
             FROM heartbeats WHERE slug = ?1 AND at >= ?2",
        )?
        .query_row(
            params![slug, sql_int(now.saturating_sub(DAY_SECS))],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let first_day = (now / DAY_SECS).saturating_sub(29) * DAY_SECS;
    let (available, total): (Option<i64>, Option<i64>) = conn
        .prepare_cached(
            "SELECT sum(up + degraded), sum(up + degraded + down) FROM daily_uptime \
             WHERE slug = ?1 AND day >= ?2",
        )?
        .query_row(params![slug, sql_int(first_day)], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    Ok(Stats {
        uptime_24h: share(day_available.unwrap_or(0), day_total),
        avg_latency_24h_ms: (latency_n > 0)
            .then(|| u32::try_from(latency_sum.unwrap_or(0) / latency_n).unwrap_or(u32::MAX)),
        uptime_30d: share(available.unwrap_or(0), total.unwrap_or(0)),
    })
}

/// What the dashboard shows next to each app, recomputed after every round of checks.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    pub uptime_24h: Option<f64>,
    pub avg_latency_24h_ms: Option<u32>,
    pub uptime_30d: Option<f64>,
}

#[allow(clippy::cast_precision_loss)] // check counts are far below 2^52
fn share(available: i64, total: i64) -> Option<f64> {
    (total > 0).then(|| available as f64 * 100.0 / total as f64)
}

/// One entry per UTC day for the last `days` days (oldest first, today last): the share
/// of available checks that day, or `None` without data.
pub fn daily(
    conn: &Connection,
    slug: &str,
    now: u64,
    days: u64,
) -> Result<Vec<DayUptime>, DbError> {
    let today = now / DAY_SECS;
    let first = today.saturating_sub(days.saturating_sub(1));
    let mut stmt = conn.prepare_cached(
        "SELECT day, up + degraded, up + degraded + down FROM daily_uptime \
         WHERE slug = ?1 AND day >= ?2 ORDER BY day",
    )?;
    let counts: Vec<(i64, i64, i64)> = stmt
        .query_map(params![slug, sql_int(first * DAY_SECS)], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok((first..=today)
        .map(|day| {
            let start = day * DAY_SECS;
            let uptime = counts
                .iter()
                .find(|(d, _, _)| *d == sql_int(start))
                .and_then(|&(_, available, total)| share(available, total));
            DayUptime { start, uptime }
        })
        .collect())
}

/// Drops checks older than `checks_before` and daily counts older than `days_before`
/// (unix seconds); returns how many rows of each went. App by app, so each delete is a
/// range of the primary key rather than a scan of the whole table.
pub fn prune(
    tx: &Transaction<'_>,
    checks_before: u64,
    days_before: u64,
) -> Result<(usize, usize), DbError> {
    let (mut checks, mut days) = (0, 0);
    for slug in slugs(tx)? {
        checks += tx
            .prepare_cached("DELETE FROM heartbeats WHERE slug = ?1 AND at < ?2")?
            .execute(params![slug, sql_int(checks_before)])?;
        days += tx
            .prepare_cached("DELETE FROM daily_uptime WHERE slug = ?1 AND day < ?2")?
            .execute(params![slug, sql_int(days_before)])?;
    }
    Ok((checks, days))
}
