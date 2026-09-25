//! HTTP handlers, and the route table shared by `main` and the route tests.

pub mod api;
pub mod apps;
pub mod dashboard;
pub mod embed;
pub mod health;
pub mod login;
pub mod status;
pub mod uptime;

use actix_files::Files;
use actix_web::web;

/// Every route and static directory. Middleware and app data are added by the caller.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(Files::new("/libs", "./libs"))
        .service(Files::new("/static", "./static"))
        .route("/healthz", web::get().to(health::healthz))
        .route("/status", web::get().to(status::show))
        .route("/login", web::get().to(login::show_login))
        .route("/login", web::post().to(login::submit_login))
        .route("/logout", web::post().to(login::logout))
        .route("/", web::get().to(dashboard::show))
        .route("/logs", web::get().to(dashboard::show_logs))
        .route("/logs/{slug}", web::get().to(dashboard::show_app))
        .route("/apps", web::get().to(apps::show))
        .route("/apps", web::post().to(apps::add))
        .route("/apps/{slug}/edit", web::post().to(apps::update))
        .route("/apps/{slug}/pause", web::post().to(apps::pause))
        .route("/apps/{slug}/public", web::post().to(apps::public))
        .route("/apps/{slug}/delete", web::post().to(apps::delete))
        .route(
            "/apps/{slug}/rotate-token",
            web::post().to(apps::rotate_token),
        )
        .route("/embed/{slug}", web::get().to(embed::status))
        .route("/api/apps/{slug}/logs", web::get().to(api::get_logs))
        .route("/api/uptime", web::get().to(uptime::overview))
        .route("/api/uptime/{slug}", web::get().to(uptime::detail));
}
