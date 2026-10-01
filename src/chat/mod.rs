//! Slash commands from Slack and Discord: `/pulse status [app]`, `pause <app> [30m]`,
//! `resume <app>` and `help`, answered in the channel they were typed in. Each service's
//! adapter (`slack`, `discord`) verifies the request's signature, turns it into a `Command`
//! and a `Caller`, and renders the `Reply`; everything in between is shared.
//!
//! Commands that change something (`pause`, `resume`) only run for the users and roles in
//! `CHAT_ADMINS`; reading the status is open to anyone who can type the command.

pub mod discord;
pub mod slack;

use std::fmt::Write as _;
use std::str::FromStr;

use crate::alerts::{Mention, format_duration};
use crate::audit::{AuditLog, Event, Outcome, Source};
use crate::i18n::I18n;
use crate::registry::{AppRegistry, RegisteredApp};
use crate::routes::apps::MAX_PAUSE_HOURS;
use crate::uptime::{MonitorSummary, Status, UptimeMonitor, unix_now};

/// Signed requests older (or newer) than this are refused, so a captured one can't be
/// replayed later.
const MAX_SIGNATURE_AGE_SECS: u64 = 300;
/// Discord refuses messages over 2000 characters; the app list stops short of it.
const MAX_REPLY_CHARS: usize = 1900;

/// Where a command came from, which decides the markup of the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Slack,
    Discord,
}

impl Service {
    fn bold(self, text: &str) -> String {
        match self {
            Self::Slack => format!("*{}*", escape_slack(text)),
            Self::Discord => format!("**{}**", escape_discord(text)),
        }
    }

    /// Text from elsewhere (a health check's error), shown as-is: no markup, no pings.
    fn plain(self, text: &str) -> String {
        match self {
            Self::Slack => escape_slack(text),
            Self::Discord => escape_discord(text),
        }
    }

    /// A moment shown in each reader's own time zone, with `fallback` where unsupported.
    fn when(self, at: u64, fallback: &str) -> String {
        match self {
            Self::Slack => format!("<!date^{at}^{{date_short_pretty}} {{time}}|{fallback}>"),
            Self::Discord => format!("<t:{at}:f>"),
        }
    }
}

/// `<@id>`: the same syntax on both services.
fn user_mention(id: &str) -> String {
    format!("<@{id}>")
}

