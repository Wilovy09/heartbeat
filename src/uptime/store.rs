//! The uptime history in SQLite: `heartbeats` (one row per check, kept for
//! `UPTIME_RETENTION_DAYS`) and `daily_uptime` (checks counted per UTC day, kept much
//! longer). Plain functions over a connection, called from `UptimeMonitor` through
//! `Db::read`/`Db::write`.

use rusqlite::types::{FromSqlError, Type};
use rusqlite::{Connection, Row, Transaction, params};

use super::{DAY_SECS, DayUptime, Heartbeat, History, Status};
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
pub fn insert(tx: &Transaction<'_>, slug: &str, beat: &Heartbeat) -> Result<bool, DbError> {
    let inserted = tx
        .prepare_cached(
            "INSERT INTO heartbeats (slug, at, status, latency_ms, message) \
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (slug, at) DO NOTHING",
        )?
        .execute(params![
            slug,
            sql_int(beat.at),
            status_code(beat.status),
            beat.latency_ms,
            Some(&beat.message).filter(|m| !m.is_empty()),
        ])?;
    if inserted == 0 {
        return Ok(false);
    }
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

/// Where a history read for the window starting at `cutoff` should begin so an outage
/// already going on at `cutoff` is seen from its first down check: the last check before
/// `cutoff` that wasn't down (or the beginning of the history).
pub fn window_start(conn: &Connection, slug: &str, cutoff: u64) -> Result<u64, DbError> {
    let start: Option<i64> = conn
        .prepare_cached(
            "SELECT max(at) FROM heartbeats WHERE slug = ?1 AND at < ?2 AND status != 2",
        )?
        .query_row(params![slug, sql_int(cutoff)], |row| row.get(0))?;
    Ok(start.map_or(0, |at| u64::try_from(at).unwrap_or(0)))
}

/// When the current run of down checks began; `None` if the last check wasn't down.
pub fn down_since(conn: &Connection, slug: &str) -> Result<Option<u64>, DbError> {
    let start: Option<i64> = conn
        .prepare_cached(
            "SELECT min(at) FROM heartbeats WHERE slug = ?1 AND status = 2 AND at > \
             coalesce((SELECT max(at) FROM heartbeats WHERE slug = ?1 AND status != 2), -1)",
        )?
        .query_row(params![slug], |row| row.get(0))?;
    Ok(start.and_then(|at| u64::try_from(at).ok()))
}

/// Checks where the status flipped (the oldest one counts), newest first, at most `max`.
///
/// Jumps from run to run instead of reading every check: for the run the newest check
/// belongs to, SQLite finds the last earlier check with another status (the scan stays
/// inside SQLite), and the check right after it is where the run began -- an event.
/// A steady app costs two queries, not a pass over its whole history in Rust.
pub fn events(conn: &Connection, slug: &str, max: usize) -> Result<Vec<Heartbeat>, DbError> {
    let mut latest = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM heartbeats WHERE slug = ?1 ORDER BY at DESC LIMIT 1"
    ))?;
    let mut boundary = conn.prepare_cached(
        "SELECT max(at) FROM heartbeats WHERE slug = ?1 AND at < ?2 AND status != ?3",
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
        let before: Option<i64> = boundary.query_row(
            params![slug, sql_int(current.at), status_code(current.status)],
            |row| row.get(0),
        )?;
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
    let day = History(since(conn, slug, now.saturating_sub(DAY_SECS))?.into());
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
        uptime_24h: day.uptime_pct(0),
        avg_latency_24h_ms: day.avg_latency_ms(0),
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
