//! Uptime monitoring: a background loop hits every registered app's `health_url` on a
//! fixed interval and records one heartbeat per check. History is kept in memory (bounded
//! to `CheckPolicy::retention`) and mirrored to one JSONL file per app -- appended on every
//! check, rewritten ("compacted") on startup and periodically so files don't grow forever.
//!
//! A failed check is retried before it's recorded as down, and every confirmed status
//! change is handed to `alerts` (when webhooks are configured).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::error::Error as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tokio::task::JoinSet;

use crate::alerts::{Alerter, StatusChange};
use crate::outbound::Outbound;
use crate::registry::{AppRegistry, RegisteredApp};

const COMPACT_EVERY: Duration = Duration::from_secs(3600);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// Max bytes of a health response read when looking for `expect_body`.
const BODY_LIMIT: usize = 256 * 1024;
/// Most points the detail endpoint returns; longer windows are aggregated server-side.
const MAX_DETAIL_POINTS: usize = 600;
const DAY_SECS: u64 = 24 * 3600;
/// How many of the latest heartbeats the summary carries for each app's bar strip. The
/// dashboard draws 40; the embed fits as many as its width allows, up to this.
const RECENT_BEATS: usize = 100;
/// How many status changes the summary/detail event tables show.
const MAX_EVENTS: usize = 30;

#[derive(Debug, thiserror::Error)]
pub enum UptimeError {
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

/// Traffic-light state of one check: green, yellow, red.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Up,
    /// Answered 2xx, but slower than `CheckPolicy::degraded_after_ms`.
    Degraded,
    Down,
}

impl Status {
    #[must_use]
    fn classify(success: bool, latency_ms: u32, degraded_after_ms: u32) -> Self {
        match (success, latency_ms > degraded_after_ms) {
            (false, _) => Self::Down,
            (true, true) => Self::Degraded,
            (true, false) => Self::Up,
        }
    }

    /// Ordering for "worst of": down > degraded > up.
    #[must_use]
    fn severity(self) -> u8 {
        match self {
            Self::Up => 0,
            Self::Degraded => 1,
            Self::Down => 2,
        }
    }

    /// Whether the app was serving requests -- slow still counts toward uptime.
    #[must_use]
    fn is_available(self) -> bool {
        match self {
            Self::Up | Self::Degraded => true,
            Self::Down => false,
        }
    }
}

/// How apps are checked and how long their history is kept.
#[derive(Debug, Clone, Copy)]
pub struct CheckPolicy {
    pub interval: Duration,
    /// Global "slow" threshold; an app's own `degraded_after_ms` overrides it.
    pub degraded_after_ms: u32,
    /// Extra attempts (`RETRY_DELAY` apart) before a failed check is recorded as down.
    pub retries: u32,
    /// A certificate expiring within this many days marks the app degraded.
    pub cert_warn_days: u32,
    pub retention: Duration,
}

/// What a single probe needs to know about one app.
struct ProbeTarget {
    url: String,
    degraded_after_ms: u32,
    expect_body: Option<String>,
    cert_warn_days: u32,
}

/// Outcome of one probe: the heartbeat, plus the TLS certificate's expiry when the
/// connection got far enough to see one.
struct ProbeResult {
    beat: Heartbeat,
    cert_expires_at: Option<u64>,
}

/// Unix-seconds expiry (`notAfter`) of a DER-encoded X.509 certificate.
fn cert_not_after(der: &[u8]) -> Option<u64> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    u64::try_from(cert.validity().not_after.timestamp()).ok()
}

/// Reads at most `BODY_LIMIT` bytes of the body, lossily as text.
async fn read_body_prefix(mut resp: reqwest::Response) -> String {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        body.extend_from_slice(&chunk);
        if body.len() >= BODY_LIMIT {
            body.truncate(BODY_LIMIT);
            break;
        }
    }
    String::from_utf8_lossy(&body).into_owned()
}

