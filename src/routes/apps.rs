use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;
use tera::{Context, Tera};

use crate::{auth, registry::AppRegistry, uptime::UptimeMonitor};

async fn render_apps(tera: &Tera, registry: &AppRegistry, error: Option<&str>) -> HttpResponse {
    let apps = registry.list().await;
    let mut ctx = Context::new();
    ctx.insert("active", "apps");
    ctx.insert("apps", &apps);
    ctx.insert("error", &error);
    match tera.render("apps.html", &ctx) {
        Ok(html) => HttpResponse::Ok().content_type("text/html").body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}

pub async fn show(
    req: HttpRequest,
    tera: web::Data<Tera>,
    registry: web::Data<AppRegistry>,
) -> HttpResponse {
    if let Err(resp) = auth::require_session(&req) {
        return resp;
    }
    render_apps(&tera, &registry, None).await
}

#[derive(Deserialize)]
pub struct AddAppForm {
    name: String,
    logs_url: String,
    health_url: String,
}

pub async fn add(
    req: HttpRequest,
    tera: web::Data<Tera>,
    registry: web::Data<AppRegistry>,
    form: web::Form<AddAppForm>,
) -> HttpResponse {
    if let Err(resp) = auth::require_session(&req) {
        return resp;
    }
    match registry
        .add(&form.name, &form.logs_url, &form.health_url)
        .await
    {
        Ok(_) => HttpResponse::Found()
            .append_header(("Location", "/apps"))
            .finish(),
        Err(e) => render_apps(&tera, &registry, Some(&e.to_string())).await,
    }
}

pub async fn delete(
    req: HttpRequest,
    tera: web::Data<Tera>,
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
        Err(e) => render_apps(&tera, &registry, Some(&e.to_string())).await,
    }
}

/// POST /apps/{slug}/rotate-token -- invalidates every embed using the app's current token.
pub async fn rotate_token(
    req: HttpRequest,
    tera: web::Data<Tera>,
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
        Err(e) => render_apps(&tera, &registry, Some(&e.to_string())).await,
    }
}
