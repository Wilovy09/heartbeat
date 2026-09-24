use actix_web::{HttpRequest, HttpResponse, web};
use tera::{Context, Tera};

use crate::{auth, registry::AppRegistry};

/// GET / -- the uptime view (see static/uptime.js).
pub async fn show(
    req: HttpRequest,
    tera: web::Data<Tera>,
    registry: web::Data<AppRegistry>,
) -> HttpResponse {
    if let Err(resp) = auth::require_session(&req) {
        return resp;
    }

    let apps = registry.list().await;
    let mut ctx = Context::new();
    ctx.insert("active", "dashboard");
    ctx.insert("apps", &apps);

    match tera.render("dashboard.html", &ctx) {
        Ok(html) => HttpResponse::Ok().content_type("text/html").body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}

/// GET /logs -- one card per registered app with its recent heartbeats. Click one to open
/// its log viewer.
pub async fn show_logs(
    req: HttpRequest,
    tera: web::Data<Tera>,
    registry: web::Data<AppRegistry>,
) -> HttpResponse {
    if let Err(resp) = auth::require_session(&req) {
        return resp;
    }

    let apps = registry.list().await;
    let mut ctx = Context::new();
    ctx.insert("active", "logs");
    ctx.insert("apps", &apps);

    match tera.render("logs.html", &ctx) {
        Ok(html) => HttpResponse::Ok().content_type("text/html").body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}

/// GET /logs/{slug} -- the log viewer for one registered app.
pub async fn show_app(
    req: HttpRequest,
    tera: web::Data<Tera>,
    registry: web::Data<AppRegistry>,
    path: web::Path<String>,
) -> HttpResponse {
    if let Err(resp) = auth::require_session(&req) {
        return resp;
    }

    let slug = path.into_inner();
    let Some(app) = registry.find(&slug).await else {
        return HttpResponse::NotFound().body(format!("App '{slug}' no está registrada"));
    };

    let mut ctx = Context::new();
    ctx.insert("active", "logs");
    ctx.insert("app", &app);

    match tera.render("log_viewer.html", &ctx) {
        Ok(html) => HttpResponse::Ok().content_type("text/html").body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}