fn error_chain(e: &reqwest::Error) -> String {
    // reqwest's top-level message is just "error sending request" -- the useful part
    // (connection refused, dns, timeout) lives down the source chain.
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
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
    fn down(message: String, latency_ms: Option<u32>) -> Self {
        Self {
            at: unix_now(),
            status: Status::Down,
            latency_ms,
            message,
        }
    }
}

impl ProbeTarget {
    async fn probe(&self, outbound: &Outbound) -> ProbeResult {
        // Re-checked on every probe, not just at registration: entries saved before the
        // allowlist existed, or before ALLOWED_HOSTS was narrowed, must not slip through.
        let url = match outbound.check(&self.url) {
            Ok(url) => url,
            Err(e) => {
                return ProbeResult {
                    beat: Heartbeat::down(format!("URL de health no permitida: {e}"), None),
                    cert_expires_at: None,
                };
            }
        };
        let started = Instant::now();
        let result = outbound
            .client()
            .get(url)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await;
        let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let resp = match result {
            Ok(resp) => resp,
            Err(e) => {
                return ProbeResult {
                    beat: Heartbeat::down(error_chain(&e), None),
                    cert_expires_at: None,
                };
            }
        };

        let cert_expires_at = resp
            .extensions()
            .get::<reqwest::tls::TlsInfo>()
            .and_then(reqwest::tls::TlsInfo::peer_certificate)
            .and_then(cert_not_after);
        let code = resp.status();
        let mut status = Status::classify(code.is_success(), latency_ms, self.degraded_after_ms);
        let mut message = format!("HTTP {code}");

        if status != Status::Down
            && let Some(expected) = &self.expect_body
            && !read_body_prefix(resp).await.contains(expected.as_str())
        {
            status = Status::Down;
            message = format!("La respuesta no contiene «{expected}»");
        }
        if status == Status::Up
            && let Some(expires) = cert_expires_at
        {
            let days_left = expires.saturating_sub(unix_now()) / DAY_SECS;
            if days_left < u64::from(self.cert_warn_days) {
                status = Status::Degraded;
                message = format!("El certificado vence en {days_left} días");
            }
        }

        ProbeResult {
            beat: Heartbeat {
                at: unix_now(),
                status,
                latency_ms: Some(latency_ms),
                message,
            },
            cert_expires_at,
        }
    }

