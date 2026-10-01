use std::env;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing environment variable {0} (see .env.example)")]
    Missing(&'static str),
    #[error("{name} has an invalid value: {value:?}")]
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

/// One local account: an email and its argon2 PHC hash (from `heartbeat hash-password`).
#[derive(Debug, Clone)]
pub struct LocalAccount {
    pub email: String,
    pub password_hash: String,
}

/// How users log in.
#[derive(Debug, Clone)]
pub enum AuthMode {
    /// Credentials are forwarded to an external login server (`LOGIN_URL`), which must
    /// answer with `access_token` and `is_admin`. The token is kept in the session and sent
    /// to registered apps' logs endpoints. With `UPSTREAM_VIEWERS=true`, users without
    /// `is_admin` get in as read-only viewers instead of being turned away.
    Upstream {
        login_url: String,
        allow_viewers: bool,
    },
    /// A local admin (`ADMIN_EMAIL` + `ADMIN_PASSWORD_HASH`) and, optionally, a read-only
    /// viewer (`VIEWER_EMAIL` + `VIEWER_PASSWORD_HASH`). Sessions carry no upstream token,
    /// so logs endpoints are authorized by `ADMIN_LOGS_KEY` alone.
    Password {
        admin: LocalAccount,
        viewer: Option<LocalAccount>,
    },
}

impl AuthMode {
    fn from_env() -> Result<Self, ConfigError> {
        match optional("AUTH_MODE").as_deref().unwrap_or("upstream") {
            "upstream" => Ok(Self::Upstream {
                login_url: required("LOGIN_URL")?,
                allow_viewers: parsed("UPSTREAM_VIEWERS", false)?,
            }),
            "password" => Ok(Self::Password {
                admin: LocalAccount {
                    email: required("ADMIN_EMAIL")?.to_lowercase(),
                    password_hash: required("ADMIN_PASSWORD_HASH")?,
                },
                viewer: match optional("VIEWER_EMAIL") {
                    Some(email) => Some(LocalAccount {
                        email: email.to_lowercase(),
                        password_hash: required("VIEWER_PASSWORD_HASH")?,
                    }),
                    None => None,
                },
            }),
            other => Err(ConfigError::Invalid {
                name: "AUTH_MODE",
                value: other.to_string(),
            }),
        }
    }
}

/// A Discord application answering `/pulse` over HTTP interactions.
#[derive(Debug, Clone)]
pub struct DiscordApp {
    pub application_id: String,
    /// Ed25519 key every interaction is signed with (the app's "Public Key", 64 hex chars).
    pub public_key: [u8; 32],
    /// Registers the slash command at startup.
    pub bot_token: String,
}

impl DiscordApp {
    /// `DISCORD_PUBLIC_KEY` turns it on; the application ID and bot token are then required.
    fn from_env() -> Result<Option<Self>, ConfigError> {
        let Some(raw) = optional("DISCORD_PUBLIC_KEY") else {
            return Ok(None);
        };
        let public_key = crate::chat::decode_hex(&raw)
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .ok_or(ConfigError::Invalid {
                name: "DISCORD_PUBLIC_KEY",
                value: raw,
            })?;
        Ok(Some(Self {
            application_id: required("DISCORD_APPLICATION_ID")?,
            public_key,
            bot_token: required("DISCORD_BOT_TOKEN")?,
        }))
    }
}

/// Comma-separated list, trimmed, blanks dropped.
fn list(name: &'static str) -> Vec<String> {
    optional(name)
        .map(|v| {
            v.split(',')
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub auth: AuthMode,
    /// UI language (`APP_LANG`: `es` or `en`).
    pub app_lang: crate::i18n::Lang,
    /// The SQLite database every store lives in (`DATABASE_PATH`).
    pub database_path: String,
    /// Heartbeat 0.2's apps list; only `heartbeat migrate` reads it.
    pub apps_file: String,
    /// Heartbeat 0.2's history (one `<slug>.jsonl` per app); only `heartbeat migrate`
    /// reads it.
    pub uptime_dir: String,
    /// Seconds between health checks of every registered app.
    pub uptime_interval_secs: u64,
    /// A 2xx health check slower than this many ms is "degraded" (yellow), not "up".
    pub uptime_degraded_ms: u32,
    /// Extra attempts before a failed check is recorded as down.
    pub uptime_retries: u32,
    /// Certificate expiring within this many days marks the app degraded.
    pub uptime_cert_warn_days: u32,
    /// Days every check is kept.
    pub uptime_retention_days: u32,
    /// Days the per-day uptime counts are kept (status page, 30-day uptime).
    pub uptime_daily_retention_days: u32,
    /// Seconds before a check gives up (each app can override it).
    pub uptime_timeout_secs: u32,
    /// Share (%) of monitored apps down at once treated as a mass outage; 0 = off.
    pub uptime_mass_down_pct: u8,
    /// Minutes between "still down" reminders; 0 = no reminders.
    pub alert_remind_mins: u32,
    /// Webhooks notified on status changes (Slack, Discord or generic JSON).
    pub alert_webhook_urls: Vec<String>,
    /// Also alert on up <-> degraded changes (off: only down and recovery).
    pub alert_on_degraded: bool,
    /// Who to ping when an app goes down (`ALERT_MENTIONS`, see `alerts::Mention`).
    pub alert_mentions: Vec<String>,
    /// Heartbeat 0.2's alert templates file; only `heartbeat migrate` reads it.
    pub alert_templates_file: String,
    /// Pinged after every round of checks (dead man's switch for the monitor itself).
    pub heartbeat_ping_url: Option<String>,
    /// Public base URL of this deployment, used for links in alerts.
    pub public_url: Option<String>,
    /// Heartbeat 0.2's sessions file; only `heartbeat migrate` reads it.
    pub sessions_file: String,
    /// Heartbeat 0.2's notices file; only `heartbeat migrate` reads it.
    pub notices_file: String,
    /// Heartbeat 0.2's custom theme file; only `heartbeat migrate` reads it.
    pub theme_file: String,
    /// Bearer token for `GET /metrics`; `None` = the endpoint is off.
    pub metrics_token: Option<String>,
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
    /// Signs every Slack slash command (`SLACK_SIGNING_SECRET`); `None` = `/slack/commands`
    /// is off.
    pub slack_signing_secret: Option<String>,
    /// `None` = `/discord/interactions` is off.
    pub discord: Option<DiscordApp>,
    /// Slash command name on both services (`CHAT_COMMAND`, default `pulse`).
    pub chat_command: String,
    /// Who may run commands that change something (`CHAT_ADMINS`: Slack user IDs, Discord
    /// user IDs, `&role` Discord roles). Empty = nobody: only read-only commands work.
    pub chat_admins: Vec<String>,
    /// Days audit log entries are kept (`AUDIT_RETENTION_DAYS`); 0 = forever.
    pub audit_retention_days: u32,
    /// MB of the database read through a memory map (`DATABASE_MMAP_MB`); 0 = off. See
    /// `Db::open_with`.
    pub database_mmap_mb: u32,
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
        let uptime_timeout_secs = parsed("UPTIME_TIMEOUT_SECS", 10)?;
        if !(1..=crate::registry::MAX_TIMEOUT_SECS).contains(&uptime_timeout_secs) {
            return Err(ConfigError::Invalid {
                name: "UPTIME_TIMEOUT_SECS",
                value: uptime_timeout_secs.to_string(),
            });
        }
        let uptime_mass_down_pct = parsed("UPTIME_MASS_DOWN_PCT", 50)?;
        if uptime_mass_down_pct > 100 {
            return Err(ConfigError::Invalid {
                name: "UPTIME_MASS_DOWN_PCT",
                value: uptime_mass_down_pct.to_string(),
            });
        }
        let chat_command = optional("CHAT_COMMAND").unwrap_or_else(|| "pulse".to_string());
        // Discord's rule for command names, which Slack also accepts.
        let valid_command = (1..=32).contains(&chat_command.len())
            && chat_command
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !valid_command {
            return Err(ConfigError::Invalid {
                name: "CHAT_COMMAND",
                value: chat_command,
            });
        }
        let chat_admins = list("CHAT_ADMINS");
        if let Some(bad) = chat_admins
            .iter()
            .find(|entry| crate::chat::Admin::parse(entry).is_none())
        {
            return Err(ConfigError::Invalid {
                name: "CHAT_ADMINS",
                value: bad.clone(),
            });
        }
        Ok(Self {
            host: optional("HOST").unwrap_or_else(|| "0.0.0.0".to_string()),
            port: parsed("PORT", 8090)?,
            auth: AuthMode::from_env()?,
            app_lang: parsed("APP_LANG", crate::i18n::Lang::Es)?,
            database_path: optional("DATABASE_PATH")
                .unwrap_or_else(|| "./data/heartbeat.db".to_string()),
            apps_file: optional("APPS_FILE").unwrap_or_else(|| "./data/apps.json".to_string()),
            uptime_dir: optional("UPTIME_DIR").unwrap_or_else(|| "./data/uptime".to_string()),
            uptime_interval_secs,
            uptime_degraded_ms: parsed("UPTIME_DEGRADED_MS", 800)?,
            uptime_retries: parsed("UPTIME_RETRIES", 2)?,
            uptime_cert_warn_days: parsed("UPTIME_CERT_WARN_DAYS", 14)?,
            uptime_retention_days: parsed("UPTIME_RETENTION_DAYS", 30)?.max(1),
            uptime_daily_retention_days: parsed("UPTIME_DAILY_RETENTION_DAYS", 400)?.max(30),
            uptime_timeout_secs,
            uptime_mass_down_pct,
            alert_remind_mins: parsed("ALERT_REMIND_MINS", 60)?,
            alert_webhook_urls: optional("ALERT_WEBHOOK_URLS")
                .map(|v| {
                    v.split(',')
                        .map(|u| u.trim().to_string())
                        .filter(|u| !u.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            alert_on_degraded: parsed("ALERT_ON_DEGRADED", false)?,
            alert_templates_file: optional("ALERT_TEMPLATES_FILE")
                .unwrap_or_else(|| "./data/alert_templates.json".to_string()),
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
            notices_file: optional("NOTICES_FILE")
                .unwrap_or_else(|| "./data/notices.json".to_string()),
            theme_file: optional("THEME_FILE").unwrap_or_else(|| "./data/theme.css".to_string()),
            metrics_token: optional("METRICS_TOKEN"),
            allowed_hosts: required("ALLOWED_HOSTS")?,
            cookie_secure: parsed("COOKIE_SECURE", true)?,
            admin_logs_key: optional("ADMIN_LOGS_KEY"),
            slack_signing_secret: optional("SLACK_SIGNING_SECRET"),
            discord: DiscordApp::from_env()?,
            chat_command,
            chat_admins,
            audit_retention_days: parsed("AUDIT_RETENTION_DAYS", 365)?,
            database_mmap_mb: parsed("DATABASE_MMAP_MB", 0)?,
        })
    }
}
