//! Public, read-only status for the `<heartbeat-status>` embed (static/embed.js), meant to
//! be fetched cross-origin from other sites. Authorized per app by its `embed_token`, not
//! by the admin session. Serves only what the widget draws -- never the health URL or the
//! check messages, which can leak internal hosts and error details.

use actix_web::{HttpResponse, web};
use serde::{Deserialize, Serialize};

use crate::{
    registry::AppRegistry,
    uptime::{Heartbeat, MonitorSummary, Status, UptimeMonitor},
};

#[derive(Serialize)]
struct EmbedBeat {
    at: u64,
    status: Status,
    latency_ms: Option<u32>,
}

impl From<Heartbeat> for EmbedBeat {
    fn from(beat: Heartbeat) -> Self {
        Self {
            at: beat.at,
            status: beat.status,
            latency_ms: beat.latency_ms,
        }
    }
}

#[derive(Serialize)]
struct EmbedStatus {
    name: String,
    status: Option<Status>,
    uptime_24h: Option<f64>,
    recent: Vec<EmbedBeat>,
}

impl From<MonitorSummary> for EmbedStatus {
    fn from(summary: MonitorSummary) -> Self {
        Self {
            name: summary.name,
            status: summary.status,
            uptime_24h: summary.uptime_24h,
            recent: summary.recent.into_iter().map(EmbedBeat::from).collect(),
        }
    }
}

#[derive(Deserialize)]
pub struct EmbedQuery {
    token: String,
}

/// GET /embed/{slug}?token=... -- an unknown slug and a wrong token both answer the same
/// 404, so the endpoint can't be used to discover which apps exist.
pub async fn status(
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
    path: web::Path<String>,
    query: web::Query<EmbedQuery>,
) -> HttpResponse {
    let app = registry
        .find(&path.into_inner())
        .await
        .filter(|app| app.embed_token_matches(&query.token));
    let Some(app) = app else {
        return HttpResponse::NotFound()
            .insert_header(("Access-Control-Allow-Origin", "*"))
            .json(serde_json::json!({ "error": "not found" }));
    };
    HttpResponse::Ok()
        .insert_header(("Access-Control-Allow-Origin", "*"))
        .insert_header(("Cache-Control", "public, max-age=15"))
        .json(EmbedStatus::from(monitor.summary(&app).await))
}
