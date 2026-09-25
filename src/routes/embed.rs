//! Public, read-only status for the `<heartbeat-status>` embed (static/embed.js), meant to
//! be fetched cross-origin from other sites. Authorized per app by its `embed_token`, not
//! by the admin session. Serves only what the widget draws -- never the health URL or the
//! check messages, which can leak internal hosts and error details.

use actix_web::{HttpResponse, web};
use serde::{Deserialize, Serialize};

use crate::{
    i18n::I18n,
    registry::AppRegistry,
    uptime::{Heartbeat, MonitorSummary, Status, UptimeMonitor},
};

#[derive(Serialize)]
pub struct EmbedBeat {
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

/// The public projection of an app's status -- also what `/status` renders. Only what the
/// widgets draw: never the health URL or the check messages.
#[derive(Serialize)]
pub struct EmbedStatus {
    pub name: String,
    pub paused: bool,
    pub status: Option<Status>,
    pub uptime_24h: Option<f64>,
    pub recent: Vec<EmbedBeat>,
}

/// What the endpoint returns: the status plus the server's UI language, which the widget
/// uses unless the embedding page sets its own `lang` attribute.
#[derive(Serialize)]
struct EmbedResponse {
    #[serde(flatten)]
    status: EmbedStatus,
    lang: &'static str,
}

impl From<MonitorSummary> for EmbedStatus {
    fn from(summary: MonitorSummary) -> Self {
        Self {
            name: summary.name,
            paused: summary.paused,
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
    i18n: web::Data<I18n>,
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
        .json(EmbedResponse {
            status: EmbedStatus::from(monitor.summary(&app).await),
            lang: i18n.lang().code(),
        })
}
