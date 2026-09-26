//! Status-change notifications. Each configured webhook gets a payload in the shape its
//! service expects: Slack incoming webhooks (`{"text"}`), Discord webhooks (`{"content"}`),
//! or, for anything else, a structured JSON event.
//!
//! Global webhooks (`ALERT_WEBHOOK_URLS`) are operator-configured, so they're trusted and
//! don't go through the `outbound` policy. Per-app webhooks are admin-entered: they must
//! be https and are sent with `outbound::public_only_client`, which refuses private
//! addresses. Every app alerts its own webhooks *and* the global ones.
//!
//! Delivery is retried with backoff on network errors, 5xx and 429, and the last outcome
//! of each webhook is kept for the settings page. While an app stays down, a reminder is
//! sent every `ALERT_REMIND_MINS`.
//!
//! `ALERT_MENTIONS` (plus each app's own mentions) pings people when an app goes down
//! (never on recovery), written once and rendered in each service's own syntax.

use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::alert_templates::{self, TemplateKind, TemplateStore, TemplateVars};
use crate::i18n::{I18n, Lang};
use crate::uptime::{Heartbeat, Status, unix_now};

const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);
/// Waits between delivery attempts: a failed alert is tried up to four times over ~42 s.
#[cfg(not(test))]
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
#[cfg(test)]
const RETRY_DELAYS: [Duration; 3] = [Duration::from_millis(10); 3];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Flavor {
    Slack,
    Discord,
    Generic,
}

impl From<&Url> for Flavor {
    fn from(url: &Url) -> Self {
        match url.host_str() {
            Some("hooks.slack.com") => Self::Slack,
            Some("discord.com" | "discordapp.com") if url.path().starts_with("/api/webhooks/") => {
                Self::Discord
            }
            _ => Self::Generic,
        }
    }
}

/// Where a webhook comes from, which decides how far it's trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// `ALERT_WEBHOOK_URLS`: set by whoever runs the server.
    Env,
    /// An app's own webhooks, entered from /apps.
    App,
}

#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error("invalid webhook URL: {0}")]
    Invalid(#[from] url::ParseError),
    #[error("webhook URLs must use https://")]
    NotHttps,
}

/// One destination for alerts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Webhook {
    url: Url,
    flavor: Flavor,
    origin: Origin,
}

impl Webhook {
    fn new(url: Url, origin: Origin) -> Self {
        let flavor = Flavor::from(&url);
        Self {
            url,
            flavor,
            origin,
        }
    }

    /// An app's webhook, as entered from /apps: https only.
    pub fn for_app(raw: &str) -> Result<Self, WebhookError> {
        let url = Url::parse(raw.trim())?;
        if url.scheme() != "https" {
            return Err(WebhookError::NotHttps);
        }
        Ok(Self::new(url, Origin::App))
    }

    /// Transient failures worth another attempt; a 4xx other than 429 won't fix itself.
    fn retryable(error: &reqwest::Error) -> bool {
        error
            .status()
            .is_none_or(|s| s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS)
    }
}

/// Someone to ping when an app goes down, parsed from one `ALERT_MENTIONS` entry:
///
/// | entry            | Slack                | Discord      |
/// |------------------|----------------------|--------------|
/// | `here`           | `<!here>`            | `@here`      |
/// | `channel`        | `<!channel>`         | `@everyone`  |
/// | `U0123ABCD`      | `<@U0123ABCD>` user  | --           |
/// | `S0123ABCD`      | `<!subteam^S0123ABCD>` user group | -- |
/// | `123456789012`   | --                   | `<@…>` user  |
/// | `&123456789012`  | --                   | `<@&…>` role |
///
/// An entry that doesn't apply to a service is left out of that service's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mention {
    Here,
    Channel,
    SlackUser(String),
    SlackGroup(String),
    DiscordUser(String),
    DiscordRole(String),
}

impl std::str::FromStr for Mention {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let entry = raw.trim().trim_start_matches('@');
        let is_slack_id = |id: &str| {
            id.len() >= 3
                && id
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        };
        let is_snowflake = |id: &str| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit());
        match entry {
            "here" => Ok(Self::Here),
            "channel" | "everyone" => Ok(Self::Channel),
            id if (id.starts_with('U') || id.starts_with('W')) && is_slack_id(id) => {
                Ok(Self::SlackUser(id.to_string()))
            }
            id if id.starts_with('S') && is_slack_id(id) => Ok(Self::SlackGroup(id.to_string())),
            id if is_snowflake(id) => Ok(Self::DiscordUser(id.to_string())),
            id if id.strip_prefix('&').is_some_and(is_snowflake) => {
                Ok(Self::DiscordRole(id[1..].to_string()))
            }
            _ => Err(raw.trim().to_string()),
        }
    }
}

impl Mention {
    fn render(&self, flavor: Flavor) -> Option<String> {
        match (flavor, self) {
            (Flavor::Slack, Self::Here) => Some("<!here>".into()),
            (Flavor::Slack, Self::Channel) => Some("<!channel>".into()),
            (Flavor::Slack, Self::SlackUser(id)) | (Flavor::Discord, Self::DiscordUser(id)) => {
                Some(format!("<@{id}>"))
            }
            (Flavor::Slack, Self::SlackGroup(id)) => Some(format!("<!subteam^{id}>")),
            (Flavor::Discord, Self::Here) => Some("@here".into()),
            (Flavor::Discord, Self::Channel) => Some("@everyone".into()),
            (Flavor::Discord, Self::DiscordRole(id)) => Some(format!("<@&{id}>")),
            (Flavor::Slack, Self::DiscordUser(_) | Self::DiscordRole(_))
            | (Flavor::Discord, Self::SlackUser(_) | Self::SlackGroup(_))
            | (Flavor::Generic, _) => None,
        }
    }

