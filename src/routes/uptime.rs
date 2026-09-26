//! JSON endpoints backing the uptime dashboard (static/uptime.js).

use actix_web::{HttpRequest, HttpResponse, http::header, web};
use serde::Deserialize;
use std::fmt::Write as _;
use std::time::Duration;

use crate::{auth, i18n::I18n, registry::AppRegistry, uptime::UptimeMonitor};

/// Longest chart window the detail endpoint serves -- matches the monitor's retention.
const MAX_WINDOW_HOURS: u64 = 30 * 24;

fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized().json(serde_json::json!({ "error": "unauthorized" }))
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
    i18n: web::Data<I18n>,
    path: web::Path<String>,
    query: web::Query<DetailQuery>,
) -> HttpResponse {
    if auth::session_token(&req).is_none() {
        return unauthorized();
    }
    let slug = path.into_inner();
    if registry.find(&slug).await.is_none() {
        return HttpResponse::NotFound().json(serde_json::json!({
            "error": i18n.text("api.not_registered", &[("slug", &slug)])
        }));
    }
    let hours = query.hours.unwrap_or(6).clamp(1, MAX_WINDOW_HOURS);
    HttpResponse::Ok().json(
        monitor
            .detail(&slug, Duration::from_secs(hours * 3600))
            .await,
    )
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    #[default]
    Csv,
    Json,
}

#[derive(Deserialize)]
pub struct ExportQuery {
    hours: Option<u64>,
    #[serde(default)]
    format: ExportFormat,
}

/// A CSV field, quoted when it holds a comma, quote or newline.
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// GET /api/uptime/{slug}/export?hours=720&format=csv|json -- every heartbeat in the
/// window, not aggregated, as a download.
pub async fn export(
    req: HttpRequest,
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
    path: web::Path<String>,
    query: web::Query<ExportQuery>,
) -> HttpResponse {
    if auth::session_token(&req).is_none() {
        return unauthorized();
    }
    let slug = path.into_inner();
    if registry.find(&slug).await.is_none() {
        return HttpResponse::NotFound().json(serde_json::json!({ "error": "not found" }));
    }
    let hours = query.hours.unwrap_or(24).clamp(1, MAX_WINDOW_HOURS);
    let beats = monitor
        .export(&slug, Duration::from_secs(hours * 3600))
        .await;
    let (body, content_type, ext) = match query.format {
        ExportFormat::Json => (
            serde_json::to_string(&beats).unwrap_or_else(|_| "[]".to_string()),
            "application/json",
            "json",
        ),
        ExportFormat::Csv => {
            let mut csv = String::from("at,status,latency_ms,message\n");
            for b in &beats {
                let status = serde_json::to_value(b.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                // Writing into a String can't fail.
                let _ = writeln!(
                    csv,
                    "{},{status},{},{}",
                    b.at,
                    b.latency_ms.map(|ms| ms.to_string()).unwrap_or_default(),
                    csv_field(&b.message)
                );
            }
            (csv, "text/csv; charset=utf-8", "csv")
        }
    };
    HttpResponse::Ok()
        .content_type(content_type)
        .insert_header((
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{slug}-{hours}h.{ext}\""),
        ))
        .body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_fields_are_quoted_only_when_needed() {
        assert_eq!(csv_field("HTTP 200 OK"), "HTTP 200 OK");
        assert_eq!(csv_field("a, \"b\""), "\"a, \"\"b\"\"\"");
    }
}
