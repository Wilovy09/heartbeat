//! GET /metrics -- every app's state in the Prometheus text format, for scraping into an
//! existing monitoring stack. Off unless `METRICS_TOKEN` is set; scrapers authenticate with
//! `Authorization: Bearer <METRICS_TOKEN>`.

use actix_web::{HttpRequest, HttpResponse, http::header, web};
use std::fmt::Write as _;

use crate::{
    config::Config,
    registry::AppRegistry,
    token,
    uptime::{MonitorSummary, Status, UptimeMonitor},
};

/// `\`, `"` and newlines escaped, as label values require.
fn label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// One gauge family: HELP/TYPE lines, then one sample per app that has a value.
fn gauge(
    out: &mut String,
    name: &str,
    help: &str,
    monitors: &[MonitorSummary],
    value: impl Fn(&MonitorSummary) -> Option<f64>,
) {
    // Writing into a String can't fail.
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge");
    for m in monitors {
        if let Some(v) = value(m) {
            let _ = writeln!(
                out,
                "{name}{{app=\"{}\",name=\"{}\"}} {v}",
                label(&m.slug),
                label(&m.name)
            );
        }
    }
}

#[must_use]
pub fn render(monitors: &[MonitorSummary]) -> String {
    let mut out = String::new();
    gauge(
        &mut out,
        "heartbeat_up",
        "1 if the last check was up or degraded, 0 if down.",
        monitors,
        |m| m.status.map(|s| f64::from(u8::from(s != Status::Down))),
    );
    gauge(
        &mut out,
        "heartbeat_degraded",
        "1 if the last check was degraded (slow, or a certificate about to expire).",
        monitors,
        |m| m.status.map(|s| f64::from(u8::from(s == Status::Degraded))),
    );
    gauge(
        &mut out,
        "heartbeat_paused",
        "1 while the app's checks are paused.",
        monitors,
        |m| Some(f64::from(u8::from(m.paused))),
    );
    gauge(
        &mut out,
        "heartbeat_latency_milliseconds",
        "Response time of the last check.",
        monitors,
        |m| m.latency_ms.map(f64::from),
    );
    gauge(
        &mut out,
        "heartbeat_uptime_24h_ratio",
        "Share of available checks over the last 24 hours (0-1).",
        monitors,
        |m| m.uptime_24h.map(|pct| pct / 100.0),
    );
    gauge(
        &mut out,
        "heartbeat_uptime_30d_ratio",
        "Share of available checks over the last 30 days (0-1).",
        monitors,
        |m| m.uptime_30d.map(|pct| pct / 100.0),
    );
    gauge(
        &mut out,
        "heartbeat_cert_expiry_timestamp_seconds",
        "When the health endpoint's TLS certificate expires (unix seconds).",
        monitors,
        #[allow(clippy::cast_precision_loss)] // unix seconds fit an f64 exactly until year 285M
        |m| m.cert_expires_at.map(|at| at as f64),
    );
    out
}

pub async fn show(
    req: HttpRequest,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
) -> HttpResponse {
    let Some(expected) = &cfg.metrics_token else {
        return HttpResponse::NotFound().finish();
    };
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !token::secret_matches(expected, presented) {
        return HttpResponse::Unauthorized()
            .insert_header((header::WWW_AUTHENTICATE, "Bearer"))
            .finish();
    }
    let apps = registry.list().await;
    let overview = monitor.overview(&apps).await;
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4")
        .body(render(&overview.monitors))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_labelled_and_escaped() {
        let m = MonitorSummary {
            slug: "api".into(),
            name: "API \"v2\"".into(),
            health_url: None,
            has_logs: false,
            paused: false,
            paused_at: None,
            paused_until: None,
            status: Some(Status::Down),
            latency_ms: None,
            avg_latency_24h_ms: None,
            uptime_24h: Some(99.5),
            uptime_30d: None,
            cert_expires_at: Some(1_900_000_000),
            recent: Vec::new(),
        };
        let out = render(&[m]);
        assert!(
            out.contains("heartbeat_up{app=\"api\",name=\"API \\\"v2\\\"\"} 0\n"),
            "{out}"
        );
        assert!(
            out.contains("heartbeat_uptime_24h_ratio{app=\"api\",name=\"API \\\"v2\\\"\"} 0.995\n")
        );
        assert!(out.contains("heartbeat_cert_expiry_timestamp_seconds{app=\"api\""));
        assert!(
            !out.contains("heartbeat_latency_milliseconds{"),
            "no value, no sample"
        );
    }
}
