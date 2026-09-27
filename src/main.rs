mod alert_templates;
mod alerts;
#[cfg(test)]
mod app_tests;
mod assets;
mod auth;
mod bundles;
mod config;
mod db;
#[cfg(feature = "demo")]
mod demo;
mod i18n;
mod migrate;
mod notices;
mod outbound;
mod password;
mod registry;
mod routes;
mod security;
mod theme;
mod token;
mod uptime;

use actix_web::middleware::from_fn;
use actix_web::{App, HttpServer, web};
use std::process::ExitCode;
use std::time::Duration;
use tera::Tera;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

use alerts::{AlertSettings, Alerter};
use auth::SessionStore;
use config::Config;
use outbound::Outbound;
use registry::AppRegistry;
use security::LoginLimiter;
use uptime::{CheckPolicy, Notifiers, UptimeMonitor};

/// Everything that can stop Heartbeat from starting. Reported once, cleanly, instead of
/// a panic backtrace.
#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error("could not load templates: {0}")]
    Templates(#[from] tera::Error),
    #[error(transparent)]
    Db(#[from] db::DbError),
    #[error(transparent)]
    Migrate(#[from] migrate::MigrateError),
    #[error(transparent)]
    Registry(#[from] registry::RegistryError),
    #[error("could not build the HTTP client: {0}")]
    Outbound(#[from] outbound::OutboundError),
    #[error(transparent)]
    AlertTemplates(#[from] alert_templates::TemplateError),
    #[error("could not build the alerts client: {0}")]
    Alerts(#[from] reqwest::Error),
    #[error(transparent)]
    Uptime(#[from] uptime::UptimeError),
    #[error(transparent)]
    Sessions(#[from] auth::SessionError),
    #[error(transparent)]
    Notices(#[from] notices::NoticeError),
    #[error(transparent)]
    Theme(#[from] theme::ThemeError),
    #[error("HTTP server error: {0}")]
    Server(#[from] std::io::Error),
}

#[actix_web::main]
async fn main() -> ExitCode {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(LevelFilter::INFO.into()))
        .init();

    // `heartbeat hash-password`: reads a password from stdin and prints the argon2 hash for
    // ADMIN_PASSWORD_HASH, then exits -- no config needed.
    if std::env::args().nth(1).as_deref() == Some("hash-password") {
        return hash_password_command();
    }
    // `heartbeat migrate [--dry-run] [--keep]`: imports a 0.2 install into the database.
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        return migrate_command().await;
    }
    // `heartbeat backup <file>`: a consistent copy of the database, taken while it runs.
    if std::env::args().nth(1).as_deref() == Some("backup") {
        return backup_command().await;
    }

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "heartbeat stopped");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), StartupError> {
    let cfg = Config::from_env()?;

    let i18n = i18n::I18n::new(cfg.app_lang);
    let mut tera = Tera::new();
    i18n.register(&mut tera);
    assets::load_templates(&mut tera)?;

    let db = db::Db::open(&cfg.database_path).await?;
    migrate::ensure_nothing_pending(&cfg, &db).await?;
    let registry = AppRegistry::load(db.clone()).await?;
    let outbound = Outbound::new(&cfg.allowed_hosts)?;

    let templates =
        std::sync::Arc::new(alert_templates::TemplateStore::load(db.clone(), &i18n).await?);
    let alerter = Alerter::new(AlertSettings {
        webhook_urls: cfg.alert_webhook_urls.clone(),
        on_degraded: cfg.alert_on_degraded,
        public_url: cfg.public_url.clone(),
        mentions: cfg.alert_mentions.clone(),
        templates: Some(templates),
        remind_after: Some(Duration::from_secs(u64::from(cfg.alert_remind_mins) * 60)),
        lang: cfg.app_lang,
    })?;

    let monitor = UptimeMonitor::load(
        db.clone(),
        CheckPolicy {
            interval: Duration::from_secs(cfg.uptime_interval_secs),
            degraded_after_ms: cfg.uptime_degraded_ms,
            retries: cfg.uptime_retries,
            cert_warn_days: cfg.uptime_cert_warn_days,
            retention: Duration::from_hours(24 * u64::from(cfg.uptime_retention_days)),
            daily_retention: Duration::from_hours(24 * u64::from(cfg.uptime_daily_retention_days)),
            timeout: Duration::from_secs(u64::from(cfg.uptime_timeout_secs)),
            mass_down_pct: cfg.uptime_mass_down_pct,
            lang: cfg.app_lang,
        },
        outbound.clone(),
        Notifiers {
            alerter: Some(alerter.clone()),
            ping_url: cfg.heartbeat_ping_url.clone(),
        },
    )
    .await?;

    let host = cfg.host.clone();
    let port = cfg.port;

    let sessions = SessionStore::load(db.clone()).await?;
    let notices = notices::NoticeStore::load(db.clone()).await?;
    let themes = theme::ThemeStore::load(db.clone()).await?;

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
    let notices_data = web::Data::new(notices);
    let themes_data = web::Data::new(themes);

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
            .app_data(notices_data.clone())
            .app_data(themes_data.clone())
            .configure(routes::configure)
    })
    .bind((host, port))?
    .run()
    .await?;
    Ok(())
}

async fn migrate_command() -> ExitCode {
    let options = match migrate::Options::from_args(std::env::args().skip(2)) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    match migrate::run(&cfg, options).await {
        Ok(report) if options.dry_run => {
            println!("Dry run: would import {report} into {}", cfg.database_path);
            ExitCode::SUCCESS
        }
        Ok(report) => {
            println!("Imported {report} into {}", cfg.database_path);
            for path in &report.renamed {
                println!("  renamed {}", path.display());
            }
            ExitCode::SUCCESS
        }
        // The data is in the database by then; only the clean-up of the old files failed.
        Err(e @ migrate::MigrateError::Rename { .. }) => {
            eprintln!("Imported, but the 0.2 files were left in place: {e}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("Migration failed, nothing was changed: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn backup_command() -> ExitCode {
    let Some(dest) = std::env::args().nth(2) else {
        eprintln!("Usage: heartbeat backup <file>");
        return ExitCode::FAILURE;
    };
    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    // Opening a missing path would create an empty database and back that up.
    if !std::path::Path::new(&cfg.database_path).exists() {
        eprintln!(
            "There's no database at {} (DATABASE_PATH).",
            cfg.database_path
        );
        return ExitCode::FAILURE;
    }
    let result = match db::Db::open(&cfg.database_path).await {
        Ok(db) => db.backup_to(dest.clone().into()).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(bytes) => {
            println!(
                "Backed up {} to {dest} ({} KB)",
                cfg.database_path,
                bytes / 1024
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Backup failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn hash_password_command() -> ExitCode {
    use std::io::{BufRead, Write};
    eprint!("Password: ");
    let mut line = String::new();
    let read = std::io::stderr()
        .flush()
        .and_then(|()| std::io::stdin().lock().read_line(&mut line));
    if let Err(e) = read {
        eprintln!("Could not read the password: {e}");
        return ExitCode::FAILURE;
    }
    let password = line.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        eprintln!("The password can't be empty.");
        return ExitCode::FAILURE;
    }
    match password::hash(password) {
        Ok(hash) => {
            println!("{hash}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Could not hash the password: {e}");
            ExitCode::FAILURE
        }
    }
}
