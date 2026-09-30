//! /settings: alerting (test the webhooks, preview and send each alert kind) and the
//! current configuration, read-only -- it comes from the environment, so it changes by
//! editing .env and restarting.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use tera::{Context, Tera};

use crate::{
    alert_templates::{AlertTemplates, MAX_TEMPLATE_CHARS},
    alerts::{Alerter, TestKind},
    audit::{self, Event},
    auth,
    config::{AuthMode, Config},
    i18n::{I18n, Localize},
    theme::{ThemeError, ThemeStore},
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

/// Slash commands as the page shows them: what's on, and what to paste into Slack and
/// Discord to turn the rest on.
#[derive(Serialize)]
struct ChatView {
    command: String,
    admins: Vec<String>,
    /// `PUBLIC_URL`, or the address this page was opened at.
    base_url: String,
    public_url_set: bool,
    slack: bool,
    slack_manifest: String,
    discord: bool,
    discord_invite: Option<String>,
}

impl ChatView {
    fn new(cfg: &Config, req: &HttpRequest, i18n: &I18n) -> Self {
        let base_url = cfg.public_url.as_deref().map_or_else(
            || {
                let info = req.connection_info();
                format!("{}://{}", info.scheme(), info.host())
            },
            |url| url.trim_end_matches('/').to_string(),
        );
        let manifest = crate::chat::slack::manifest(&base_url, &cfg.chat_command, i18n);
        Self {
            command: cfg.chat_command.clone(),
            admins: cfg.chat_admins.clone(),
            public_url_set: cfg.public_url.is_some(),
            slack: cfg.slack_signing_secret.is_some(),
            slack_manifest: serde_json::to_string_pretty(&manifest).unwrap_or_default(),
            discord: cfg.discord.is_some(),
            discord_invite: cfg.discord.as_ref().map(crate::chat::discord::invite_url),
            base_url,
        }
    }
}

/// GET /settings
pub async fn show(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    alerter: web::Data<Alerter>,
    themes: web::Data<ThemeStore>,
    i18n: web::Data<I18n>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    let mut ctx = Context::new();
    ctx.insert("chat", &ChatView::new(&cfg, &req, &i18n));
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
    ctx.insert(
        "theme_json",
        &serde_json::to_string(&themes.css()).unwrap_or_else(|_| "\"\"".to_string()),
    );
    ctx.insert("theme_max_kb", &(crate::theme::MAX_THEME_BYTES / 1024));
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
    let event = Event::web(&req, "settings.test_alerts")
        .detail(serde_json::json!({ "kind": body.kind, "webhook": body.target }));
    audit::record(&req, event).await;
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
    let detail = serde_json::to_value(&templates).unwrap_or_default();
    let result = alerter.templates().save(templates).await;
    let event = Event::web(&req, "templates.save")
        .result(&result)
        .detail(detail);
    audit::record(&req, event).await;
    if let Err(e) = result {
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

#[derive(Deserialize)]
pub struct ThemeRequest {
    css: String,
}

/// POST /settings/theme -- saves the custom theme CSS (blank = none).
pub async fn save_theme(
    req: HttpRequest,
    themes: web::Data<ThemeStore>,
    i18n: web::Data<I18n>,
    body: web::Json<ThemeRequest>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin_json(&req) {
        return resp;
    }
    let result = themes.save(&body.css).await;
    let event = Event::web(&req, "theme.save")
        .result(&result)
        .detail(serde_json::json!({ "css": body.css }));
    audit::record(&req, event).await;
    match result {
        Ok(()) => {
            tracing::info!("settings: custom theme updated");
            HttpResponse::Ok().json(serde_json::json!({ "ok": true }))
        }
        Err(e @ ThemeError::Io { .. }) => {
            tracing::error!(error = %e, "settings: could not save the custom theme");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({ "error": e.localize(&i18n) }))
        }
        Err(e) => {
            HttpResponse::BadRequest().json(serde_json::json!({ "error": e.localize(&i18n) }))
        }
    }
}

/// GET /theme/custom.css -- the custom theme, public: the login and status pages use it
/// too. Revalidated on every load (cheap 304s), so a saved change shows up right away.
pub async fn custom_css(req: HttpRequest, themes: web::Data<ThemeStore>) -> HttpResponse {
    let css = themes.css();
    let etag = format!(
        "\"{:x}\"",
        css.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    );
    let fresh = req
        .headers()
        .get(actix_web::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag);
    if fresh {
        return HttpResponse::NotModified()
            .insert_header(("ETag", etag))
            .finish();
    }
    HttpResponse::Ok()
        .content_type("text/css; charset=utf-8")
        .insert_header(("Cache-Control", "no-cache"))
        .insert_header(("ETag", etag))
        .body(css)
}