fn escape_slack(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_discord(text: &str) -> String {
    text.chars().fold(String::new(), |mut out, c| {
        if matches!(c, '\\' | '*' | '_' | '~' | '`' | '|' | '<' | '>') {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

/// Who typed the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    Slack {
        user: String,
    },
    /// `roles`: the member's roles in the server (empty in a DM).
    Discord {
        user: String,
        roles: Vec<String>,
    },
}

impl Caller {
    fn service(&self) -> Service {
        match self {
            Self::Slack { .. } => Service::Slack,
            Self::Discord { .. } => Service::Discord,
        }
    }

    fn user(&self) -> &str {
        match self {
            Self::Slack { user } | Self::Discord { user, .. } => user,
        }
    }
}

/// One `CHAT_ADMINS` entry, in `ALERT_MENTIONS` syntax: a Slack user (`U0123ABCD`), a
/// Discord user (`123456789012345678`) or a Discord role (`&123456789012345678`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admin {
    SlackUser(String),
    DiscordUser(String),
    DiscordRole(String),
}

impl Admin {
    /// `None` for anything that isn't a user or a Discord role (`here`, a Slack group...).
    pub fn parse(entry: &str) -> Option<Self> {
        match Mention::from_str(entry).ok()? {
            Mention::SlackUser(id) => Some(Self::SlackUser(id)),
            Mention::DiscordUser(id) => Some(Self::DiscordUser(id)),
            Mention::DiscordRole(id) => Some(Self::DiscordRole(id)),
            Mention::Here | Mention::Channel | Mention::SlackGroup(_) => None,
        }
    }

    fn covers(&self, caller: &Caller) -> bool {
        match (self, caller) {
            (Self::SlackUser(id), Caller::Slack { user })
            | (Self::DiscordUser(id), Caller::Discord { user, .. }) => id == user,
            (Self::DiscordRole(id), Caller::Discord { roles, .. }) => roles.contains(id),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    /// `None` = every app.
    Status(Option<String>),
    /// `for_secs`: `None` = until resumed by hand.
    Pause {
        app: String,
        for_secs: Option<u64>,
    },
    Resume(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Unknown(String),
    /// `pause` or `resume` without an app.
    MissingApp(&'static str),
    BadDuration(String),
}

impl Command {
    /// Slack's free text after the command: `status`, `status billing api`,
    /// `pause billing 2h`, `resume billing`. Blank = `status`.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut words = text.split_whitespace();
        let Some(sub) = words.next() else {
            return Ok(Self::Status(None));
        };
        let rest: Vec<&str> = words.collect();
        match sub.to_lowercase().as_str() {
            "help" | "ayuda" => Ok(Self::Help),
            "status" | "estado" => Ok(Self::Status((!rest.is_empty()).then(|| rest.join(" ")))),
            "pause" | "pausar" => {
                // App names may contain spaces: a duration can only be the last word.
                let (app, duration) = match rest.split_last() {
                    Some((last, head))
                        if !head.is_empty() && last.starts_with(char::is_numeric) =>
                    {
                        (head.join(" "), Some(*last))
                    }
                    _ => (rest.join(" "), None),
                };
                Self::pause(&app, duration)
            }
            "resume" | "reanudar" => Self::resume(&rest.join(" ")),
            _ => Err(ParseError::Unknown(sub.to_string())),
        }
    }

    pub fn pause(app: &str, duration: Option<&str>) -> Result<Self, ParseError> {
        let app = app.trim();
        if app.is_empty() {
            return Err(ParseError::MissingApp("pause"));
        }
        Ok(Self::Pause {
            app: app.to_string(),
            for_secs: duration
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(parse_duration)
                .transpose()?,
        })
    }

    pub fn resume(app: &str) -> Result<Self, ParseError> {
        let app = app.trim();
        if app.is_empty() {
            return Err(ParseError::MissingApp("resume"));
        }
        Ok(Self::Resume(app.to_string()))
    }

    fn changes_state(&self) -> bool {
        matches!(self, Self::Pause { .. } | Self::Resume(_))
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::Status(_) => "status",
            Self::Pause { .. } => "pause",
            Self::Resume(_) => "resume",
        }
    }
}

/// `30m`, `2h`, `1d`, or a bare number of hours (as in the /apps form); from one minute
/// up to `MAX_PAUSE_HOURS`.
fn parse_duration(raw: &str) -> Result<u64, ParseError> {
    let bad = || ParseError::BadDuration(raw.to_string());
    let lower = raw.to_lowercase();
    let (number, unit) = match lower.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => lower.split_at(at),
        None => (lower.as_str(), "h"),
    };
    let unit_secs = match unit {
        "m" | "min" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(bad()),
    };
    let secs = number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(unit_secs))
        .ok_or_else(bad)?;
    if (60..=MAX_PAUSE_HOURS * 3600).contains(&secs) {
        Ok(secs)
    } else {
        Err(bad())
    }
}

/// What the command answers. `public` replies show in the channel; the rest only to the
/// person who typed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub public: bool,
    /// How the command went, for the audit log.
    pub outcome: Outcome,
    /// The app it acted on, once found.
    pub app: Option<String>,
}

impl Reply {
    fn public(text: String) -> Self {
        Self {
            text,
            public: true,
            outcome: Outcome::Ok,
            app: None,
        }
    }

    fn private(text: String) -> Self {
        Self {
            public: false,
            ..Self::public(text)
        }
    }

    fn denied(self) -> Self {
        Self {
            outcome: Outcome::Denied,
            ..self
        }
    }

    fn failed(self) -> Self {
        Self {
            outcome: Outcome::Error,
            ..self
        }
    }

    fn on(self, app: &RegisteredApp) -> Self {
        Self {
            app: Some(app.slug.clone()),
            ..self
        }
    }
}

enum Lookup {
    Found(Box<RegisteredApp>),
    Missing,
    Ambiguous(Vec<String>),
}

/// An exact slug or name (any case) wins; otherwise the one app whose slug or name
/// contains the query.
fn lookup(apps: &[RegisteredApp], query: &str) -> Lookup {
    let query = query.trim().to_lowercase();
    if let Some(app) = apps
        .iter()
        .find(|a| a.slug == query || a.name.to_lowercase() == query)
    {
        return Lookup::Found(Box::new(app.clone()));
    }
    let mut matches: Vec<&RegisteredApp> = apps
        .iter()
        .filter(|a| a.slug.contains(&query) || a.name.to_lowercase().contains(&query))
        .collect();
    match matches.len() {
        0 => Lookup::Missing,
        1 => Lookup::Found(Box::new(matches.remove(0).clone())),
        _ => Lookup::Ambiguous(matches.iter().map(|a| a.name.clone()).collect()),
    }
}

/// Apps whose slug or name contains `typed`, for Discord's autocomplete: (name, slug).
pub(crate) fn suggestions(apps: &[RegisteredApp], typed: &str) -> Vec<(String, String)> {
    let typed = typed.trim().to_lowercase();
    apps.iter()
        .filter(|a| a.slug.contains(&typed) || a.name.to_lowercase().contains(&typed))
        .map(|a| (a.name.clone(), a.slug.clone()))
        .collect()
}

fn emoji(summary: &MonitorSummary) -> &'static str {
    match (summary.paused, summary.status) {
        (true, _) => "⏸️",
        (false, Some(Status::Up)) => "🟢",
        (false, Some(Status::Degraded)) => "🟡",
        (false, Some(Status::Down)) => "🔴",
        (false, None) => "⚪",
    }
}

