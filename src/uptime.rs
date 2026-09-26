//! Uptime monitoring: a background loop hits every registered app's `health_url` on a
//! fixed interval and records one heartbeat per check. History is kept in memory (bounded
//! to `CheckPolicy::retention`) and mirrored to one JSONL file per app -- appended on every
//! check, rewritten ("compacted") on startup and periodically so files don't grow forever.
//!
//! Each app is checked on its own interval (the global one unless it sets its own), and
//! apps under scheduled maintenance resume by themselves when it ends.
//!
//! A failed check is retried before it's recorded as down, and every confirmed status
//! change goes through the mass-outage gate (`gate`) to `alerts`. While an app stays
//! down, a reminder goes out every `ALERT_REMIND_MINS`.

mod gate;
pub mod incidents;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::error::Error as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tokio::task::JoinSet;

use crate::alerts::{Alerter, StatusChange};
use crate::bundles;
use crate::i18n::{I18n, Lang, Localize};
use crate::outbound::{HealthTarget, Outbound};
use crate::registry::{AppRegistry, CheckHeader, RegisteredApp};
use gate::OutageGate;
use incidents::IncidentReport;

const COMPACT_EVERY: Duration = Duration::from_secs(3600);
/// How often the scheduler looks for apps that are due: the finest per-app interval
/// granularity.
const SCHEDULER_TICK: Duration = Duration::from_secs(5);
/// Timeout of the dead man's switch ping.
const PING_TIMEOUT: Duration = Duration::from_secs(10);
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
/// How many incidents the detail endpoint lists.
const MAX_INCIDENTS: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum UptimeError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not serialize a heartbeat: {0}")]
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
    /// Global check timeout; an app's own `timeout_secs` overrides it.
    pub timeout: Duration,
    /// `UPTIME_MASS_DOWN_PCT`: share of apps down at once that counts as a mass outage
    /// (0 = off, see `gate`).
    pub mass_down_pct: u8,
    /// Language of the check messages (`APP_LANG`): they're shown in the UI and alerts.
    pub lang: Lang,
}

impl CheckPolicy {
    fn interval_for(&self, app: &RegisteredApp) -> Duration {
        app.interval_secs
            .map_or(self.interval, |s| Duration::from_secs(u64::from(s)))
    }
}

/// What a single probe needs to know about one app.
struct ProbeTarget {
    url: String,
    degraded_after_ms: u32,
    expect_body: Option<String>,
    cert_warn_days: u32,
    /// Also verify the page's JS/CSS bundles (single-page apps, see `bundles`).
    check_assets: bool,
    timeout: Duration,
    /// Status codes that count as up; empty = any 2xx.
    expect_status: Vec<u16>,
    headers: Vec<CheckHeader>,
    i18n: I18n,
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
    /// `None` for an app without a health URL.
    fn new(app: &RegisteredApp, policy: &CheckPolicy, i18n: &I18n) -> Option<Self> {
        Some(Self {
            url: app.health_url.clone()?,
            degraded_after_ms: app.degraded_after_ms.unwrap_or(policy.degraded_after_ms),
            expect_body: app.expect_body.clone(),
            check_assets: app.check_assets,
            cert_warn_days: policy.cert_warn_days,
            timeout: app
                .timeout_secs
                .map_or(policy.timeout, |s| Duration::from_secs(u64::from(s))),
            expect_status: app.expect_status.clone(),
            headers: app.headers.clone(),
            i18n: i18n.clone(),
        })
    }

    async fn probe(&self, outbound: &Outbound) -> ProbeResult {
        // Re-checked on every probe, not just at registration: entries saved before the
        // allowlist existed, or before ALLOWED_HOSTS was narrowed, must not slip through.
        match outbound.check_health(&self.url) {
            Ok(HealthTarget::Http(url)) => self.probe_http(outbound, url).await,
            Ok(HealthTarget::Tcp { host, port }) => self.probe_tcp(&host, port).await,
            Err(e) => ProbeResult {
                beat: Heartbeat::down(
                    self.i18n
                        .text("check.url_refused", &[("error", &e.localize(&self.i18n))]),
                    None,
                ),
                cert_expires_at: None,
            },
        }
    }

    /// A TCP check only opens (and drops) a connection: up if it connects in time.
    async fn probe_tcp(&self, host: &str, port: u16) -> ProbeResult {
        let started = Instant::now();
        let result = Outbound::connect_tcp(host, port, self.timeout).await;
        let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let beat = match result {
            Ok(_) => Heartbeat {
                at: unix_now(),
                status: Status::classify(true, latency_ms, self.degraded_after_ms),
                latency_ms: Some(latency_ms),
                message: format!("TCP {host}:{port} OK"),
            },
            Err(e) => Heartbeat::down(format!("TCP {host}:{port}: {e}"), None),
        };
        ProbeResult {
            beat,
            cert_expires_at: None,
        }
    }