    /// The entry as configured, for the generic JSON payload.
    fn raw(&self) -> String {
        match self {
            Self::Here => "here".into(),
            Self::Channel => "channel".into(),
            Self::SlackUser(id) | Self::SlackGroup(id) | Self::DiscordUser(id) => id.clone(),
            Self::DiscordRole(id) => format!("&{id}"),
        }
    }
}

/// An app's own alert destinations, added to the global ones. Entries were validated when
/// saved; anything that no longer parses is skipped with a warning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppRoute {
    pub webhooks: Vec<String>,
    pub mentions: Vec<String>,
}

impl AppRoute {
    fn webhooks(&self) -> impl Iterator<Item = Webhook> + '_ {
        self.webhooks
            .iter()
            .filter_map(|raw| match Webhook::for_app(raw) {
                Ok(hook) => Some(hook),
                Err(e) => {
                    tracing::warn!(error = %e, "alerts: skipping an invalid app webhook");
                    None
                }
            })
    }

    fn mentions(&self) -> impl Iterator<Item = Mention> + '_ {
        self.mentions.iter().filter_map(|raw| raw.parse().ok())
    }
}

/// Alert configuration, straight from the environment.
#[derive(Debug, Clone, Default)]
pub struct AlertSettings {
    pub webhook_urls: Vec<String>,
    pub on_degraded: bool,
    /// `PUBLIC_URL`, for linking an alert to the app's dashboard page.
    pub public_url: Option<String>,
    /// `ALERT_MENTIONS` entries (see `Mention`).
    pub mentions: Vec<String>,
    /// Editable message templates; `None` = defaults for `lang`, nothing persisted (tests).
    pub templates: Option<Arc<TemplateStore>>,
    /// `ALERT_REMIND_MINS`: remind every so often while an app stays down; `None` = never.
    pub remind_after: Option<Duration>,
    /// Language of the fixed messages (pings, mass outages).
    pub lang: Lang,
}

/// How serious a status is for alerting. With `on_degraded` off, degraded counts as fine,
/// so up <-> degraded flapping stays quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Ok,
    Degraded,
    Down,
}

/// One state change worth telling someone about. `from == to == Down` is a reminder that
/// the app is still down.
#[derive(Debug, Clone)]
pub struct StatusChange {
    pub slug: String,
    pub name: String,
    pub from: Status,
    pub to: Status,
    pub beat: Heartbeat,
    /// When the current (or just ended) outage started, for `{duration}`.
    pub down_since: Option<u64>,
    pub route: AppRoute,
}

impl StatusChange {
    #[must_use]
    pub fn is_reminder(&self) -> bool {
        self.from == Status::Down && self.to == Status::Down
    }
}

/// Many apps failing at once: most likely Heartbeat's own network, not the apps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MassOutage {
    Started { down: usize, total: usize },
    Ended { total: usize },
}

#[derive(Serialize)]
struct GenericEvent<'a> {
    event: &'static str,
    app: GenericApp<'a>,
    from: Status,
    to: Status,
    at: u64,
    latency_ms: Option<u32>,
    message: &'a str,
    url: Option<String>,
    /// Seconds since the outage started, on reminders and recoveries.
    down_for_secs: Option<u64>,
    /// Mention entries, only on a change to down (and reminders).
    mentions: Vec<String>,
    /// The message rendered from the template, as Slack/Discord would show it.
    text: String,
}

#[derive(Serialize)]
struct GenericApp<'a> {
    slug: &'a str,
    name: &'a str,
}

/// The last delivery attempt to one webhook (real alerts and tests alike).
#[derive(Debug, Clone, Serialize)]
pub struct Delivery {
    pub at: u64,
    pub ok: bool,
    /// HTTP status or the network error; empty on success.
    pub detail: String,
}

#[derive(Clone)]
pub struct Alerter {
    /// For `ALERT_WEBHOOK_URLS` (trusted).
    client: Client,
    /// For admin-entered app webhooks (public addresses only, no redirects).
    app_client: Client,
    webhooks: Vec<Webhook>,
    on_degraded: bool,
    public_url: Option<String>,
    mentions: Vec<Mention>,
    /// `ALERT_MENTIONS` entries that didn't parse, shown on the settings page.
    rejected_mentions: Vec<String>,
    templates: Arc<TemplateStore>,
    remind_after: Option<Duration>,
    i18n: I18n,
    /// Last delivery per webhook URL.
    deliveries: Arc<Mutex<HashMap<String, Delivery>>>,
}

/// Which sample the settings page sends: a plain connectivity check, or one of the real
/// alert messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TestKind {
    Ping,
    Down,
    Reminder,
    Degraded,
    Recovered,
}

