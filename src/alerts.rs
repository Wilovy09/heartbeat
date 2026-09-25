//! Status-change notifications. Each configured webhook gets a payload in the shape its
//! service expects: Slack incoming webhooks (`{"text"}`), Discord webhooks (`{"content"}`),
//! or, for anything else, a structured JSON event.
//!
//! Webhook URLs are operator-configured (env), not admin-entered data, so they don't go
//! through the `outbound` allowlist -- they're expected to point at third-party services.
//!
//! `ALERT_MENTIONS` pings people when an app goes down (never on recovery), written once
//! and rendered in each service's own syntax.

use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

use crate::alert_templates::{self, TemplateKind, TemplateStore, TemplateVars};
use crate::uptime::{Heartbeat, Status};
use std::sync::Arc;

const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Alert configuration, straight from the environment.
#[derive(Debug, Clone, Default)]
pub struct AlertSettings {
    pub webhook_urls: Vec<String>,
    pub on_degraded: bool,
    /// `PUBLIC_URL`, for linking an alert to the app's dashboard page.
    pub public_url: Option<String>,
    /// `ALERT_MENTIONS` entries (see `Mention`).
    pub mentions: Vec<String>,
    /// Editable message templates; `None` = Spanish defaults, nothing persisted (tests).
    pub templates: Option<Arc<TemplateStore>>,
}

/// How serious a status is for alerting. With `on_degraded` off, degraded counts as fine,
/// so up <-> degraded flapping stays quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Ok,
    Degraded,
    Down,
}

/// One state change worth telling someone about.
#[derive(Debug, Clone)]
pub struct StatusChange {
    pub slug: String,
    pub name: String,
    pub from: Status,
    pub to: Status,
    pub beat: Heartbeat,
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
    /// `ALERT_MENTIONS` entries, only on a change to down.
    mentions: Vec<String>,
    /// The message rendered from the template, as Slack/Discord would show it.
    text: String,
}

#[derive(Serialize)]
struct GenericApp<'a> {
    slug: &'a str,
    name: &'a str,
}

#[derive(Clone)]
pub struct Alerter {
    client: Client,
    webhooks: Vec<(Url, Flavor)>,
    on_degraded: bool,
    public_url: Option<String>,
    mentions: Vec<Mention>,
    /// `ALERT_MENTIONS` entries that didn't parse, shown on the settings page.
    rejected_mentions: Vec<String>,
    templates: Arc<TemplateStore>,
}

/// Which sample the settings page sends: a plain connectivity check, or one of the real
/// alert messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TestKind {
    Ping,
    Down,
    Degraded,
    Recovered,
}

/// One webhook as the settings page shows it -- never the full URL, which is a secret.
#[derive(Debug, Serialize)]
pub struct WebhookInfo {
    pub index: usize,
    pub service: Flavor,
    pub masked: String,
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
fn mask(url: &Url) -> String {
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

impl Alerter {
    /// `None` when no webhook is configured: there's nothing to send to. Invalid webhook
    /// URLs and mention entries are logged and skipped rather than failing startup.
    pub fn new(settings: AlertSettings) -> Result<Option<Self>, reqwest::Error> {
        let webhooks: Vec<(Url, Flavor)> = settings
            .webhook_urls
            .iter()
            .filter_map(|raw| match Url::parse(raw.trim()) {
                Ok(url) => {
                    let flavor = Flavor::from(&url);
                    Some((url, flavor))
                }
                Err(e) => {
                    tracing::error!(error = %e, "alerts: ignoring an invalid webhook URL");
                    None
                }
            })
            .collect();
        if webhooks.is_empty() {
            return Ok(None);
        }
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
        Ok(Some(Self {
            client,
            webhooks,
            on_degraded: settings.on_degraded,
            public_url: settings
                .public_url
                .map(|u| u.trim_end_matches('/').to_string()),
            mentions,
            rejected_mentions,
            templates: settings.templates.unwrap_or_else(|| {
                Arc::new(TemplateStore::in_memory(&crate::i18n::I18n::new(
                    crate::i18n::Lang::Es,
                )))
            }),
        }))
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

    /// Mentions only page on a change to down.
    fn pages(&self, change: &StatusChange) -> bool {
        self.severity(change.to) == Severity::Down
    }

    fn kind(&self, change: &StatusChange) -> TemplateKind {
        match self.severity(change.to) {
            Severity::Down => TemplateKind::Down,
            Severity::Degraded => TemplateKind::Degraded,
            Severity::Ok => TemplateKind::Recovered,
        }
    }

    fn vars(&self, flavor: Flavor, change: &StatusChange) -> TemplateVars {
        let mentions: Vec<String> = if self.pages(change) {
            self.mentions
                .iter()
                .filter_map(|m| m.render(flavor))
                .collect()
        } else {
            Vec::new()
        };
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
                event: "status_change",
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
                mentions: if self.pages(change) {
                    self.mentions.iter().map(Mention::raw).collect()
                } else {
                    Vec::new()
                },
                text: self.text(flavor, change),
            })
            .unwrap_or_default(),
        }
    }

