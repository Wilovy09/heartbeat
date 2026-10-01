//! JSON endpoints behind the dashboard's system panels: Heartbeat's own host, and each
//! app that has a system URL.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;
use std::time::Duration;

use crate::{
    auth,
    registry::AppRegistry,
    system::{HOST, Reading, SystemMonitor},
};

/// Longest chart window: the default retention.
const MAX_WINDOW_HOURS: u64 = 7 * 24;

#[derive(Deserialize)]
pub struct WindowQuery {
    hours: Option<u64>,
}

fn window(query: &WindowQuery) -> Duration {
    Duration::from_secs(query.hours.unwrap_or(24).clamp(1, MAX_WINDOW_HOURS) * 3600)
}

fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized().json(serde_json::json!({ "error": "unauthorized" }))
}

/// The figures an app's card shows, from its latest reading.
fn summary(reading: &Reading) -> serde_json::Value {
    let snapshot = reading.snapshot.as_ref();
    serde_json::json!({
        "at": reading.at,
        "error": reading.error,
        "cpu_pct": snapshot.map(|s| s.cpu.usage_pct),
        "memory_pct": snapshot.map(crate::system::Snapshot::memory_pct),
        "disk_pct": snapshot.and_then(|s| s.fullest_disk()).map(|d| {
            #[allow(clippy::cast_precision_loss)]
            let pct = d.used_bytes as f64 * 100.0 / d.total_bytes.max(1) as f64;
            pct
        }),
    })
}

/// GET /api/system?hours=24 -- Heartbeat's host (latest reading and chart), and a summary
/// of every app's latest reading.
pub async fn host(
    req: HttpRequest,
    system: web::Data<SystemMonitor>,
    query: web::Query<WindowQuery>,
) -> HttpResponse {
    if auth::session_token(&req).is_none() {
        return unauthorized();
    }
    let samples = match system.samples(HOST, window(&query)).await {
        Ok(samples) => samples,
        Err(e) => {
            tracing::error!(error = %e, "system: could not read the samples");
            Vec::new()
        }
    };
    let apps: serde_json::Map<String, serde_json::Value> = system
        .latest_all()
        .await
        .iter()
        .filter(|(source, _)| source.as_str() != HOST)
        .map(|(source, reading)| (source.clone(), summary(reading)))
        .collect();
    HttpResponse::Ok().json(serde_json::json!({
        "interval_secs": system.interval().as_secs(),
        "reading": system.latest(HOST).await,
        "samples": samples,
        "apps": apps,
    }))
}

/// GET /api/system/{slug}?hours=24 -- one app's latest reading and chart.
pub async fn app(
    req: HttpRequest,
    registry: web::Data<AppRegistry>,
    system: web::Data<SystemMonitor>,
    path: web::Path<String>,
    query: web::Query<WindowQuery>,
) -> HttpResponse {
    if auth::session_token(&req).is_none() {
        return unauthorized();
    }
    let slug = path.into_inner();
    if registry
        .find(&slug)
        .await
        .is_none_or(|a| a.system_url.is_none())
    {
        return HttpResponse::NotFound().json(serde_json::json!({ "error": "not found" }));
    }
    let samples = system
        .samples(&slug, window(&query))
        .await
        .unwrap_or_else(|e| {
            tracing::error!(app = %slug, error = %e, "system: could not read the samples");
            Vec::new()
        });
    HttpResponse::Ok().json(serde_json::json!({
        "interval_secs": system.interval().as_secs(),
        "reading": system.latest(&slug).await,
        "samples": samples,
    }))
}
