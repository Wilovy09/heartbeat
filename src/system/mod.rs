//! CPU, memory and disk: of Heartbeat's own host (`host`), and of each app that has a
//! system URL, which answers the same `Snapshot` JSON (see "System" in the README).
//!
//! Every `SYSTEM_INTERVAL_SECS` the monitor samples them all, keeps the full latest
//! reading of each in memory (every disk, the busiest processes), stores a compact row per
//! sample (`system_samples`, kept `SYSTEM_RETENTION_DAYS`) for the charts, and alerts when
//! a resource stays over its threshold.

pub mod host;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio::task::JoinSet;

use crate::alerts::{Alerter, AppRoute, ResourceAlert};
use crate::db::{Db, DbError};
use crate::outbound::Outbound;
use crate::registry::{AppRegistry, RegisteredApp};
use crate::uptime::unix_now;

/// The source name Heartbeat's own host is stored under; never an app's slug.
pub const HOST: &str = "@heartbeat";
/// Processes kept per reading (an app's are cut to this too).
pub const MAX_PROCESSES: usize = 20;
/// Disks kept per reading.
const MAX_DISKS: usize = 16;
/// Longest process name or mount point kept.
const MAX_NAME_CHARS: usize = 64;
/// Most bytes read from an app's system URL.
const BODY_LIMIT: usize = 256 * 1024;
/// How long an app's system URL gets to answer.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// A resource that crossed its threshold is back once it's this many points under it, so
/// a value hovering at the edge doesn't flap.
const HYSTERESIS_PCT: f64 = 5.0;
/// Most points a chart gets; longer windows are averaged into buckets.
const MAX_CHART_POINTS: usize = 360;

/// One reading, as the system URL answers it (and as Heartbeat reads its own host).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub cpu: Cpu,
    pub memory: Memory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap: Option<Swap>,
    #[serde(default)]
    pub disks: Vec<Disk>,
    #[serde(default)]
    pub processes: Vec<Process>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cpu {
    /// Of the whole machine, 0-100.
    pub usage_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    /// 1, 5 and 15 minute load averages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<[f64; 3]>,
}

// Field names are the JSON contract's (see "System" in the README).
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub total_bytes: u64,
    pub used_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
}

// Field names are the JSON contract's (see "System" in the README).
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Swap {
    pub total_bytes: u64,
    pub used_bytes: u64,
}

// Field names are the JSON contract's (see "System" in the README).
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Disk {
    pub mount: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    pub name: String,
    /// Of one core, like `top`: a busy multi-threaded process can pass 100.
    pub cpu_pct: f64,
    pub memory_bytes: u64,
}

fn pct(used: u64, total: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)] // byte counts far below 2^52 lose nothing visible
    if total == 0 {
        0.0
    } else {
        used as f64 * 100.0 / total as f64
    }
}

fn short(text: &str) -> String {
    text.chars().take(MAX_NAME_CHARS).collect()
}

