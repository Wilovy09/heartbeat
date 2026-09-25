//! /settings: alerting (test the webhooks, preview and send each alert kind) and the
//! current configuration, read-only -- it comes from the environment, so it changes by
//! editing .env and restarting.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use tera::{Context, Tera};

use crate::{
    alerts::{Alerter, TestKind},
    auth,
    config::{AuthMode, Config},
};

/// Configuration as the page shows it: secrets reduced to "set / not set".
#[derive(Serialize)]
struct ConfigView {
    auth_mode: &'static str,
    login_url: Option<String>,
    admin_email: Option<String>,
    app_lang: &'static str,
    public_url: Option<String>,
    allowed_hosts: String,
    admin_logs_key: bool,
    interval_secs: u64,
    degraded_ms: u32,
    retries: u32,
    cert_warn_days: u32,
    retention_days: u32,
    dead_mans_switch: bool,
    alert_on_degraded: bool,
}

impl From<&Config> for ConfigView {
    fn from(cfg: &Config) -> Self {
        let (auth_mode, login_url, admin_email) = match &cfg.auth {
            AuthMode::Upstream { login_url } => ("upstream", Some(login_url.clone()), None),
            AuthMode::Password { email, .. } => ("password", None, Some(email.clone())),
        };
        Self {
            auth_mode,
            login_url,
            admin_email,
            app_lang: cfg.app_lang.code(),
            public_url: cfg.public_url.clone(),
            allowed_hosts: cfg.allowed_hosts.clone(),
            admin_logs_key: cfg.admin_logs_key.is_some(),
            interval_secs: cfg.uptime_interval_secs,
            degraded_ms: cfg.uptime_degraded_ms,
            retries: cfg.uptime_retries,
            cert_warn_days: cfg.uptime_cert_warn_days,
            retention_days: cfg.uptime_retention_days,
            dead_mans_switch: cfg.heartbeat_ping_url.is_some(),
            alert_on_degraded: cfg.alert_on_degraded,
        }
    }
}

/// GET /settings
pub async fn show(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    alerter: web::Data<Option<Alerter>>,
) -> HttpResponse {
    if let Err(resp) = auth::require_session(&req) {
        return resp;
    }
    let mut ctx = Context::new();
    ctx.insert("active", "settings");
    ctx.insert("alerts", &alerter.as_ref().as_ref().map(Alerter::overview));
    ctx.insert("config", &ConfigView::from(cfg.get_ref()));
    match tera.render("settings.html", &ctx) {
        Ok(html) => HttpResponse::Ok().content_type("text/html").body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}

#[derive(Deserialize)]
pub struct TestRequest {
    /// Webhook index; absent = every webhook.
    target: Option<usize>,
    kind: TestKind,
}

/// POST /settings/alerts/test -- sends a ping or a sample alert and reports each
/// webhook's answer.
pub async fn test_alert(
    req: HttpRequest,
    alerter: web::Data<Option<Alerter>>,
    body: web::Json<TestRequest>,
) -> HttpResponse {
    if auth::session_token(&req).is_none() {
        return HttpResponse::Unauthorized().json(serde_json::json!({ "error": "unauthorized" }));
    }
    let Some(alerter) = alerter.as_ref().as_ref() else {
        return HttpResponse::Conflict().json(serde_json::json!({ "error": "no webhooks" }));
    };
    let outcomes = alerter.test(body.target, body.kind).await;
    HttpResponse::Ok().json(serde_json::json!({ "outcomes": outcomes }))
}
