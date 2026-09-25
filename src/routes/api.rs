//! Server-side proxy to a registered app's /admin/logs endpoint. The browser only ever
//! talks to this app (same origin, cookie auth) -- the session token is attached here and
//! never sent to client-side JS, so there's nothing for a compromised page to steal it
//! with. The registered endpoint enforces its own admin check on every call; a stale or
//! revoked token surfaces here as a 401/403 we pass straight through, which the dashboard
//! JS treats as "log in again."

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;

use crate::{auth, config::Config, outbound::Outbound, registry::AppRegistry};

#[derive(Deserialize)]
pub struct LogsQuery {
    stream: Option<String>,
    lines: Option<String>,
}

pub async fn get_logs(
    req: HttpRequest,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    outbound: web::Data<Outbound>,
    path: web::Path<String>,
    query: web::Query<LogsQuery>,
) -> HttpResponse {
    let Some(token) = auth::session_token(&req) else {
        return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "No hay sesión activa"
        }));
    };

    let slug = path.into_inner();
    let Some(app) = registry.find(&slug).await else {
        return HttpResponse::NotFound().json(serde_json::json!({
            "error": format!("App '{slug}' no está registrada")
        }));
    };

    // Checked before anything is sent: this request carries the admin's JWT and
    // ADMIN_LOGS_KEY, which must only ever reach allowlisted https hosts.
    let logs_url = match outbound.check(&app.logs_url) {
        Ok(url) => url,
        Err(e) => {
            tracing::warn!(app = %slug, error = %e, "logs proxy: refused non-allowlisted url");
            return HttpResponse::Forbidden().json(serde_json::json!({
                "error": format!("La URL de logs de '{}' no está permitida: {e}", app.name)
            }));
        }
    };

    // The user's own token still goes along (harmless, and it's what authorizes a
    // registered app that hasn't opted into the shared-key path) -- ADMIN_LOGS_KEY, when
    // configured, is what actually authorizes across environments. See Config::admin_logs_key.
    let mut req_builder = outbound.client().get(logs_url);
    // Empty in AuthMode::Password: there's no upstream JWT, only ADMIN_LOGS_KEY.
    if !token.is_empty() {
        req_builder = req_builder.bearer_auth(&token);
    }
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
            tracing::warn!(app = %slug, error = %e, "logs proxy: upstream request failed");
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
        // The body itself is never echoed back: if the URL ever pointed somewhere it
        // shouldn't, the response must not become a way to read that endpoint.
        Err(_) => HttpResponse::build(out_status).json(serde_json::json!({
            "error": format!(
                "'{}' respondió {} con un cuerpo no-JSON ({} bytes)",
                app.name,
                status.as_u16(),
                raw_body.len()
            )
        })),
    }
}