    /// Sends to every webhook concurrently, in the background: a slow or dead webhook
    /// never delays the next round of checks.
    pub fn send(&self, change: &StatusChange) {
        for (url, flavor) in &self.webhooks {
            let request = self
                .client
                .post(url.clone())
                .json(&self.payload(*flavor, change));
            let host = url.host_str().unwrap_or_default().to_string();
            let slug = change.slug.clone();
            tokio::spawn(async move {
                match request
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status)
                {
                    Ok(_) => tracing::info!(app = %slug, %host, "alerts: webhook delivered"),
                    Err(e) => {
                        tracing::error!(app = %slug, %host, error = %e, "alerts: webhook failed");
                    }
                }
            });
        }
    }
    /// A sample change for `kind`, marked as a test so nobody mistakes it for an outage.
    /// `None` for `Ping`, which isn't a status change.
    fn sample(kind: TestKind) -> Option<StatusChange> {
        let (from, to, latency_ms, message) = match kind {
            TestKind::Ping => return None,
            TestKind::Down => (
                Status::Up,
                Status::Down,
                None,
                "HTTP 503 Service Unavailable",
            ),
            TestKind::Degraded => (Status::Up, Status::Degraded, Some(2100), "HTTP 200 OK"),
            TestKind::Recovered => (Status::Down, Status::Up, Some(180), "HTTP 200 OK"),
        };
        Some(StatusChange {
            slug: String::new(),
            name: "[Prueba] Heartbeat".to_string(),
            from,
            to,
            beat: Heartbeat {
                at: crate::uptime::unix_now(),
                status: to,
                latency_ms,
                message: message.to_string(),
            },
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
        let text = match &self.public_url {
            Some(url) => format!("🏓 Ping de Heartbeat: las alertas llegan a este canal.\n{url}/"),
            None => "🏓 Ping de Heartbeat: las alertas llegan a este canal.".to_string(),
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
                .map(|(index, (url, service))| WebhookInfo {
                    index,
                    service: *service,
                    masked: mask(url),
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
            templates: TemplateKind::ALL
                .into_iter()
                .filter_map(|kind| {
                    let test_kind = match kind {
                        TemplateKind::Down => TestKind::Down,
                        TemplateKind::Degraded => TestKind::Degraded,
                        TemplateKind::Recovered => TestKind::Recovered,
                    };
                    Self::sample(test_kind).map(|change| TemplateInfo {
                        kind,
                        template: self.templates.effective(kind),
                        default: self.templates.default_for(kind),
                        custom: self.templates.is_custom(kind),
                        sample: samples.vars(Flavor::Slack, &change),
                    })
                })
                .collect(),
            previews: [TestKind::Down, TestKind::Degraded, TestKind::Recovered]
                .into_iter()
                .filter_map(|kind| {
                    Self::sample(kind).map(|change| AlertPreview {
                        kind,
                        text: samples.text(Flavor::Slack, &change),
                    })
                })
                .collect(),
        }
    }

    /// Sends a test message of `kind` to one webhook (`Some(index)`) or all of them, and
    /// waits for each answer -- unlike real alerts, the caller wants the result.
    pub async fn test(&self, target: Option<usize>, kind: TestKind) -> Vec<TestOutcome> {
        let samples = self.for_samples();
        let change = Self::sample(kind);
        let mut outcomes = Vec::new();
        for (index, (url, flavor)) in self.webhooks.iter().enumerate() {
            if target.is_some_and(|t| t != index) {
                continue;
            }
            let payload = match &change {
                Some(change) => samples.payload(*flavor, change),
                None => self.ping_payload(*flavor),
            };
            let started = Instant::now();
            let result = self.client.post(url.clone()).json(&payload).send().await;
            let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
            let outcome = match result {
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    TestOutcome {
                        index,
                        service: *flavor,
                        ok: status.is_success(),
                        status: Some(status.as_u16()),
                        latency_ms,
                        // Slack answers "ok"; a failure explains itself in the body.
                        detail: body.chars().take(200).collect(),
                    }
                }
                Err(e) => TestOutcome {
                    index,
                    service: *flavor,
                    ok: false,
                    status: None,
                    latency_ms,
                    detail: e.to_string(),
                },
            };
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
        })
        .unwrap()
        .unwrap()
    }

    fn alerter(on_degraded: bool) -> Alerter {
        alerter_with(on_degraded, &[])
    }

    #[test]
    fn detects_the_webhook_flavor_and_skips_invalid_urls() {
        let flavors: Vec<Flavor> = alerter(false).webhooks.iter().map(|(_, f)| *f).collect();
        assert_eq!(flavors, [Flavor::Slack, Flavor::Discord, Flavor::Generic]);
        assert!(Alerter::new(AlertSettings::default()).unwrap().is_none());
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
            [TestKind::Down, TestKind::Degraded, TestKind::Recovered]
        );
        assert!(
            overview.previews[0]
                .text
                .starts_with("<@U0123ABCD> 🔴 [Prueba]")
        );
        assert!(
            overview.previews[1].text.starts_with("🟡"),
            "degraded preview even when off"
        );
        assert!(overview.previews[2].text.starts_with("🟢"));
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
        .unwrap()
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
        .unwrap()
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
}
