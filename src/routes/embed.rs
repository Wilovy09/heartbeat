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

#[derive(Deserialize)]
pub struct BadgeQuery {
    token: String,
    /// Left-hand text; defaults to the app's name.
    label: Option<String>,
}

/// Rough width of `text` in 11px Verdana, the shields.io metric.
fn text_width(text: &str) -> usize {
    text.chars()
        .map(|c| match c {
            'i' | 'l' | 'j' | '.' | ',' | ':' | '|' | '!' | '\'' => 4,
            'm' | 'w' | 'M' | 'W' => 10,
            c if c.is_uppercase() => 8,
            _ => 7,
        })
        .sum()
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A shields.io-style badge: `label | state`, colored by state. Its gradient and clip ids
/// are derived from the content, so several badges inlined in one page don't share (and
/// clip each other with) the first one's definitions.
#[must_use]
pub fn badge_svg(label: &str, state: &str, color: &str) -> String {
    let label: String = label.chars().take(40).collect();
    let (lw, sw) = (text_width(&label) + 12, text_width(state) + 12);
    let total = lw + sw;
    let id = format!(
        "hb{:x}",
        format!("{label}|{state}|{color}")
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            })
    );
    let (label, state) = (xml_escape(&label), xml_escape(state));
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{total}" height="20" role="img" aria-label="{label}: {state}"><title>{label}: {state}</title><linearGradient id="{id}s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient><clipPath id="{id}r"><rect width="{total}" height="20" rx="3" fill="#fff"/></clipPath><g clip-path="url(#{id}r)"><rect width="{lw}" height="20" fill="#555"/><rect x="{lw}" width="{sw}" height="20" fill="{color}"/><rect width="{total}" height="20" fill="url(#{id}s)"/></g><g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11"><text x="{lx}" y="14">{label}</text><text x="{sx}" y="14">{state}</text></g></svg>"##,
        lx = lw / 2,
        sx = lw + sw / 2,
    )
}

/// GET /badge/{slug}.svg?token=... -- the app's status as an image, for READMEs and
/// wikis. Same token and the same 404 as the embed.
pub async fn badge(
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
    i18n: web::Data<I18n>,
    path: web::Path<String>,
    query: web::Query<BadgeQuery>,
) -> HttpResponse {
    let slug = path.into_inner();
    let slug = slug.strip_suffix(".svg").unwrap_or(&slug);
    let Some(app) = registry
        .find(slug)
        .await
        .filter(|app| app.embed_token_matches(&query.token))
    else {
        return HttpResponse::NotFound().finish();
    };
    let summary = monitor.summary(&app).await;
    let (key, color) = match (summary.paused, summary.status) {
        (true, _) => ("paused", "#6e737c"),
        (false, Some(Status::Up)) => ("up", "#3fb950"),
        (false, Some(Status::Degraded)) => ("degraded", "#d29922"),
        (false, Some(Status::Down)) => ("down", "#e5534b"),
        (false, None) => ("unknown", "#6e737c"),
    };
    let state = i18n.text(&format!("status.state_{key}"), &[]);
    let label = query.label.as_deref().unwrap_or(&app.name);
    HttpResponse::Ok()
        .content_type("image/svg+xml")
        .insert_header(("Cache-Control", "public, max-age=60"))
        .insert_header(("Access-Control-Allow-Origin", "*"))
        .body(badge_svg(label, &state, color))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_escape_their_text() {
        let svg = badge_svg("<script>", "Operational", "#3fb950");
        assert!(svg.starts_with("<svg") && svg.contains("&lt;script&gt;"));
        assert!(!svg.contains("<script>"));
    }

    #[test]
    fn badges_on_one_page_never_share_ids() {
        let up = badge_svg("API", "Operational", "#3fb950");
        let down = badge_svg("API", "Down", "#e5534b");
        let clip = |svg: &str| svg.split("clipPath id=\"").nth(1).unwrap()[..10].to_string();
        assert_ne!(clip(&up), clip(&down));
    }
}
