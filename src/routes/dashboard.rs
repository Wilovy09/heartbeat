use actix_web::{HttpRequest, HttpResponse, web};
use tera::Tera;

use crate::{auth, i18n::I18n, registry::AppRegistry};

/// GET / -- the uptime view (see static/uptime.js).
pub async fn show(
    req: HttpRequest,
    tera: web::Data<Tera>,
    registry: web::Data<AppRegistry>,
) -> HttpResponse {
    let session = match auth::require_session(&req) {
        Ok(session) => session,
        Err(resp) => return resp,
    };

    let apps = registry.list().await;
    let mut ctx = auth::page_context(&session, "dashboard");
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
    let session = match auth::require_admin(&req) {
        Ok(session) => session,
        Err(resp) => return resp,
    };

    let apps = registry.list().await;
    let mut ctx = auth::page_context(&session, "logs");
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
    i18n: web::Data<I18n>,
    path: web::Path<String>,
) -> HttpResponse {
    let session = match auth::require_admin(&req) {
        Ok(session) => session,
        Err(resp) => return resp,
    };

    let slug = path.into_inner();
    let Some(app) = registry.find(&slug).await else {
        return HttpResponse::NotFound().body(i18n.text("api.not_registered", &[("slug", &slug)]));
    };
    // Monitor-only app: nothing to show here, its dashboard page is the useful view.
    if app.logs_url.is_none() {
        return HttpResponse::Found()
            .append_header(("Location", format!("/#{slug}")))
            .finish();
    }

    let mut ctx = auth::page_context(&session, "logs");
    ctx.insert("app", &app);

    match tera.render("log_viewer.html", &ctx) {
        Ok(html) => HttpResponse::Ok().content_type("text/html").body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}
