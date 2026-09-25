//! Status-change notifications. Each configured webhook gets a payload in the shape its
//! service expects: Slack incoming webhooks (`{"text"}`), Discord webhooks (`{"content"}`),
//! or, for anything else, a structured JSON event.
//!
//! Webhook URLs are operator-configured (env), not admin-entered data, so they don't go
//! through the `outbound` allowlist -- they're expected to point at third-party services.

use reqwest::{Client, Url};
use serde::Serialize;
use std::time::Duration;

use crate::uptime::{Heartbeat, Status};

const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
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
    /// `PUBLIC_URL`, for linking the alert to the app's dashboard page.
    public_url: Option<String>,
}

impl Alerter {
    /// `None` when no webhook is configured: there's nothing to send to.
    pub fn new(
        webhook_urls: &[String],
        on_degraded: bool,
        public_url: Option<String>,
    ) -> Result<Option<Self>, reqwest::Error> {
        let webhooks: Vec<(Url, Flavor)> = webhook_urls
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
        let client = Client::builder()
            .timeout(WEBHOOK_TIMEOUT)
            .user_agent("heartbeat")
            .build()?;
        Ok(Some(Self {
            client,
            webhooks,
            on_degraded,
            public_url: public_url.map(|u| u.trim_end_matches('/').to_string()),
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

    fn text(&self, change: &StatusChange) -> String {
        let latency = change
            .beat
            .latency_ms
            .map(|ms| format!(" ({ms} ms)"))
            .unwrap_or_default();
        let headline = match (self.severity(change.from), self.severity(change.to)) {
            (_, Severity::Down) => {
                format!("🔴 {} está caída: {}", change.name, change.beat.message)
            }
            (_, Severity::Degraded) => format!("🟡 {} está degradada{latency}", change.name),
            (Severity::Down, Severity::Ok) => format!("🟢 {} se recuperó{latency}", change.name),
            (_, Severity::Ok) => format!("🟢 {} volvió a la normalidad{latency}", change.name),
        };
        match self.dashboard_url(&change.slug) {
            Some(url) => format!("{headline}\n{url}"),
            None => headline,
        }
    }

    fn dashboard_url(&self, slug: &str) -> Option<String> {
        self.public_url
            .as_ref()
            .map(|base| format!("{base}/#{slug}"))
    }

    fn payload(&self, flavor: Flavor, change: &StatusChange) -> serde_json::Value {
        match flavor {
            Flavor::Slack => serde_json::json!({ "text": self.text(change) }),
            Flavor::Discord => serde_json::json!({ "content": self.text(change) }),
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

    fn alerter(on_degraded: bool) -> Alerter {
        Alerter::new(
            &[
                "https://hooks.slack.com/services/T/B/x".to_string(),
                "https://discord.com/api/webhooks/1/x".to_string(),
                "https://example.com/hook".to_string(),
                "not a url".to_string(),
            ],
            on_degraded,
            Some("https://status.example.com/".to_string()),
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn detects_the_webhook_flavor_and_skips_invalid_urls() {
        let flavors: Vec<Flavor> = alerter(false).webhooks.iter().map(|(_, f)| *f).collect();
        assert_eq!(flavors, [Flavor::Slack, Flavor::Discord, Flavor::Generic]);
        assert!(Alerter::new(&[], false, None).unwrap().is_none());
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
            a.text(&change(Status::Down, Status::Up))
                .starts_with("🟢 Billing API se recuperó")
        );
    }
}
