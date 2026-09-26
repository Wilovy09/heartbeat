//! Synthetic logs for `just demo` (cargo feature `demo`, never in release builds). The demo
//! apps' logs URLs point at `*.heartbeat.invalid`, which can't resolve; the logs proxy asks
//! this module first and serves generated lines in the real endpoint's shape instead.
//!
//! Lines are a pure function of the app and the clock: each 4-second slot holds one event
//! whose content is seeded by (slug, slot). A refresh shows the same older lines plus the
//! new ones, the way a real `tail` would, so the viewer's live mode looks live.

use reqwest::Url;
use serde_json::{Value, json};
use std::fmt::Write as _;

use crate::uptime::unix_now;

/// Host suffix of the demo apps' logs URLs.
const DEMO_HOST_SUFFIX: &str = ".heartbeat.invalid";
const SLOT_SECS: u64 = 4;
const MAX_LINES: usize = 2000;
/// A panic lands on stderr about once every this many slots (~10 min) on unstable apps.
const PANIC_EVERY_SLOTS: u64 = 150;

/// How an app behaves, from the demo slug: steady apps log mostly INFO, broken ones errors.
#[derive(Clone, Copy)]
enum Profile {
    Steady,
    Flaky,
    Down,
    Slow,
    /// Down until 20 minutes ago, steady since.
    Recovered,
}

impl Profile {
    fn for_slug(slug: &str) -> Self {
        match slug {
            s if s.contains("caida") => Self::Down,
            s if s.contains("intermitente") => Self::Flaky,
            s if s.contains("degradada") => Self::Slow,
            s if s.contains("recuperada") => Self::Recovered,
            _ => Self::Steady,
        }
    }

    /// The profile in effect `ago` slots back.
    fn at(self, ago: u64) -> Self {
        match self {
            Self::Recovered if ago * SLOT_SECS > 20 * 60 => Self::Down,
            Self::Recovered => Self::Steady,
            other => other,
        }
    }

    /// Relative weight of each `EVENTS` entry.
    fn weights(self) -> [u64; 9] {
        match self {
            Self::Steady | Self::Recovered => [30, 30, 20, 6, 10, 2, 2, 0, 0],
            Self::Flaky => [25, 25, 15, 4, 8, 6, 8, 6, 3],
            Self::Down => [4, 10, 4, 1, 4, 4, 14, 30, 12],
            Self::Slow => [25, 20, 15, 3, 6, 25, 4, 1, 1],
        }
    }

    fn panics(self) -> bool {
        matches!(self, Self::Flaky | Self::Down)
    }
}

/// splitmix64: tiny, deterministic, good enough to vary demo lines.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        let len = u64::try_from(items.len()).unwrap_or(1);
        items[usize::try_from(self.below(len)).unwrap_or(0)]
    }
}

fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// `2026-09-26T07:44:24.327168Z` for unix milliseconds (tracing's timestamp shape).
fn rfc3339(millis: u64) -> String {
    let secs = millis / 1000;
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}000Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        millis % 1000,
    )
}

/// One kind of event: level, target suffix, message, and how to fill its fields.
struct Event {
    level: &'static str,
    target: &'static str,
    message: &'static str,
}

const EVENTS: [Event; 9] = [
    Event {
        level: "INFO",
        target: "http",
        message: "request completed",
    },
    Event {
        level: "DEBUG",
        target: "sqlx::query",
        message: "summary",
    },
    Event {
        level: "INFO",
        target: "http",
        message: "started processing request",
    },
    Event {
        level: "TRACE",
        target: "tower_http::trace",
        message: "on_request",
    },
    Event {
        level: "INFO",
        target: "jobs",
        message: "batch finished",
    },
    Event {
        level: "WARN",
        target: "db",
        message: "slow query",
    },
    Event {
        level: "WARN",
        target: "http",
        message: "retrying upstream call",
    },
    Event {
        level: "ERROR",
        target: "http",
        message: "request failed",
    },
    Event {
        level: "ERROR",
        target: "jobs",
        message: "job failed",
    },
];