    fn accepts(&self, code: reqwest::StatusCode) -> bool {
        if self.expect_status.is_empty() {
            code.is_success()
        } else {
            self.expect_status.contains(&code.as_u16())
        }
    }

    async fn probe_http(&self, outbound: &Outbound, url: reqwest::Url) -> ProbeResult {
        let started = Instant::now();
        let mut request = outbound.client().get(url).timeout(self.timeout);
        for header in &self.headers {
            request = request.header(&header.name, &header.value);
        }
        let result = request.send().await;
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
        let page_url = resp.url().clone();
        let mut status = Status::classify(self.accepts(code), latency_ms, self.degraded_after_ms);
        let mut message = format!("HTTP {code}");

        // The body is only read when something needs it, and only once.
        let body = if status != Status::Down && (self.expect_body.is_some() || self.check_assets) {
            Some(read_body_prefix(resp).await)
        } else {
            None
        };
        if let (Some(expected), Some(body)) = (&self.expect_body, &body)
            && !body.contains(expected.as_str())
        {
            status = Status::Down;
            message = self
                .i18n
                .text("check.keyword_missing", &[("keyword", expected)]);
        }
        if status != Status::Down
            && self.check_assets
            && let Some(body) = &body
            && let Some(why) = bundles::verify(outbound, &bundles::extract(body, &page_url)).await
        {
            status = Status::Down;
            message = why.localize(&self.i18n);
        }
        if status == Status::Up
            && let Some(expires) = cert_expires_at
        {
            let days_left = expires.saturating_sub(unix_now()) / DAY_SECS;
            if days_left < u64::from(self.cert_warn_days) {
                status = Status::Degraded;
                message = self
                    .i18n
                    .text("check.cert_expiring", &[("days", &days_left.to_string())]);
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
                result.beat.message = self.i18n.text(
                    "check.attempts",
                    &[
                        ("message", &result.beat.message),
                        ("n", &(retries + 1).to_string()),
                    ],
                );
            }
        }
        result
    }
}

