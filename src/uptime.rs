//! Uptime monitoring: a background loop hits every registered app's `health_url` on a
//! fixed interval and records one heartbeat per check in the database (`store`): every
//! check for `CheckPolicy::retention`, and per-day counts for much longer. What the
//! dashboard polls (latest checks, status changes, 24 h / 30 d figures) is kept in memory
//! and refreshed after every round, so page loads never wait on the database.
//!
//! Each app is checked on its own interval (the global one unless it sets its own), and
//! apps under scheduled maintenance resume by themselves when it ends.
//!
//! A failed check is retried before it's recorded as down, and every confirmed status
//! change goes through the mass-outage gate (`gate`) to `alerts`. While an app stays
//! down, a reminder goes out every `ALERT_REMIND_MINS`.

mod gate;
pub mod incidents;
mod store;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::error::Error as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::task::JoinSet;

use crate::alerts::{Alerter, StatusChange};
use crate::bundles;
use crate::db::{Db, DbError};
use crate::i18n::{I18n, Lang, Localize};
use crate::outbound::{HealthTarget, Outbound};
use crate::registry::{AppRegistry, CheckHeader, RegisteredApp};
use gate::OutageGate;
use incidents::IncidentReport;
use store::Stats;
pub(crate) use store::{count_day as count_heartbeat_day, insert as store_heartbeat};