/// Worst first: down, degraded, not checked yet, paused, up.
fn rank(summary: &MonitorSummary) -> u8 {
    match (summary.paused, summary.status) {
        (false, Some(Status::Down)) => 0,
        (false, Some(Status::Degraded)) => 1,
        (false, None) => 2,
        (true, _) => 3,
        (false, Some(Status::Up)) => 4,
    }
}

fn percent(value: f64) -> String {
    if value >= 100.0 {
        "100 %".to_string()
    } else {
        format!("{value:.2} %")
    }
}

fn last_message(summary: &MonitorSummary) -> Option<&str> {
    summary
        .recent
        .last()
        .map(|b| b.message.trim())
        .filter(|m| !m.is_empty())
}

/// Everything a command needs from the running instance.
pub struct Chat<'a> {
    pub registry: &'a AppRegistry,
    pub monitor: &'a UptimeMonitor,
    pub i18n: &'a I18n,
    /// `CHAT_ADMINS`, validated at startup.
    pub admins: &'a [String],
    /// `CHAT_COMMAND`, for the usage hints.
    pub command: &'a str,
    pub public_url: Option<&'a str>,
    /// `None` only in tests that don't look at it.
    pub audit: Option<&'a AuditLog>,
    /// For the server's CPU, memory and disk in an app's detail; `None` in tests.
    pub system: Option<&'a crate::system::SystemMonitor>,
}