pub(crate) fn unix_now() -> u64 {
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

    /// When the current run of down checks started; `None` if the last check wasn't down.
    fn down_streak_start(&self) -> Option<u64> {
        self.0
            .iter()
            .rev()
            .take_while(|b| b.status == Status::Down)
            .last()
            .map(|b| b.at)
    }

    /// One entry per UTC day for the last `days` days (oldest first, today last): the share
    /// of available checks that day, or `None` without data.
    fn daily(&self, now: u64, days: u64) -> Vec<DayUptime> {
        let today = now / DAY_SECS;
        let first = today.saturating_sub(days.saturating_sub(1));
        let mut counts = vec![(0u32, 0u32); usize::try_from(today - first + 1).unwrap_or(0)];
        for beat in self.since(first * DAY_SECS) {
            let Ok(i) = usize::try_from(beat.at / DAY_SECS - first) else {
                continue;
            };
            if let Some((up, total)) = counts.get_mut(i) {
                *up += u32::from(beat.status.is_available());
                *total += 1;
            }
        }
        counts
            .into_iter()
            .zip(first..)
            .map(|((up, total), day)| DayUptime {
                start: day * DAY_SECS,
                uptime: (total > 0).then(|| f64::from(up) * 100.0 / f64::from(total)),
            })
            .collect()
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
            has_logs: app.logs_url.is_some(),
            paused: app.paused,
            paused_at: app.paused_at,
            paused_until: app.paused_until,
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
    /// `false` for monitor-only apps (no logs endpoint).
    pub has_logs: bool,
    /// Paused apps aren't probed; `status` is then the last reading before the pause.
    pub paused: bool,
    /// Unix seconds when the current pause began.
    pub paused_at: Option<u64>,
    /// Unix seconds when a scheduled pause ends by itself.
    pub paused_until: Option<u64>,
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

/// One day of an app's history, for the public status page.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DayUptime {
    /// Unix seconds at 00:00 UTC.
    pub start: u64,
    pub uptime: Option<f64>,
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
    /// Outages in the window, with MTTR and total downtime.
    pub incidents: IncidentReport,
}

/// What `record` learned about the heartbeat it stored.
struct Recorded {
    /// Status of the check before it; `None` for an app's first ever.
    previous: Option<Status>,
    /// Start of the outage this check continues or ends, or starts (when down).
    down_since: Option<u64>,
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
    gate: Mutex<OutageGate>,
    i18n: I18n,
    /// When each app that's down was last alerted about (down or reminder).
    reminded: Mutex<HashMap<String, u64>>,
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
            gate: Mutex::default(),
            i18n: I18n::new(policy.lang),
            reminded: Mutex::default(),
        };
        monitor.compact_all().await?;
        Ok(monitor)
    }

    fn file_for(&self, slug: &str) -> PathBuf {
        self.dir.join(format!("{slug}.jsonl"))
    }

    /// Runs forever: every `SCHEDULER_TICK`, ends expired maintenance and checks the apps
    /// that are due; pings the dead man's switch once per global interval and compacts
    /// once an hour.
    pub async fn run(self: Arc<Self>, registry: Arc<AppRegistry>) {
        let mut ticker = tokio::time::interval(SCHEDULER_TICK.min(self.policy.interval));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut next_due: HashMap<String, Instant> = HashMap::new();
        let mut last_ping: Option<Instant> = None;
        let mut last_compact = Instant::now();
        let ping_client = reqwest::Client::builder()
            .timeout(PING_TIMEOUT)
            .build()
            .ok();
        loop {
            ticker.tick().await;
            match registry.resume_expired(unix_now()).await {
                Ok(resumed) => {
                    for slug in resumed {
                        tracing::info!(app = %slug, "uptime: maintenance over, checks resumed");
                    }
                }
                Err(e) => tracing::error!(error = %e, "uptime: could not resume apps"),
            }

            let apps = registry.list().await;
            let now = Instant::now();
            next_due.retain(|slug, _| apps.iter().any(|a| &a.slug == slug));
            let due: Vec<RegisteredApp> = apps
                .iter()
                .filter(|a| a.is_monitored())
                .filter(|a| next_due.get(&a.slug).is_none_or(|at| *at <= now))
                .cloned()
                .collect();
            for app in &due {
                next_due.insert(app.slug.clone(), now + self.policy.interval_for(app));
            }
            if !due.is_empty() {
                self.check_all(&due, &apps).await;
            }

            if last_ping.is_none_or(|at| at.elapsed() >= self.policy.interval) {
                last_ping = Some(Instant::now());
                if let (Some(url), Some(client)) = (&self.notifiers.ping_url, &ping_client)
                    && let Err(e) = client
                        .get(url)
                        .send()
                        .await
                        .and_then(reqwest::Response::error_for_status)
                {
                    tracing::warn!(error = %e, "uptime: dead man's switch ping failed");
                }
            }
            if last_compact.elapsed() >= COMPACT_EVERY {
                if let Err(e) = self.compact_all().await {
                    tracing::error!(error = %e, "uptime: compaction failed");
                }
                last_compact = Instant::now();
            }
        }
    }

    /// Probes `due` concurrently, records each result and alerts; `all` is every
    /// registered app, for the mass-outage count.
    async fn check_all(&self, due: &[RegisteredApp], all: &[RegisteredApp]) {
        let mut probes = JoinSet::new();
        for app in due {
            let Some(target) = ProbeTarget::new(app, &self.policy, &self.i18n) else {
                continue;
            };
            let outbound = self.outbound.clone();
            let retries = self.policy.retries;
            let app = app.clone();
            probes.spawn(async move {
                let result = target.probe_confirmed(&outbound, retries).await;
                (app, result)
            });
        }
        let mut changes = Vec::new();
        while let Some(joined) = probes.join_next().await {
            let (app, result) = match joined {
                Ok(done) => done,
                Err(e) => {
                    tracing::error!(error = %e, "uptime: probe task panicked");
                    continue;
                }
            };
            if let Some(expires) = result.cert_expires_at {
                self.certs.write().await.insert(app.slug.clone(), expires);
            }
            match self.record(&app.slug, result.beat.clone()).await {
                Ok(recorded) => changes.extend(self.alert_for(&app, &recorded, result.beat)),
                Err(e) => {
                    tracing::error!(app = %app.slug, error = %e, "uptime: failed to persist heartbeat");
                }
            }
        }
        self.dispatch(changes, all).await;
    }

    /// The alert this check deserves, if any: a status change, or a reminder that the
    /// app is still down.
    fn alert_for(
        &self,
        app: &RegisteredApp,
        recorded: &Recorded,
        beat: Heartbeat,
    ) -> Option<StatusChange> {
        let alerter = self.notifiers.alerter.as_ref()?;
        let mut reminded = self.reminded.lock().unwrap_or_else(PoisonError::into_inner);
        if beat.status != Status::Down {
            reminded.remove(&app.slug);
        }
        let previous = recorded.previous?;
        let from = if alerter.worth_alerting(previous, beat.status) {
            previous
        } else if previous == Status::Down && beat.status == Status::Down {
            let every = alerter.remind_after()?.as_secs();
            // After a restart nobody knows when the last alert went out: start counting now.
            let last = *reminded.entry(app.slug.clone()).or_insert(beat.at);
            if beat.at.saturating_sub(last) < every {
                return None;
            }
            Status::Down
        } else {
            return None;
        };
        Some(StatusChange {
            slug: app.slug.clone(),
            name: app.name.clone(),
            from,
            to: beat.status,
            beat,
            down_since: recorded.down_since,
            route: app.alert_route(),
        })
    }

    /// Runs the round's alerts through the mass-outage gate and sends what survives.
    async fn dispatch(&self, changes: Vec<StatusChange>, all: &[RegisteredApp]) {
        let Some(alerter) = &self.notifiers.alerter else {
            return;
        };
        let monitored: Vec<&RegisteredApp> = all.iter().filter(|a| a.is_monitored()).collect();
        let latest: HashMap<&str, (Heartbeat, Option<u64>)> = {
            let histories = self.histories.read().await;
            monitored
                .iter()
                .filter_map(|app| {
                    let history = histories.get(&app.slug)?;
                    let last = history.0.back()?.clone();
                    Some((app.slug.as_str(), (last, history.down_streak_start())))
                })
                .collect()
        };
        let down = latest
            .values()
            .filter(|(beat, _)| beat.status == Status::Down)
            .count();
        let review = self
            .gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .review(changes, down, monitored.len(), self.policy.mass_down_pct);

        if let Some(outage) = review.outage {
            tracing::warn!(?outage, "uptime: mass outage");
            alerter.send_mass(outage);
        }
        let mut deliver = review.deliver;
        for slug in review.release {
            let Some(app) = monitored.iter().find(|a| a.slug == slug) else {
                continue;
            };
            if let Some((beat, down_since)) = latest.get(slug.as_str())
                && beat.status == Status::Down
            {
                deliver.push(StatusChange {
                    slug: app.slug.clone(),
                    name: app.name.clone(),
                    from: Status::Up,
                    to: Status::Down,
                    beat: beat.clone(),
                    down_since: *down_since,
                    route: app.alert_route(),
                });
            }
        }
        let mut reminded = self.reminded.lock().unwrap_or_else(PoisonError::into_inner);
        for change in &deliver {
            if change.to == Status::Down {
                reminded.insert(change.slug.clone(), change.beat.at);
            }
            alerter.send(change);
        }
    }

    /// Stores a heartbeat and reports what came before it.
    async fn record(&self, slug: &str, beat: Heartbeat) -> Result<Recorded, UptimeError> {
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
        let recorded = {
            let mut histories = self.histories.write().await;
            let history = histories.entry(slug.to_string()).or_default();
            let previous = history.0.back().map(|b| b.status);
            let down_since = match (previous, beat.status) {
                (Some(Status::Down), _) => history.down_streak_start(),
                (_, Status::Down) => Some(beat.at),
                _ => None,
            };
            history.push(beat, self.policy.retention);
            Recorded {
                previous,
                down_since,
            }
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
        Ok(recorded)
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
                incidents: IncidentReport::default(),
            };
        };
        let now = unix_now();
        let cutoff = now.saturating_sub(window.as_secs());
        MonitorDetail {
            beats: history.downsampled(cutoff, now, MAX_DETAIL_POINTS),
            events: history.events(),
            incidents: IncidentReport::build(history.0.iter(), cutoff, now, MAX_INCIDENTS),
        }
    }

    /// Daily uptime for the last `days` days, oldest first.
    pub async fn daily(&self, slug: &str, days: u64) -> Vec<DayUptime> {
        let histories = self.histories.read().await;
        histories
            .get(slug)
            .unwrap_or(&History::default())
            .daily(unix_now(), days)
    }

    /// Every heartbeat in the window, not aggregated (for exports).
    pub async fn export(&self, slug: &str, window: Duration) -> Vec<Heartbeat> {
        let cutoff = unix_now().saturating_sub(window.as_secs());
        self.histories
            .read()
            .await
            .get(slug)
            .map(|h| h.since(cutoff).cloned().collect())
            .unwrap_or_default()
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
            timeout: Duration::from_secs(10),
            mass_down_pct: 50,
            lang: Lang::Es,
        }
    }

    fn target(url: &str) -> ProbeTarget {
        ProbeTarget {
            url: url.to_string(),
            degraded_after_ms: 1000,
            expect_body: None,
            cert_warn_days: 14,
            check_assets: false,
            timeout: Duration::from_secs(10),
            expect_status: Vec::new(),
            headers: Vec::new(),
            i18n: I18n::new(Lang::Es),
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
            assert_eq!(
                first.previous, None,
                "an app's first heartbeat has no predecessor"
            );
            let second = monitor
                .record("app", beat(unix_now(), Status::Down, None))
                .await
                .unwrap();
            assert_eq!(second.previous, Some(Status::Up));
            assert!(second.down_since.is_some(), "a new outage starts now");
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

    fn registered(slug: &str) -> RegisteredApp {
        serde_json::from_value(serde_json::json!({
            "slug": slug,
            "name": slug,
            "health_url": "https://x.example.com/health",
        }))
        .unwrap()
    }

    /// Records one check, then alerts on it the way `check_all` does.
    async fn step(
        monitor: &UptimeMonitor,
        app: &RegisteredApp,
        at: u64,
        status: Status,
    ) -> Option<StatusChange> {
        let beat = beat(at, status, None);
        let recorded = monitor.record(&app.slug, beat.clone()).await.unwrap();
        let change = monitor.alert_for(app, &recorded, beat);
        monitor
            .dispatch(
                change.clone().into_iter().collect(),
                std::slice::from_ref(app),
            )
            .await;
        change
    }

    #[tokio::test]
    async fn reminders_repeat_on_schedule_and_stop_after_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let alerter = Alerter::new(crate::alerts::AlertSettings {
            remind_after: Some(Duration::from_mins(10)),
            ..Default::default()
        })
        .unwrap();
        let notifiers = Notifiers {
            alerter: Some(alerter),
            ping_url: None,
        };
        let monitor = UptimeMonitor::load(dir.path(), policy(), outbound(), notifiers)
            .await
            .unwrap();
        let app = registered("api");
        let t = unix_now();

        assert!(step(&monitor, &app, t, Status::Up).await.is_none());
        let down = step(&monitor, &app, t + 60, Status::Down).await.unwrap();
        assert_eq!((down.from, down.down_since), (Status::Up, Some(t + 60)));
        assert!(step(&monitor, &app, t + 120, Status::Down).await.is_none());
        assert!(step(&monitor, &app, t + 659, Status::Down).await.is_none());

        let reminder = step(&monitor, &app, t + 660, Status::Down).await.unwrap();
        assert!(reminder.is_reminder());
        assert_eq!(
            reminder.down_since,
            Some(t + 60),
            "counts from the outage start"
        );
        assert!(
            step(&monitor, &app, t + 720, Status::Down).await.is_none(),
            "the next one waits another interval"
        );

        let recovered = step(&monitor, &app, t + 780, Status::Up).await.unwrap();
        assert_eq!(recovered.down_since, Some(t + 60));
        assert!(
            monitor.reminded.lock().unwrap().is_empty(),
            "recovery clears the reminder clock"
        );
    }

    #[test]
    fn expected_status_codes_replace_the_2xx_rule() {
        let mut t = target("https://x.example.com/");
        assert!(t.accepts(reqwest::StatusCode::OK));
        assert!(!t.accepts(reqwest::StatusCode::UNAUTHORIZED));
        t.expect_status = vec![401];
        assert!(t.accepts(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!t.accepts(reqwest::StatusCode::OK));
    }

    #[test]
    fn daily_uptime_buckets_by_utc_day() {
        let day = DAY_SECS;
        let history = History(VecDeque::from([
            beat(10 * day + 10, Status::Up, None),
            beat(10 * day + 20, Status::Down, None),
            beat(12 * day + 5, Status::Degraded, None),
        ]));
        let days = history.daily(12 * day + 100, 3);
        assert_eq!(days.len(), 3);
        assert_eq!(
            days[0],
            DayUptime {
                start: 10 * day,
                uptime: Some(50.0)
            }
        );
        assert_eq!(days[1].uptime, None, "no checks that day");
        assert_eq!(
            days[2].uptime,
            Some(100.0),
            "degraded still counts as available"
        );
    }

    #[test]
    fn down_streak_start_finds_where_the_outage_began() {
        let history = History(VecDeque::from([
            beat(0, Status::Down, None),
            beat(60, Status::Up, None),
            beat(120, Status::Down, None),
            beat(180, Status::Down, None),
        ]));
        assert_eq!(history.down_streak_start(), Some(120));
        let up = History(VecDeque::from([beat(0, Status::Up, None)]));
        assert_eq!(up.down_streak_start(), None);
    }
}
