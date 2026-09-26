//! /apps: register, edit, pause, publish, rotate the embed token of, and delete apps.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use tera::{Context, Tera};

use crate::{
    alerts::{Alerter, TestKind},
    auth,
    config::Config,
    i18n::{I18n, Localize},
    outbound::Outbound,
    registry::{AppRegistry, AppSettings, CheckHeader},
    uptime::{UptimeMonitor, unix_now},
};

/// Longest scheduled maintenance, in hours (30 days).
const MAX_PAUSE_HOURS: u64 = 720;
/// An indefinite pause older than this is flagged as probably forgotten.
pub const STALE_PAUSE_SECS: u64 = 24 * 3600;

/// The register/edit form, as submitted. Numbers arrive as text so a blank field can mean
/// "use the global default" instead of failing to deserialize.
#[derive(Deserialize, Serialize, Clone)]
pub struct AppForm {
    name: String,
    logs_url: String,
    health_url: String,
    #[serde(default)]
    degraded_after_ms: String,
    #[serde(default)]
    expect_body: String,
    /// Checkbox: present ("on") when ticked, absent otherwise.
    #[serde(default)]
    check_assets: Option<String>,
    #[serde(default)]
    interval_secs: String,
    #[serde(default)]
    timeout_secs: String,
    /// `200, 401`: comma- or space-separated.
    #[serde(default)]
    expect_status: String,
    /// One `Name: value` per line.
    #[serde(default)]
    headers: String,
    /// One URL per line.
    #[serde(default)]
    alert_webhooks: String,
    /// Comma- or space-separated, `ALERT_MENTIONS` syntax.
    #[serde(default)]
    alert_mentions: String,
}

/// Splits a free-text list on commas and whitespace.
fn entries(raw: &str) -> Vec<String> {
    raw.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect()
}

/// Blank = `None` (use the global default); otherwise a whole number.
fn optional_number(raw: &str, label_key: &str, i18n: &I18n) -> Result<Option<u32>, String> {
    match raw.trim() {
        "" => Ok(None),
        value => value
            .parse()
            .map(Some)
            .map_err(|_| i18n.text("err.number", &[("label", &i18n.text(label_key, &[]))])),
    }
}