impl Snapshot {
    /// What an app answered, made safe to keep and show: bounded lists and names, used
    /// never above total, percentages that are numbers. `None` if it's unusable.
    fn sanitized(mut self) -> Option<Self> {
        if self.memory.total_bytes == 0 || !self.cpu.usage_pct.is_finite() {
            return None;
        }
        self.cpu.usage_pct = self.cpu.usage_pct.clamp(0.0, 100.0);
        self.cpu.load = self.cpu.load.filter(|l| l.iter().all(|v| v.is_finite()));
        self.memory.used_bytes = self.memory.used_bytes.min(self.memory.total_bytes);
        if let Some(swap) = &mut self.swap {
            swap.used_bytes = swap.used_bytes.min(swap.total_bytes);
        }
        self.disks.retain(|d| d.total_bytes > 0);
        self.disks.truncate(MAX_DISKS);
        for disk in &mut self.disks {
            disk.used_bytes = disk.used_bytes.min(disk.total_bytes);
            disk.mount = short(&disk.mount);
        }
        self.processes.retain(|p| p.cpu_pct.is_finite());
        self.processes
            .sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));
        self.processes.truncate(MAX_PROCESSES);
        for process in &mut self.processes {
            process.name = short(&process.name);
            process.cpu_pct = process.cpu_pct.max(0.0);
        }
        Some(self)
    }

    #[must_use]
    pub fn memory_pct(&self) -> f64 {
        pct(self.memory.used_bytes, self.memory.total_bytes)
    }

    /// The disk closest to full: the one the disk alert and the summary are about.
    #[must_use]
    pub fn fullest_disk(&self) -> Option<&Disk> {
        self.disks.iter().max_by(|a, b| {
            pct(a.used_bytes, a.total_bytes).total_cmp(&pct(b.used_bytes, b.total_bytes))
        })
    }

    fn sample(&self, at: u64) -> Sample {
        let disk = self.fullest_disk();
        Sample {
            at,
            cpu_pct: self.cpu.usage_pct,
            memory_used: self.memory.used_bytes,
            memory_total: self.memory.total_bytes,
            disk_used: disk.map(|d| d.used_bytes),
            disk_total: disk.map(|d| d.total_bytes),
            load1: self.cpu.load.map(|l| l[0]),
        }
    }
}

/// One stored row: what the charts draw.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    pub at: u64,
    pub cpu_pct: f64,
    pub memory_used: u64,
    pub memory_total: u64,
    pub disk_used: Option<u64>,
    pub disk_total: Option<u64>,
    pub load1: Option<f64>,
}