impl From<TemplateKind> for TestKind {
    fn from(kind: TemplateKind) -> Self {
        match kind {
            TemplateKind::Down => Self::Down,
            TemplateKind::Reminder => Self::Reminder,
            TemplateKind::Degraded => Self::Degraded,
            TemplateKind::Recovered => Self::Recovered,
        }
    }
}

/// One webhook as the settings page shows it -- never the full URL, which is a secret.
#[derive(Debug, Serialize)]
pub struct WebhookInfo {
    pub index: usize,
    pub service: Flavor,
    pub masked: String,
    pub last: Option<Delivery>,
}

#[derive(Debug, Serialize)]
pub struct MentionInfo {
    pub raw: String,
    pub slack: Option<String>,
    pub discord: Option<String>,
}

/// One editable template as the settings page shows it.
#[derive(Debug, Serialize)]
pub struct TemplateInfo {
    pub kind: TemplateKind,
    pub template: String,
    pub default: String,
    pub custom: bool,
    /// Sample values (Slack-flavored mentions) for the live preview.
    pub sample: TemplateVars,
}

/// Everything the settings page shows about alerting.
#[derive(Debug, Serialize)]
pub struct AlertOverview {
    pub webhooks: Vec<WebhookInfo>,
    pub mentions: Vec<MentionInfo>,
    pub rejected_mentions: Vec<String>,
    pub on_degraded: bool,
    pub remind_mins: Option<u64>,
    /// Slack-flavored preview of each alert kind, exactly as it would be sent.
    pub previews: Vec<AlertPreview>,
    pub templates: Vec<TemplateInfo>,
}

#[derive(Debug, Serialize)]
pub struct AlertPreview {
    pub kind: TestKind,
    pub text: String,
}

/// Result of one test send ("pong").
#[derive(Debug, Serialize)]
pub struct TestOutcome {
    pub index: usize,
    pub service: Flavor,
    pub ok: bool,
    pub status: Option<u16>,
    pub latency_ms: u32,
    pub detail: String,
}

/// `https://example.com/services/T000…/B000…/…abcd`: enough to tell webhooks apart,
/// not enough to use one.
#[must_use]
pub fn mask(url: &Url) -> String {
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    let masked: Vec<String> = segments
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            let chars: Vec<char> = seg.chars().collect();
            if i + 1 == segments.len() && chars.len() > 4 {
                format!("…{}", chars[chars.len() - 4..].iter().collect::<String>())
            } else if chars.len() > 8 {
                format!("{}…", chars[..4].iter().collect::<String>())
            } else {
                (*seg).to_string()
            }
        })
        .collect();
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!(
        "{}://{}{port}/{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        masked.join("/")
    )
}

