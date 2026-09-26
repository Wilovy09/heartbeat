//! /settings: alerting (test the webhooks, preview and send each alert kind) and the
//! current configuration, read-only -- it comes from the environment, so it changes by
//! editing .env and restarting.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use tera::{Context, Tera};

use crate::{
    alert_templates::{AlertTemplates, MAX_TEMPLATE_CHARS},
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
    timeout_secs: u32,
    mass_down_pct: u8,
    remind_mins: u32,
}

impl From<&Config> for ConfigView {
    fn from(cfg: &Config) -> Self {
        let (auth_mode, login_url, admin_email) = match &cfg.auth {
            AuthMode::Upstream { login_url, .. } => ("upstream", Some(login_url.clone()), None),
            AuthMode::Password { admin, .. } => ("password", None, Some(admin.email.clone())),
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
            timeout_secs: cfg.uptime_timeout_secs,
            mass_down_pct: cfg.uptime_mass_down_pct,
            remind_mins: cfg.alert_remind_mins,
        }
    }
}

/// GET /settings
pub async fn show(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    alerter: web::Data<Alerter>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    let mut ctx = Context::new();
    ctx.insert("active", "settings");
    ctx.insert("is_admin", &true);
    let overview = alerter.overview();
    // For the Alpine component: templates, defaults and sample values, as a JS literal.
    let templates_json =
        serde_json::to_string(&overview.templates).unwrap_or_else(|_| "[]".to_string());
    let has_mentions = !overview.mentions.is_empty();
    ctx.insert("alerts", &overview);
    ctx.insert("templates_json", &templates_json);
    ctx.insert("has_mentions", &has_mentions);
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
    alerter: web::Data<Alerter>,
    body: web::Json<TestRequest>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin_json(&req) {
        return resp;
    }
    if !alerter.has_global_webhooks() {
        return HttpResponse::Conflict().json(serde_json::json!({ "error": "no webhooks" }));
    }
    let outcomes = alerter.test(body.target, body.kind).await;
    HttpResponse::Ok().json(serde_json::json!({ "outcomes": outcomes }))
}

/// POST /settings/alerts/templates -- saves the alert message templates (blank = back to
/// the default) and answers with the fresh previews.
pub async fn save_templates(
    req: HttpRequest,
    alerter: web::Data<Alerter>,
    body: web::Json<AlertTemplates>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin_json(&req) {
        return resp;
    }
    let templates = body.into_inner();
    if let Some(kind) = templates.too_long() {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("template too long (max {MAX_TEMPLATE_CHARS} characters)"),
            "kind": kind,
        }));
    }
    if let Err(e) = alerter.templates().save(templates).await {
        tracing::error!(error = %e, "settings: could not save alert templates");
        return HttpResponse::InternalServerError()
            .json(serde_json::json!({ "error": e.to_string() }));
    }
    tracing::info!("settings: alert templates updated");
    let overview = alerter.overview();
    HttpResponse::Ok().json(serde_json::json!({
        "templates": overview.templates,
        "previews": overview.previews,
    }))
}