    /// Probes, retrying a failure up to `retries` times before accepting it: one dropped
    /// packet shouldn't paint an app red (or page anyone).
    async fn probe_confirmed(&self, outbound: &Outbound, retries: u32) -> ProbeResult {
        let mut result = self.probe(outbound).await;
        for attempt in 1..=retries {
            if result.beat.status != Status::Down {
                break;
            }
            tokio::time::sleep(RETRY_DELAY).await;
            result = self.probe(outbound).await;
            if result.beat.status == Status::Down && attempt == retries {
                result.beat.message = format!("{} ({} intentos)", result.beat.message, retries + 1);
            }
        }
        result
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
    fn push(&mut self, beat: Heartbeat, retention: Duration) {
        self.0.push_back(beat);
        self.prune(unix_now(), retention);
    }

    fn prune(&mut self, now: u64, retention: Duration) {
        let cutoff = now.saturating_sub(retention.as_secs());
        while self.0.front().is_some_and(|b| b.at < cutoff) {
            self.0.pop_front();
        }
    }

    fn since(&self, cutoff: u64) -> impl Iterator<Item = &Heartbeat> {
        self.0.iter().filter(move |b| b.at >= cutoff)
    }

    /// Share of available (up or degraded) heartbeats since `cutoff`, as a percentage.
    /// `None` with no data.
    fn uptime_pct(&self, cutoff: u64) -> Option<f64> {
        let (up, total) = self.since(cutoff).fold((0u32, 0u32), |(up, total), b| {
            (up + u32::from(b.status.is_available()), total + 1)
        });
        (total > 0).then(|| f64::from(up) * 100.0 / f64::from(total))
    }

    fn avg_latency_ms(&self, cutoff: u64) -> Option<u32> {
        let (sum, n) = self
            .since(cutoff)
            .filter(|b| b.status.is_available())
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

    fn summary(
        &self,
        app: &RegisteredApp,
        now: u64,
        cert_expires_at: Option<u64>,
    ) -> MonitorSummary {
        let last = self.0.back();
        let day_ago = now.saturating_sub(DAY_SECS);
        MonitorSummary {
            slug: app.slug.clone(),
            name: app.name.clone(),
            health_url: app.health_url.clone(),
            paused: app.paused,
            status: last.map(|b| b.status),
            latency_ms: last.and_then(|b| b.latency_ms),
            avg_latency_24h_ms: self.avg_latency_ms(day_ago),
            uptime_24h: self.uptime_pct(day_ago),
            uptime_30d: self.uptime_pct(now.saturating_sub(30 * DAY_SECS)),
            cert_expires_at,
            recent: self.recent(RECENT_BEATS),
        }
    }

    /// Beats since `cutoff`, aggregated into at most `max_points` time buckets when there
    /// are more: each bucket reports its worst status (with that beat's message) and the
    /// average latency of its available beats -- so an outage never averages away.
    fn downsampled(&self, cutoff: u64, now: u64, max_points: usize) -> Vec<Heartbeat> {
        let beats: Vec<&Heartbeat> = self.since(cutoff).collect();
        if beats.len() <= max_points {
            return beats.into_iter().cloned().collect();
        }
        let span = now.saturating_sub(cutoff).max(1);
        let bucket_secs = span.div_ceil(max_points as u64).max(1);
        let mut out: Vec<Heartbeat> = Vec::with_capacity(max_points);
        let mut current: Option<(u64, Heartbeat, u64, u64)> = None; // (bucket, worst, sum, n)
        let flush = |out: &mut Vec<Heartbeat>,
                     (bucket, mut worst, sum, n): (u64, Heartbeat, u64, u64)| {
            worst.at = cutoff + bucket * bucket_secs + bucket_secs / 2;
            worst.latency_ms = (n > 0).then(|| u32::try_from(sum / n).unwrap_or(u32::MAX));
            out.push(worst);
        };
        for beat in beats {
            let bucket = beat.at.saturating_sub(cutoff) / bucket_secs;
            let latency = beat.latency_ms.filter(|_| beat.status.is_available());
            match &mut current {
                Some((b, worst, sum, n)) if *b == bucket => {
                    if beat.status.severity() > worst.status.severity() {
                        *worst = beat.clone();
                    }
                    if let Some(ms) = latency {
                        *sum += u64::from(ms);
                        *n += 1;
                    }
                }
                _ => {
                    if let Some(done) = current.take() {
                        flush(&mut out, done);
                    }
                    let (sum, n) = latency.map_or((0, 0), |ms| (u64::from(ms), 1));
                    current = Some((bucket, beat.clone(), sum, n));
                }
            }
        }
        if let Some(done) = current {
            flush(&mut out, done);
        }
        out
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
    /// Paused apps aren't probed; `status` is then the last reading before the pause.
    pub paused: bool,
    /// Status of the latest heartbeat; `None` = no health URL or not checked yet.
    pub status: Option<Status>,
    pub latency_ms: Option<u32>,
    pub avg_latency_24h_ms: Option<u32>,
    pub uptime_24h: Option<f64>,
    pub uptime_30d: Option<f64>,
    /// Unix seconds when the health endpoint's TLS certificate expires (last check).
    pub cert_expires_at: Option<u64>,
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
    pub degraded_after_ms: u32,
    pub monitors: Vec<MonitorSummary>,
    /// Status changes across every app, newest first.
    pub events: Vec<AppEvent>,
}

#[derive(Debug, Serialize)]
pub struct MonitorDetail {
    pub beats: Vec<Heartbeat>,
    pub events: Vec<Heartbeat>,
}

/// Where a monitor reports to, besides its own dashboard.
#[derive(Clone, Default)]
pub struct Notifiers {
    pub alerter: Option<Alerter>,
    /// Pinged after every round of checks, so an external dead man's switch notices when
    /// this monitor itself stops (e.g. a healthchecks.io URL).
    pub ping_url: Option<String>,
}

pub struct UptimeMonitor {
    dir: PathBuf,
    policy: CheckPolicy,
    outbound: Outbound,
    notifiers: Notifiers,
    histories: RwLock<HashMap<String, History>>,
    /// Latest TLS certificate expiry per app (memory only; refreshed every check).
    certs: RwLock<HashMap<String, u64>>,
}

impl UptimeMonitor {
    /// Loads (and compacts) every `<slug>.jsonl` found in `dir`. Malformed lines are skipped
    /// with a warning rather than failing startup over one torn write.
    pub async fn load(
        dir: impl Into<PathBuf>,
        policy: CheckPolicy,
        outbound: Outbound,
        notifiers: Notifiers,
    ) -> Result<Self, UptimeError> {
        let dir = dir.into();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(UptimeError::io(&dir))?;

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
            history.prune(unix_now(), policy.retention);
            histories.insert(slug.to_string(), history);
        }

        let monitor = Self {
            dir,
            policy,
            outbound,
            notifiers,
            histories: RwLock::new(histories),
            certs: RwLock::default(),
        };
        monitor.compact_all().await?;
        Ok(monitor)
    }

    fn file_for(&self, slug: &str) -> PathBuf {
        self.dir.join(format!("{slug}.jsonl"))
    }

    /// Runs forever: one check round per interval, compaction once an hour.
    pub async fn run(self: Arc<Self>, registry: Arc<AppRegistry>) {
        let mut ticker = tokio::time::interval(self.policy.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_compact = Instant::now();
        let ping_client = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .ok();
        loop {
            ticker.tick().await;
            self.check_all(&registry.list().await).await;
            if let (Some(url), Some(client)) = (&self.notifiers.ping_url, &ping_client)
                && let Err(e) = client
                    .get(url)
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status)
            {
                tracing::warn!(error = %e, "uptime: dead man's switch ping failed");
            }
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
        for app in apps.iter().filter(|a| !a.paused) {
            let Some(url) = app.health_url.clone() else {
                continue;
            };
            let target = ProbeTarget {
                url,
                degraded_after_ms: app
                    .degraded_after_ms
                    .unwrap_or(self.policy.degraded_after_ms),
                expect_body: app.expect_body.clone(),
                cert_warn_days: self.policy.cert_warn_days,
            };
            let outbound = self.outbound.clone();
            let retries = self.policy.retries;
            let (slug, name) = (app.slug.clone(), app.name.clone());
            probes.spawn(async move {
                let result = target.probe_confirmed(&outbound, retries).await;
                (slug, name, result)
            });
        }
        while let Some(joined) = probes.join_next().await {
            let (slug, name, result) = match joined {
                Ok(done) => done,
                Err(e) => {
                    tracing::error!(error = %e, "uptime: probe task panicked");
                    continue;
                }
            };
            if let Some(expires) = result.cert_expires_at {
                self.certs.write().await.insert(slug.clone(), expires);
            }
            let beat = result.beat;
            match self.record(&slug, beat.clone()).await {
                Ok(Some(previous)) => self.maybe_alert(&slug, &name, previous, beat),
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(app = %slug, error = %e, "uptime: failed to persist heartbeat");
                }
            }
        }
    }

    fn maybe_alert(&self, slug: &str, name: &str, previous: Status, beat: Heartbeat) {
        let Some(alerter) = &self.notifiers.alerter else {
            return;
        };
        if alerter.worth_alerting(previous, beat.status) {
            alerter.send(&StatusChange {
                slug: slug.to_string(),
                name: name.to_string(),
                from: previous,
                to: beat.status,
                beat,
            });
        }
    }

    /// Stores a heartbeat; returns the status it follows (`None` for an app's first ever).
    async fn record(&self, slug: &str, beat: Heartbeat) -> Result<Option<Status>, UptimeError> {
        match beat.status {
            Status::Down => {
                tracing::warn!(app = %slug, message = %beat.message, "uptime: app is down");
            }
            Status::Degraded => {
                tracing::warn!(app = %slug, latency_ms = ?beat.latency_ms, "uptime: app is slow");
            }
            Status::Up => {}
        }
        let mut line = serde_json::to_string(&beat)?;
        line.push('\n');
        let previous = {
            let mut histories = self.histories.write().await;
            let history = histories.entry(slug.to_string()).or_default();
            let previous = history.0.back().map(|b| b.status);
            history.push(beat, self.policy.retention);
            previous
        };

        let path = self.file_for(slug);
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(UptimeError::io(&path))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(UptimeError::io(&path))?;
        Ok(previous)
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
                    history.prune(now, self.policy.retention);
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
        let certs = self.certs.read().await;
        let now = unix_now();
        let empty = History::default();

        let mut events = Vec::new();
        let monitors = apps
            .iter()
            .map(|app| {
                let history = histories.get(&app.slug).unwrap_or(&empty);
                events.extend(history.events().into_iter().map(|beat| AppEvent {
                    slug: app.slug.clone(),
                    name: app.name.clone(),
                    beat,
                }));
                history.summary(app, now, certs.get(&app.slug).copied())
            })
            .collect();
        events.sort_by_key(|e| std::cmp::Reverse(e.beat.at));
        events.truncate(MAX_EVENTS);

        Overview {
            interval_secs: self.policy.interval.as_secs(),
            degraded_after_ms: self.policy.degraded_after_ms,
            monitors,
            events,
        }
    }

    /// One app's summary -- what the public embed endpoint serves.
    pub async fn summary(&self, app: &RegisteredApp) -> MonitorSummary {
        let histories = self.histories.read().await;
        let cert = self.certs.read().await.get(&app.slug).copied();
        histories
            .get(&app.slug)
            .unwrap_or(&History::default())
            .summary(app, unix_now(), cert)
    }

    pub async fn detail(&self, slug: &str, window: Duration) -> MonitorDetail {
        let histories = self.histories.read().await;
        let Some(history) = histories.get(slug) else {
            return MonitorDetail {
                beats: Vec::new(),
                events: Vec::new(),
            };
        };
        let now = unix_now();
        let cutoff = now.saturating_sub(window.as_secs());
        MonitorDetail {
            beats: history.downsampled(cutoff, now, MAX_DETAIL_POINTS),
            events: history.events(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RETENTION: Duration = Duration::from_hours(720);

    fn outbound() -> Outbound {
        Outbound::new("*.example.com").unwrap()
    }

    fn policy() -> CheckPolicy {
        CheckPolicy {
            interval: Duration::from_secs(60),
            degraded_after_ms: 1000,
            retries: 0,
            cert_warn_days: 14,
            retention: RETENTION,
        }
    }

    fn target(url: &str) -> ProbeTarget {
        ProbeTarget {
            url: url.to_string(),
            degraded_after_ms: 1000,
            expect_body: None,
            cert_warn_days: 14,
        }
    }

    #[tokio::test]
    async fn probe_refuses_urls_outside_the_policy_without_sending() {
        for url in [
            "http://127.0.0.1:1/health",
            "https://169.254.169.254/latest/meta-data/",
        ] {
            let beat = target(url).probe(&outbound()).await.beat;
            assert_eq!(beat.status, Status::Down);
            assert!(
                beat.message.starts_with("URL de health no permitida"),
                "{url}"
            );
        }
    }

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
            beat(30, Status::Degraded, Some(1400)),
            beat(40, Status::Down, Some(900)),
        ]));
        // Degraded still counts as available.
        assert_eq!(history.uptime_pct(0), Some(50.0));
        assert_eq!(history.uptime_pct(20), Some(200.0 / 3.0));
        // Down heartbeats' latency is excluded from the average; degraded ones are not.
        assert_eq!(history.avg_latency_ms(0), Some(750));
        assert_eq!(History::default().uptime_pct(0), None);
    }

    #[test]
    fn classify_is_a_traffic_light() {
        assert_eq!(Status::classify(true, 200, 1000), Status::Up);
        assert_eq!(Status::classify(true, 1000, 1000), Status::Up);
        assert_eq!(Status::classify(true, 1001, 1000), Status::Degraded);
        assert_eq!(Status::classify(false, 50, 1000), Status::Down);
        assert_eq!(Status::classify(false, 5000, 1000), Status::Down);
    }

    #[test]
    fn events_are_status_flips_newest_first() {
        let history = History(VecDeque::from([
            beat(1, Status::Up, None),
            beat(2, Status::Up, None),
            beat(3, Status::Down, None),
            beat(4, Status::Degraded, None),
            beat(5, Status::Up, None),
        ]));
        let ats: Vec<u64> = history.events().iter().map(|b| b.at).collect();
        assert_eq!(ats, [5, 4, 3, 1]);
    }

    #[test]
    fn prune_drops_heartbeats_past_retention() {
        let now = RETENTION.as_secs() + 1000;
        let mut history = History(VecDeque::from([
            beat(500, Status::Up, None),
            beat(1500, Status::Up, None),
        ]));
        history.prune(now, RETENTION);
        assert_eq!(history.0.len(), 1);
        assert_eq!(history.0[0].at, 1500);
    }

    #[tokio::test]
    async fn records_persist_across_a_reload_and_forget_removes_them() {
        let dir = tempfile::tempdir().unwrap();
        {
            let monitor =
                UptimeMonitor::load(dir.path(), policy(), outbound(), Notifiers::default())
                    .await
                    .unwrap();
            let first = monitor
                .record("app", beat(unix_now(), Status::Up, Some(12)))
                .await
                .unwrap();
            assert_eq!(first, None, "an app's first heartbeat has no predecessor");
            let second = monitor
                .record("app", beat(unix_now(), Status::Down, None))
                .await
                .unwrap();
            assert_eq!(second, Some(Status::Up));
        }
        let monitor = UptimeMonitor::load(dir.path(), policy(), outbound(), Notifiers::default())
            .await
            .unwrap();
        assert_eq!(
            monitor
                .detail("app", Duration::from_secs(3600))
                .await
                .beats
                .len(),
            2
        );
        monitor.forget("app").await.unwrap();
        assert!(!dir.path().join("app.jsonl").exists());
        assert!(monitor.detail("app", RETENTION).await.beats.is_empty());
    }

    #[test]
    fn downsampling_keeps_the_worst_status_and_averages_latency() {
        // 1000 one-second beats, one outage at t=500: 10 buckets of 100 beats each.
        let history = History(
            (0..1000)
                .map(|t| {
                    if t == 500 {
                        beat(t, Status::Down, None)
                    } else {
                        beat(
                            t,
                            Status::Up,
                            Some(100 + u32::try_from(t % 2).unwrap() * 100),
                        )
                    }
                })
                .collect(),
        );
        let points = history.downsampled(0, 1000, 10);
        assert_eq!(points.len(), 10);
        assert_eq!(
            points.iter().filter(|b| b.status == Status::Down).count(),
            1
        );
        assert_eq!(
            points[5].status,
            Status::Down,
            "the outage survives aggregation"
        );
        assert_eq!(points[0].latency_ms, Some(150));
        // Few beats: returned as-is.
        assert_eq!(history.downsampled(990, 1000, 600).len(), 10);
    }

    #[tokio::test]
    async fn retries_are_skipped_when_the_first_probe_is_final() {
        // A URL outside the policy fails instantly; with retries = 0 there's no sleep and
        // no "(N intentos)" suffix.
        let result = target("http://127.0.0.1:1/")
            .probe_confirmed(&outbound(), 0)
            .await;
        assert_eq!(result.beat.status, Status::Down);
        assert!(!result.beat.message.contains("intentos"));
    }
}
