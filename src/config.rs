use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// Full URL of the login endpoint this app authenticates against, e.g.
    /// `https://api-pulso-test.adquiere.co/api/v1/auth/login`. That endpoint already
    /// returns `is_admin` (checked live against its own DB, not trusted from a JWT claim)
    /// -- this app leans on that instead of re-implementing admin verification itself.
    pub login_url: String,
    /// Where the registered-apps list (name + logs endpoint URL) is persisted. A plain
    /// JSON file, not a database -- a handful of rows that change rarely don't earn a DB
    /// dependency for a small internal viewer.
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
    /// it (see pulso-backend's `routes::logs::admin_logs_key_matches`). `None` (unset)
    /// just means no key is sent -- registered apps without this feature still work via
    /// the user's own JWT, same as before.
    pub admin_logs_key: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(8090),
            login_url: env::var("LOGIN_URL").unwrap_or_else(|_| {
                "https://api-pulso-test.adquiere.co/api/v1/auth/login".to_string()
            }),
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
            allowed_hosts: env::var("ALLOWED_HOSTS")
                .unwrap_or_else(|_| "*.adquiere.co".to_string()),
            cookie_secure: env::var("COOKIE_SECURE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(true),
            admin_logs_key: env::var("ADMIN_LOGS_KEY").ok(),
        }
    }
}
