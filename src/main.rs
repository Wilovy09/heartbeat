mod alerts;
#[cfg(test)]
mod app_tests;
mod auth;
mod config;
mod i18n;
mod outbound;
mod password;
mod registry;
mod routes;
mod security;
mod token;
mod uptime;

use actix_web::middleware::from_fn;
use actix_web::{App, HttpServer, web};
use std::time::Duration;
use tera::Tera;
use tracing_subscriber::EnvFilter;

use alerts::{AlertSettings, Alerter};
use auth::SessionStore;
use config::Config;
use outbound::Outbound;
use registry::AppRegistry;
use security::LoginLimiter;
use uptime::{CheckPolicy, Notifiers, UptimeMonitor};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    // `heartbeat hash-password`: reads a password from stdin and prints the argon2 hash for
    // ADMIN_PASSWORD_HASH, then exits -- no config needed.
    if std::env::args().nth(1).as_deref() == Some("hash-password") {
        return hash_password_command();
    }

    let cfg = Config::from_env().unwrap_or_else(|e| panic!("{e}"));

    let i18n = i18n::I18n::new(cfg.app_lang);
    let mut tera = Tera::new();
    i18n.register(&mut tera);
    tera.load_from_glob("templates/**/*.html")
        .unwrap_or_else(|e| panic!("error cargando templates: {e}"));

    let registry = AppRegistry::load(&cfg.apps_file)
        .await
        .unwrap_or_else(|e| panic!("error cargando {}: {e}", cfg.apps_file));

    let outbound = Outbound::new(&cfg.allowed_hosts)
        .unwrap_or_else(|e| panic!("error creando el cliente HTTP: {e}"));

    let alerter = Alerter::new(AlertSettings {
        webhook_urls: cfg.alert_webhook_urls.clone(),
        on_degraded: cfg.alert_on_degraded,
        public_url: cfg.public_url.clone(),
        mentions: cfg.alert_mentions.clone(),
    })
    .unwrap_or_else(|e| panic!("error creando el cliente de alertas: {e}"));

    let monitor = UptimeMonitor::load(
        &cfg.uptime_dir,
        CheckPolicy {
            interval: Duration::from_secs(cfg.uptime_interval_secs),
            degraded_after_ms: cfg.uptime_degraded_ms,
            retries: cfg.uptime_retries,
            cert_warn_days: cfg.uptime_cert_warn_days,
            retention: Duration::from_hours(24 * u64::from(cfg.uptime_retention_days)),
        },
        outbound.clone(),
        Notifiers {
            alerter: alerter.clone(),
            ping_url: cfg.heartbeat_ping_url.clone(),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("error cargando {}: {e}", cfg.uptime_dir));

    let host = cfg.host.clone();
    let port = cfg.port;

    let sessions = SessionStore::load(&cfg.sessions_file)
        .await
        .unwrap_or_else(|e| panic!("error cargando {}: {e}", cfg.sessions_file));

    let auth_mode = match &cfg.auth {
        config::AuthMode::Upstream { .. } => "upstream",
        config::AuthMode::Password { .. } => "password",
    };
    tracing::info!(
        host = %host,
        port,
        auth_mode,
        alerts = !cfg.alert_webhook_urls.is_empty(),
        "heartbeat starting"
    );

    let cfg_data = web::Data::new(cfg);
    let tera_data = web::Data::new(tera);
    let registry_data = web::Data::new(registry);
    let sessions_data = web::Data::new(sessions);
    let login_limiter = web::Data::new(LoginLimiter::default());
    let i18n_data = web::Data::new(i18n);
    let alerter_data = web::Data::new(alerter);
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
            .wrap(from_fn(security::csrf))
            .wrap(security::headers())
            .app_data(login_limiter.clone())
            .app_data(i18n_data.clone())
            .app_data(alerter_data.clone())
            .app_data(cfg_data.clone())
            .app_data(tera_data.clone())
            .app_data(registry_data.clone())
            .app_data(sessions_data.clone())
            .app_data(outbound_data.clone())
            .app_data(monitor_data.clone())
            .configure(routes::configure)
    })
    .bind((host, port))?
    .run()
    .await
}

fn hash_password_command() -> std::io::Result<()> {
    use std::io::{BufRead, Write};
    eprint!("Password: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let password = line.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        eprintln!("El password no puede estar vacío.");
        std::process::exit(1);
    }
    match password::hash(password) {
        Ok(hash) => {
            println!("{hash}");
            Ok(())
        }
        Err(e) => {
            eprintln!("No se pudo generar el hash: {e}");
            std::process::exit(1);
        }
    }
}
