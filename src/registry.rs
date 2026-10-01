//! The list of registered apps (name + their /admin/logs-shaped endpoint URL + a health
//! check URL for uptime monitoring), stored in the `apps` table. Nearly every request
//! reads it, so a copy stays in memory; each change is written to the database first and
//! only then applied to the copy, so the two never disagree.

use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;

use crate::db::{Db, DbError};

/// Every way a registry operation can fail. `Display` is for logs; the /apps form shows
/// the localized text (`Localize`).
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("the name can't be empty")]
    EmptyName,
    #[error("the name needs at least one letter or digit")]
    NameWithoutAlphanumerics,
    #[error("the {0} URL must start with http:// or https://")]
    InvalidUrl(&'static str),
    #[error("check interval must be between {MIN_INTERVAL_SECS} and {MAX_INTERVAL_SECS} s")]
    IntervalOutOfRange,
    #[error("check timeout must be between 1 and {MAX_TIMEOUT_SECS} s")]
    TimeoutOutOfRange,
    #[error("expected status {0} isn't an HTTP status code")]
    InvalidStatus(u16),
    #[error("invalid header: {0}")]
    InvalidHeader(String),
    #[error("invalid alert webhook {0}")]
    InvalidWebhook(String),
    #[error("unrecognized mention {0}")]
    InvalidMention(String),
    #[error("too many entries in {0}")]
    TooMany(&'static str),
    #[error("an app with an equivalent name is already registered")]
    SlugTaken,
    #[error("app '{0}' not found")]
    NotFound(String),
    #[error("could not generate a token: {0}")]
    Token(#[from] getrandom::Error),
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a valid apps file: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Db(#[from] DbError),
}

impl RegistryError {
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        move |source| Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

type Result<T> = std::result::Result<T, RegistryError>;

/// Per-app check interval bounds, in seconds.
pub const MIN_INTERVAL_SECS: u32 = 10;
pub const MAX_INTERVAL_SECS: u32 = 86_400;
/// Longest per-app check timeout, in seconds.
pub const MAX_TIMEOUT_SECS: u32 = 60;
/// Most custom headers, and most alert webhooks, per app.
const MAX_HEADERS: usize = 10;
const MAX_WEBHOOKS: usize = 5;

/// One extra request header sent with an app's health check (e.g. an auth token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckHeader {
    pub name: String,
    pub value: String,
}

impl CheckHeader {
    /// Headers that would change what the request *is* rather than annotate it.
    const RESERVED: [&'static str; 5] = [
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "upgrade",
    ];

    /// Parses one `Name: value` line.
    pub fn parse_line(line: &str) -> Result<Self> {
        let invalid = || RegistryError::InvalidHeader(line.trim().to_string());
        let (name, value) = line.split_once(':').ok_or_else(invalid)?;
        let header = Self {
            name: name.trim().to_string(),
            value: value.trim().to_string(),
        };
        header.validate().map_err(|()| invalid())?;
        Ok(header)
    }

    fn validate(&self) -> std::result::Result<(), ()> {
        let name = reqwest::header::HeaderName::from_bytes(self.name.as_bytes()).map_err(|_| ())?;
        reqwest::header::HeaderValue::from_str(&self.value).map_err(|_| ())?;
        if Self::RESERVED.contains(&name.as_str()) {
            return Err(());
        }
        Ok(())
    }
}

impl crate::i18n::Localize for RegistryError {
    fn localize(&self, i18n: &crate::i18n::I18n) -> String {
        match self {
            Self::EmptyName => i18n.text("err.empty_name", &[]),
            Self::NameWithoutAlphanumerics => i18n.text("err.name_no_alnum", &[]),
            Self::InvalidUrl(label) => i18n.text("err.invalid_url", &[("label", label)]),
            Self::IntervalOutOfRange => i18n.text(
                "err.interval",
                &[
                    ("min", &MIN_INTERVAL_SECS.to_string()),
                    ("max", &MAX_INTERVAL_SECS.to_string()),
                ],
            ),
            Self::TimeoutOutOfRange => {
                i18n.text("err.timeout", &[("max", &MAX_TIMEOUT_SECS.to_string())])
            }
            Self::InvalidStatus(code) => i18n.text("err.status", &[("code", &code.to_string())]),
            Self::InvalidHeader(line) => i18n.text("err.header", &[("header", line)]),
            Self::InvalidWebhook(url) => i18n.text("err.webhook", &[("url", url)]),
            Self::InvalidMention(entry) => i18n.text("err.mention", &[("entry", entry)]),
            Self::TooMany(field) => i18n.text("err.too_many", &[("field", field)]),
            Self::SlugTaken => i18n.text("err.slug_taken", &[]),
            Self::NotFound(slug) => i18n.text("err.not_found", &[("slug", slug)]),
            Self::Token(_) | Self::Io { .. } | Self::Json { .. } | Self::Db(_) => {
                i18n.text("err.internal", &[("error", &self.to_string())])
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredApp {
    /// URL-safe identifier derived from `name` at registration time -- what routes address
    /// it by (`/api/apps/{slug}/logs`), since `name` itself may contain spaces/accents.
    pub slug: String,
    pub name: String,
    /// `/admin/logs`-shaped endpoint; `None` = a monitor-only app (e.g. a frontend).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs_url: Option<String>,
    /// Endpoint an uptime check can hit. `None` only for apps registered before this field
    /// existed -- `add` requires it for every new registration.
    #[serde(default)]
    pub health_url: Option<String>,
    /// Secret that authorizes the public, read-only status embed (`<heartbeat-status>`).
    /// Never empty after `AppRegistry::load` -- entries saved before this field existed get
    /// one generated there.
    #[serde(default)]
    pub embed_token: String,
    /// Paused apps aren't probed, so maintenance time doesn't count against their uptime.
    #[serde(default)]
    pub paused: bool,
    /// Listed on the public `/status` page.
    #[serde(default)]
    pub public: bool,
    /// Per-app "degraded" threshold; `None` = the global `UPTIME_DEGRADED_MS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_after_ms: Option<u32>,
    /// Text the health response body must contain to count as up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_body: Option<String>,
    /// For single-page apps: also check that every JS/CSS bundle the health page
    /// references actually loads as JS/CSS (see `uptime::check_assets`).
    #[serde(default)]
    pub check_assets: bool,
    /// Unix seconds when the current pause started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<u64>,
    /// Scheduled maintenance: the pause ends by itself at this unix time. `None` while
    /// paused = until resumed by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_until: Option<u64>,
    /// Seconds between checks; `None` = the global `UPTIME_INTERVAL_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u32>,
    /// Seconds before a check gives up; `None` = the global `UPTIME_TIMEOUT_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u32>,
    /// Status codes that count as up; empty = any 2xx.
    #[serde(default)]
    pub expect_status: Vec<u16>,
    /// Extra headers sent with the health check.
    #[serde(default)]
    pub headers: Vec<CheckHeader>,
    /// This app's own alert webhooks, notified besides the global ones.
    #[serde(default)]
    pub alert_webhooks: Vec<String>,
    /// This app's own mentions (`ALERT_MENTIONS` syntax), added to the global ones.
    #[serde(default)]
    pub alert_mentions: Vec<String>,
}

/// What an admin sets when registering or editing an app. The slug and embed token are
/// not part of it: they're assigned once and survive every edit.
#[derive(Debug, Clone, Default)]
pub struct AppSettings {
    pub name: String,
    /// Blank = monitor-only app.
    pub logs_url: String,
    pub health_url: String,
    pub degraded_after_ms: Option<u32>,
    pub expect_body: Option<String>,
    pub check_assets: bool,
    pub interval_secs: Option<u32>,
    pub timeout_secs: Option<u32>,
    pub expect_status: Vec<u16>,
    pub headers: Vec<CheckHeader>,
    pub alert_webhooks: Vec<String>,
    pub alert_mentions: Vec<String>,
}

/// `AppSettings` after `normalized`: trimmed, validated, blanks turned into `None`.
struct ValidSettings {
    name: String,
    logs_url: Option<String>,
    health_url: String,
    degraded_after_ms: Option<u32>,
    expect_body: Option<String>,
    check_assets: bool,
    interval_secs: Option<u32>,
    timeout_secs: Option<u32>,
    expect_status: Vec<u16>,
    headers: Vec<CheckHeader>,
    alert_webhooks: Vec<String>,
    alert_mentions: Vec<String>,
}

impl AppSettings {
    /// Trims every field, turns blank optionals into `None`, and validates.
    fn normalized(self) -> Result<ValidSettings> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err(RegistryError::EmptyName);
        }
        let logs_url = Some(self.logs_url.trim().to_string()).filter(|u| !u.is_empty());
        let health_url = self.health_url.trim().to_string();
        if let Some(url) = &logs_url {
            validate_url(url, "logs", &["http://", "https://"])?;
        }
        validate_url(&health_url, "health", &["http://", "https://", "tcp://"])?;
        let expect_body = self
            .expect_body
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let interval_secs = self.interval_secs.filter(|&s| s > 0);
        if interval_secs.is_some_and(|s| !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&s)) {
            return Err(RegistryError::IntervalOutOfRange);
        }
        let timeout_secs = self.timeout_secs.filter(|&s| s > 0);
        if timeout_secs.is_some_and(|s| s > MAX_TIMEOUT_SECS) {
            return Err(RegistryError::TimeoutOutOfRange);
        }
        if let Some(&code) = self
            .expect_status
            .iter()
            .find(|c| !(100..=599).contains(*c))
        {
            return Err(RegistryError::InvalidStatus(code));
        }
        if self.headers.len() > MAX_HEADERS {
            return Err(RegistryError::TooMany("headers"));
        }
        if let Some(bad) = self.headers.iter().find(|h| h.validate().is_err()) {
            return Err(RegistryError::InvalidHeader(bad.name.clone()));
        }
        let alert_webhooks = trimmed(self.alert_webhooks);
        if alert_webhooks.len() > MAX_WEBHOOKS {
            return Err(RegistryError::TooMany("webhooks"));
        }
        if let Some(bad) = alert_webhooks
            .iter()
            .find(|u| crate::alerts::Webhook::for_app(u).is_err())
        {
            return Err(RegistryError::InvalidWebhook(bad.clone()));
        }
        let alert_mentions = trimmed(self.alert_mentions);
        if let Some(bad) = alert_mentions
            .iter()
            .find(|m| m.parse::<crate::alerts::Mention>().is_err())
        {
            return Err(RegistryError::InvalidMention(bad.clone()));
        }
        let mut expect_status = self.expect_status;
        expect_status.sort_unstable();
        expect_status.dedup();
        Ok(ValidSettings {
            name,
            logs_url,
            health_url,
            degraded_after_ms: self.degraded_after_ms.filter(|&ms| ms > 0),
            expect_body,
            check_assets: self.check_assets,
            interval_secs,
            timeout_secs,
            expect_status,
            headers: self.headers,
            alert_webhooks,
            alert_mentions,
        })
    }
}

/// Trimmed, without blanks or duplicates, order kept.
fn trimmed(entries: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        let entry = entry.trim().to_string();
        if !entry.is_empty() && !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

impl RegisteredApp {
    /// Where this app's alerts go besides the global webhooks.
    #[must_use]
    pub fn alert_route(&self) -> crate::alerts::AppRoute {
        crate::alerts::AppRoute {
            webhooks: self.alert_webhooks.clone(),
            mentions: self.alert_mentions.clone(),
        }
    }

    /// Whether this app gets health checks at all.
    #[must_use]
    pub fn is_monitored(&self) -> bool {
        !self.paused && self.health_url.is_some()
    }

    fn apply(&mut self, settings: ValidSettings) {
        self.name = settings.name;
        self.logs_url = settings.logs_url;
        self.health_url = Some(settings.health_url);
        self.degraded_after_ms = settings.degraded_after_ms;
        self.expect_body = settings.expect_body;
        self.check_assets = settings.check_assets;
        self.interval_secs = settings.interval_secs;
        self.timeout_secs = settings.timeout_secs;
        self.expect_status = settings.expect_status;
        self.headers = settings.headers;
        self.alert_webhooks = settings.alert_webhooks;
        self.alert_mentions = settings.alert_mentions;
    }

    /// Constant-time comparison, so response timing doesn't leak how much of a guessed
    /// token was right.
    #[must_use]
    pub fn embed_token_matches(&self, candidate: &str) -> bool {
        crate::token::secret_matches(&self.embed_token, candidate)
    }
}

/// 24 random bytes, hex-encoded (48 chars).
pub(crate) fn new_embed_token() -> Result<String> {
    Ok(crate::token::random_hex(24)?)
}

pub struct AppRegistry {
    db: Db,
    /// Every app, in registration order.
    apps: RwLock<Vec<RegisteredApp>>,
    /// Bumped after every change to `apps`, while it's still locked: whoever reads the
    /// generation and then the list never pairs a newer generation with older apps.
    generation: AtomicU64,
}

/// The `settings` JSON column: everything about an app that's only read together with it.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    degraded_after_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expect_body: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    check_assets: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interval_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    expect_status: Vec<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    headers: Vec<CheckHeader>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    alert_webhooks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    alert_mentions: Vec<String>,
}

impl From<&RegisteredApp> for StoredSettings {
    fn from(app: &RegisteredApp) -> Self {
        Self {
            degraded_after_ms: app.degraded_after_ms,
            expect_body: app.expect_body.clone(),
            check_assets: app.check_assets,
            interval_secs: app.interval_secs,
            timeout_secs: app.timeout_secs,
            expect_status: app.expect_status.clone(),
            headers: app.headers.clone(),
            alert_webhooks: app.alert_webhooks.clone(),
            alert_mentions: app.alert_mentions.clone(),
        }
    }
}

const SELECT_APPS: &str = "SELECT slug, name, health_url, logs_url, embed_token, public, paused, \
     paused_at, paused_until, settings FROM apps";

fn app_from_row(row: &rusqlite::Row<'_>) -> std::result::Result<RegisteredApp, DbError> {
    let settings: StoredSettings = serde_json::from_str(&row.get::<_, String>("settings")?)?;
    let time = |col: &str| -> rusqlite::Result<Option<u64>> {
        let value: Option<i64> = row.get(col)?;
        Ok(value.and_then(|v| u64::try_from(v).ok()))
    };
    Ok(RegisteredApp {
        slug: row.get("slug")?,
        name: row.get("name")?,
        logs_url: row.get("logs_url")?,
        health_url: row.get("health_url")?,
        embed_token: row.get("embed_token")?,
        paused: row.get("paused")?,
        public: row.get("public")?,
        paused_at: time("paused_at")?,
        paused_until: time("paused_until")?,
        degraded_after_ms: settings.degraded_after_ms,
        expect_body: settings.expect_body,
        check_assets: settings.check_assets,
        interval_secs: settings.interval_secs,
        timeout_secs: settings.timeout_secs,
        expect_status: settings.expect_status,
        headers: settings.headers,
        alert_webhooks: settings.alert_webhooks,
        alert_mentions: settings.alert_mentions,
    })
}

/// Unix seconds as SQLite stores them.
fn sql_time(at: Option<u64>) -> Option<i64> {
    at.map(|t| i64::try_from(t).unwrap_or(i64::MAX))
}

/// Inserts `app` at the end of the list, or updates it in place (its position never
/// changes once it's registered).
pub(crate) fn save_app(
    tx: &rusqlite::Transaction<'_>,
    app: &RegisteredApp,
) -> std::result::Result<(), DbError> {
    let settings = serde_json::to_string(&StoredSettings::from(app))?;
    tx.prepare_cached(
        "INSERT INTO apps (slug, position, name, health_url, logs_url, embed_token, public, \
             paused, paused_at, paused_until, settings) \
         VALUES (?1, (SELECT coalesce(max(position), -1) + 1 FROM apps), ?2, ?3, ?4, ?5, ?6, \
             ?7, ?8, ?9, ?10) \
         ON CONFLICT (slug) DO UPDATE SET name = excluded.name, \
             health_url = excluded.health_url, logs_url = excluded.logs_url, \
             embed_token = excluded.embed_token, public = excluded.public, \
             paused = excluded.paused, paused_at = excluded.paused_at, \
             paused_until = excluded.paused_until, settings = excluded.settings",
    )?
    .execute(params![
        app.slug,
        app.name,
        app.health_url,
        app.logs_url,
        app.embed_token,
        app.public,
        app.paused,
        sql_time(app.paused_at),
        sql_time(app.paused_until),
        settings,
    ])?;
    Ok(())
}

/// Reads a 0.2 `apps.json` (for `heartbeat migrate`). A missing file is an empty list.
pub fn read_legacy_file(path: &Path) -> Result<Vec<RegisteredApp>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|source| RegistryError::Json {
            path: path.to_path_buf(),
            source,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(RegistryError::io(path)(e)),
    }
}

/// Lowercase, spaces/anything-not-alphanumeric collapsed to a single hyphen, trimmed --
/// "Billing API (staging)" -> "billing-api-staging".
fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_was_dash = true; // swallow a leading dash
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

fn validate_url(url: &str, label: &'static str, schemes: &[&str]) -> Result<()> {
    if schemes.iter().any(|scheme| url.starts_with(scheme)) {
        Ok(())
    } else {
        Err(RegistryError::InvalidUrl(label))
    }
}

impl AppRegistry {
    pub async fn load(db: Db) -> Result<Self> {
        let apps = db
            .read(|conn| {
                let mut stmt = conn.prepare(&format!("{SELECT_APPS} ORDER BY position"))?;
                let mut rows = stmt.query([])?;
                let mut apps = Vec::new();
                while let Some(row) = rows.next()? {
                    apps.push(app_from_row(row)?);
                }
                Ok(apps)
            })
            .await?;
        Ok(Self {
            db,
            apps: RwLock::new(apps),
            generation: AtomicU64::new(0),
        })
    }

    /// Changes whenever any app does (see `generation` on the struct).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Writes `app` to the database; the caller updates the in-memory copy after.
    async fn save(&self, app: RegisteredApp) -> Result<RegisteredApp> {
        let saved = self
            .db
            .write(move |tx| {
                save_app(tx, &app)?;
                Ok(app)
            })
            .await?;
        Ok(saved)
    }

    pub async fn list(&self) -> Vec<RegisteredApp> {
        self.apps.read().await.clone()
    }

    pub async fn find(&self, slug: &str) -> Option<RegisteredApp> {
        self.apps
            .read()
            .await
            .iter()
            .find(|a| a.slug == slug)
            .cloned()
    }

    /// Registers an app, returns its new slug. Errors on a blank name/URL or a slug
    /// collision (two names that normalize the same way, e.g. "API Test" and "api-test")
    /// rather than silently overwriting the earlier registration.
    pub async fn add(&self, settings: AppSettings) -> Result<String> {
        let settings = settings.normalized()?;
        let slug = slugify(&settings.name);
        if slug.is_empty() {
            return Err(RegistryError::NameWithoutAlphanumerics);
        }

        // Held until the database write is done: two registrations can't race to a slug.
        let mut apps = self.apps.write().await;
        if apps.iter().any(|a| a.slug == slug) {
            return Err(RegistryError::SlugTaken);
        }
        let mut app = RegisteredApp {
            slug: slug.clone(),
            name: String::new(),
            logs_url: None,
            health_url: None,
            embed_token: new_embed_token()?,
            paused: false,
            public: false,
            degraded_after_ms: None,
            expect_body: None,
            check_assets: false,
            paused_at: None,
            paused_until: None,
            interval_secs: None,
            timeout_secs: None,
            expect_status: Vec::new(),
            headers: Vec::new(),
            alert_webhooks: Vec::new(),
            alert_mentions: Vec::new(),
        };
        app.apply(settings);
        apps.push(self.save(app).await?);
        self.changed();
        Ok(slug)
    }

    /// Applies `change` to a copy of one app, saves it, then swaps it in; or reports the
    /// app missing.
    async fn modify(
        &self,
        slug: &str,
        change: impl FnOnce(&mut RegisteredApp) -> Result<()>,
    ) -> Result<()> {
        let mut apps = self.apps.write().await;
        let app = apps
            .iter_mut()
            .find(|a| a.slug == slug)
            .ok_or_else(|| RegistryError::NotFound(slug.to_string()))?;
        let mut changed = app.clone();
        change(&mut changed)?;
        *app = self.save(changed).await?;
        self.changed();
        Ok(())
    }

    /// Replaces an app's settings. Its slug, embed token and history stay the same.
    pub async fn update(&self, slug: &str, settings: AppSettings) -> Result<()> {
        let settings = settings.normalized()?;
        self.modify(slug, |app| {
            app.apply(settings);
            Ok(())
        })
        .await
    }

    /// Pauses (optionally until `until`, unix seconds) or resumes an app's checks.
    pub async fn set_paused(&self, slug: &str, paused: bool, until: Option<u64>) -> Result<()> {
        let now = crate::uptime::unix_now();
        self.modify(slug, |app| {
            // Re-pausing a paused app (e.g. to set an end time) keeps when the pause began.
            app.paused_at = match (paused, app.paused) {
                (true, true) => app.paused_at.or(Some(now)),
                (true, false) => Some(now),
                (false, _) => None,
            };
            app.paused = paused;
            app.paused_until = until.filter(|_| paused);
            Ok(())
        })
        .await
    }

    /// Ends every scheduled pause whose time has come; returns the slugs it resumed.
    pub async fn resume_expired(&self, now: u64) -> Result<Vec<String>> {
        let mut apps = self.apps.write().await;
        let resumed: Vec<RegisteredApp> = apps
            .iter()
            .filter(|a| a.paused && a.paused_until.is_some_and(|until| until <= now))
            .map(|a| RegisteredApp {
                paused: false,
                paused_at: None,
                paused_until: None,
                ..a.clone()
            })
            .collect();
        if resumed.is_empty() {
            return Ok(Vec::new());
        }
        let resumed = self
            .db
            .write(move |tx| {
                for app in &resumed {
                    save_app(tx, app)?;
                }
                Ok(resumed)
            })
            .await?;
        let slugs = resumed.iter().map(|a| a.slug.clone()).collect();
        for app in resumed {
            if let Some(slot) = apps.iter_mut().find(|a| a.slug == app.slug) {
                *slot = app;
            }
        }
        self.changed();
        Ok(slugs)
    }

    pub async fn set_public(&self, slug: &str, public: bool) -> Result<()> {
        self.modify(slug, |app| {
            app.public = public;
            Ok(())
        })
        .await
    }

    /// Replaces the app's embed token, invalidating every embed that uses the old one.
    pub async fn rotate_embed_token(&self, slug: &str) -> Result<()> {
        self.modify(slug, |app| {
            app.embed_token = new_embed_token()?;
            Ok(())
        })
        .await
    }

    /// Unregisters an app; its history goes with it (`ON DELETE CASCADE`).
    pub async fn remove(&self, slug: &str) -> Result<()> {
        let mut apps = self.apps.write().await;
        if !apps.iter().any(|a| a.slug == slug) {
            return Err(RegistryError::NotFound(slug.to_string()));
        }
        let owned = slug.to_string();
        self.db
            .write(move |tx| {
                tx.execute("DELETE FROM apps WHERE slug = ?1", [owned])?;
                Ok(())
            })
            .await?;
        apps.retain(|a| a.slug != slug);
        self.changed();
        Ok(())
    }

    /// One app straight from the database, bypassing the in-memory copy (tests).
    #[cfg(test)]
    async fn stored(&self, slug: &str) -> Option<RegisteredApp> {
        let slug = slug.to_string();
        self.db
            .read(move |conn| {
                let mut stmt = conn.prepare(&format!("{SELECT_APPS} WHERE slug = ?1"))?;
                let mut rows = stmt.query([slug])?;
                rows.next()?.map(app_from_row).transpose()
            })
            .await
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn registry() -> AppRegistry {
        AppRegistry::load(Db::open_in_memory()).await.unwrap()
    }

    fn settings(name: &str, logs_url: &str, health_url: &str) -> AppSettings {
        AppSettings {
            name: name.to_string(),
            logs_url: logs_url.to_string(),
            health_url: health_url.to_string(),
            ..AppSettings::default()
        }
    }

    #[tokio::test]
    async fn update_keeps_slug_and_token_and_pause_persists() {
        let db = Db::open_in_memory();
        let registry = AppRegistry::load(db.clone()).await.unwrap();
        let slug = registry
            .add(settings(
                "Billing API",
                "https://x/logs",
                "https://x/health",
            ))
            .await
            .unwrap();
        let token = registry.find(&slug).await.unwrap().embed_token;

        let mut changed = settings("Billing API v2", "https://y/logs", "https://y/health");
        changed.degraded_after_ms = Some(1500);
        changed.expect_body = Some("  ok  ".to_string());
        registry.update(&slug, changed).await.unwrap();
        registry.set_paused(&slug, true, None).await.unwrap();
        registry.set_public(&slug, true).await.unwrap();

        let app = AppRegistry::load(db)
            .await
            .unwrap()
            .find(&slug)
            .await
            .unwrap();
        assert_eq!(app.name, "Billing API v2");
        assert_eq!(app.embed_token, token);
        assert_eq!(app.degraded_after_ms, Some(1500));
        assert_eq!(app.expect_body.as_deref(), Some("ok"));
        assert!(app.paused && app.public);

        assert!(matches!(
            registry
                .update("missing", settings("a", "https://x", "https://x"))
                .await,
            Err(RegistryError::NotFound(_))
        ));
    }

    #[test]
    fn slugify_lowercases_and_collapses_punctuation() {
        assert_eq!(slugify("Billing API (staging)"), "billing-api-staging");
        assert_eq!(slugify("  leading/trailing  "), "leading-trailing");
        assert_eq!(slugify("já-acentuado"), "j-acentuado");
    }

    #[tokio::test]
    async fn add_then_find_round_trips() {
        let registry = registry().await;
        let slug = registry
            .add(settings(
                "Billing API",
                "https://x/logs",
                "https://x/health",
            ))
            .await
            .unwrap();
        assert_eq!(slug, "billing-api");
        let found = registry.find(&slug).await.unwrap();
        assert_eq!(found.name, "Billing API");
        assert_eq!(found.logs_url.as_deref(), Some("https://x/logs"));
        assert_eq!(found.health_url.as_deref(), Some("https://x/health"));
    }

    #[tokio::test]
    async fn add_rejects_a_slug_collision() {
        let registry = registry().await;
        registry
            .add(settings(
                "Billing API",
                "https://x/logs",
                "https://x/health",
            ))
            .await
            .unwrap();
        let err = registry
            .add(settings(
                "billing api",
                "https://y/logs",
                "https://y/health",
            ))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn add_rejects_a_url_without_a_scheme() {
        let registry = registry().await;
        let err = registry
            .add(settings("Bad", "not-a-url", "https://x/health"))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn add_rejects_a_health_url_without_a_scheme() {
        let registry = registry().await;
        let err = registry.add(settings("Bad", "https://x/logs", "")).await;
        assert!(err.is_err());
    }

    #[test]
    fn legacy_files_load_entries_saved_before_health_url_existed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("apps.json");
        std::fs::write(
            &file,
            r#"[{"slug":"old","name":"Old","logs_url":"https://x/logs"}]"#,
        )
        .unwrap();
        let apps = read_legacy_file(&file).unwrap();
        assert_eq!(apps[0].health_url, None);
        assert!(apps[0].embed_token.is_empty(), "the importer fills it in");
        assert!(
            read_legacy_file(&dir.path().join("missing.json"))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn add_generates_an_embed_token_and_rotate_replaces_it() {
        let registry = registry().await;
        let slug = registry
            .add(settings(
                "Billing API",
                "https://x/logs",
                "https://x/health",
            ))
            .await
            .unwrap();
        let app = registry.find(&slug).await.unwrap();
        assert_eq!(app.embed_token.len(), 48);
        assert!(app.embed_token_matches(&app.embed_token.clone()));
        assert!(!app.embed_token_matches(""));
        assert!(!app.embed_token_matches(&"0".repeat(48)));

        registry.rotate_embed_token(&slug).await.unwrap();
        let rotated = registry.find(&slug).await.unwrap();
        assert_ne!(rotated.embed_token, app.embed_token);
        assert!(!rotated.embed_token_matches(&app.embed_token));
    }

    #[tokio::test]
    async fn persists_across_a_reload_in_registration_order() {
        let db = Db::open_in_memory();
        let registry = AppRegistry::load(db.clone()).await.unwrap();
        for name in ["Zeta", "Alpha", "Mid"] {
            registry
                .add(settings(name, "", "https://x/health"))
                .await
                .unwrap();
        }
        let names: Vec<String> = AppRegistry::load(db)
            .await
            .unwrap()
            .list()
            .await
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, ["Zeta", "Alpha", "Mid"]);
    }

    #[tokio::test]
    async fn remove_then_reload_reflects_the_removal() {
        let db = Db::open_in_memory();
        let registry = AppRegistry::load(db.clone()).await.unwrap();
        let slug = registry
            .add(settings(
                "Billing API",
                "https://x/logs",
                "https://x/health",
            ))
            .await
            .unwrap();
        registry.remove(&slug).await.unwrap();
        assert!(registry.list().await.is_empty());
        assert!(AppRegistry::load(db).await.unwrap().list().await.is_empty());
        assert!(matches!(
            registry.remove(&slug).await,
            Err(RegistryError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn monitor_only_apps_have_no_logs_url() {
        let registry = registry().await;
        let mut frontend = settings("Frontend", "   ", "https://x/");
        frontend.check_assets = true;
        let slug = registry.add(frontend).await.unwrap();
        let app = registry.stored(&slug).await.unwrap();
        assert_eq!(app.logs_url, None);
        assert!(app.check_assets);
    }

    #[tokio::test]
    async fn every_setting_survives_the_database() {
        let registry = registry().await;
        let mut full = settings("Full", "https://x/logs", "https://x/health");
        full.degraded_after_ms = Some(900);
        full.expect_body = Some("ok".into());
        full.check_assets = true;
        full.interval_secs = Some(30);
        full.timeout_secs = Some(5);
        full.expect_status = vec![200, 401];
        full.headers = vec![CheckHeader::parse_line("X-Token: abc").unwrap()];
        full.alert_webhooks = vec!["https://hooks.slack.com/services/T/B/x".into()];
        full.alert_mentions = vec!["here".into()];
        let slug = registry.add(full).await.unwrap();
        registry.set_paused(&slug, true, Some(5000)).await.unwrap();
        let memory = registry.find(&slug).await.unwrap();
        let stored = registry.stored(&slug).await.unwrap();
        assert_eq!(
            serde_json::to_value(&memory).unwrap(),
            serde_json::to_value(&stored).unwrap()
        );
        assert_eq!(stored.paused_until, Some(5000));
        assert_eq!(stored.headers[0].value, "abc");
    }

    /// Breaks one field, then checks the error it produces.
    type Case = (fn(&mut AppSettings), fn(&RegistryError) -> bool);

    #[tokio::test]
    async fn check_options_are_validated_and_normalized() {
        let registry = registry().await;
        let mut good = settings("Db", "", "tcp://db.example.com:5432");
        good.interval_secs = Some(30);
        good.expect_status = vec![401, 200, 401];
        good.headers = vec![CheckHeader::parse_line("X-Token: abc").unwrap()];
        good.alert_webhooks = vec![
            " https://hooks.slack.com/services/T/B/x ".into(),
            String::new(),
        ];
        good.alert_mentions = vec!["U0TEAM".into()];
        let slug = registry.add(good).await.unwrap();
        let app = registry.find(&slug).await.unwrap();
        assert_eq!(app.expect_status, [200, 401]);
        assert_eq!(
            app.alert_webhooks,
            ["https://hooks.slack.com/services/T/B/x"]
        );
        assert_eq!(app.alert_route().mentions, ["U0TEAM"]);

        let cases: [Case; 5] = [
            (
                |s| s.interval_secs = Some(5),
                |e| matches!(e, RegistryError::IntervalOutOfRange),
            ),
            (
                |s| s.timeout_secs = Some(600),
                |e| matches!(e, RegistryError::TimeoutOutOfRange),
            ),
            (
                |s| s.expect_status = vec![42],
                |e| matches!(e, RegistryError::InvalidStatus(42)),
            ),
            (
                |s| s.alert_webhooks = vec!["http://hooks.example.com/x".into()],
                |e| matches!(e, RegistryError::InvalidWebhook(_)),
            ),
            (
                |s| s.alert_mentions = vec!["@someone".into()],
                |e| matches!(e, RegistryError::InvalidMention(_)),
            ),
        ];
        for (i, (break_it, expected)) in cases.into_iter().enumerate() {
            let mut bad = settings(&format!("Bad {i}"), "", "https://x.example.com/health");
            break_it(&mut bad);
            let err = registry.add(bad).await.unwrap_err();
            assert!(expected(&err), "case {i}: {err}");
        }
    }

    #[test]
    fn headers_parse_from_lines_and_reserved_ones_are_refused() {
        let h = CheckHeader::parse_line("Authorization:  Bearer x ").unwrap();
        assert_eq!(
            (h.name.as_str(), h.value.as_str()),
            ("Authorization", "Bearer x")
        );
        for bad in [
            "no colon",
            "Host: evil.example.com",
            "Bad Name: x",
            "Connection: close",
        ] {
            assert!(CheckHeader::parse_line(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn scheduled_pauses_end_by_themselves() {
        let registry = registry().await;
        let a = registry
            .add(settings("A", "", "https://x/health"))
            .await
            .unwrap();
        let b = registry
            .add(settings("B", "", "https://y/health"))
            .await
            .unwrap();
        registry.set_paused(&a, true, Some(1000)).await.unwrap();
        registry.set_paused(&b, true, None).await.unwrap();
        let paused_at = registry.find(&b).await.unwrap().paused_at;
        assert!(paused_at.is_some());

        assert_eq!(
            registry.resume_expired(999).await.unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(registry.resume_expired(1000).await.unwrap(), [a.as_str()]);
        let resumed = registry.find(&a).await.unwrap();
        assert!(!resumed.paused && resumed.paused_until.is_none() && resumed.paused_at.is_none());
        assert!(
            registry.find(&b).await.unwrap().paused,
            "indefinite pauses stay"
        );

        registry.set_paused(&b, true, Some(5000)).await.unwrap();
        assert_eq!(
            registry.find(&b).await.unwrap().paused_at,
            paused_at,
            "rescheduling keeps when the pause began"
        );
    }
}
