//! Uptime monitoring: a background loop hits every registered app's `health_url` on a
//! fixed interval and records one heartbeat per check. History is kept in memory (bounded
//! to `RETENTION`) and mirrored to one JSONL file per app -- appended on every check,
//! rewritten ("compacted") on startup and periodically so the files don't grow forever.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::error::Error as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tokio::task::JoinSet;

use crate::registry::{AppRegistry, RegisteredApp};

const RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);
const COMPACT_EVERY: Duration = Duration::from_secs(3600);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How many of the latest heartbeats the summary carries for each app's bar strip.
const RECENT_BEATS: usize = 40;
/// How many status changes the summary/detail event tables show.
const MAX_EVENTS: usize = 30;

#[derive(Debug, thiserror::Error)]
pub enum UptimeError {
    #[error("no se pudo crear el cliente HTTP: {0}")]
    Client(#[from] reqwest::Error),
    #[error("error de E/S en {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("no se pudo serializar un heartbeat: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl UptimeError {
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        move |source| Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Unix seconds.
    pub at: u64,
    pub status: Status,
    /// `None` when the request never got a response (DNS, connect, timeout).
    pub latency_ms: Option<u32>,
    pub message: String,
}

impl Heartbeat {
    async fn probe(client: &reqwest::Client, url: &str) -> Self {
        let started = Instant::now();
        let result = client.get(url).send().await;
        let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let at = unix_now();
        match result {
            Ok(resp) => {
                let code = resp.status();
                Self {
                    at,
                    status: if code.is_success() {
                        Status::Up
                    } else {
                        Status::Down
                    },
                    latency_ms: Some(latency_ms),
                    message: format!("HTTP {code}"),
                }
            }
            Err(e) => {
                // reqwest's top-level message is just "error sending request" -- the useful
                // part (connection refused, dns, timeout) lives down the source chain.
                let mut message = e.to_string();
                let mut source = e.source();
                while let Some(cause) = source {
                    message.push_str(": ");
                    message.push_str(&cause.to_string());
                    source = cause.source();
                }
                Self {
                    at,
                    status: Status::Down,
                    latency_ms: None,
                    message,
                }
            }
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// One app's heartbeat history, oldest first.
#[derive(Debug, Default)]
struct History(VecDeque<Heartbeat>);

impl History {
    fn push(&mut self, beat: Heartbeat) {
        self.0.push_back(beat);
        self.prune(unix_now());
    }

    fn prune(&mut self, now: u64) {
        let cutoff = now.saturating_sub(RETENTION.as_secs());
        while self.0.front().is_some_and(|b| b.at < cutoff) {
            self.0.pop_front();
        }
    }

    fn since(&self, cutoff: u64) -> impl Iterator<Item = &Heartbeat> {
        self.0.iter().filter(move |b| b.at >= cutoff)
    }

    /// Share of `Up` heartbeats since `cutoff`, as a percentage. `None` with no data.
    fn uptime_pct(&self, cutoff: u64) -> Option<f64> {
        let (up, total) = self.since(cutoff).fold((0u32, 0u32), |(up, total), b| {
            (up + u32::from(b.status == Status::Up), total + 1)
        });
        (total > 0).then(|| f64::from(up) * 100.0 / f64::from(total))
    }

    fn avg_latency_ms(&self, cutoff: u64) -> Option<u32> {
        let (sum, n) = self
            .since(cutoff)
            .filter(|b| b.status == Status::Up)
            .filter_map(|b| b.latency_ms)
            .fold((0u64, 0u64), |(sum, n), ms| (sum + u64::from(ms), n + 1));
        (n > 0).then(|| u32::try_from(sum / n).unwrap_or(u32::MAX))
    }

    /// Heartbeats where the status flipped (the first one counts), newest first.
    fn events(&self) -> Vec<Heartbeat> {
        let mut prev = None;
        let mut events: Vec<Heartbeat> = self
            .0
            .iter()
            .filter(|b| {
                let changed = prev != Some(b.status);
                prev = Some(b.status);
                changed
            })
            .cloned()
            .collect();
        events.reverse();
        events.truncate(MAX_EVENTS);
        events
    }

    fn recent(&self, n: usize) -> Vec<Heartbeat> {
        self.0
            .iter()
            .skip(self.0.len().saturating_sub(n))
            .cloned()
            .collect()
    }
}

#[derive(Debug, Serialize)]
pub struct MonitorSummary {
    pub slug: String,
    pub name: String,
    pub health_url: Option<String>,
    /// Status of the latest heartbeat; `None` = no health URL or not checked yet.
    pub status: Option<Status>,
    pub latency_ms: Option<u32>,
    pub avg_latency_24h_ms: Option<u32>,
    pub uptime_24h: Option<f64>,
    pub uptime_30d: Option<f64>,
    pub recent: Vec<Heartbeat>,
}

#[derive(Debug, Serialize)]
pub struct AppEvent {
    pub slug: String,
    pub name: String,
    #[serde(flatten)]
    pub beat: Heartbeat,
}

#[derive(Debug, Serialize)]
pub struct Overview {
    pub interval_secs: u64,
    pub monitors: Vec<MonitorSummary>,
    /// Status changes across every app, newest first.
    pub events: Vec<AppEvent>,
}

#[derive(Debug, Serialize)]
pub struct MonitorDetail {
    pub beats: Vec<Heartbeat>,
    pub events: Vec<Heartbeat>,
}

pub struct UptimeMonitor {
    dir: PathBuf,
    interval: Duration,
    client: reqwest::Client,
    histories: RwLock<HashMap<String, History>>,
}

impl UptimeMonitor {
    /// Loads (and compacts) every `<slug>.jsonl` found in `dir`. Malformed lines are skipped
    /// with a warning rather than failing startup over one torn write.
    pub async fn load(dir: impl Into<PathBuf>, interval: Duration) -> Result<Self, UptimeError> {
        let dir = dir.into();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(UptimeError::io(&dir))?;
        let client = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .user_agent("adquiere-logs-uptime")
            .build()?;

        let mut histories = HashMap::new();
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(UptimeError::io(&dir))?;
        while let Some(entry) = entries.next_entry().await.map_err(UptimeError::io(&dir))? {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let raw = tokio::fs::read_to_string(&path)
                .await
                .map_err(UptimeError::io(&path))?;
            let mut history = History(
                raw.lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| match serde_json::from_str(l) {
                        Ok(beat) => Some(beat),
                        Err(e) => {
                            tracing::warn!(file = %path.display(), error = %e, "uptime: skipping malformed heartbeat");
                            None
                        }
                    })
                    .collect(),
            );
            history.prune(unix_now());
            histories.insert(slug.to_string(), history);
        }

        let monitor = Self {
            dir,
            interval,
            client,
            histories: RwLock::new(histories),
        };
        monitor.compact_all().await?;
        Ok(monitor)
    }

    fn file_for(&self, slug: &str) -> PathBuf {
        self.dir.join(format!("{slug}.jsonl"))
    }

    /// Runs forever: one check round per interval, compaction once an hour.
    pub async fn run(self: Arc<Self>, registry: Arc<AppRegistry>) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_compact = Instant::now();
        loop {
            ticker.tick().await;
            self.check_all(&registry.list().await).await;
            if last_compact.elapsed() >= COMPACT_EVERY {
                if let Err(e) = self.compact_all().await {
                    tracing::error!(error = %e, "uptime: compaction failed");
                }
                last_compact = Instant::now();
            }
        }
    }

    async fn check_all(&self, apps: &[RegisteredApp]) {
        let mut probes = JoinSet::new();
        for app in apps {
            let Some(url) = app.health_url.clone() else {
                continue;
            };
            let client = self.client.clone();
            let slug = app.slug.clone();
            probes.spawn(async move { (slug, Heartbeat::probe(&client, &url).await) });
        }
        while let Some(joined) = probes.join_next().await {
            match joined {
                Ok((slug, beat)) => {
                    if let Err(e) = self.record(&slug, beat).await {
                        tracing::error!(app = %slug, error = %e, "uptime: failed to persist heartbeat");
                    }
                }
                Err(e) => tracing::error!(error = %e, "uptime: probe task panicked"),
            }
        }
    }

    async fn record(&self, slug: &str, beat: Heartbeat) -> Result<(), UptimeError> {
        if beat.status == Status::Down {
            tracing::warn!(app = %slug, message = %beat.message, "uptime: app is down");
        }
        let mut line = serde_json::to_string(&beat)?;
        line.push('\n');
        self.histories
            .write()
            .await
            .entry(slug.to_string())
            .or_default()
            .push(beat);

        let path = self.file_for(slug);
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(UptimeError::io(&path))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(UptimeError::io(&path))
    }

    /// Rewrites every app's file with only its retained heartbeats (temp file + rename, so a
    /// crash mid-write never leaves a truncated history).
    async fn compact_all(&self) -> Result<(), UptimeError> {
        let snapshots: Vec<(String, String)> = {
            let mut histories = self.histories.write().await;
            let now = unix_now();
            histories
                .iter_mut()
                .map(|(slug, history)| {
                    history.prune(now);
                    let body = history
                        .0
                        .iter()
                        .map(|b| serde_json::to_string(b).map(|l| l + "\n"))
                        .collect::<Result<String, _>>()?;
                    Ok((slug.clone(), body))
                })
                .collect::<Result<_, UptimeError>>()?
        };
        for (slug, body) in snapshots {
            let path = self.file_for(&slug);
            let tmp = path.with_extension("jsonl.tmp");
            tokio::fs::write(&tmp, body)
                .await
                .map_err(UptimeError::io(&tmp))?;
            tokio::fs::rename(&tmp, &path)
                .await
                .map_err(UptimeError::io(&path))?;
        }
        Ok(())
    }

    /// Drops an app's history (memory + file) -- called when the app is unregistered, so a
    /// later registration under the same slug starts clean.
    pub async fn forget(&self, slug: &str) -> Result<(), UptimeError> {
        self.histories.write().await.remove(slug);
        let path = self.file_for(slug);
        match tokio::fs::remove_file(&path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(UptimeError::io(&path)(e)),
            _ => Ok(()),
        }
    }

    pub async fn overview(&self, apps: &[RegisteredApp]) -> Overview {
        let histories = self.histories.read().await;
        let now = unix_now();
        let day_ago = now.saturating_sub(24 * 3600);
        let empty = History::default();

        let mut events = Vec::new();
        let monitors = apps
            .iter()
            .map(|app| {
                let history = histories.get(&app.slug).unwrap_or(&empty);
                let last = history.0.back();
                events.extend(history.events().into_iter().map(|beat| AppEvent {
                    slug: app.slug.clone(),
                    name: app.name.clone(),
                    beat,
                }));
                MonitorSummary {
                    slug: app.slug.clone(),
                    name: app.name.clone(),
                    health_url: app.health_url.clone(),
                    status: last.map(|b| b.status),
                    latency_ms: last.and_then(|b| b.latency_ms),
                    avg_latency_24h_ms: history.avg_latency_ms(day_ago),
                    uptime_24h: history.uptime_pct(day_ago),
                    uptime_30d: history.uptime_pct(0),
                    recent: history.recent(RECENT_BEATS),
                }
            })
            .collect();
        events.sort_by(|a, b| b.beat.at.cmp(&a.beat.at));
        events.truncate(MAX_EVENTS);

        Overview {
            interval_secs: self.interval.as_secs(),
            monitors,
            events,
        }
    }

    pub async fn detail(&self, slug: &str, window: Duration) -> MonitorDetail {
        let histories = self.histories.read().await;
        let Some(history) = histories.get(slug) else {
            return MonitorDetail {
                beats: Vec::new(),
                events: Vec::new(),
            };
        };
        let cutoff = unix_now().saturating_sub(window.as_secs());
        MonitorDetail {
            beats: history.since(cutoff).cloned().collect(),
            events: history.events(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(at: u64, status: Status, latency_ms: Option<u32>) -> Heartbeat {
        Heartbeat {
            at,
            status,
            latency_ms,
            message: String::new(),
        }
    }

    #[test]
    fn uptime_and_avg_latency_only_count_the_window() {
        let history = History(VecDeque::from([
            beat(10, Status::Down, None),
            beat(20, Status::Up, Some(100)),
            beat(30, Status::Up, Some(200)),
            beat(40, Status::Down, Some(900)),
        ]));
        assert_eq!(history.uptime_pct(0), Some(50.0));
        assert_eq!(history.uptime_pct(20), Some(200.0 / 3.0));
        // Down heartbeats' latency is excluded from the average.
        assert_eq!(history.avg_latency_ms(0), Some(150));
        assert_eq!(History::default().uptime_pct(0), None);
    }

    #[test]
    fn events_are_status_flips_newest_first() {
        let history = History(VecDeque::from([
            beat(1, Status::Up, None),
            beat(2, Status::Up, None),
            beat(3, Status::Down, None),
            beat(4, Status::Down, None),
            beat(5, Status::Up, None),
        ]));
        let ats: Vec<u64> = history.events().iter().map(|b| b.at).collect();
        assert_eq!(ats, [5, 3, 1]);
    }

    #[test]
    fn prune_drops_heartbeats_past_retention() {
        let now = RETENTION.as_secs() + 1000;
        let mut history = History(VecDeque::from([
            beat(500, Status::Up, None),
            beat(1500, Status::Up, None),
        ]));
        history.prune(now);
        assert_eq!(history.0.len(), 1);
        assert_eq!(history.0[0].at, 1500);
    }

    #[tokio::test]
    async fn records_persist_across_a_reload_and_forget_removes_them() {
        let dir = tempfile::tempdir().unwrap();
        let interval = Duration::from_secs(60);
        {
            let monitor = UptimeMonitor::load(dir.path(), interval).await.unwrap();
            monitor
                .record("app", beat(unix_now(), Status::Up, Some(12)))
                .await
                .unwrap();
        }
        let monitor = UptimeMonitor::load(dir.path(), interval).await.unwrap();
        assert_eq!(
            monitor
                .detail("app", Duration::from_secs(3600))
                .await
                .beats
                .len(),
            1
        );
        monitor.forget("app").await.unwrap();
        assert!(!dir.path().join("app.jsonl").exists());
        assert!(monitor.detail("app", RETENTION).await.beats.is_empty());
    }
}
