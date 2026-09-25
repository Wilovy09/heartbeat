use std::env;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("falta la variable de entorno {0} (ver .env.example)")]
    Missing(&'static str),
    #[error("{name} tiene un valor inválido: {value:?}")]
    Invalid { name: &'static str, value: String },
}

/// A required variable: unset or blank is an error, never a silent default -- these name
/// the deployment's own login server and hosts, which no default could guess.
fn required(name: &'static str) -> Result<String, ConfigError> {
    optional(name).ok_or(ConfigError::Missing(name))
}

/// Set and non-blank, trimmed.
fn optional(name: &'static str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Parsed value, or `default` when unset. A value that is set but doesn't parse is an
/// error rather than a silent fallback: a typo shouldn't quietly change behavior.
fn parsed<T: std::str::FromStr>(name: &'static str, default: T) -> Result<T, ConfigError> {
    match optional(name) {
        None => Ok(default),
        Some(value) => value
            .parse()
            .map_err(|_| ConfigError::Invalid { name, value }),
    }
}

/// How admins log in.
#[derive(Debug, Clone)]
pub enum AuthMode {
    /// Credentials are forwarded to an external login server (`LOGIN_URL`), which must
    /// answer with `access_token` and `is_admin`. The token is kept in the session and sent
    /// to registered apps' logs endpoints.
    Upstream { login_url: String },
    /// A single local admin: `ADMIN_EMAIL` + `ADMIN_PASSWORD_HASH` (argon2 PHC string, from
    /// `heartbeat hash-password`). Sessions carry no upstream token, so logs endpoints are
    /// authorized by `ADMIN_LOGS_KEY` alone.
    Password {
        email: String,
        password_hash: String,
    },
}

impl AuthMode {
    fn from_env() -> Result<Self, ConfigError> {
        match optional("AUTH_MODE").as_deref().unwrap_or("upstream") {
            "upstream" => Ok(Self::Upstream {
                login_url: required("LOGIN_URL")?,
            }),
            "password" => Ok(Self::Password {
                email: required("ADMIN_EMAIL")?.to_lowercase(),
                password_hash: required("ADMIN_PASSWORD_HASH")?,
            }),
            other => Err(ConfigError::Invalid {
                name: "AUTH_MODE",
                value: other.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub auth: AuthMode,
    /// UI language (`APP_LANG`: `es` or `en`).
    pub app_lang: crate::i18n::Lang,
    /// Where the registered-apps list (name + logs endpoint URL) is persisted. A plain
    /// JSON file, not a database -- a handful of rows that change rarely don't earn a DB
    /// dependency.
    pub apps_file: String,
    /// Directory holding one `<slug>.jsonl` heartbeat history per app (see `uptime`).
    pub uptime_dir: String,
    /// Seconds between health checks of every registered app.
    pub uptime_interval_secs: u64,
    /// A 2xx health check slower than this many ms is "degraded" (yellow), not "up".
    pub uptime_degraded_ms: u32,
    /// Extra attempts before a failed check is recorded as down.
    pub uptime_retries: u32,
    /// Certificate expiring within this many days marks the app degraded.
    pub uptime_cert_warn_days: u32,
    /// Days of heartbeat history kept.
    pub uptime_retention_days: u32,
    /// Webhooks notified on status changes (Slack, Discord or generic JSON).
    pub alert_webhook_urls: Vec<String>,
    /// Also alert on up <-> degraded changes (off: only down and recovery).
    pub alert_on_degraded: bool,
    /// Who to ping when an app goes down (`ALERT_MENTIONS`, see `alerts::Mention`).
    pub alert_mentions: Vec<String>,
    /// Pinged after every round of checks (dead man's switch for the monitor itself).
    pub heartbeat_ping_url: Option<String>,
    /// Public base URL of this deployment, used for links in alerts.
    pub public_url: Option<String>,
    /// Where sessions are persisted so restarts don't log everyone out.
    pub sessions_file: String,
    /// Comma-separated hosts registered apps' URLs may point at (`*.example.com` = any
    /// subdomain). Requests carry the admin's JWT and `ADMIN_LOGS_KEY`, so they only go to
    /// these hosts, over https -- see `outbound`.
    pub allowed_hosts: String,
    /// Mark the session cookie `Secure` (HTTPS-only). Only turn off for local http dev.
    pub cookie_secure: bool,
    /// Shared secret sent as `X-Admin-Logs-Key` on every proxied call to a registered
    /// app's logs endpoint, instead of (well, in addition to) the logged-in user's own
    /// session token. A personal JWT is only meaningful inside the environment that issued
    /// it -- being an admin in prod has no corresponding row in test's database and vice
    /// versa (each environment provisions accounts independently) -- so forwarding the
    /// user's token can't be the thing that authorizes cross-environment log access. This
    /// key must match `ADMIN_LOGS_KEY` on every registered app that's expected to accept
    /// it (see "Contrato de los endpoints" in the README). `None` (unset)
    /// just means no key is sent -- registered apps without this feature still work via
    /// the user's own JWT, same as before.
    pub admin_logs_key: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let uptime_interval_secs = parsed("UPTIME_INTERVAL_SECS", 60)?;
        if uptime_interval_secs == 0 {
            return Err(ConfigError::Invalid {
                name: "UPTIME_INTERVAL_SECS",
                value: "0".to_string(),
            });
        }
        Ok(Self {
            host: optional("HOST").unwrap_or_else(|| "0.0.0.0".to_string()),
            port: parsed("PORT", 8090)?,
            auth: AuthMode::from_env()?,
            app_lang: parsed("APP_LANG", crate::i18n::Lang::Es)?,
            apps_file: optional("APPS_FILE").unwrap_or_else(|| "./data/apps.json".to_string()),
            uptime_dir: optional("UPTIME_DIR").unwrap_or_else(|| "./data/uptime".to_string()),
            uptime_interval_secs,
            uptime_degraded_ms: parsed("UPTIME_DEGRADED_MS", 800)?,
            uptime_retries: parsed("UPTIME_RETRIES", 2)?,
            uptime_cert_warn_days: parsed("UPTIME_CERT_WARN_DAYS", 14)?,
            uptime_retention_days: parsed("UPTIME_RETENTION_DAYS", 30)?.max(1),
            alert_webhook_urls: optional("ALERT_WEBHOOK_URLS")
                .map(|v| {
                    v.split(',')
                        .map(|u| u.trim().to_string())
                        .filter(|u| !u.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            alert_on_degraded: parsed("ALERT_ON_DEGRADED", false)?,
            alert_mentions: optional("ALERT_MENTIONS")
                .map(|v| {
                    v.split(',')
                        .map(|m| m.trim().to_string())
                        .filter(|m| !m.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            heartbeat_ping_url: optional("HEARTBEAT_PING_URL"),
            public_url: optional("PUBLIC_URL"),
            sessions_file: optional("SESSIONS_FILE")
                .unwrap_or_else(|| "./data/sessions.json".to_string()),
            allowed_hosts: required("ALLOWED_HOSTS")?,
            cookie_secure: parsed("COOKIE_SECURE", true)?,
            admin_logs_key: optional("ADMIN_LOGS_KEY"),
        })
    }
}
