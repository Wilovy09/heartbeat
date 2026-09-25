//! GET /status -- the public status page: every app marked public, with the same
//! information the embed shows (never URLs or check messages). No login; opt-in per app
//! from /apps.

use actix_web::{HttpResponse, web};
use serde::Serialize;
use tera::{Context, Tera};

use crate::{
    registry::AppRegistry,
    uptime::{MonitorSummary, Status, UptimeMonitor},
};

/// Checks drawn per app. Phones hide the oldest ones in CSS.
const STRIP_SLOTS: usize = 60;

#[derive(Serialize)]
struct StatusRow {
    name: String,
    /// "up" | "degraded" | "down" | "paused" | "unknown" -- the CSS state key.
    state: &'static str,
    uptime_24h: String,
    /// Oldest first, left-padded with `None` so the latest check is always rightmost.
    strip: Vec<Option<Status>>,
}

impl From<MonitorSummary> for StatusRow {
    fn from(summary: MonitorSummary) -> Self {
        let state = match (summary.paused, summary.status) {
            (true, _) => "paused",
            (false, Some(Status::Up)) => "up",
            (false, Some(Status::Degraded)) => "degraded",
            (false, Some(Status::Down)) => "down",
            (false, None) => "unknown",
        };
        let latest = summary.recent.len().saturating_sub(STRIP_SLOTS);
        let mut strip: Vec<Option<Status>> =
            vec![None; STRIP_SLOTS - (summary.recent.len() - latest)];
        strip.extend(summary.recent[latest..].iter().map(|b| Some(b.status)));
        Self {
            name: summary.name,
            state,
            uptime_24h: summary
                .uptime_24h
                .map_or_else(|| "—".to_string(), |pct| format!("{pct:.2}%")),
            strip,
        }
    }
}

pub async fn show(
    tera: web::Data<Tera>,
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
) -> HttpResponse {
    let mut rows = Vec::new();
    for app in registry.list().await.iter().filter(|a| a.public) {
        rows.push(StatusRow::from(monitor.summary(app).await));
    }
    let verdict = ["down", "degraded", "up"]
        .into_iter()
        .find(|state| rows.iter().any(|r| r.state == *state))
        .unwrap_or("unknown");

    let mut ctx = Context::new();
    ctx.insert("rows", &rows);
    ctx.insert("verdict", verdict);
    match tera.render("status.html", &ctx) {
        Ok(html) => HttpResponse::Ok()
            .content_type("text/html")
            .insert_header(("Cache-Control", "public, max-age=30"))
            .body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}
