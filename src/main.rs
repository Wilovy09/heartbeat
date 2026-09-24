mod auth;
mod config;
mod outbound;
mod registry;
mod routes;
mod token;
mod uptime;

use actix_files::Files;
use actix_web::{App, HttpServer, web};
use std::time::Duration;
use tera::Tera;
use tracing_subscriber::EnvFilter;

use auth::SessionStore;
use config::Config;
use outbound::Outbound;
use registry::AppRegistry;
use uptime::{CheckPolicy, UptimeMonitor};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let cfg = Config::from_env().unwrap_or_else(|e| panic!("{e}"));

    let mut tera = Tera::new();
    tera.load_from_glob("templates/**/*.html")
        .unwrap_or_else(|e| panic!("error cargando templates: {e}"));

    let registry = AppRegistry::load(&cfg.apps_file)
        .await
        .unwrap_or_else(|e| panic!("error cargando {}: {e}", cfg.apps_file));

    let outbound = Outbound::new(&cfg.allowed_hosts)
        .unwrap_or_else(|e| panic!("error creando el cliente HTTP: {e}"));

    let monitor = UptimeMonitor::load(
        &cfg.uptime_dir,
        CheckPolicy {
            interval: Duration::from_secs(cfg.uptime_interval_secs),
            degraded_after_ms: cfg.uptime_degraded_ms,
        },
        outbound.clone(),
    )
    .await
    .unwrap_or_else(|e| panic!("error cargando {}: {e}", cfg.uptime_dir));

    let host = cfg.host.clone();
    let port = cfg.port;

    tracing::info!(host = %host, port, login_url = %cfg.login_url, "heartbeat starting");

    let cfg_data = web::Data::new(cfg);
    let tera_data = web::Data::new(tera);
    let registry_data = web::Data::new(registry);
    let sessions_data = web::Data::new(SessionStore::default());
    let outbound_data = web::Data::new(outbound);
    let monitor_data = web::Data::new(monitor);

    tokio::spawn(
        monitor_data
            .clone()
            .into_inner()
            .run(registry_data.clone().into_inner()),
    );

    HttpServer::new(move || {
        App::new()
            .app_data(cfg_data.clone())
            .app_data(tera_data.clone())
            .app_data(registry_data.clone())
            .app_data(sessions_data.clone())
            .app_data(outbound_data.clone())
            .app_data(monitor_data.clone())
            .service(Files::new("/libs", "./libs"))
            .service(Files::new("/static", "./static"))
            .route("/login", web::get().to(routes::login::show_login))
            .route("/login", web::post().to(routes::login::submit_login))
            .route("/logout", web::get().to(routes::login::logout))
            .route("/", web::get().to(routes::dashboard::show))
            .route("/logs", web::get().to(routes::dashboard::show_logs))
            .route("/logs/{slug}", web::get().to(routes::dashboard::show_app))
            .route("/apps", web::get().to(routes::apps::show))
            .route("/apps", web::post().to(routes::apps::add))
            .route("/apps/{slug}/delete", web::post().to(routes::apps::delete))
            .route(
                "/apps/{slug}/rotate-token",
                web::post().to(routes::apps::rotate_token),
            )
            .route("/embed/{slug}", web::get().to(routes::embed::status))
            .route(
                "/api/apps/{slug}/logs",
                web::get().to(routes::api::get_logs),
            )
            .route("/api/uptime", web::get().to(routes::uptime::overview))
            .route("/api/uptime/{slug}", web::get().to(routes::uptime::detail))
    })
    .bind((host, port))?
    .run()
    .await
}