const PATHS: [&str; 6] = [
    "/api/invoices",
    "/api/customers/:id",
    "/api/payments",
    "/health",
    "/api/webhooks/stripe",
    "/api/reports/monthly",
];
const METHODS: [&str; 4] = ["GET", "GET", "POST", "PUT"];
const STATEMENTS: [&str; 4] = [
    "SELECT * FROM invoices WHERE customer_id = $1",
    "UPDATE payments SET status = $1 WHERE id = $2",
    "INSERT INTO events (kind, payload) VALUES ($1, $2)",
    "SELECT count(*) FROM sessions WHERE expires_at > now()",
];
const ERRORS: [&str; 3] = [
    "upstream connect error: connection refused (db-primary:5432)",
    "pool timed out while waiting for an open connection",
    "deadline has elapsed",
];

impl Event {
    fn fields(&self, rng: &mut Rng, profile: Profile) -> Value {
        let slow = matches!(profile, Profile::Slow);
        let latency = if slow {
            800 + rng.below(2400)
        } else {
            12 + rng.below(380)
        };
        match (self.level, self.message) {
            (_, "request completed") => json!({
                "message": self.message,
                "method": rng.pick(&METHODS),
                "path": rng.pick(&PATHS),
                "status": 200,
                "latency_ms": latency,
            }),
            (_, "summary") => json!({
                "message": self.message,
                "db.statement": rng.pick(&STATEMENTS),
                "rows_affected": rng.below(40),
                "elapsed_ms": latency / 4,
            }),
            (_, "started processing request" | "on_request") => json!({
                "message": self.message,
                "method": rng.pick(&METHODS),
                "path": rng.pick(&PATHS),
            }),
            (_, "batch finished") => json!({
                "message": self.message,
                "job": "invoices",
                "processed": 50 + rng.below(400),
            }),
            (_, "slow query") => json!({
                "message": self.message,
                "db.statement": rng.pick(&STATEMENTS),
                "elapsed_ms": 1200 + rng.below(3000),
            }),
            (_, "retrying upstream call") => json!({
                "message": self.message,
                "attempt": 1 + rng.below(3),
                "upstream": "payments-api",
            }),
            _ => json!({
                "message": self.message,
                "error": rng.pick(&ERRORS),
                "path": rng.pick(&PATHS),
                "status": 503,
            }),
        }
    }
}

/// Demo · Backend is a bigger service: its app-local events spread over many modules, so
/// the viewer's module filter has more than it shows collapsed.
const BACKEND_MODULES: [&str; 14] = [
    "http",
    "http::middleware",
    "auth",
    "auth::sessions",
    "billing",
    "billing::invoices",
    "cache",
    "db",
    "jobs",
    "jobs::scheduler",
    "mailer",
    "queue",
    "search",
    "webhooks",
];

/// The app's "crate name" for targets: `demo-backend` -> `backend`.
fn crate_name(slug: &str) -> String {
    slug.trim_start_matches("demo-").replace('-', "_")
}

fn stdout_line(slug: &str, profile: Profile, slot: u64, ago: u64) -> String {
    let mut rng = Rng::new(fnv1a(slug) ^ slot.wrapping_mul(0x2545_F491_4F6C_DD1D));
    let profile = profile.at(ago);
    let weights = profile.weights();
    let mut roll = rng.below(weights.iter().sum());
    let event = EVENTS
        .iter()
        .zip(weights)
        .find(|(_, w)| {
            if roll < *w {
                true
            } else {
                roll -= w;
                false
            }
        })
        .map_or(&EVENTS[0], |(e, _)| e);
    let target = if event.target.contains("::") {
        event.target.to_string()
    } else if slug == "demo-backend" {
        format!("{}::{}", crate_name(slug), rng.pick(&BACKEND_MODULES))
    } else {
        format!("{}::{}", crate_name(slug), event.target)
    };
    let millis = slot * SLOT_SECS * 1000 + rng.below(SLOT_SECS * 1000);
    json!({
        "timestamp": rfc3339(millis),
        "level": event.level,
        "fields": event.fields(&mut rng, profile),
        "target": target,
    })
    .to_string()
}

