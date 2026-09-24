use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use tera::{Context, Tera};

use crate::{
    auth, config::Config, outbound::Outbound, registry::AppRegistry, uptime::UptimeMonitor,
};

/// `form`: what the user submitted, echoed back into the inputs when registration fails.
async fn render_apps(
    tera: &Tera,
    cfg: &Config,
    registry: &AppRegistry,
    error: Option<&str>,
    form: Option<&AddAppForm>,
) -> HttpResponse {
    let apps = registry.list().await;
    let mut ctx = Context::new();
    ctx.insert("active", "apps");
    ctx.insert("apps", &apps);
    ctx.insert("error", &error);
    ctx.insert("form", &form);
    let allowed_hosts: Vec<&str> = cfg
        .allowed_hosts
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .collect();
    ctx.insert("allowed_hosts", &allowed_hosts.join(", "));
    ctx.insert("uptime_interval_secs", &cfg.uptime_interval_secs);
    ctx.insert("uptime_degraded_ms", &cfg.uptime_degraded_ms);
    match tera.render("apps.html", &ctx) {
        Ok(html) => HttpResponse::Ok().content_type("text/html").body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}

pub async fn show(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
) -> HttpResponse {
    if let Err(resp) = auth::require_session(&req) {
        return resp;
    }
    render_apps(&tera, &cfg, &registry, None, None).await
}

#[derive(Deserialize, Serialize)]
pub struct AddAppForm {
    name: String,
    logs_url: String,
    health_url: String,
}

pub async fn add(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    outbound: web::Data<Outbound>,
    form: web::Form<AddAppForm>,
) -> HttpResponse {
    if let Err(resp) = auth::require_session(&req) {
        return resp;
    }
    for (label, url) in [("logs", &form.logs_url), ("health", &form.health_url)] {
        if let Err(e) = outbound.check(url) {
            let msg = format!("URL de {label}: {e}");
            return render_apps(&tera, &cfg, &registry, Some(&msg), Some(&form)).await;
        }
    }
    match registry
        .add(&form.name, &form.logs_url, &form.health_url)
        .await
    {
        Ok(_) => HttpResponse::Found()
            .append_header(("Location", "/apps"))
            .finish(),
        Err(e) => render_apps(&tera, &cfg, &registry, Some(&e.to_string()), Some(&form)).await,
    }
}

pub async fn delete(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
    path: web::Path<String>,
) -> HttpResponse {
    if let Err(resp) = auth::require_session(&req) {
        return resp;
    }
    let slug = path.into_inner();
    match registry.remove(&slug).await {
        Ok(()) => {
            if let Err(e) = monitor.forget(&slug).await {
                tracing::error!(app = %slug, error = %e, "uptime: failed to drop history");
            }
            HttpResponse::Found()
                .append_header(("Location", "/apps"))
                .finish()
        }
        Err(e) => render_apps(&tera, &cfg, &registry, Some(&e.to_string()), None).await,
    }
}

/// POST /apps/{slug}/rotate-token -- invalidates every embed using the app's current token.
pub async fn rotate_token(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    path: web::Path<String>,
) -> HttpResponse {
    if let Err(resp) = auth::require_session(&req) {
        return resp;
    }
    match registry.rotate_embed_token(&path.into_inner()).await {
        Ok(()) => HttpResponse::Found()
            .append_header(("Location", "/apps"))
            .finish(),
        Err(e) => render_apps(&tera, &cfg, &registry, Some(&e.to_string()), None).await,
    }
}