impl Chat<'_> {
    fn t(&self, key: &str, args: &[(&str, &str)]) -> String {
        self.i18n.text(key, args)
    }

    fn is_admin(&self, caller: &Caller) -> bool {
        self.admins
            .iter()
            .filter_map(|entry| Admin::parse(entry))
            .any(|admin| admin.covers(caller))
    }

    /// Answers the command and records it in the audit log, whatever the outcome.
    /// `context`: what the service sent besides the command (channel, raw text...).
    pub async fn run(
        &self,
        parsed: Result<Command, ParseError>,
        caller: &Caller,
        context: serde_json::Value,
    ) -> Reply {
        let action = match &parsed {
            Ok(Command::Help) => "chat.help",
            Ok(Command::Status(_)) => "chat.status",
            Ok(Command::Pause { .. }) => "chat.pause",
            Ok(Command::Resume(_)) => "chat.resume",
            Err(_) => "chat.invalid",
        };
        let reply = self.answer(parsed, caller).await;
        if let Some(log) = self.audit {
            let source = match caller.service() {
                Service::Slack => Source::Slack,
                Service::Discord => Source::Discord,
            };
            let mut event = Event::chat(source, caller.user(), action)
                .outcome(reply.outcome)
                .detail(context);
            if let Some(app) = &reply.app {
                event = event.target(app);
            }
            log.record(event).await;
        }
        reply
    }

    async fn answer(&self, parsed: Result<Command, ParseError>, caller: &Caller) -> Reply {
        let command = match parsed {
            Ok(command) => command,
            Err(e) => return Reply::private(self.parse_error(&e)).failed(),
        };
        if command.changes_state() && !self.is_admin(caller) {
            tracing::warn!(
                user = caller.user(),
                command = command.name(),
                "chat: not in CHAT_ADMINS"
            );
            let key = if self.admins.is_empty() {
                "chat.no_admins"
            } else {
                "chat.forbidden"
            };
            return Reply::private(self.t(key, &[("sub", command.name()), ("id", caller.user())]))
                .denied();
        }
        match command {
            Command::Help => Reply::private(self.t("chat.help", &[("cmd", self.command)])),
            Command::Status(None) => self.overview(caller.service()).await,
            Command::Status(Some(query)) => match self.find(&query).await {
                Ok(app) => self.detail(caller.service(), &app).await,
                Err(reply) => reply,
            },
            Command::Pause { app, for_secs } => match self.find(&app).await {
                Ok(app) => self.pause(caller, &app, for_secs).await,
                Err(reply) => reply,
            },
            Command::Resume(app) => match self.find(&app).await {
                Ok(app) => self.resume(caller, &app).await,
                Err(reply) => reply,
            },
        }
    }

    fn parse_error(&self, e: &ParseError) -> String {
        match e {
            ParseError::Unknown(sub) => {
                self.t("chat.unknown", &[("sub", sub), ("cmd", self.command)])
            }
            ParseError::MissingApp("pause") => self.t("chat.usage_pause", &[("cmd", self.command)]),
            ParseError::MissingApp(_) => self.t("chat.usage_resume", &[("cmd", self.command)]),
            ParseError::BadDuration(raw) => self.t(
                "chat.bad_duration",
                &[("value", raw), ("max", &MAX_PAUSE_HOURS.to_string())],
            ),
        }
    }

    async fn find(&self, query: &str) -> Result<RegisteredApp, Reply> {
        match lookup(&self.registry.list().await, query) {
            Lookup::Found(app) => Ok(*app),
            Lookup::Missing => {
                Err(Reply::private(self.t("chat.no_app", &[("query", query)])).failed())
            }
            Lookup::Ambiguous(names) => Err(Reply::private(self.t(
                "chat.ambiguous",
                &[("query", query), ("apps", &names.join(", "))],
            ))
            .failed()),
        }
    }

    async fn overview(&self, service: Service) -> Reply {
        let apps = self.registry.list().await;
        if apps.is_empty() {
            return Reply::private(self.t("chat.no_apps", &[]));
        }
        let mut monitors = self.monitor.overview(&apps).await.monitors;
        monitors.sort_by_key(rank);
        let count = |f: fn(&MonitorSummary) -> bool| monitors.iter().filter(|m| f(m)).count();
        let header = self.t(
            "chat.overview",
            &[
                ("total", &monitors.len().to_string()),
                (
                    "up",
                    &count(|m| !m.paused && m.status == Some(Status::Up)).to_string(),
                ),
                (
                    "degraded",
                    &count(|m| !m.paused && m.status == Some(Status::Degraded)).to_string(),
                ),
                (
                    "down",
                    &count(|m| !m.paused && m.status == Some(Status::Down)).to_string(),
                ),
                ("paused", &count(|m| m.paused).to_string()),
            ],
        );
        let mut text = service.bold(&header);
        for (shown, m) in monitors.iter().enumerate() {
            let line = format!("\n{}", self.line(service, m));
            if text.len() + line.len() > MAX_REPLY_CHARS {
                let more = (monitors.len() - shown).to_string();
                text.push('\n');
                text.push_str(&self.t("chat.more", &[("n", &more)]));
                break;
            }
            text.push_str(&line);
        }
        if let Some(base) = self.public_url {
            let _ = write!(text, "\n{base}/");
        }
        Reply::public(text)
    }

    /// `🔴 *Billing API* · timeout after 10 s · 24 h 99.10 %`
    fn line(&self, service: Service, m: &MonitorSummary) -> String {
        let detail = match (m.paused, m.status) {
            (true, _) => self.t("chat.paused", &[]),
            (false, Some(Status::Down)) => last_message(m).map_or_else(
                || self.t("chat.down", &[]),
                |msg| service.plain(&msg.chars().take(80).collect::<String>()),
            ),
            (false, Some(_)) => m
                .latency_ms
                .map_or_else(|| self.t("chat.up", &[]), |ms| format!("{ms} ms")),
            (false, None) => self.t("chat.not_checked", &[]),
        };
        let mut line = format!("{} {} · {detail}", emoji(m), service.bold(&m.name));
        if let Some(uptime) = m.uptime_24h {
            let _ = write!(line, " · 24 h {}", percent(uptime));
        }
        line
    }

    async fn detail(&self, service: Service, app: &RegisteredApp) -> Reply {
        let m = self.monitor.summary(app).await;
        let state = match (m.paused, m.status) {
            (true, _) => self.t("chat.paused", &[]),
            (false, Some(Status::Up)) => self.t("chat.up", &[]),
            (false, Some(Status::Degraded)) => self.t("chat.degraded", &[]),
            (false, Some(Status::Down)) => self.t("chat.down", &[]),
            (false, None) => self.t("chat.not_checked", &[]),
        };
        let mut lines = vec![format!("{} {} · {state}", emoji(&m), service.bold(&m.name))];
        if m.status == Some(Status::Down)
            && let Some(msg) = last_message(&m)
        {
            lines.push(self.t("chat.detail_error", &[("message", &service.plain(msg))]));
        }
        if let Some(ms) = m.latency_ms {
            let avg = m
                .avg_latency_24h_ms
                .map_or_else(|| "—".to_string(), |a| format!("{a} ms"));
            lines.push(self.t(
                "chat.detail_latency",
                &[("ms", &ms.to_string()), ("avg", &avg)],
            ));
        }
        if m.uptime_24h.is_some() || m.uptime_30d.is_some() {
            let show = |v: Option<f64>| v.map_or_else(|| "—".to_string(), percent);
            lines.push(self.t(
                "chat.detail_uptime",
                &[("day", &show(m.uptime_24h)), ("month", &show(m.uptime_30d))],
            ));
        }
        if m.paused {
            lines.push(self.pause_note(service, m.paused_until));
        }
        if let Some(expires) = m.cert_expires_at {
            let left = format_duration(expires.saturating_sub(unix_now()));
            lines.push(self.t(
                "chat.detail_cert",
                &[("when", &service.when(expires, &left))],
            ));
        }
        if let Some(beat) = m.recent.last() {
            let ago = format_duration(unix_now().saturating_sub(beat.at));
            lines.push(self.t("chat.detail_checked", &[("ago", &ago)]));
        }
        if let Some(line) = self.system_line(app).await {
            lines.push(line);
        }
        if let Some(base) = self.public_url {
            lines.push(format!("{base}/#{}", app.slug));
        }
        Reply::public(lines.join("\n")).on(app)
    }

    /// `🖥️ CPU 23 % · memory 42 % · disk / 37 %`, from the latest reading of the app's
    /// system URL; `None` when it has none.
    async fn system_line(&self, app: &RegisteredApp) -> Option<String> {
        app.system_url.as_ref()?;
        let reading = self.system?.latest(&app.slug).await?;
        let Some(s) = reading.snapshot else {
            let error = reading.error.unwrap_or_default();
            return Some(self.t("chat.detail_system_error", &[("error", &error)]));
        };
        let disk = s.fullest_disk().map_or_else(
            || "—".to_string(),
            |d| {
                #[allow(clippy::cast_precision_loss)]
                let pct = d.used_bytes as f64 * 100.0 / d.total_bytes.max(1) as f64;
                format!("{} {pct:.0} %", d.mount)
            },
        );
        Some(self.t(
            "chat.detail_system",
            &[
                ("cpu", &format!("{:.0} %", s.cpu.usage_pct)),
                ("memory", &format!("{:.0} %", s.memory_pct())),
                ("disk", &disk),
            ],
        ))
    }

    /// "until <date>" or "until resumed".
    fn pause_note(&self, service: Service, until: Option<u64>) -> String {
        match until {
            Some(at) => {
                let left = format_duration(at.saturating_sub(unix_now()));
                self.t("chat.paused_until", &[("when", &service.when(at, &left))])
            }
            None => self.t("chat.paused_forever", &[]),
        }
    }

    async fn pause(&self, caller: &Caller, app: &RegisteredApp, for_secs: Option<u64>) -> Reply {
        let until = for_secs.map(|secs| unix_now() + secs);
        if let Err(e) = self.registry.set_paused(&app.slug, true, until).await {
            return Reply::private(self.t("chat.save_failed", &[("error", &e.to_string())]))
                .failed()
                .on(app);
        }
        tracing::info!(app = %app.slug, user = caller.user(), ?until, "chat: paused");
        let service = caller.service();
        Reply::public(self.t(
            "chat.paused_by",
            &[
                ("user", &user_mention(caller.user())),
                ("app", &service.bold(&app.name)),
                ("until", &self.pause_note(service, until)),
            ],
        ))
        .on(app)
    }

    async fn resume(&self, caller: &Caller, app: &RegisteredApp) -> Reply {
        let service = caller.service();
        if !app.paused {
            return Reply::private(self.t("chat.not_paused", &[("app", &service.bold(&app.name))]))
                .failed()
                .on(app);
        }
        if let Err(e) = self.registry.set_paused(&app.slug, false, None).await {
            return Reply::private(self.t("chat.save_failed", &[("error", &e.to_string())]))
                .failed()
                .on(app);
        }
        tracing::info!(app = %app.slug, user = caller.user(), "chat: resumed");
        Reply::public(self.t(
            "chat.resumed_by",
            &[
                ("user", &user_mention(caller.user())),
                ("app", &service.bold(&app.name)),
            ],
        ))
        .on(app)
    }
}