/// How often old checks and daily counts are deleted, and every app's figures recomputed.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
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
/// Longest a cached overview is served without being rebuilt, even if nothing changed:
/// events age out of the retention window with time alone.
const OVERVIEW_TTL: Duration = Duration::from_secs(30);
/// Details kept at once (an app and a window each); past it, stale ones go first.
const MAX_CACHED_DETAILS: usize = 256;
/// How many incidents the detail endpoint lists.
const MAX_INCIDENTS: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum UptimeError {
    #[error(transparent)]
    Db(#[from] DbError),
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
    #[cfg(test)]
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
    /// How long every check is kept.
    pub retention: Duration,
    /// How long the per-day counts (status page, 30-day uptime) are kept.
    pub daily_retention: Duration,
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

pub(crate) fn error_chain(e: &reqwest::Error) -> String {
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

/// A run of heartbeats, oldest first, aggregated in Rust: the reference the SQLite
/// versions in `store` (`stats`, `buckets`, `incident_beats`) are checked against.
#[cfg(test)]
#[derive(Debug, Default)]
struct History(VecDeque<Heartbeat>);

#[cfg(test)]
impl History {
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
}

/// Width of the buckets a window from `cutoff` to `now` is downsampled into, so it fits
/// `MAX_DETAIL_POINTS` (what `History::downsampled` uses for that many points).
fn bucket_secs(cutoff: u64, now: u64) -> u64 {
    now.saturating_sub(cutoff)
        .max(1)
        .div_ceil(MAX_DETAIL_POINTS as u64)
        .max(1)
}

/// What's kept in memory about one app between rounds.
#[derive(Debug, Default)]
struct Live {
    /// The latest `RECENT_BEATS` checks, oldest first.
    recent: VecDeque<Heartbeat>,
    /// Start of the outage in progress; `None` unless the last check was down.
    down_since: Option<u64>,
    /// Status changes, newest first, at most `MAX_EVENTS`.
    events: Vec<Heartbeat>,
    stats: Stats,
    /// The monitor's generation when this app's checks last changed: what a cached
    /// detail of the app is valid for. Unique across apps, so an app removed and
    /// registered again under the same slug never matches an older entry.
    version: u64,
}

impl Live {
    /// Rebuilds an app's state from its stored history (startup).
    fn load(conn: &rusqlite::Connection, slug: &str, now: u64) -> Result<Self, DbError> {
        let recent: VecDeque<Heartbeat> = store::recent(conn, slug, RECENT_BEATS)?.into();
        let down_since = if recent.back().is_some_and(|b| b.status == Status::Down) {
            store::down_since(conn, slug)?
        } else {
            None
        };
        Ok(Self {
            recent,
            down_since,
            events: store::events(conn, slug, MAX_EVENTS)?,
            stats: store::stats(conn, slug, now)?,
            version: 0,
        })
    }

    /// Takes in a new check; reports what came before it.
    fn push(&mut self, beat: &Heartbeat) -> Recorded {
        let previous = self.recent.back().map(|b| b.status);
        let down_since = match (previous, beat.status) {
            (Some(Status::Down), _) => self.down_since.or(Some(beat.at)),
            (_, Status::Down) => Some(beat.at),
            _ => None,
        };
        self.down_since = down_since.filter(|_| beat.status == Status::Down);
        if previous != Some(beat.status) {
            self.events.insert(0, beat.clone());
            self.events.truncate(MAX_EVENTS);
        }
        self.recent.push_back(beat.clone());
        while self.recent.len() > RECENT_BEATS {
            self.recent.pop_front();
        }
        Recorded {
            previous,
            down_since,
        }
    }

    fn summary(&self, app: &RegisteredApp, cert_expires_at: Option<u64>) -> MonitorSummary {
        let last = self.recent.back();
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
            avg_latency_24h_ms: self.stats.avg_latency_24h_ms,
            uptime_24h: self.stats.uptime_24h,
            uptime_30d: self.stats.uptime_30d,
            cert_expires_at,
            recent: self.recent.iter().cloned().collect(),
        }
    }

    /// Status changes still inside the retention window.
    fn events_since(&self, cutoff: u64) -> impl Iterator<Item = &Heartbeat> {
        self.events.iter().filter(move |b| b.at >= cutoff)
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
    /// Where the end of a scheduled pause is recorded.
    pub audit: Option<crate::audit::AuditLog>,
}

pub struct UptimeMonitor {
    db: Db,
    policy: CheckPolicy,
    outbound: Outbound,
    notifiers: Notifiers,
    live: RwLock<HashMap<String, Live>>,
    /// Latest TLS certificate expiry per app (memory only; refreshed every check).
    certs: RwLock<HashMap<String, u64>>,
    gate: Mutex<OutageGate>,
    i18n: I18n,
    /// When each app that's down was last alerted about (down or reminder).
    reminded: Mutex<HashMap<String, u64>>,
    /// Bumped after every change to `live` or `certs`.
    generation: AtomicU64,
    /// `overview_json`'s last answer.
    overview_cache: OverviewCache<CachedJson>,
    /// `metrics_text`'s last answer.
    metrics_cache: OverviewCache<actix_web::web::Bytes>,
    /// `detail_json`'s answers by (slug, window seconds), with the app version and time
    /// they were built for.
    detail_cache: Mutex<DetailCache>,
}

type DetailCache = HashMap<(String, u64), ((Option<u64>, Instant), CachedJson)>;

/// Something built from the overview, kept until the state behind it changes (or
/// `OVERVIEW_TTL` passes). An async lock, held while rebuilding: concurrent requests wait
/// for that one build instead of each doing their own.
struct OverviewCache<T>(tokio::sync::Mutex<Option<Cached<T>>>);

struct Cached<T> {
    /// (monitor generation, registry generation) read before building it.
    key: (u64, u64),
    built: Instant,
    value: T,
}

impl<T> Default for OverviewCache<T> {
    fn default() -> Self {
        Self(tokio::sync::Mutex::new(None))
    }
}

/// A cached JSON answer, plain and gzipped (the overview shrinks about 15 times: most of
/// it is the recent checks, which repeat).
#[derive(Clone)]
pub struct CachedJson {
    pub plain: actix_web::web::Bytes,
    pub gzip: actix_web::web::Bytes,
}

impl CachedJson {
    fn new(plain: Vec<u8>) -> Self {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(
            Vec::with_capacity(plain.len() / 8),
            flate2::Compression::default(),
        );
        // Writing into a Vec can't fail.
        let gzip = encoder
            .write_all(&plain)
            .and_then(|()| encoder.finish())
            .unwrap_or_default();
        Self {
            plain: plain.into(),
            gzip: gzip.into(),
        }
    }
}

impl UptimeMonitor {
    /// Rebuilds every registered app's in-memory state from the database.
    pub async fn load(
        db: Db,
        policy: CheckPolicy,
        outbound: Outbound,
        notifiers: Notifiers,
    ) -> Result<Self, UptimeError> {
        // Split across the reader connections: rebuilding an app is a handful of
        // queries, and with hundreds of apps the startup is mostly that.
        let slugs = db.read(store::slugs).await?;
        let chunk = slugs.len().div_ceil(db.read_parallelism()).max(1);
        let now = unix_now();
        let mut loads = JoinSet::new();
        for part in slugs.chunks(chunk) {
            let (db, part) = (db.clone(), part.to_vec());
            loads.spawn(async move {
                db.read(move |conn| {
                    part.into_iter()
                        .map(|slug| Ok((slug.clone(), Live::load(conn, &slug, now)?)))
                        .collect::<Result<Vec<_>, DbError>>()
                })
                .await
            });
        }
        let mut live = HashMap::with_capacity(slugs.len());
        while let Some(part) = loads.join_next().await {
            live.extend(part.map_err(DbError::from)??);
        }
        Ok(Self {
            db,
            policy,
            outbound,
            notifiers,
            live: RwLock::new(live),
            certs: RwLock::default(),
            gate: Mutex::default(),
            i18n: I18n::new(policy.lang),
            reminded: Mutex::default(),
            generation: AtomicU64::new(0),
            overview_cache: OverviewCache::default(),
            metrics_cache: OverviewCache::default(),
            detail_cache: Mutex::default(),
        })
    }

    /// Runs forever: every `SCHEDULER_TICK`, ends expired maintenance and checks the apps
    /// that are due; pings the dead man's switch once per global interval and prunes old
    /// history once an hour.
    pub async fn run(self: Arc<Self>, registry: Arc<AppRegistry>) {
        let mut ticker = tokio::time::interval(SCHEDULER_TICK.min(self.policy.interval));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut next_due: HashMap<String, Instant> = HashMap::new();
        let mut last_ping: Option<Instant> = None;
        let mut last_prune: Option<Instant> = None;
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
                        if let Some(audit) = &self.notifiers.audit {
                            let event = crate::audit::Event::system("app.resume")
                                .target(&slug)
                                .detail(serde_json::json!({ "reason": "scheduled" }));
                            audit.record(event).await;
                        }
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
            if last_prune.is_none_or(|at| at.elapsed() >= PRUNE_EVERY) {
                last_prune = Some(Instant::now());
                if let Err(e) = self.prune(unix_now()).await {
                    tracing::error!(error = %e, "uptime: pruning old history failed");
                }
                let slugs: Vec<String> = apps.iter().map(|a| a.slug.clone()).collect();
                self.refresh_stats(slugs).await;
            }
        }
    }

    /// Probes `due` concurrently, records the round and alerts; `all` is every
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
        let mut done = Vec::new();
        while let Some(joined) = probes.join_next().await {
            match joined {
                Ok(probe) => done.push(probe),
                Err(e) => tracing::error!(error = %e, "uptime: probe task panicked"),
            }
        }
        {
            let mut certs = self.certs.write().await;
            for (app, result) in &done {
                if let Some(expires) = result.cert_expires_at {
                    certs.insert(app.slug.clone(), expires);
                }
            }
        }
        self.changed();
        let beats = done
            .iter()
            .map(|(app, result)| (app.slug.clone(), result.beat.clone()))
            .collect();
        let recorded = self.record_all(beats).await;
        let changes = done
            .into_iter()
            .zip(recorded)
            .filter_map(|((app, result), recorded)| self.alert_for(&app, &recorded, result.beat))
            .collect();
        self.dispatch(changes, all).await;
        self.refresh_stats(due.iter().map(|a| a.slug.clone()).collect())
            .await;
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
            let live = self.live.read().await;
            monitored
                .iter()
                .filter_map(|app| {
                    let state = live.get(&app.slug)?;
                    let last = state.recent.back()?.clone();
                    Some((app.slug.as_str(), (last, state.down_since)))
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

    /// Stores a round of heartbeats in one transaction and reports, for each, what came
    /// before it. If the database write fails the round still counts in memory: the
    /// dashboard and the alerts keep working, only the stored history has a gap.
    async fn record_all(&self, beats: Vec<(String, Heartbeat)>) -> Vec<Recorded> {
        for (slug, beat) in &beats {
            match beat.status {
                Status::Down => {
                    tracing::warn!(app = %slug, message = %beat.message, "uptime: app is down");
                }
                Status::Degraded => {
                    tracing::warn!(app = %slug, latency_ms = ?beat.latency_ms, "uptime: app is slow");
                }
                Status::Up => {}
            }
        }
        let to_store = beats.clone();
        let stored = self
            .db
            .write(move |tx| {
                for (slug, beat) in &to_store {
                    store::insert(tx, slug, beat)?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = stored {
            tracing::error!(error = %e, checks = beats.len(), "uptime: failed to store heartbeats");
        }
        let mut live = self.live.write().await;
        let recorded = beats
            .iter()
            .map(|(slug, beat)| live.entry(slug.clone()).or_default().push(beat))
            .collect();
        let version = self.changed();
        for (slug, _) in &beats {
            if let Some(state) = live.get_mut(slug) {
                state.version = version;
            }
        }
        recorded
    }

    /// Stores one heartbeat (tests; the monitor records whole rounds).
    #[cfg(test)]
    async fn record(&self, slug: &str, beat: Heartbeat) -> Recorded {
        self.record_all(vec![(slug.to_string(), beat)])
            .await
            .remove(0)
    }

    /// Recomputes the 24 h / 30 d figures of `slugs` from the database.
    async fn refresh_stats(&self, slugs: Vec<String>) {
        let fresh = self
            .db
            .read(move |conn| {
                let now = unix_now();
                slugs
                    .into_iter()
                    .map(|slug| Ok((store::stats(conn, &slug, now)?, slug)))
                    .collect::<Result<Vec<_>, DbError>>()
            })
            .await;
        match fresh {
            Ok(fresh) => {
                let mut live = self.live.write().await;
                for (stats, slug) in fresh {
                    live.entry(slug).or_default().stats = stats;
                }
                self.changed();
            }
            Err(e) => tracing::error!(error = %e, "uptime: could not refresh uptime figures"),
        }
    }

    /// Deletes checks past `retention` and daily counts past `daily_retention`.
    async fn prune(&self, now: u64) -> Result<(), UptimeError> {
        let checks_before = now.saturating_sub(self.policy.retention.as_secs());
        let days_before =
            now.saturating_sub(self.policy.daily_retention.as_secs()) / DAY_SECS * DAY_SECS;
        let (checks, days) = self
            .db
            .write(move |tx| store::prune(tx, checks_before, days_before))
            .await?;
        if checks + days > 0 {
            tracing::info!(checks, days, "uptime: pruned old history");
        }
        // Lets SQLite refresh its query planner statistics now and then. It may write
        // them, so it runs on the writer.
        self.db
            .write(|tx| Ok(tx.execute_batch("PRAGMA optimize")?))
            .await?;
        Ok(())
    }

    /// Drops an app's in-memory state -- called when the app is unregistered; its stored
    /// history goes with the app itself (`ON DELETE CASCADE`).
    pub async fn forget(&self, slug: &str) {
        self.live.write().await.remove(slug);
        self.certs.write().await.remove(slug);
        self.changed();
    }

    /// Called after the state behind `overview` changes, never before: a reader that
    /// saw the old generation can only have read data at least that new.
    fn changed(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Release) + 1
    }

    /// `detail` serialized, cached per app and window until that app gets a new check
    /// (or `OVERVIEW_TTL` passes): the dashboard asks for the selected app's detail every
    /// 30 s, and a 30-day window is its most expensive read.
    pub async fn detail_json(&self, slug: &str, window: Duration) -> CachedJson {
        // Read before the data, so a check stored meanwhile shows up as a newer version.
        let version = self.live.read().await.get(slug).map(|state| state.version);
        let key = (slug.to_string(), window.as_secs());
        {
            let cache = self
                .detail_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some((_, json)) = cache
                .get(&key)
                .filter(|(at, _)| at.0 == version && at.1.elapsed() < OVERVIEW_TTL)
            {
                return json.clone();
            }
        }
        let json = CachedJson::new(
            serde_json::to_vec(&self.detail(slug, window).await).unwrap_or_default(),
        );
        let mut cache = self
            .detail_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if cache.len() >= MAX_CACHED_DETAILS {
            cache.retain(|_, (at, _)| at.1.elapsed() < OVERVIEW_TTL);
            if cache.len() >= MAX_CACHED_DETAILS {
                cache.clear();
            }
        }
        cache.insert(key, ((version, Instant::now()), json.clone()));
        json
    }

    /// `overview` of every registered app, serialized. Rebuilt only when an app or a check
    /// changed since the last build (or after `OVERVIEW_TTL`): the dashboard polls it
    /// from every open tab, and between rounds of checks every answer is the same.
    pub async fn overview_json(&self, registry: &AppRegistry) -> CachedJson {
        self.cached(registry, &self.overview_cache, |overview| {
            CachedJson::new(serde_json::to_vec(overview).unwrap_or_default())
        })
        .await
    }

    /// `render` applied to the overview's monitors (the `/metrics` text), cached like
    /// `overview_json`: scrapers ask far more often than a round of checks ends.
    pub async fn metrics_text(
        &self,
        registry: &AppRegistry,
        render: fn(&[MonitorSummary]) -> String,
    ) -> actix_web::web::Bytes {
        self.cached(registry, &self.metrics_cache, |overview| {
            render(&overview.monitors).into()
        })
        .await
    }

    async fn cached<T: Clone>(
        &self,
        registry: &AppRegistry,
        cache: &OverviewCache<T>,
        build: impl FnOnce(&Overview) -> T,
    ) -> T {
        let mut slot = cache.0.lock().await;
        // Read before the data, so a change made meanwhile shows up as a newer key.
        let key = (
            self.generation.load(Ordering::Acquire),
            registry.generation(),
        );
        if let Some(cached) = slot
            .as_ref()
            .filter(|c| c.key == key && c.built.elapsed() < OVERVIEW_TTL)
        {
            return cached.value.clone();
        }
        let apps = registry.list().await;
        let value = build(&self.overview(&apps).await);
        *slot = Some(Cached {
            key,
            built: Instant::now(),
            value: value.clone(),
        });
        value
    }

    pub async fn overview(&self, apps: &[RegisteredApp]) -> Overview {
        let live = self.live.read().await;
        let certs = self.certs.read().await;
        let cutoff = unix_now().saturating_sub(self.policy.retention.as_secs());
        let empty = Live::default();

        let mut events = Vec::new();
        let monitors = apps
            .iter()
            .map(|app| {
                let state = live.get(&app.slug).unwrap_or(&empty);
                events.extend(state.events_since(cutoff).map(|beat| AppEvent {
                    slug: app.slug.clone(),
                    name: app.name.clone(),
                    beat: beat.clone(),
                }));
                state.summary(app, certs.get(&app.slug).copied())
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
        let live = self.live.read().await;
        let cert = self.certs.read().await.get(&app.slug).copied();
        live.get(&app.slug)
            .unwrap_or(&Live::default())
            .summary(app, cert)
    }

    pub async fn detail(&self, slug: &str, window: Duration) -> MonitorDetail {
        let now = unix_now();
        let cutoff = now.saturating_sub(window.as_secs());
        let events: Vec<Heartbeat> = {
            let retained = now.saturating_sub(self.policy.retention.as_secs());
            let live = self.live.read().await;
            live.get(slug)
                .map(|state| state.events_since(retained).cloned().collect())
                .unwrap_or_default()
        };
        let owned = slug.to_string();
        let read = self
            .db
            .read(move |conn| {
                // A long window is downsampled by SQLite, a short one read as is; the
                // incidents only need the down checks and the flips. Either way, a 30-day
                // window never loads its tens of thousands of checks.
                let points = if store::more_than(conn, &owned, cutoff, MAX_DETAIL_POINTS)? {
                    store::buckets(conn, &owned, cutoff, bucket_secs(cutoff, now))?
                } else {
                    store::since(conn, &owned, cutoff)?
                };
                // From before the window when an outage was already going on at its start,
                // so that incident is reported from its real beginning.
                let from = store::window_start(conn, &owned, cutoff)?;
                Ok((points, store::incident_beats(conn, &owned, from)?))
            })
            .await;
        let (beats, incident_beats) = read.unwrap_or_else(|e| {
            tracing::error!(app = %slug, error = %e, "uptime: could not read the history");
            (Vec::new(), Vec::new())
        });
        MonitorDetail {
            beats,
            events,
            incidents: IncidentReport::build(incident_beats.iter(), cutoff, now, MAX_INCIDENTS),
        }
    }

    /// Daily uptime for the last `days` days, oldest first.
    pub async fn daily(&self, slug: &str, days: u64) -> Vec<DayUptime> {
        let owned = slug.to_string();
        match self
            .db
            .read(move |conn| store::daily(conn, &owned, unix_now(), days))
            .await
        {
            Ok(days) => days,
            Err(e) => {
                tracing::error!(app = %slug, error = %e, "uptime: could not read daily uptime");
                Vec::new()
            }
        }
    }

    /// Every heartbeat in the window, not aggregated (for exports).
    pub async fn export(&self, slug: &str, window: Duration) -> Vec<Heartbeat> {
        let cutoff = unix_now().saturating_sub(window.as_secs());
        let owned = slug.to_string();
        match self
            .db
            .read(move |conn| store::since(conn, &owned, cutoff))
            .await
        {
            Ok(beats) => beats,
            Err(e) => {
                tracing::error!(app = %slug, error = %e, "uptime: could not export the history");
                Vec::new()
            }
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
            daily_retention: Duration::from_hours(400 * 24),
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

    /// A database with these apps registered (history rows need their app).
    async fn db_with(slugs: &[&str]) -> Db {
        let db = Db::open_in_memory();
        let apps: Vec<RegisteredApp> = slugs.iter().map(|s| registered(s)).collect();
        db.write(move |tx| {
            for app in &apps {
                crate::registry::save_app(tx, app)?;
            }
            Ok(())
        })
        .await
        .unwrap();
        db
    }

    /// Stores `beats` for `slug` directly, the way a round of checks does.
    async fn store_beats(db: &Db, slug: &str, beats: Vec<Heartbeat>) {
        let slug = slug.to_string();
        db.write(move |tx| {
            for beat in &beats {
                store::insert(tx, &slug, beat)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn events_are_status_flips_newest_first() {
        let db = db_with(&["app"]).await;
        store_beats(
            &db,
            "app",
            vec![
                beat(1, Status::Up, None),
                beat(2, Status::Up, None),
                beat(3, Status::Down, None),
                beat(4, Status::Degraded, None),
                beat(5, Status::Up, None),
            ],
        )
        .await;
        for (max, expected) in [(30, vec![5, 4, 3, 1]), (2, vec![5, 4])] {
            let ats: Vec<u64> = db
                .read(move |conn| store::events(conn, "app", max))
                .await
                .unwrap()
                .iter()
                .map(|b| b.at)
                .collect();
            assert_eq!(ats, expected, "max {max}");
        }
    }

    #[tokio::test]
    async fn events_match_a_full_scan_on_random_histories() {
        // Every flip, found the slow way: walk the whole history.
        fn full_scan(beats: &[Heartbeat], max: usize) -> Vec<u64> {
            let mut events: Vec<u64> = beats
                .iter()
                .enumerate()
                .filter(|(i, b)| *i == 0 || beats[i - 1].status != b.status)
                .map(|(_, b)| b.at)
                .collect();
            events.reverse();
            events.truncate(max);
            events
        }
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        for round in 0..50 {
            let db = db_with(&["app"]).await;
            let len = 1 + usize::try_from(seed % 200).unwrap();
            let beats: Vec<Heartbeat> = (0..len)
                .map(|i| {
                    // xorshift: runs of every length, including single checks.
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let status = match seed % 7 {
                        0 => Status::Down,
                        1 => Status::Degraded,
                        _ => Status::Up,
                    };
                    beat(1000 + 60 * i as u64, status, None)
                })
                .collect();
            store_beats(&db, "app", beats.clone()).await;
            for max in [1, 5, 30] {
                let found: Vec<u64> = db
                    .read(move |conn| store::events(conn, "app", max))
                    .await
                    .unwrap()
                    .iter()
                    .map(|b| b.at)
                    .collect();
                assert_eq!(found, full_scan(&beats, max), "round {round}, max {max}");
            }
        }
    }

    #[tokio::test]
    async fn prune_drops_checks_and_days_past_their_retention() {
        let db = db_with(&["app"]).await;
        store_beats(
            &db,
            "app",
            vec![
                beat(DAY_SECS + 10, Status::Up, None),
                beat(5 * DAY_SECS + 10, Status::Up, None),
            ],
        )
        .await;
        let (checks, days) = db
            .write(|tx| store::prune(tx, 3 * DAY_SECS, 2 * DAY_SECS))
            .await
            .unwrap();
        assert_eq!((checks, days), (1, 1));
        let left = db.read(|conn| store::since(conn, "app", 0)).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].at, 5 * DAY_SECS + 10);
    }

    #[tokio::test]
    async fn the_hourly_prune_runs_on_a_file_database() {
        // A file database has read-only readers next to the writer (memory ones don't):
        // everything the prune does must go through the writer.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("heartbeat.db")).await.unwrap();
        let app = registered("app");
        db.write(move |tx| crate::registry::save_app(tx, &app))
            .await
            .unwrap();
        store_beats(&db, "app", vec![beat(10, Status::Up, None)]).await;
        let monitor = UptimeMonitor::load(db, policy(), outbound(), Notifiers::default())
            .await
            .unwrap();
        monitor.prune(unix_now()).await.unwrap();
        assert!(monitor.export("app", RETENTION * 100).await.is_empty());
    }

    #[tokio::test]
    async fn a_repeated_second_is_stored_and_counted_once() {
        let db = db_with(&["app"]).await;
        let inserted = db
            .write(|tx| {
                let first = store::insert(tx, "app", &beat(100, Status::Up, Some(5)))?;
                let again = store::insert(tx, "app", &beat(100, Status::Down, None))?;
                Ok((first, again))
            })
            .await
            .unwrap();
        assert_eq!(inserted, (true, false));
        let days = db
            .read(|conn| store::daily(conn, "app", 200, 1))
            .await
            .unwrap();
        assert_eq!(days[0].uptime, Some(100.0));
    }

    #[tokio::test]
    async fn records_persist_across_a_reload_and_forget_removes_them() {
        let db = db_with(&["app"]).await;
        {
            let monitor =
                UptimeMonitor::load(db.clone(), policy(), outbound(), Notifiers::default())
                    .await
                    .unwrap();
            let first = monitor
                .record("app", beat(unix_now(), Status::Up, Some(12)))
                .await;
            assert_eq!(
                first.previous, None,
                "an app's first heartbeat has no predecessor"
            );
            let second = monitor
                .record("app", beat(unix_now() + 1, Status::Down, None))
                .await;
            assert_eq!(second.previous, Some(Status::Up));
            assert!(second.down_since.is_some(), "a new outage starts now");
        }
        let monitor = UptimeMonitor::load(db.clone(), policy(), outbound(), Notifiers::default())
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
        // Unregistering the app deletes its rows; `forget` drops what's in memory.
        db.write(|tx| {
            tx.execute("DELETE FROM apps WHERE slug = 'app'", [])?;
            Ok(())
        })
        .await
        .unwrap();
        monitor.forget("app").await;
        assert!(monitor.detail("app", RETENTION).await.beats.is_empty());
        assert!(
            monitor.overview(&[registered("app")]).await.monitors[0]
                .recent
                .is_empty()
        );
    }

    #[tokio::test]
    async fn events_from_the_flip_index_match_walking_the_history() {
        // A small deterministic generator: the same histories on every run.
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for case in 0..40 {
            let db = db_with(&["app"]).await;
            let len = 1 + next(400);
            // Runs of random length and status, so some apps flip a lot and some never.
            let mut beats = Vec::new();
            let mut status = Status::Up;
            for i in 0..len {
                if next(10) == 0 {
                    status = [Status::Up, Status::Degraded, Status::Down]
                        [usize::try_from(next(3)).unwrap()];
                }
                beats.push(beat(1_000 + i * 60, status, Some(100)));
            }
            // Mostly in order, like rounds of checks; some cases shuffled, like an import
            // of overlapping files, with a few duplicates thrown in.
            if case % 3 == 0 {
                for i in (1..beats.len()).rev() {
                    let j = usize::try_from(next(u64::try_from(i).unwrap() + 1)).unwrap();
                    beats.swap(i, j);
                }
                let dupes: Vec<Heartbeat> = beats.iter().step_by(17).cloned().collect();
                beats.extend(dupes);
            }
            store_beats(&db, "app", beats).await;
            // Prune a prefix in some cases, as the retention does.
            if case % 2 == 1 {
                let cutoff = 1_000 + next(len) * 60;
                db.write(move |tx| {
                    tx.execute(
                        "DELETE FROM heartbeats WHERE slug = 'app' AND at < ?1",
                        [i64::try_from(cutoff).unwrap()],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
            }
            for max in [1, 5, 30, 1000] {
                let (indexed, walked) = db
                    .read(move |conn| {
                        Ok((
                            store::events(conn, "app", max)?,
                            store::events_by_walking(conn, "app", max)?,
                        ))
                    })
                    .await
                    .unwrap();
                let key =
                    |e: &Vec<Heartbeat>| e.iter().map(|b| (b.at, b.status)).collect::<Vec<_>>();
                assert_eq!(key(&indexed), key(&walked), "case {case}, max {max}");
            }
        }
    }

    #[tokio::test]
    async fn the_detail_read_by_sqlite_matches_the_whole_history() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let now = 1_000_000;
        for case in 0..20 {
            let db = db_with(&["app"]).await;
            // Irregular spacing, every status, ties for the worst within a bucket,
            // missing latencies and an outage going on across the window's start.
            let mut beats = Vec::new();
            let mut at = now - 200_000 - next(5_000);
            let mut status = Status::Up;
            while at < now {
                if next(8) == 0 {
                    status = [Status::Up, Status::Degraded, Status::Down]
                        [usize::try_from(next(3)).unwrap()];
                }
                let latency = (next(9) != 0).then(|| 20 + u32::try_from(next(2_000)).unwrap());
                let mut b = beat(at, status, latency);
                b.message = format!("m{}", next(4));
                beats.push(b);
                at += 1 + next(120);
            }
            store_beats(&db, "app", beats.clone()).await;
            let history = History(beats.into());
            let cutoff = now - 150_000 - next(10_000);

            let (points, incident_beats) = db
                .read(move |conn| {
                    let from = store::window_start(conn, "app", cutoff)?;
                    Ok((
                        store::buckets(conn, "app", cutoff, bucket_secs(cutoff, now))?,
                        store::incident_beats(conn, "app", from)?,
                    ))
                })
                .await
                .unwrap();
            let key = |v: &[Heartbeat]| {
                v.iter()
                    .map(|b| (b.at, b.status, b.latency_ms, b.message.clone()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                key(&points),
                key(&history.downsampled(cutoff, now, MAX_DETAIL_POINTS)),
                "case {case}: buckets"
            );
            let all = IncidentReport::build(history.0.iter(), cutoff, now, MAX_INCIDENTS);
            let read = IncidentReport::build(incident_beats.iter(), cutoff, now, MAX_INCIDENTS);
            assert_eq!(read.incidents, all.incidents, "case {case}: incidents");
            assert_eq!(
                (read.mttr_secs, read.total_down_secs),
                (all.mttr_secs, all.total_down_secs),
                "case {case}: statistics"
            );
        }
    }

    #[tokio::test]
    async fn stats_counted_by_sqlite_match_the_checks() {
        let db = db_with(&["api"]).await;
        let now = unix_now();
        // Every status, missing latencies, and checks just outside the 24 h window.
        let beats: Vec<Heartbeat> = (0..3000_u32)
            .map(|i| {
                let status = match i % 7 {
                    0 => Status::Down,
                    1 | 2 => Status::Degraded,
                    _ => Status::Up,
                };
                let latency = (i % 11 != 0).then_some(50 + i % 900);
                beat(now - u64::from(i) * 37, status, latency)
            })
            .collect();
        store_beats(&db, "api", beats.clone()).await;
        let stats = db
            .read(move |conn| store::stats(conn, "api", now))
            .await
            .unwrap();
        let mut sorted = beats;
        sorted.sort_by_key(|b| b.at);
        let history = History(sorted.into());
        let cutoff = now - DAY_SECS;
        assert_eq!(stats.uptime_24h, history.uptime_pct(cutoff));
        assert_eq!(stats.avg_latency_24h_ms, history.avg_latency_ms(cutoff));
        assert!(stats.uptime_24h.is_some() && stats.avg_latency_24h_ms.is_some());

        let empty = db_with(&["new"]).await;
        let none = empty
            .read(move |conn| store::stats(conn, "new", now))
            .await
            .unwrap();
        assert_eq!((none.uptime_24h, none.avg_latency_24h_ms), (None, None));
    }

    #[tokio::test]
    async fn the_cached_detail_is_per_app_and_follows_its_checks() {
        let db = db_with(&["api", "web"]).await;
        let monitor = UptimeMonitor::load(db, policy(), outbound(), Notifiers::default())
            .await
            .unwrap();
        let window = Duration::from_hours(6);
        let first = monitor.detail_json("api", window).await;
        let again = monitor.detail_json("api", window).await;
        assert_eq!(
            first.plain.as_ptr(),
            again.plain.as_ptr(),
            "served from the cache"
        );

        monitor
            .record("web", beat(unix_now(), Status::Up, Some(5)))
            .await;
        let other_app = monitor.detail_json("api", window).await;
        assert_eq!(
            first.plain.as_ptr(),
            other_app.plain.as_ptr(),
            "another app's check doesn't touch it"
        );

        monitor
            .record("api", beat(unix_now(), Status::Down, None))
            .await;
        let fresh: serde_json::Value =
            serde_json::from_slice(&monitor.detail_json("api", window).await.plain).unwrap();
        assert_eq!(
            fresh["beats"].as_array().unwrap().len(),
            1,
            "its own check rebuilds it"
        );
        assert_eq!(fresh["incidents"]["incidents"].as_array().unwrap().len(), 1);
    }

    /// The /metrics text, as a string.
    async fn metrics_of(monitor: &UptimeMonitor, registry: &AppRegistry) -> String {
        let text = monitor
            .metrics_text(registry, crate::routes::metrics::render)
            .await;
        String::from_utf8(text.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn the_cached_overview_follows_new_checks_and_app_changes() {
        let db = db_with(&["api"]).await;
        let registry = AppRegistry::load(db.clone()).await.unwrap();
        let monitor = UptimeMonitor::load(db, policy(), outbound(), Notifiers::default())
            .await
            .unwrap();
        let read = |json: CachedJson| -> serde_json::Value {
            let mut unzipped = Vec::new();
            std::io::Read::read_to_end(
                &mut flate2::read::GzDecoder::new(&json.gzip[..]),
                &mut unzipped,
            )
            .unwrap();
            assert_eq!(
                unzipped, json.plain,
                "both encodings carry the same overview"
            );
            serde_json::from_slice(&json.plain).unwrap()
        };

        let empty = monitor.overview_json(&registry).await;
        assert_eq!(
            read(empty.clone())["monitors"][0]["recent"],
            serde_json::json!([])
        );
        let again = monitor.overview_json(&registry).await;
        assert_eq!(
            empty.plain.as_ptr(),
            again.plain.as_ptr(),
            "unchanged state is served from the cache"
        );

        monitor
            .record("api", beat(unix_now(), Status::Down, None))
            .await;
        let after_check = read(monitor.overview_json(&registry).await);
        assert_eq!(
            after_check["monitors"][0]["status"], "down",
            "a check rebuilds it"
        );

        registry.set_paused("api", true, None).await.unwrap();
        let after_pause = read(monitor.overview_json(&registry).await);
        assert_eq!(
            after_pause["monitors"][0]["paused"], true,
            "an app change rebuilds it"
        );

        // /metrics has its own cache, kept by the same generations.
        let paused = metrics_of(&monitor, &registry).await;
        assert!(
            paused.contains(r#"heartbeat_paused{app="api",name="api"} 1"#),
            "{paused}"
        );
        registry.set_paused("api", false, None).await.unwrap();
        let resumed = metrics_of(&monitor, &registry).await;
        assert!(
            resumed.contains(r#"heartbeat_paused{app="api",name="api"} 0"#),
            "{resumed}"
        );
    }

    #[tokio::test]
    async fn a_restart_rebuilds_the_outage_in_progress() {
        let db = db_with(&["api"]).await;
        let t = unix_now() - 600;
        store_beats(
            &db,
            "api",
            vec![
                beat(t, Status::Down, None),
                beat(t + 60, Status::Up, Some(80)),
                beat(t + 120, Status::Down, None),
                beat(t + 180, Status::Down, None),
            ],
        )
        .await;
        let monitor = UptimeMonitor::load(db, policy(), outbound(), Notifiers::default())
            .await
            .unwrap();
        let summary = monitor.summary(&registered("api")).await;
        assert_eq!(summary.recent.len(), 4);
        assert_eq!(
            summary.uptime_24h,
            Some(25.0),
            "loaded from the stored checks"
        );

        let recovered = monitor
            .record("api", beat(t + 240, Status::Up, Some(90)))
            .await;
        assert_eq!(recovered.previous, Some(Status::Down));
        assert_eq!(
            recovered.down_since,
            Some(t + 120),
            "the outage began at the first down after the last up"
        );
    }

    #[tokio::test]
    async fn the_detail_reports_an_outage_that_began_before_the_window() {
        let db = db_with(&["api"]).await;
        let now = unix_now();
        store_beats(
            &db,
            "api",
            vec![
                beat(now - 7200, Status::Up, Some(50)),
                beat(now - 5400, Status::Down, None),
                beat(now - 1800, Status::Down, None),
                beat(now - 600, Status::Up, Some(50)),
            ],
        )
        .await;
        let monitor = UptimeMonitor::load(db, policy(), outbound(), Notifiers::default())
            .await
            .unwrap();
        let detail = monitor.detail("api", Duration::from_secs(3600)).await;
        assert_eq!(detail.beats.len(), 2, "only the window's checks are drawn");
        assert_eq!(detail.incidents.incidents.len(), 1);
        assert_eq!(detail.incidents.incidents[0].started_at, now - 5400);
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
        let recorded = monitor.record(&app.slug, beat.clone()).await;
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
        let alerter = Alerter::new(crate::alerts::AlertSettings {
            remind_after: Some(Duration::from_mins(10)),
            ..Default::default()
        })
        .unwrap();
        let notifiers = Notifiers {
            alerter: Some(alerter),
            ..Notifiers::default()
        };
        let monitor = UptimeMonitor::load(db_with(&["api"]).await, policy(), outbound(), notifiers)
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

    #[tokio::test]
    async fn daily_uptime_buckets_by_utc_day() {
        let day = DAY_SECS;
        let db = db_with(&["app"]).await;
        store_beats(
            &db,
            "app",
            vec![
                beat(10 * day + 10, Status::Up, None),
                beat(10 * day + 20, Status::Down, None),
                beat(12 * day + 5, Status::Degraded, None),
            ],
        )
        .await;
        let days = db
            .read(move |conn| store::daily(conn, "app", 12 * day + 100, 3))
            .await
            .unwrap();
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
}
