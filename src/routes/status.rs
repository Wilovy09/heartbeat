//! GET /status/{slug} -- an app's public status page: its state, 30 days of daily uptime
//! and the notices that name it. Only for apps published from /apps; anything else is the
//! same 404, so the page can't be used to discover apps. Never shows URLs or check
//! messages. No login.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Serialize;
use tera::{Context, Tera};

use crate::{
    notices::NoticeStore,
    registry::AppRegistry,
    uptime::{DayUptime, MonitorSummary, Status, UptimeMonitor, unix_now},
};

/// Days of history drawn per app. Phones hide the oldest ones in CSS.
const HISTORY_DAYS: u64 = 30;

/// `100%`, or two decimals below it (`99.95%`).
fn pct(value: f64) -> String {
    if value >= 99.995 {
        "100%".to_string()
    } else {
        format!("{value:.2}%")
    }
}

/// One day's bar: its uptime, and how it reads at a glance.
#[derive(Serialize)]
struct DayBar {
    start: u64,
    /// Formatted for the tooltip; empty without data.
    uptime: String,
    /// "up" | "degraded" | "down" | "none" -- the CSS state key.
    level: &'static str,
}

impl From<DayUptime> for DayBar {
    fn from(day: DayUptime) -> Self {
        let level = match day.uptime {
            None => "none",
            Some(pct) if pct >= 99.95 => "up",
            Some(pct) if pct >= 99.0 => "degraded",
            Some(_) => "down",
        };
        Self {
            start: day.start,
            uptime: day.uptime.map(pct).unwrap_or_default(),
            level,
        }
    }
}

#[derive(Serialize)]
struct StatusRow {
    name: String,
    /// "up" | "degraded" | "down" | "paused" | "unknown" -- the CSS state key.
    state: &'static str,
    /// Over the whole drawn history.
    uptime: String,
    days: Vec<DayBar>,
}

impl StatusRow {
    fn new(summary: &MonitorSummary, days: Vec<DayUptime>) -> Self {
        let state = match (summary.paused, summary.status) {
            (true, _) => "paused",
            (false, Some(Status::Up)) => "up",
            (false, Some(Status::Degraded)) => "degraded",
            (false, Some(Status::Down)) => "down",
            (false, None) => "unknown",
        };
        let measured: Vec<f64> = days.iter().filter_map(|d| d.uptime).collect();
        #[allow(clippy::cast_precision_loss)] // at most 30 days
        let uptime = (!measured.is_empty())
            .then(|| measured.iter().sum::<f64>() / measured.len() as f64)
            .map_or_else(|| "—".to_string(), pct);
        Self {
            name: summary.name.clone(),
            state,
            uptime,
            days: days.into_iter().map(DayBar::from).collect(),
        }
    }
}

/// The headline: the app's state, unless an open notice says more. A notice while the
/// checks are green still means someone is dealing with something.
fn verdict(state: &'static str, open_notices: bool) -> &'static str {
    match (state, open_notices) {
        ("down", _) | (_, false) => state,
        (_, true) => "incident",
    }
}

pub async fn show(
    req: HttpRequest,
    tera: web::Data<Tera>,
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
    notices: web::Data<NoticeStore>,
    path: web::Path<String>,
) -> HttpResponse {
    let slug = path.into_inner();
    let Some(app) = registry.find(&slug).await.filter(|a| a.public) else {
        return HttpResponse::NotFound()
            .content_type("text/plain; charset=utf-8")
            .body("404");
    };
    let summary = monitor.summary(&app).await;
    let row = StatusRow::new(&summary, monitor.daily(&app.slug, HISTORY_DAYS).await);
    let now = unix_now();
    let notices = notices.public_for(&app.slug, now).await;

    let mut ctx = Context::new();
    ctx.insert("verdict", verdict(row.state, !notices.open.is_empty()));
    ctx.insert("row", &row);
    ctx.insert("notices", &notices);
    ctx.insert("now", &now);
    ctx.insert("history_days", &HISTORY_DAYS);
    ctx.insert("page_url", req.path());
    match tera.render("status.html", &ctx) {
        Ok(html) => HttpResponse::Ok()
            .content_type("text/html")
            .insert_header(("Cache-Control", "public, max-age=30"))
            .body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}