/// Lowercase or uppercase hex; `None` for an odd length or a non-hex character.
pub fn decode_hex(raw: &str) -> Option<Vec<u8>> {
    let raw = raw.trim().as_bytes();
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    raw.chunks(2)
        .map(|pair| {
            let digit = |c: u8| char::from(c).to_digit(16);
            u8::try_from(digit(pair[0])? * 16 + digit(pair[1])?).ok()
        })
        .collect()
}

/// Whether a request signed at `timestamp` (unix seconds, as sent) is recent enough.
fn is_fresh(timestamp: &str, now: u64) -> bool {
    timestamp
        .trim()
        .parse::<u64>()
        .is_ok_and(|at| at.abs_diff(now) <= MAX_SIGNATURE_AGE_SECS)
}

/// Lowercase hex, as both services' signatures are sent.
#[cfg(test)]
pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_text_parses_into_commands() {
        assert_eq!(Command::parse(""), Ok(Command::Status(None)));
        assert_eq!(Command::parse("HELP"), Ok(Command::Help));
        assert_eq!(
            Command::parse("status Billing API"),
            Ok(Command::Status(Some("Billing API".into())))
        );
        assert_eq!(
            Command::parse("pause billing api 30m"),
            Ok(Command::Pause {
                app: "billing api".into(),
                for_secs: Some(1800)
            })
        );
        assert_eq!(
            Command::parse("pausar web"),
            Ok(Command::Pause {
                app: "web".into(),
                for_secs: None
            })
        );
        // A lone number is the app's name, not a duration.
        assert_eq!(
            Command::parse("pause 2048"),
            Ok(Command::Pause {
                app: "2048".into(),
                for_secs: None
            })
        );
        assert_eq!(
            Command::parse("resume web"),
            Ok(Command::Resume("web".into()))
        );
        assert_eq!(
            Command::parse("pause"),
            Err(ParseError::MissingApp("pause"))
        );
        assert_eq!(
            Command::parse("resume"),
            Err(ParseError::MissingApp("resume"))
        );
        assert_eq!(
            Command::parse("reboot"),
            Err(ParseError::Unknown("reboot".into()))
        );
    }

    #[test]
    fn durations_accept_minutes_hours_and_days_within_the_limit() {
        assert_eq!(parse_duration("30m"), Ok(1800));
        assert_eq!(parse_duration("2h"), Ok(7200));
        assert_eq!(parse_duration("3"), Ok(3 * 3600));
        assert_eq!(parse_duration("1D"), Ok(86_400));
        assert_eq!(parse_duration("30d"), Ok(720 * 3600));
        for bad in ["31d", "0m", "10s", "2x", "h", "99999999999999999999d"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn admins_are_users_or_discord_roles_matched_per_service() {
        assert_eq!(Admin::parse("here"), None);
        assert_eq!(Admin::parse("S0123ABCD"), None);
        let slack = Admin::parse("U0123ABCD").unwrap();
        let role = Admin::parse("&42").unwrap();
        assert!(slack.covers(&Caller::Slack {
            user: "U0123ABCD".into()
        }));
        assert!(!slack.covers(&Caller::Discord {
            user: "U0123ABCD".into(),
            roles: vec![]
        }));
        assert!(role.covers(&Caller::Discord {
            user: "7".into(),
            roles: vec!["42".into()]
        }));
    }

    #[test]
    fn markup_is_escaped_per_service() {
        assert_eq!(Service::Slack.bold("A<b>&c"), "*A&lt;b&gt;&amp;c*");
        assert_eq!(Service::Discord.bold("my_app*"), "**my\\_app\\***");
    }

    #[test]
    fn hex_and_freshness() {
        assert_eq!(decode_hex("00ffA1"), Some(vec![0, 255, 161]));
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("zz"), None);
        assert!(is_fresh("1000", 1200));
        assert!(!is_fresh("1000", 1400));
        assert!(!is_fresh("soon", 1000));
    }
}