impl AppForm {
    fn settings(&self, i18n: &I18n) -> Result<AppSettings, String> {
        let degraded_after_ms = match self.degraded_after_ms.trim() {
            "" => None,
            raw => Some(
                raw.parse::<u32>()
                    .map_err(|_| i18n.text("err.threshold", &[]))?,
            ),
        };
        let expect_status = entries(&self.expect_status)
            .iter()
            .map(|code| {
                code.parse::<u16>()
                    .map_err(|_| i18n.text("err.status_list", &[("code", code)]))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let headers = self
            .headers
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| CheckHeader::parse_line(line).map_err(|e| e.localize(i18n)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AppSettings {
            name: self.name.clone(),
            logs_url: self.logs_url.clone(),
            health_url: self.health_url.clone(),
            degraded_after_ms,
            expect_body: Some(self.expect_body.clone()),
            check_assets: self.check_assets.is_some(),
            interval_secs: optional_number(&self.interval_secs, "apps.interval", i18n)?,
            timeout_secs: optional_number(&self.timeout_secs, "apps.timeout", i18n)?,
            expect_status,
            headers,
            alert_webhooks: entries(&self.alert_webhooks),
            alert_mentions: entries(&self.alert_mentions),
        })
    }

    /// Both URLs must pass the outbound policy before they're ever stored (a blank logs
    /// URL is fine: monitor-only app; the health URL may be `tcp://host:port`).
    fn check_urls(&self, outbound: &Outbound, i18n: &I18n) -> Result<(), String> {
        let field_error = |label_key: &str, e: &crate::outbound::OutboundError| {
            let label = i18n.text(label_key, &[]);
            i18n.text(
                "err.url_field",
                &[("label", &label), ("error", &e.localize(i18n))],
            )
        };
        if !self.logs_url.trim().is_empty() {
            outbound
                .check(&self.logs_url)
                .map_err(|e| field_error("err.label_logs", &e))?;
        }
        outbound
            .check_health(&self.health_url)
            .map_err(|e| field_error("err.label_health", &e))?;
        Ok(())
    }
}

/// A failed submission, re-rendered with the user's input: `slug` is the app being edited,
/// `None` for the registration form.
struct FormEcho<'a> {
    slug: Option<&'a str>,
    error: &'a str,
    values: &'a AppForm,
}

async fn render_apps(
    tera: &Tera,
    cfg: &Config,
    registry: &AppRegistry,
    error: Option<&str>,
    echo: Option<FormEcho<'_>>,
) -> HttpResponse {
    let apps = registry.list().await;
    let mut ctx = Context::new();
    ctx.insert("active", "apps");
    ctx.insert("is_admin", &true);
    ctx.insert("apps", &apps);
    ctx.insert("error", &error);
    // Split for the template: the error goes to whichever form failed (registration when
    // `slug` is `None`, else that app's edit form), and that form keeps the typed values.
    let (register_values, edit_slug, edit_values) = match &echo {
        Some(FormEcho {
            slug: None, values, ..
        }) => (Some(*values), "", None),
        Some(FormEcho {
            slug: Some(slug),
            values,
            ..
        }) => (None, *slug, Some(*values)),
        None => (None, "", None),
    };
    ctx.insert("form_error", &echo.as_ref().map(|e| e.error));
    ctx.insert("register_values", &register_values);
    ctx.insert("edit_slug", edit_slug);
    ctx.insert("edit_values", &edit_values);
    let allowed_hosts: Vec<&str> = cfg
        .allowed_hosts
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .collect();
    ctx.insert("allowed_hosts", &allowed_hosts.join(", "));
    ctx.insert("uptime_interval_secs", &cfg.uptime_interval_secs);
    ctx.insert("uptime_timeout_secs", &cfg.uptime_timeout_secs);
    // Indefinite pauses older than this get flagged: someone probably forgot to resume.
    ctx.insert(
        "stale_pause_before",
        &unix_now().saturating_sub(STALE_PAUSE_SECS),
    );
    ctx.insert("uptime_degraded_ms", &cfg.uptime_degraded_ms);
    match tera.render("apps.html", &ctx) {
        Ok(html) => HttpResponse::Ok().content_type("text/html").body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}

fn back_to_apps() -> HttpResponse {
    HttpResponse::Found()
        .append_header(("Location", "/apps"))
        .finish()
}

pub async fn show(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    render_apps(&tera, &cfg, &registry, None, None).await
}

/// POST /apps -- register.
pub async fn add(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    outbound: web::Data<Outbound>,
    i18n: web::Data<I18n>,
    form: web::Form<AppForm>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    let result = match form
        .check_urls(&outbound, &i18n)
        .and_then(|()| form.settings(&i18n))
    {
        Ok(settings) => registry
            .add(settings)
            .await
            .map(|_| ())
            .map_err(|e| e.localize(&i18n)),
        Err(msg) => Err(msg),
    };
    match result {
        Ok(()) => back_to_apps(),
        Err(msg) => {
            let echo = FormEcho {
                slug: None,
                error: &msg,
                values: &form,
            };
            render_apps(&tera, &cfg, &registry, None, Some(echo)).await
        }
    }
}

/// POST /apps/{slug}/edit -- replace settings; slug, token and history are kept.
#[allow(clippy::too_many_arguments)] // each is an actix extractor; bundling them adds nothing
pub async fn update(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    outbound: web::Data<Outbound>,
    i18n: web::Data<I18n>,
    path: web::Path<String>,
    form: web::Form<AppForm>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    let slug = path.into_inner();
    let result = match form
        .check_urls(&outbound, &i18n)
        .and_then(|()| form.settings(&i18n))
    {
        Ok(settings) => registry
            .update(&slug, settings)
            .await
            .map_err(|e| e.localize(&i18n)),
        Err(msg) => Err(msg),
    };
    match result {
        Ok(()) => back_to_apps(),
        Err(msg) => {
            let echo = FormEcho {
                slug: Some(&slug),
                error: &msg,
                values: &form,
            };
            render_apps(&tera, &cfg, &registry, None, Some(echo)).await
        }
    }
}

#[derive(Deserialize)]
pub struct ToggleForm {
    value: bool,
}

#[derive(Deserialize)]
pub struct PauseForm {
    value: bool,
    /// Scheduled maintenance length; blank = until resumed by hand.
    #[serde(default)]
    hours: String,
}

/// POST /apps/{slug}/pause -- `value=true` pauses checks (for `hours`, or until resumed),
/// `false` resumes them.
pub async fn pause(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    i18n: web::Data<I18n>,
    path: web::Path<String>,
    form: web::Form<PauseForm>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    let until = match form.hours.trim() {
        "" => None,
        raw => match raw.parse::<u64>() {
            Ok(hours) if (1..=MAX_PAUSE_HOURS).contains(&hours) => Some(unix_now() + hours * 3600),
            _ => {
                let msg = i18n.text("err.pause_hours", &[("max", &MAX_PAUSE_HOURS.to_string())]);
                return render_apps(&tera, &cfg, &registry, Some(&msg), None).await;
            }
        },
    };
    match registry
        .set_paused(&path.into_inner(), form.value, until)
        .await
    {
        Ok(()) => back_to_apps(),
        Err(e) => render_apps(&tera, &cfg, &registry, Some(&e.localize(&i18n)), None).await,
    }
}

/// POST /apps/{slug}/public -- show (`value=true`) or hide the app on /status.
pub async fn public(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    i18n: web::Data<I18n>,
    path: web::Path<String>,
    form: web::Form<ToggleForm>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    match registry.set_public(&path.into_inner(), form.value).await {
        Ok(()) => back_to_apps(),
        Err(e) => render_apps(&tera, &cfg, &registry, Some(&e.localize(&i18n)), None).await,
    }
}

pub async fn delete(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
    i18n: web::Data<I18n>,
    path: web::Path<String>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    let slug = path.into_inner();
    match registry.remove(&slug).await {
        Ok(()) => {
            if let Err(e) = monitor.forget(&slug).await {
                tracing::error!(app = %slug, error = %e, "uptime: failed to drop history");
            }
            back_to_apps()
        }
        Err(e) => render_apps(&tera, &cfg, &registry, Some(&e.localize(&i18n)), None).await,
    }
}

/// POST /apps/{slug}/rotate-token -- invalidates every embed using the app's current token.
pub async fn rotate_token(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    i18n: web::Data<I18n>,
    path: web::Path<String>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    match registry.rotate_embed_token(&path.into_inner()).await {
        Ok(()) => back_to_apps(),
        Err(e) => render_apps(&tera, &cfg, &registry, Some(&e.localize(&i18n)), None).await,
    }
}

#[derive(Deserialize)]
pub struct AppTestRequest {
    kind: TestKind,
}

/// POST /apps/{slug}/alerts/test -- pings (or sends a sample alert to) the app's own
/// webhooks and reports each answer.
pub async fn test_alerts(
    req: HttpRequest,
    registry: web::Data<AppRegistry>,
    alerter: web::Data<Alerter>,
    path: web::Path<String>,
    body: web::Json<AppTestRequest>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin_json(&req) {
        return resp;
    }
    let Some(app) = registry.find(&path.into_inner()).await else {
        return HttpResponse::NotFound().json(serde_json::json!({ "error": "not found" }));
    };
    let outcomes = alerter.test_app(&app.alert_route(), body.kind).await;
    HttpResponse::Ok().json(serde_json::json!({ "outcomes": outcomes }))
}
