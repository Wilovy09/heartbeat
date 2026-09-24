//! Server-side proxy to a registered app's /admin/logs endpoint. The browser only ever
//! talks to this app (same origin, cookie auth) -- the session token is attached here and
//! never sent to client-side JS, so there's nothing for a compromised page to steal it
//! with. The registered endpoint enforces its own admin check on every call; a stale or
//! revoked token surfaces here as a 401/403 we pass straight through, which the dashboard
//! JS treats as "log in again."

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;

use crate::{auth, config::Config, registry::AppRegistry};

#[derive(Deserialize)]
pub struct LogsQuery {
    stream: Option<String>,
    lines: Option<String>,
}

pub async fn get_logs(
    req: HttpRequest,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    path: web::Path<String>,
    query: web::Query<LogsQuery>,
) -> HttpResponse {
    let token = match auth::session_token(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "No hay sesión activa"
            }));
        }
    };

    let slug = path.into_inner();
    let Some(app) = registry.find(&slug).await else {
        return HttpResponse::NotFound().json(serde_json::json!({
            "error": format!("App '{slug}' no está registrada")
        }));
    };

    let client = reqwest::Client::new();
    // The user's own token still goes along (harmless, and it's what authorizes a
    // registered app that hasn't opted into the shared-key path) -- ADMIN_LOGS_KEY, when
    // configured, is what actually authorizes across environments. See Config::admin_logs_key.
    let mut req_builder = client.get(&app.logs_url).bearer_auth(&token);
    if let Some(key) = &cfg.admin_logs_key {
        req_builder = req_builder.header("X-Admin-Logs-Key", key);
    }
    if let Some(stream) = &query.stream {
        req_builder = req_builder.query(&[("stream", stream)]);
    }
    if let Some(lines) = &query.lines {
        req_builder = req_builder.query(&[("lines", lines)]);
    }

    let upstream = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(app = %slug, url = %app.logs_url, error = %e, "logs proxy: upstream request failed");
            return HttpResponse::BadGateway().json(serde_json::json!({
                "error": format!("No se pudo conectar con '{}': {e}", app.name)
            }));
        }
    };

    let status = upstream.status();
    let out_status = actix_web::http::StatusCode::from_u16(status.as_u16())
        .unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY);
    let raw_body = match upstream.text().await {
        Ok(t) => t,
        Err(e) => {
            return HttpResponse::BadGateway().json(serde_json::json!({
                "error": format!("No se pudo leer la respuesta de '{}': {e}", app.name)
            }));
        }
    };

    // Forward the real upstream status either way -- a 404 (route not deployed yet on
    // that app), a 401/403 (bad/expired token) or an empty body all matter for the
    // dashboard JS to react to correctly, not just "the JSON didn't parse."
    match serde_json::from_str::<serde_json::Value>(&raw_body) {
        Ok(json) => HttpResponse::build(out_status).json(json),
        Err(_) => HttpResponse::build(out_status).json(serde_json::json!({
            "error": format!(
                "'{}' respondió {} con un cuerpo no-JSON: {}",
                app.name,
                status.as_u16(),
                if raw_body.is_empty() { "(vacío)" } else { &raw_body }
            )
        })),
    }
}