/// `45 s`, `12 min`, `3 h 20 min`, `2 d 4 h`: how long an outage has lasted.
#[must_use]
pub fn format_duration(secs: u64) -> String {
    let (days, hours, mins) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (days, hours, mins) {
        (0, 0, 0) => format!("{secs} s"),
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, 0, _) => format!("{d} d"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

impl Alerter {
    /// Invalid webhook URLs and mention entries are logged and skipped rather than failing
    /// startup. With no global webhook, only apps with their own webhooks alert.
    pub fn new(settings: AlertSettings) -> Result<Self, reqwest::Error> {
        let webhooks: Vec<Webhook> = settings
            .webhook_urls
            .iter()
            .filter_map(|raw| match Url::parse(raw.trim()) {
                Ok(url) => Some(Webhook::new(url, Origin::Env)),
                Err(e) => {
                    tracing::error!(error = %e, "alerts: ignoring an invalid webhook URL");
                    None
                }
            })
            .collect();
        let mut mentions = Vec::new();
        let mut rejected_mentions = Vec::new();
        for raw in &settings.mentions {
            match raw.parse::<Mention>() {
                Ok(mention) => mentions.push(mention),
                Err(entry) => {
                    tracing::error!(%entry, "alerts: ignoring an unrecognized ALERT_MENTIONS entry");
                    rejected_mentions.push(entry);
                }
            }
        }
        let client = Client::builder()
            .timeout(WEBHOOK_TIMEOUT)
            .user_agent("heartbeat")
            .build()?;
        let i18n = I18n::new(settings.lang);
        Ok(Self {
            client,
            app_client: crate::outbound::public_only_client(WEBHOOK_TIMEOUT)?,
            webhooks,
            on_degraded: settings.on_degraded,
            public_url: settings
                .public_url
                .map(|u| u.trim_end_matches('/').to_string()),
            mentions,
            rejected_mentions,
            templates: settings
                .templates
                .unwrap_or_else(|| Arc::new(TemplateStore::in_memory(&i18n))),
            remind_after: settings.remind_after.filter(|d| !d.is_zero()),
            i18n,
            deliveries: Arc::default(),
        })
    }

    /// Whether any global webhook is configured (apps may still have their own).
    #[must_use]
    pub fn has_global_webhooks(&self) -> bool {
        !self.webhooks.is_empty()
    }

    #[must_use]
    pub fn remind_after(&self) -> Option<Duration> {
        self.remind_after
    }

    fn severity(&self, status: Status) -> Severity {
        match status {
            Status::Down => Severity::Down,
            Status::Degraded if self.on_degraded => Severity::Degraded,
            Status::Up | Status::Degraded => Severity::Ok,
        }
    }

    /// Whether going `from` -> `to` deserves an alert.
    #[must_use]
    pub fn worth_alerting(&self, from: Status, to: Status) -> bool {
        self.severity(from) != self.severity(to)
    }

    /// Mentions only page on a change to down, and on reminders.
    fn pages(&self, change: &StatusChange) -> bool {
        self.severity(change.to) == Severity::Down
    }

    fn kind(&self, change: &StatusChange) -> TemplateKind {
        match self.severity(change.to) {
            Severity::Down if change.is_reminder() => TemplateKind::Reminder,
            Severity::Down => TemplateKind::Down,
            Severity::Degraded => TemplateKind::Degraded,
            Severity::Ok => TemplateKind::Recovered,
        }
    }

    /// Global mentions plus the app's own, without duplicates.
    fn mentions_for(&self, change: &StatusChange) -> Vec<Mention> {
        if !self.pages(change) {
            return Vec::new();
        }
        let mut all = self.mentions.clone();
        for mention in change.route.mentions() {
            if !all.contains(&mention) {
                all.push(mention);
            }
        }
        all
    }

    fn vars(&self, flavor: Flavor, change: &StatusChange) -> TemplateVars {
        let mentions: Vec<String> = self
            .mentions_for(change)
            .iter()
            .filter_map(|m| m.render(flavor))
            .collect();
        TemplateVars {
            app: change.name.clone(),
            message: change.beat.message.clone(),
            latency: change
                .beat
                .latency_ms
                .map(|ms| format!("{ms} ms"))
                .unwrap_or_default(),
            link: self.dashboard_url(&change.slug).unwrap_or_default(),
            mentions: mentions.join(" "),
            duration: change
                .down_since
                .map(|since| format_duration(change.beat.at.saturating_sub(since)))
                .unwrap_or_default(),
        }
    }

    fn text(&self, flavor: Flavor, change: &StatusChange) -> String {
        let template = self.templates.effective(self.kind(change));
        alert_templates::render(&template, &self.vars(flavor, change))
    }

    /// Where the settings page saves edited templates.
    #[must_use]
    pub fn templates(&self) -> &TemplateStore {
        &self.templates
    }

    fn dashboard_url(&self, slug: &str) -> Option<String> {
        self.public_url.as_ref().map(|base| {
            if slug.is_empty() {
                format!("{base}/")
            } else {
                format!("{base}/#{slug}")
            }
        })
    }

    fn payload(&self, flavor: Flavor, change: &StatusChange) -> serde_json::Value {
        match flavor {
            Flavor::Slack => serde_json::json!({ "text": self.text(flavor, change) }),
            Flavor::Discord => serde_json::json!({ "content": self.text(flavor, change) }),
            Flavor::Generic => serde_json::to_value(GenericEvent {
                event: if change.is_reminder() {
                    "still_down"
                } else {
                    "status_change"
                },
                app: GenericApp {
                    slug: &change.slug,
                    name: &change.name,
                },
                from: change.from,
                to: change.to,
                at: change.beat.at,
                latency_ms: change.beat.latency_ms,
                message: &change.beat.message,
                url: self.dashboard_url(&change.slug),
                down_for_secs: change
                    .down_since
                    .map(|since| change.beat.at.saturating_sub(since)),
                mentions: self.mentions_for(change).iter().map(Mention::raw).collect(),
                text: self.text(flavor, change),
            })
            .unwrap_or_default(),
        }
    }

    /// Global webhooks plus the app's own, without duplicates.
    fn destinations(&self, route: &AppRoute) -> Vec<Webhook> {
        let mut all = self.webhooks.clone();
        for hook in route.webhooks() {
            if !all.iter().any(|h| h.url == hook.url) {
                all.push(hook);
            }
        }
        all
    }

    fn client_for(&self, hook: &Webhook) -> &Client {
        match hook.origin {
            Origin::Env => &self.client,
            Origin::App => &self.app_client,
        }
    }

    fn record(&self, hook: &Webhook, delivery: Delivery) {
        self.deliveries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(hook.url.to_string(), delivery);
    }

    fn last_delivery(&self, hook: &Webhook) -> Option<Delivery> {
        self.deliveries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(hook.url.as_str())
            .cloned()
    }

    /// Posts `payload` to `hook` in the background, retrying transient failures
    /// (`RETRY_DELAYS`): a slow or dead webhook never delays the next round of checks.
    fn deliver(&self, hook: Webhook, payload: serde_json::Value, what: String) {
        let alerter = self.clone();
        tokio::spawn(async move {
            let host = hook.url.host_str().unwrap_or_default().to_string();
            let mut attempt = 0;
            let error = loop {
                let result = alerter
                    .client_for(&hook)
                    .post(hook.url.clone())
                    .json(&payload)
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status);
                match result {
                    Ok(_) => {
                        tracing::info!(%what, %host, attempt, "alerts: webhook delivered");
                        alerter.record(
                            &hook,
                            Delivery {
                                at: unix_now(),
                                ok: true,
                                detail: String::new(),
                            },
                        );
                        return;
                    }
                    Err(e) if Webhook::retryable(&e) && attempt < RETRY_DELAYS.len() => {
                        tracing::warn!(%what, %host, attempt, error = %e, "alerts: webhook failed, retrying");
                        tokio::time::sleep(RETRY_DELAYS[attempt]).await;
                        attempt += 1;
                    }
                    Err(e) => break e,
                }
            };
            tracing::error!(%what, %host, attempts = attempt + 1, error = %error, "alerts: webhook failed");
            alerter.record(
                &hook,
                Delivery {
                    at: unix_now(),
                    ok: false,
                    detail: error.to_string(),
                },
            );
        });
    }

    /// Sends `change` to every global webhook and the app's own.
    pub fn send(&self, change: &StatusChange) {
        for hook in self.destinations(&change.route) {
            let payload = self.payload(hook.flavor, change);
            self.deliver(hook, payload, change.slug.clone());
        }
    }

    /// Mass outage notices go to the global webhooks only: they're about Heartbeat, not
    /// about any one app.
    pub fn send_mass(&self, outage: MassOutage) {
        for hook in &self.webhooks {
            let payload = self.mass_payload(hook.flavor, outage);
            self.deliver(hook.clone(), payload, "mass-outage".to_string());
        }
    }

    fn mass_text(&self, flavor: Flavor, outage: MassOutage) -> String {
        let link = self.dashboard_url("").unwrap_or_default();
        let text = match outage {
            MassOutage::Started { down, total } => {
                let mentions: Vec<String> = self
                    .mentions
                    .iter()
                    .filter_map(|m| m.render(flavor))
                    .collect();
                self.i18n.text(
                    "alert.mass_down",
                    &[
                        ("mentions", &mentions.join(" ")),
                        ("down", &down.to_string()),
                        ("total", &total.to_string()),
                        ("link", &link),
                    ],
                )
            }
            MassOutage::Ended { total } => self.i18n.text(
                "alert.mass_recovered",
                &[("total", &total.to_string()), ("link", &link)],
            ),
        };
        alert_templates::render(&text, &TemplateVars::default())
    }

    fn mass_payload(&self, flavor: Flavor, outage: MassOutage) -> serde_json::Value {
        let text = self.mass_text(flavor, outage);
        match (flavor, outage) {
            (Flavor::Slack, _) => serde_json::json!({ "text": text }),
            (Flavor::Discord, _) => serde_json::json!({ "content": text }),
            (Flavor::Generic, MassOutage::Started { down, total }) => serde_json::json!({
                "event": "mass_outage_started",
                "down": down,
                "total": total,
                "at": unix_now(),
                "mentions": self.mentions.iter().map(Mention::raw).collect::<Vec<_>>(),
                "text": text,
            }),
            (Flavor::Generic, MassOutage::Ended { total }) => serde_json::json!({
                "event": "mass_outage_ended",
                "total": total,
                "at": unix_now(),
                "text": text,
            }),
        }
    }

    /// A sample change for `kind`, marked as a test so nobody mistakes it for an outage.
    /// `None` for `Ping`, which isn't a status change.
    fn sample(&self, kind: TestKind) -> Option<StatusChange> {
        let now = unix_now();
        let (from, to, latency_ms, message, down_for) = match kind {
            TestKind::Ping => return None,
            TestKind::Down => (
                Status::Up,
                Status::Down,
                None,
                "HTTP 503 Service Unavailable",
                None,
            ),
            TestKind::Reminder => (
                Status::Down,
                Status::Down,
                None,
                "HTTP 503 Service Unavailable",
                Some(25 * 60),
            ),
            TestKind::Degraded => (
                Status::Up,
                Status::Degraded,
                Some(2100),
                "HTTP 200 OK",
                None,
            ),
            TestKind::Recovered => (
                Status::Down,
                Status::Up,
                Some(180),
                "HTTP 200 OK",
                Some(420),
            ),
        };
        Some(StatusChange {
            slug: String::new(),
            name: self.i18n.text("alert.test_app", &[]),
            from,
            to,
            beat: Heartbeat {
                at: now,
                status: to,
                latency_ms,
                message: message.to_string(),
            },
            down_since: down_for.map(|secs: u64| now - secs),
            route: AppRoute::default(),
        })
    }

    /// Degraded samples are shown and sent even with `ALERT_ON_DEGRADED` off, so the
    /// message can be previewed before turning it on.
    fn for_samples(&self) -> Self {
        Self {
            on_degraded: true,
            ..self.clone()
        }
    }

    fn ping_payload(&self, flavor: Flavor) -> serde_json::Value {
        let base = self.i18n.text("alert.ping", &[]);
        let text = match &self.public_url {
            Some(url) => format!("{base}\n{url}/"),
            None => base,
        };
        match flavor {
            Flavor::Slack => serde_json::json!({ "text": text }),
            Flavor::Discord => serde_json::json!({ "content": text }),
            Flavor::Generic => serde_json::json!({ "event": "ping", "message": text }),
        }
    }

    #[must_use]
    pub fn overview(&self) -> AlertOverview {
        let samples = self.for_samples();
        AlertOverview {
            webhooks: self
                .webhooks
                .iter()
                .enumerate()
                .map(|(index, hook)| WebhookInfo {
                    index,
                    service: hook.flavor,
                    masked: mask(&hook.url),
                    last: self.last_delivery(hook),
                })
                .collect(),
            mentions: self
                .mentions
                .iter()
                .map(|m| MentionInfo {
                    raw: m.raw(),
                    slack: m.render(Flavor::Slack),
                    discord: m.render(Flavor::Discord),
                })
                .collect(),
            rejected_mentions: self.rejected_mentions.clone(),
            on_degraded: self.on_degraded,
            remind_mins: self.remind_after.map(|d| d.as_secs() / 60),
            templates: TemplateKind::ALL
                .into_iter()
                .filter_map(|kind| {
                    self.sample(kind.into()).map(|change| TemplateInfo {
                        kind,
                        template: self.templates.effective(kind),
                        default: self.templates.default_for(kind),
                        custom: self.templates.is_custom(kind),
                        sample: samples.vars(Flavor::Slack, &change),
                    })
                })
                .collect(),
            previews: TemplateKind::ALL
                .into_iter()
                .map(TestKind::from)
                .filter_map(|kind| {
                    self.sample(kind).map(|change| AlertPreview {
                        kind,
                        text: samples.text(Flavor::Slack, &change),
                    })
                })
                .collect(),
        }
    }

    /// Sends a test message of `kind` to one global webhook (`Some(index)`) or all of
    /// them, and waits for each answer -- unlike real alerts, the caller wants the result.
    pub async fn test(&self, target: Option<usize>, kind: TestKind) -> Vec<TestOutcome> {
        let hooks: Vec<(usize, &Webhook)> = self
            .webhooks
            .iter()
            .enumerate()
            .filter(|(index, _)| target.is_none_or(|t| t == *index))
            .collect();
        self.test_hooks(&hooks, kind, &AppRoute::default()).await
    }

    /// Pings (or sends a sample to) an app's own webhooks, with its own mentions.
    pub async fn test_app(&self, route: &AppRoute, kind: TestKind) -> Vec<TestOutcome> {
        let hooks: Vec<Webhook> = route.webhooks().collect();
        let indexed: Vec<(usize, &Webhook)> = hooks.iter().enumerate().collect();
        self.test_hooks(&indexed, kind, route).await
    }

    async fn test_hooks(
        &self,
        hooks: &[(usize, &Webhook)],
        kind: TestKind,
        route: &AppRoute,
    ) -> Vec<TestOutcome> {
        let samples = self.for_samples();
        let change = self.sample(kind).map(|c| StatusChange {
            route: route.clone(),
            ..c
        });
        let mut outcomes = Vec::new();
        for &(index, hook) in hooks {
            let payload = match &change {
                Some(change) => samples.payload(hook.flavor, change),
                None => self.ping_payload(hook.flavor),
            };
            let started = Instant::now();
            let result = self
                .client_for(hook)
                .post(hook.url.clone())
                .json(&payload)
                .send()
                .await;
            let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
            let outcome = match result {
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    TestOutcome {
                        index,
                        service: hook.flavor,
                        ok: status.is_success(),
                        status: Some(status.as_u16()),
                        latency_ms,
                        // Slack answers "ok"; a failure explains itself in the body.
                        detail: body.chars().take(200).collect(),
                    }
                }
                Err(e) => TestOutcome {
                    index,
                    service: hook.flavor,
                    ok: false,
                    status: None,
                    latency_ms,
                    detail: e.to_string(),
                },
            };
            self.record(
                hook,
                Delivery {
                    at: unix_now(),
                    ok: outcome.ok,
                    detail: if outcome.ok {
                        String::new()
                    } else {
                        outcome.detail.clone()
                    },
                },
            );
            tracing::info!(index, ok = outcome.ok, status = ?outcome.status, ?kind, "alerts: test sent");
            outcomes.push(outcome);
        }
        outcomes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(from: Status, to: Status) -> StatusChange {
        StatusChange {
            slug: "billing".to_string(),
            name: "Billing API".to_string(),
            from,
            to,
            beat: Heartbeat {
                at: 100,
                status: to,
                latency_ms: Some(1200),
                message: "HTTP 503 Service Unavailable".to_string(),
            },
            down_since: Some(40),
            route: AppRoute::default(),
        }
    }

    fn alerter_with(on_degraded: bool, mentions: &[&str]) -> Alerter {
        Alerter::new(AlertSettings {
            webhook_urls: vec![
                "https://hooks.slack.com/services/T/B/x".to_string(),
                "https://discord.com/api/webhooks/1/x".to_string(),
                "https://example.com/hook".to_string(),
                "not a url".to_string(),
            ],
            on_degraded,
            public_url: Some("https://status.example.com/".to_string()),
            mentions: mentions.iter().map(|m| (*m).to_string()).collect(),
            templates: None,
            ..AlertSettings::default()
        })
        .unwrap()
    }

    fn alerter(on_degraded: bool) -> Alerter {
        alerter_with(on_degraded, &[])
    }

    #[test]
    fn detects_the_webhook_flavor_and_skips_invalid_urls() {
        let flavors: Vec<Flavor> = alerter(false).webhooks.iter().map(|h| h.flavor).collect();
        assert_eq!(flavors, [Flavor::Slack, Flavor::Discord, Flavor::Generic]);
        assert!(
            !Alerter::new(AlertSettings::default())
                .unwrap()
                .has_global_webhooks()
        );
    }

    #[test]
    fn degraded_only_alerts_when_enabled() {
        let quiet = alerter(false);
        assert!(!quiet.worth_alerting(Status::Up, Status::Degraded));
        assert!(quiet.worth_alerting(Status::Degraded, Status::Down));
        assert!(quiet.worth_alerting(Status::Down, Status::Degraded));
        assert!(!quiet.worth_alerting(Status::Up, Status::Up));

        let loud = alerter(true);
        assert!(loud.worth_alerting(Status::Up, Status::Degraded));
    }

    #[test]
    fn payloads_match_each_service() {
        let a = alerter(false);
        let down = change(Status::Up, Status::Down);
        let slack = a.payload(Flavor::Slack, &down);
        assert_eq!(
            slack["text"],
            "🔴 Billing API está caída: HTTP 503 Service Unavailable\nhttps://status.example.com/#billing"
        );
        assert!(a.payload(Flavor::Discord, &down)["content"].is_string());

        let generic = a.payload(Flavor::Generic, &change(Status::Down, Status::Up));
        assert_eq!(generic["event"], "status_change");
        assert_eq!(generic["from"], "down");
        assert_eq!(generic["to"], "up");
        assert_eq!(generic["app"]["slug"], "billing");
        assert!(
            a.text(Flavor::Slack, &change(Status::Down, Status::Up))
                .starts_with("🟢 Billing API se recuperó")
        );
    }

    #[test]
    fn mention_entries_parse_to_the_right_kind() {
        let parsed: Vec<Mention> = [
            "here",
            "@channel",
            "U0123ABCD",
            "S02XYZ",
            "123456789",
            "&987654",
        ]
        .iter()
        .map(|e| e.parse().unwrap())
        .collect();
        assert_eq!(
            parsed,
            [
                Mention::Here,
                Mention::Channel,
                Mention::SlackUser("U0123ABCD".into()),
                Mention::SlackGroup("S02XYZ".into()),
                Mention::DiscordUser("123456789".into()),
                Mention::DiscordRole("987654".into()),
            ]
        );
        assert!(
            "@emiliano".parse::<Mention>().is_err(),
            "names can't be resolved, only IDs"
        );
    }

    #[test]
    fn mentions_page_only_on_down_in_each_services_syntax() {
        let a = alerter_with(
            false,
            &["U0123ABCD", "S02XYZ", "here", "123456789", "nope nope"],
        );
        assert_eq!(a.mentions.len(), 4, "the invalid entry is skipped");

        let down = change(Status::Up, Status::Down);
        let slack = a.payload(Flavor::Slack, &down)["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            slack.starts_with("<@U0123ABCD> <!subteam^S02XYZ> <!here> 🔴 Billing API"),
            "{slack}"
        );
        assert!(
            !slack.contains("123456789"),
            "Discord IDs stay out of Slack: {slack}"
        );

        let discord = a.payload(Flavor::Discord, &down)["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(discord.starts_with("@here <@123456789> 🔴"), "{discord}");

        let generic = a.payload(Flavor::Generic, &down);
        assert_eq!(generic["mentions"].as_array().unwrap().len(), 4);

        let recovered = change(Status::Down, Status::Up);
        let slack_ok = a.payload(Flavor::Slack, &recovered)["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            slack_ok.starts_with("🟢"),
            "recovery doesn't page: {slack_ok}"
        );
        assert_eq!(
            a.payload(Flavor::Generic, &recovered)["mentions"],
            serde_json::json!([])
        );
    }

    #[test]
    fn mask_keeps_webhooks_distinguishable_but_unusable() {
        let url = Url::parse(
            "https://example.com/services/T00000FAKE/B00000FAKE/XXXXXXXXXXXXXXXXXXXXabcd",
        )
        .unwrap();
        let masked = mask(&url);
        assert_eq!(masked, "https://example.com/services/T000…/B000…/…abcd");
        assert!(!masked.contains("XXXXXXXXXXXXXXXXXXXX"));
    }

    #[test]
    fn overview_previews_every_alert_kind_with_mentions_on_down() {
        let overview = alerter_with(false, &["U0123ABCD", "bogus entry"]).overview();
        assert_eq!(overview.webhooks.len(), 3);
        assert_eq!(overview.rejected_mentions, ["bogus entry"]);
        let kinds: Vec<TestKind> = overview.previews.iter().map(|p| p.kind).collect();
        assert_eq!(
            kinds,
            [
                TestKind::Down,
                TestKind::Reminder,
                TestKind::Degraded,
                TestKind::Recovered
            ]
        );
        assert!(
            overview.previews[0]
                .text
                .starts_with("<@U0123ABCD> 🔴 [Prueba]")
        );
        assert!(
            overview.previews[1].text.contains("sigue caída (25 min)"),
            "{}",
            overview.previews[1].text
        );
        assert!(
            overview.previews[2].text.starts_with("🟡"),
            "degraded preview even when off"
        );
        assert!(overview.previews[3].text.starts_with("🟢"));
    }

    /// A one-shot HTTP server that answers 200 "ok" and hands back the request it got.
    async fn fake_webhook() -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = socket.read(&mut buf).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn test_sends_report_the_webhooks_answer() {
        let (url, request) = fake_webhook().await;
        let alerter = Alerter::new(AlertSettings {
            webhook_urls: vec![url],
            mentions: vec!["here".to_string()],
            ..AlertSettings::default()
        })
        .unwrap();
        let outcomes = alerter.test(None, TestKind::Down).await;
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].ok, "{outcomes:?}");
        assert_eq!(outcomes[0].status, Some(200));
        assert_eq!(outcomes[0].detail, "ok");
        let sent = request.await.unwrap();
        assert!(sent.contains("\"event\":\"status_change\""), "{sent}");
        assert!(sent.contains("\"mentions\":[\"here\"]"), "{sent}");

        let none = alerter.test(Some(7), TestKind::Ping).await;
        assert!(none.is_empty(), "an out-of-range target sends nothing");
    }

    #[tokio::test]
    async fn real_alerts_use_the_saved_template() {
        let store = Arc::new(TemplateStore::in_memory(&crate::i18n::I18n::new(
            crate::i18n::Lang::En,
        )));
        let alerter = Alerter::new(AlertSettings {
            webhook_urls: vec!["https://hooks.slack.com/services/T/B/x".to_string()],
            mentions: vec!["here".to_string()],
            templates: Some(store.clone()),
            ..AlertSettings::default()
        })
        .unwrap();
        let down = change(Status::Up, Status::Down);
        assert!(
            alerter
                .text(Flavor::Slack, &down)
                .starts_with("<!here> 🔴 Billing API is down")
        );

        store
            .save(crate::alert_templates::AlertTemplates {
                down: Some("{mentions} CAÍDA {app} -> {message}".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            alerter.text(Flavor::Slack, &down),
            "<!here> CAÍDA Billing API -> HTTP 503 Service Unavailable"
        );
        let generic = alerter.payload(Flavor::Generic, &down);
        assert_eq!(
            generic["text"],
            "CAÍDA Billing API -> HTTP 503 Service Unavailable"
        );
    }

    /// Answers `failures` requests with 500, then 200; reports how many it got.
    async fn flaky_webhook(failures: usize) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 16 * 1024];
                let _ = socket.read(&mut buf).await;
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let reply: &[u8] = if n < failures {
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                };
                let _ = socket.write_all(reply).await;
            }
        });
        (url, hits)
    }

    async fn wait_for_delivery(alerter: &Alerter) -> Delivery {
        for _ in 0..200 {
            if let Some(last) = alerter.overview().webhooks[0].last.clone() {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no delivery recorded");
    }

    #[tokio::test]
    async fn transient_failures_are_retried_until_delivered() {
        let (url, hits) = flaky_webhook(2).await;
        let alerter = Alerter::new(AlertSettings {
            webhook_urls: vec![url],
            ..AlertSettings::default()
        })
        .unwrap();
        alerter.send(&change(Status::Up, Status::Down));
        let last = wait_for_delivery(&alerter).await;
        assert!(last.ok, "{last:?}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_webhook_that_keeps_failing_is_reported() {
        let (url, hits) = flaky_webhook(usize::MAX).await;
        let alerter = Alerter::new(AlertSettings {
            webhook_urls: vec![url],
            ..AlertSettings::default()
        })
        .unwrap();
        alerter.send(&change(Status::Up, Status::Down));
        let last = wait_for_delivery(&alerter).await;
        assert!(!last.ok && last.detail.contains("500"), "{last:?}");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            RETRY_DELAYS.len() + 1
        );
    }

    #[test]
    fn app_routes_add_their_own_webhooks_and_mentions() {
        let a = alerter_with(false, &["here"]);
        let mut down = change(Status::Up, Status::Down);
        down.route = AppRoute {
            webhooks: vec![
                "https://hooks.slack.com/services/T/B/x".into(), // already global
                "https://hooks.slack.com/services/T/B/team".into(),
                "http://insecure.example.com/hook".into(), // not https: skipped
            ],
            mentions: vec!["U0TEAM".into(), "here".into()],
        };
        assert_eq!(a.destinations(&down.route).len(), 4);
        let slack = a.text(Flavor::Slack, &down);
        assert!(slack.starts_with("<!here> <@U0TEAM> 🔴"), "{slack}");
    }

    #[test]
    fn reminders_and_recoveries_say_how_long_it_has_been_down() {
        let a = alerter(false);
        let mut still = change(Status::Down, Status::Down);
        still.beat.at = 10_000;
        still.down_since = Some(10_000 - 25 * 60);
        assert!(still.is_reminder());
        assert!(
            a.text(Flavor::Slack, &still)
                .contains("sigue caída (25 min)")
        );
        assert_eq!(a.payload(Flavor::Generic, &still)["event"], "still_down");
        assert_eq!(a.payload(Flavor::Generic, &still)["down_for_secs"], 25 * 60);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(45), "45 s");
        assert_eq!(format_duration(12 * 60), "12 min");
        assert_eq!(format_duration(3 * 3600), "3 h");
        assert_eq!(format_duration(3 * 3600 + 20 * 60), "3 h 20 min");
        assert_eq!(format_duration(2 * 86_400 + 4 * 3600 + 59), "2 d 4 h");
    }

    #[test]
    fn mass_outage_notices_are_short_and_page() {
        let a = alerter_with(false, &["here"]);
        let started = a.mass_text(Flavor::Slack, MassOutage::Started { down: 4, total: 5 });
        assert!(started.starts_with("<!here> 🌐 4 de 5 apps"), "{started}");
        let ended = a.mass_payload(Flavor::Generic, MassOutage::Ended { total: 5 });
        assert_eq!(ended["event"], "mass_outage_ended");
    }
}