fn stderr_lines(slug: &str, profile: Profile, now_slot: u64, window: u64) -> Vec<String> {
    let mut lines = Vec::new();
    let first = now_slot.saturating_sub(window);
    for slot in first..=now_slot {
        let ago = now_slot - slot;
        if !profile.at(ago).panics()
            || !slot
                .wrapping_add(fnv1a(slug))
                .is_multiple_of(PANIC_EVERY_SLOTS)
        {
            continue;
        }
        let mut file = String::new();
        let _ = write!(
            file,
            "src/jobs/{}.rs:{}:14",
            crate_name(slug),
            40 + slot % 90
        );
        lines.push(format!("thread 'tokio-runtime-worker' panicked at {file}:"));
        lines.push("called `Result::unwrap()` on an `Err` value: PoolTimedOut".to_string());
        lines.push(
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace"
                .to_string(),
        );
    }
    lines
}

/// Generated logs for a demo logs URL (`https://logs.heartbeat.invalid/<slug>/logs`), in
/// the endpoint contract's shape; `None` for any other URL.
#[must_use]
pub fn logs(url: &Url, stream: Option<&str>, lines: Option<&str>) -> Option<Value> {
    if !url.host_str()?.ends_with(DEMO_HOST_SUFFIX) {
        return None;
    }
    let slug = url.path_segments()?.next()?.to_string();
    let profile = Profile::for_slug(&slug);
    let wanted = lines
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(500)
        .clamp(1, MAX_LINES);
    let now_slot = unix_now() / SLOT_SECS;
    let window = u64::try_from(wanted).unwrap_or(500);

    // Oldest first, like `tail -n`.
    let out: Vec<String> = (0..window)
        .rev()
        .map(|ago| stdout_line(&slug, profile, now_slot - ago, ago))
        .collect();
    let mut err = stderr_lines(&slug, profile, now_slot, window);
    err.truncate(wanted);

    let empty = || json!({ "lines": [], "error": null });
    let (show_out, show_err) = match stream.unwrap_or("both") {
        "out" => (true, false),
        "error" => (false, true),
        _ => (true, true),
    };
    Some(json!({
        "out": if show_out { json!({ "lines": out, "error": null }) } else { empty() },
        "error": if show_err { json!({ "lines": err, "error": null }) } else { empty() },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(slug: &str) -> Url {
        Url::parse(&format!("https://logs.heartbeat.invalid/{slug}/logs")).unwrap()
    }

    #[test]
    fn only_demo_urls_get_generated_logs() {
        assert!(
            logs(
                &Url::parse("https://api.example.com/logs").unwrap(),
                None,
                None
            )
            .is_none()
        );
        let body = logs(&url("demo-caida"), Some("both"), Some("50")).unwrap();
        let out = body["out"]["lines"].as_array().unwrap();
        assert_eq!(out.len(), 50);
        let first: Value = serde_json::from_str(out[0].as_str().unwrap()).unwrap();
        assert!(first["timestamp"].as_str().unwrap().ends_with('Z'));
        assert!(first["target"].as_str().unwrap().contains("::"));
        assert!(
            out.iter()
                .any(|l| l.as_str().unwrap().contains("\"ERROR\"")),
            "a down app logs errors"
        );
    }

    #[test]
    fn the_backend_logs_from_more_modules_than_fit_collapsed() {
        let body = logs(&url("demo-backend"), None, Some("500")).unwrap();
        let targets: std::collections::HashSet<String> = body["out"]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l.as_str()?).ok())
            .filter_map(|l| l["target"].as_str().map(str::to_string))
            .collect();
        assert!(targets.len() > 8, "{targets:?}");
    }

    #[test]
    fn refreshing_keeps_the_older_lines() {
        let a = stdout_line("demo-estable", Profile::Steady, 1000, 3);
        let b = stdout_line("demo-estable", Profile::Steady, 1000, 3);
        assert_eq!(a, b);
    }

    #[test]
    fn timestamps_are_valid_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000000Z");
        assert!(rfc3339(1_790_000_000_123).starts_with("2026-09-21T"));
    }
}
