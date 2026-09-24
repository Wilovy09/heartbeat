use std::env;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("falta la variable de entorno {0} (ver .env.example)")]
    Missing(&'static str),
}

/// A required variable: unset or blank is an error, never a silent default -- these name
/// the deployment's own login server and hosts, which no default could guess.
fn required(name: &'static str) -> Result<String, ConfigError> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or(ConfigError::Missing(name))
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// Full URL of the login endpoint this app authenticates against (required), e.g.
    /// `https://api.example.com/auth/login`. It must answer a JSON `{email, password}` POST
    /// with `access_token` and `is_admin` -- this app trusts that verdict instead of
    /// re-implementing admin verification itself.
    pub login_url: String,
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
        Ok(Self {
            host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(8090),
            login_url: required("LOGIN_URL")?,
            apps_file: env::var("APPS_FILE").unwrap_or_else(|_| "./data/apps.json".to_string()),
            uptime_dir: env::var("UPTIME_DIR").unwrap_or_else(|_| "./data/uptime".to_string()),
            uptime_interval_secs: env::var("UPTIME_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&secs| secs > 0)
                .unwrap_or(60),
            uptime_degraded_ms: env::var("UPTIME_DEGRADED_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(800),
            allowed_hosts: required("ALLOWED_HOSTS")?,
            cookie_secure: env::var("COOKIE_SECURE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(true),
            admin_logs_key: env::var("ADMIN_LOGS_KEY").ok(),
        })
    }
}