/// The latest attempt at reading a source: the reading, or why there's none.
#[derive(Debug, Clone, Serialize)]
pub struct Reading {
    pub at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// When to alert. A 0 percentage turns that resource's alert off.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub cpu_pct: u8,
    pub memory_pct: u8,
    pub disk_pct: u8,
    /// How long CPU and memory must stay over before alerting (a disk alerts at once).
    pub sustain: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct SystemPolicy {
    pub interval: Duration,
    pub retention: Duration,
    pub thresholds: Thresholds,
}

/// A resource to check: its name, value (%), threshold, sustain (s) and the disk's mount.
type Check = (&'static str, Option<f64>, u8, u64, Option<String>);

/// One resource of one source, between samples.
#[derive(Debug, Default, Clone, Copy)]
struct Watch {
    /// Since when it's been at or over the threshold, while it is.
    over_since: Option<u64>,
    alerted: bool,
}

impl Watch {
    /// `Some(true)` when it just crossed (for long enough), `Some(false)` when it's just
    /// back under the threshold minus the hysteresis.
    fn observe(&mut self, value: f64, threshold: u8, sustain: u64, now: u64) -> Option<bool> {
        let threshold = f64::from(threshold);
        if value >= threshold {
            let since = *self.over_since.get_or_insert(now);
            if !self.alerted && now.saturating_sub(since) >= sustain {
                self.alerted = true;
                return Some(true);
            }
        } else {
            self.over_since = None;
            if self.alerted && value < threshold - HYSTERESIS_PCT {
                self.alerted = false;
                return Some(false);
            }
        }
        None
    }
}

pub struct SystemMonitor {
    db: Db,
    policy: SystemPolicy,
    outbound: Outbound,
    alerter: Alerter,
    /// Sent to every system URL, as to the logs endpoints.
    admin_key: Option<String>,
    latest: RwLock<HashMap<String, Reading>>,
    watches: Mutex<HashMap<(String, &'static str), Watch>>,
    host: Arc<Mutex<host::HostSampler>>,
}

impl SystemMonitor {
    #[must_use]
    pub fn new(
        db: Db,
        policy: SystemPolicy,
        outbound: Outbound,
        alerter: Alerter,
        admin_key: Option<String>,
    ) -> Self {
        let host = host::HostSampler::new(db.path());
        Self {
            db,
            policy,
            outbound,
            alerter,
            admin_key,
            latest: RwLock::default(),
            watches: Mutex::default(),
            host: Arc::new(Mutex::new(host)),
        }
    }

    #[must_use]
    pub fn interval(&self) -> Duration {
        self.policy.interval
    }

    /// Runs forever: a round every `SYSTEM_INTERVAL_SECS`, old samples pruned hourly.
    pub async fn run(self: Arc<Self>, registry: Arc<AppRegistry>) {
        let mut ticker = tokio::time::interval(self.policy.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_prune = 0;
        loop {
            ticker.tick().await;
            self.round(&registry.list().await).await;
            let now = unix_now();
            if now.saturating_sub(last_prune) >= 3600 {
                last_prune = now;
                if let Err(e) = self.prune(now).await {
                    tracing::error!(error = %e, "system: could not prune old samples");
                }
            }
        }
    }

    /// Samples Heartbeat's host and every app with a system URL, stores the samples and
    /// alerts on what crossed a threshold.
    pub async fn round(&self, apps: &[RegisteredApp]) {
        let now = unix_now();
        let mut fetches = JoinSet::new();
        for app in apps.iter().filter(|a| a.system_url.is_some()) {
            let (outbound, key, app) = (self.outbound.clone(), self.admin_key.clone(), app.clone());
            fetches.spawn(async move {
                let url = app.system_url.clone().unwrap_or_default();
                let reading = fetch(&outbound, key.as_deref(), &url).await;
                (app, reading)
            });
        }
        let host_sampler = Arc::clone(&self.host);
        let host = tokio::task::spawn_blocking(move || {
            host_sampler
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .sample()
        })
        .await
        .map_err(|e| e.to_string());

        let mut readings: Vec<(Option<RegisteredApp>, Result<Snapshot, String>)> =
            vec![(None, host)];
        while let Some(done) = fetches.join_next().await {
            match done {
                Ok((app, reading)) => readings.push((Some(app), reading)),
                Err(e) => tracing::error!(error = %e, "system: a fetch task panicked"),
            }
        }

        let samples: Vec<(String, Sample)> = readings
            .iter()
            .filter_map(|(app, r)| {
                let source = app.as_ref().map_or(HOST, |a| &a.slug).to_string();
                r.as_ref().ok().map(|s| (source, s.sample(now)))
            })
            .collect();
        if let Err(e) = self.db.write(move |tx| store(tx, &samples)).await {
            tracing::error!(error = %e, "system: could not store samples");
        }

        let mut latest = self.latest.write().await;
        latest.retain(|source, _| source == HOST || apps.iter().any(|a| &a.slug == source));
        for (app, reading) in readings {
            match (&reading, &app) {
                (Ok(snapshot), None) => {
                    self.check_thresholds("", "Heartbeat", &AppRoute::default(), snapshot, now);
                }
                // A paused app is in maintenance: sampled, but it doesn't alert.
                (Ok(snapshot), Some(app)) if !app.paused => {
                    self.check_thresholds(&app.slug, &app.name, &app.alert_route(), snapshot, now);
                }
                (Err(e), Some(app)) => {
                    tracing::warn!(app = %app.slug, error = %e, "system: could not read the system URL");
                }
                (Err(e), None) => tracing::error!(error = %e, "system: could not sample this host"),
                (Ok(_), Some(_)) => {}
            }
            let source = app.map_or_else(|| HOST.to_string(), |a| a.slug);
            let (snapshot, error) = match reading {
                Ok(s) => (Some(s), None),
                Err(e) => (None, Some(e)),
            };
            latest.insert(
                source,
                Reading {
                    at: now,
                    snapshot,
                    error,
                },
            );
        }
    }

    /// Updates the watches of one source and sends what crossed (or came back).
    fn check_thresholds(
        &self,
        slug: &str,
        name: &str,
        route: &AppRoute,
        snapshot: &Snapshot,
        now: u64,
    ) {
        let t = self.policy.thresholds;
        let sustain = t.sustain.as_secs();
        let disk = snapshot.fullest_disk();
        // (resource, value, threshold, sustain, detail)
        let checks: [Check; 3] = [
            (
                "cpu",
                Some(snapshot.cpu.usage_pct),
                t.cpu_pct,
                sustain,
                None,
            ),
            (
                "memory",
                Some(snapshot.memory_pct()),
                t.memory_pct,
                sustain,
                None,
            ),
            (
                "disk",
                disk.map(|d| pct(d.used_bytes, d.total_bytes)),
                t.disk_pct,
                0,
                disk.map(|d| d.mount.clone()),
            ),
        ];
        let mut watches = self.watches.lock().unwrap_or_else(PoisonError::into_inner);
        for (resource, value, threshold, sustain, detail) in checks {
            let (Some(value), true) = (value, threshold > 0) else {
                continue;
            };
            let watch = watches.entry((slug.to_string(), resource)).or_default();
            if let Some(started) = watch.observe(value, threshold, sustain, now) {
                tracing::info!(app = %slug, resource, value, started, "system: threshold");
                self.alerter.send_resource(&ResourceAlert {
                    slug: slug.to_string(),
                    name: name.to_string(),
                    route: route.clone(),
                    resource,
                    detail,
                    pct: value,
                    threshold,
                    started,
                });
            }
        }
    }

    /// The latest reading of `source` (`HOST` or an app's slug).
    pub async fn latest(&self, source: &str) -> Option<Reading> {
        self.latest.read().await.get(source).cloned()
    }

    /// The latest reading of every app that has one.
    pub async fn latest_all(&self) -> HashMap<String, Reading> {
        self.latest.read().await.clone()
    }

    /// `source`'s samples over the last `window`, at most `MAX_CHART_POINTS`.
    pub async fn samples(&self, source: &str, window: Duration) -> Result<Vec<Sample>, DbError> {
        let now = unix_now();
        let cutoff = now.saturating_sub(window.as_secs());
        let source = source.to_string();
        let rows = self
            .db
            .read(move |conn| since(conn, &source, cutoff))
            .await?;
        Ok(downsampled(rows, cutoff, now, MAX_CHART_POINTS))
    }

    async fn prune(&self, now: u64) -> Result<usize, DbError> {
        let cutoff =
            i64::try_from(now.saturating_sub(self.policy.retention.as_secs())).unwrap_or(i64::MAX);
        self.db
            .write(move |tx| Ok(tx.execute("DELETE FROM system_samples WHERE at < ?1", [cutoff])?))
            .await
    }
}

/// Reads an app's system URL: same outbound policy and key as the logs proxy.
async fn fetch(outbound: &Outbound, key: Option<&str>, raw: &str) -> Result<Snapshot, String> {
    let url = outbound.check(raw).map_err(|e| e.to_string())?;
    #[cfg(feature = "demo")]
    if let Some(snapshot) = crate::demo::system(&url) {
        return Ok(snapshot);
    }
    let mut request = outbound.client().get(url).timeout(FETCH_TIMEOUT);
    if let Some(key) = key {
        request = request.header("X-Admin-Logs-Key", key);
    }
    let mut resp = request
        .send()
        .await
        .map_err(|e| crate::uptime::error_chain(&e))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > BODY_LIMIT {
            return Err(format!("the answer is over {} KB", BODY_LIMIT / 1024));
        }
    }
    let snapshot: Snapshot =
        serde_json::from_slice(&body).map_err(|e| format!("not the expected JSON: {e}"))?;
    snapshot
        .sanitized()
        .ok_or_else(|| "the answer has no memory total or CPU usage".to_string())
}

fn sql_int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn store(tx: &rusqlite::Transaction<'_>, samples: &[(String, Sample)]) -> Result<(), DbError> {
    let mut stmt = tx.prepare_cached(
        "INSERT INTO system_samples (source, at, cpu_pct, memory_used, memory_total, \
         disk_used, disk_total, load1) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (source, at) DO NOTHING",
    )?;
    for (source, s) in samples {
        stmt.execute(params![
            source,
            sql_int(s.at),
            s.cpu_pct,
            sql_int(s.memory_used),
            sql_int(s.memory_total),
            s.disk_used.map(sql_int),
            s.disk_total.map(sql_int),
            s.load1,
        ])?;
    }
    Ok(())
}

fn since(conn: &rusqlite::Connection, source: &str, cutoff: u64) -> Result<Vec<Sample>, DbError> {
    let mut stmt = conn.prepare_cached(
        "SELECT at, cpu_pct, memory_used, memory_total, disk_used, disk_total, load1 \
         FROM system_samples WHERE source = ?1 AND at >= ?2 ORDER BY at",
    )?;
    let int = |v: i64| u64::try_from(v).unwrap_or(0);
    let rows = stmt
        .query_map(params![source, sql_int(cutoff)], |row| {
            Ok(Sample {
                at: int(row.get(0)?),
                cpu_pct: row.get(1)?,
                memory_used: int(row.get(2)?),
                memory_total: int(row.get(3)?),
                disk_used: row.get::<_, Option<i64>>(4)?.map(int),
                disk_total: row.get::<_, Option<i64>>(5)?.map(int),
                load1: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Averages `rows` into at most `max_points` buckets of equal time (a bucket's memory and
/// disk are its peaks, so a spike never averages away).
fn downsampled(rows: Vec<Sample>, cutoff: u64, now: u64, max_points: usize) -> Vec<Sample> {
    if rows.len() <= max_points {
        return rows;
    }
    let bucket_secs = now
        .saturating_sub(cutoff)
        .max(1)
        .div_ceil(max_points as u64)
        .max(1);
    let mut out: Vec<Sample> = Vec::with_capacity(max_points);
    let mut group: Vec<Sample> = Vec::new();
    let flush = |group: &mut Vec<Sample>, out: &mut Vec<Sample>| {
        let Some(last) = group.last().cloned() else {
            return;
        };
        #[allow(clippy::cast_precision_loss)] // at most a few hundred samples per bucket
        let n = group.len() as f64;
        let load_values: Vec<f64> = group.iter().filter_map(|s| s.load1).collect();
        #[allow(clippy::cast_precision_loss)]
        let load1 = (!load_values.is_empty())
            .then(|| load_values.iter().sum::<f64>() / load_values.len() as f64);
        out.push(Sample {
            at: last.at,
            cpu_pct: group.iter().map(|s| s.cpu_pct).sum::<f64>() / n,
            memory_used: group.iter().map(|s| s.memory_used).max().unwrap_or(0),
            memory_total: last.memory_total,
            disk_used: group.iter().filter_map(|s| s.disk_used).max(),
            disk_total: last.disk_total,
            load1,
        });
        group.clear();
    };
    let mut current = None;
    for row in rows {
        let bucket = row.at.saturating_sub(cutoff) / bucket_secs;
        if current != Some(bucket) {
            flush(&mut group, &mut out);
            current = Some(bucket);
        }
        group.push(row);
    }
    flush(&mut group, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(cpu: f64, mem_used: u64, disk_used: u64) -> Snapshot {
        Snapshot {
            cpu: Cpu {
                usage_pct: cpu,
                cores: Some(2),
                load: Some([0.5, 0.4, 0.3]),
            },
            memory: Memory {
                total_bytes: 1000,
                used_bytes: mem_used,
                available_bytes: None,
            },
            swap: None,
            disks: vec![
                Disk {
                    mount: "/".into(),
                    total_bytes: 1000,
                    used_bytes: 100,
                },
                Disk {
                    mount: "/data".into(),
                    total_bytes: 1000,
                    used_bytes: disk_used,
                },
            ],
            processes: Vec::new(),
            uptime_secs: None,
        }
    }

    #[test]
    fn an_app_answer_is_bounded_and_made_consistent() {
        let raw = serde_json::json!({
            "cpu": { "usage_pct": 250.0 },
            "memory": { "total_bytes": 100, "used_bytes": 500 },
            "disks": (0..40).map(|i| serde_json::json!({
                "mount": format!("/{}", "x".repeat(200)), "total_bytes": 10, "used_bytes": 20 + i,
            })).collect::<Vec<_>>(),
            "processes": (0..100).map(|i| serde_json::json!({
                "pid": i, "name": "p", "cpu_pct": f64::from(i), "memory_bytes": 1,
            })).collect::<Vec<_>>(),
        });
        let s: Snapshot = serde_json::from_value(raw).unwrap();
        let s = s.sanitized().unwrap();
        assert!((s.cpu.usage_pct - 100.0).abs() < f64::EPSILON);
        assert_eq!(s.memory.used_bytes, 100);
        assert_eq!(s.disks.len(), MAX_DISKS);
        assert!(
            s.disks
                .iter()
                .all(|d| d.used_bytes == 10 && d.mount.chars().count() == MAX_NAME_CHARS)
        );
        assert_eq!(s.processes.len(), MAX_PROCESSES);
        assert_eq!(s.processes[0].pid, 99, "busiest first");

        let no_memory: Snapshot = serde_json::from_value(serde_json::json!({
            "cpu": { "usage_pct": 1.0 }, "memory": { "total_bytes": 0, "used_bytes": 0 },
        }))
        .unwrap();
        assert!(no_memory.sanitized().is_none());
    }

    #[test]
    fn watches_wait_for_the_sustain_and_dont_flap_at_the_edge() {
        let mut w = Watch::default();
        assert_eq!(w.observe(95.0, 90, 300, 0), None, "not long enough yet");
        assert_eq!(w.observe(96.0, 90, 300, 200), None);
        assert_eq!(w.observe(97.0, 90, 300, 300), Some(true));
        assert_eq!(w.observe(99.0, 90, 300, 330), None, "alerts once");
        assert_eq!(w.observe(88.0, 90, 300, 360), None, "inside the hysteresis");
        assert_eq!(w.observe(84.0, 90, 300, 390), Some(false));
        assert_eq!(
            w.observe(91.0, 90, 300, 400),
            None,
            "the sustain starts over"
        );

        let mut disk = Watch::default();
        assert_eq!(
            disk.observe(91.0, 90, 0, 0),
            Some(true),
            "a disk alerts at once"
        );
    }

    #[test]
    fn the_fullest_disk_is_the_one_reported() {
        let s = snapshot(10.0, 500, 950);
        assert_eq!(s.fullest_disk().unwrap().mount, "/data");
        let sample = s.sample(42);
        assert_eq!(
            (sample.disk_used, sample.disk_total),
            (Some(950), Some(1000))
        );
        assert!((s.memory_pct() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn long_windows_are_bucketed_keeping_the_peaks() {
        let rows: Vec<Sample> = (0..1000_u64)
            .map(|i| Sample {
                at: i * 10,
                cpu_pct: if i % 2 == 0 { 0.0 } else { 100.0 },
                memory_used: if i == 500 { 900 } else { 100 },
                memory_total: 1000,
                disk_used: Some(i),
                disk_total: Some(1000),
                load1: Some(1.0),
            })
            .collect();
        let points = downsampled(rows, 0, 10_000, 100);
        assert!(points.len() <= 100);
        assert!(
            points.iter().all(|p| (p.cpu_pct - 50.0).abs() < 1.0),
            "cpu averaged"
        );
        assert_eq!(
            points.iter().map(|p| p.memory_used).max(),
            Some(900),
            "spike kept"
        );
        assert_eq!(points.last().unwrap().disk_used, Some(999));
    }

    #[tokio::test]
    async fn samples_are_stored_read_back_and_pruned() {
        let db = Db::open_in_memory();
        let rows = vec![
            ("api".to_string(), snapshot(10.0, 100, 100).sample(1_000)),
            ("api".to_string(), snapshot(20.0, 200, 200).sample(2_000)),
            (HOST.to_string(), snapshot(30.0, 300, 300).sample(2_000)),
        ];
        db.write(move |tx| store(tx, &rows)).await.unwrap();
        let api = db.read(|conn| since(conn, "api", 1_500)).await.unwrap();
        assert_eq!(api.len(), 1);
        assert!((api[0].cpu_pct - 20.0).abs() < f64::EPSILON);
        let deleted = db
            .write(|tx| Ok(tx.execute("DELETE FROM system_samples WHERE at < 1500", [])?))
            .await
            .unwrap();
        assert_eq!(deleted, 1);
    }
}
