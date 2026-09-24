//! JSON endpoints backing the uptime dashboard (static/uptime.js).

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;
use std::time::Duration;

use crate::{auth, registry::AppRegistry, uptime::UptimeMonitor};

/// Longest chart window the detail endpoint serves -- matches the monitor's retention.
const MAX_WINDOW_HOURS: u64 = 30 * 24;

fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized().json(serde_json::json!({ "error": "No hay sesión activa" }))
}

/// GET /api/uptime -- every registered app's current status, uptime and recent heartbeats.
pub async fn overview(
    req: HttpRequest,
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
) -> HttpResponse {
    if auth::session_token(&req).is_none() {
        return unauthorized();
    }
    let apps = registry.list().await;
    HttpResponse::Ok().json(monitor.overview(&apps).await)
}

#[derive(Deserialize)]
pub struct DetailQuery {
    hours: Option<u64>,
}

/// GET /api/uptime/{slug}?hours=6 -- heartbeats in the window (for the latency chart) plus
/// the app's status-change events.
pub async fn detail(
    req: HttpRequest,
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
    path: web::Path<String>,
    query: web::Query<DetailQuery>,
) -> HttpResponse {
    if auth::session_token(&req).is_none() {
        return unauthorized();
    }
    let slug = path.into_inner();
    if registry.find(&slug).await.is_none() {
        return HttpResponse::NotFound().json(serde_json::json!({
            "error": format!("App '{slug}' no está registrada")
        }));
    }
    let hours = query.hours.unwrap_or(6).clamp(1, MAX_WINDOW_HOURS);
    HttpResponse::Ok().json(
        monitor
            .detail(&slug, Duration::from_secs(hours * 3600))
            .await,
    )
}
