//! The audit log (`audit_log` table): who did what, from the web UI, Slack, Discord or
//! Heartbeat itself -- logins, every change to apps, notices, templates and the theme,
//! alert tests, exports, reading an app's logs and every chat command, allowed or not.
//!
//! Recording never fails the action it describes: a write error is logged and the request
//! carries on. Secrets never get in: tokens and passwords aren't passed here, header values
//! are reduced to their names and webhook URLs are masked (see `app_detail`).
//!
//! Entries older than `AUDIT_RETENTION_DAYS` are deleted hourly; 0 keeps them forever.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use actix_web::{HttpRequest, web};
use rusqlite::{ToSql, params_from_iter};
use serde::Serialize;
use serde_json::{Value, json};

use crate::db::{Db, DbError};
use crate::registry::RegisteredApp;
use crate::uptime::unix_now;

/// Reading an app's logs polls every few seconds: one entry per person and app per window.
const VIEW_WINDOW_SECS: u64 = 15 * 60;
/// Most entries one query returns (a page of /audit, or a CSV export).
pub const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Web,
    Slack,
    Discord,
    System,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    /// Refused: bad password, throttled, not in `CHAT_ADMINS`...
    Denied,
    /// Allowed, but it failed (a database error, an invalid form).
    Error,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Denied => "denied",
            Self::Error => "error",
        }
    }
}

/// Something that happened, before it's stored.
#[derive(Debug, Clone)]
pub struct Event {
    source: Source,
    actor: String,
    action: &'static str,
    target: Option<String>,
    outcome: Outcome,
    detail: Option<Value>,
    ip: Option<String>,
}

impl Event {
    fn new(source: Source, actor: String, action: &'static str) -> Self {
        Self {
            source,
            actor,
            action,
            target: None,
            outcome: Outcome::Ok,
            detail: None,
            ip: None,
        }
    }

    /// By whoever is logged in on `req` (empty actor without a session, or for one from
    /// before sessions kept the email), from the request's client IP.
    pub fn web(req: &HttpRequest, action: &'static str) -> Self {
        let actor = crate::auth::current_session(req)
            .map(|s| s.email)
            .unwrap_or_default();
        Self {
            ip: Some(crate::security::client_ip(req)),
            ..Self::new(Source::Web, actor, action)
        }
    }

    pub fn chat(source: Source, user: &str, action: &'static str) -> Self {
        Self::new(source, user.to_string(), action)
    }

    pub fn system(action: &'static str) -> Self {
        Self::new(Source::System, "system".to_string(), action)
    }

    #[must_use]
    pub fn actor(mut self, actor: &str) -> Self {
        self.actor = actor.to_string();
        self
    }

    #[must_use]
    pub fn target(mut self, target: &str) -> Self {
        self.target = Some(target.to_string());
        self
    }

    #[must_use]
    pub fn outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// `Ok` or `Error` depending on how the action went.
    #[must_use]
    pub fn result<T, E>(self, result: &Result<T, E>) -> Self {
        self.outcome(if result.is_ok() {
            Outcome::Ok
        } else {
            Outcome::Error
        })
    }

    #[must_use]
    pub fn detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

/// One stored entry, as /audit shows it.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: i64,
    pub at: u64,
    pub source: String,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub outcome: String,
    pub detail: Option<Value>,
    pub ip: Option<String>,
}

/// What /audit asks for; `None` = no filter on that field.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Part of the actor (case-insensitive).
    pub actor: Option<String>,
    /// An action's prefix: `app.` for every app action, `chat.pause` for one.
    pub action: Option<String>,
    /// An exact target (an app's slug).
    pub target: Option<String>,
    pub source: Option<String>,
    pub outcome: Option<String>,
    /// Unix seconds, inclusive.
    pub from: Option<u64>,
    /// Unix seconds, exclusive.
    pub until: Option<u64>,
    /// Entries older than this ID (the next page).
    pub before_id: Option<i64>,
    pub limit: usize,
}

#[derive(Clone)]
pub struct AuditLog {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    /// `None` = keep every entry.
    retention: Option<Duration>,
    /// Last recorded view per (actor, target), for `VIEW_WINDOW_SECS`.
    views: Mutex<HashMap<(String, String), u64>>,
}

fn sql_int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl AuditLog {
    /// `retention_days`: 0 keeps every entry.
    pub fn new(db: Db, retention_days: u32) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                retention: (retention_days > 0)
                    .then(|| Duration::from_hours(24 * u64::from(retention_days))),
                views: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub async fn record(&self, event: Event) {
        let at = sql_int(unix_now());
        let action = event.action;
        let result = self
            .inner
            .db
            .write(move |tx| {
                tx.prepare_cached(
                    "INSERT INTO audit_log (at, source, actor, action, target, outcome, detail, ip) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?
                .execute(rusqlite::params![
                    at,
                    event.source.as_str(),
                    event.actor,
                    event.action,
                    event.target,
                    event.outcome.as_str(),
                    event.detail.map(|d| d.to_string()),
                    event.ip,
                ])?;
                Ok(())
            })
            .await;
        if let Err(e) = result {
            tracing::error!(error = %e, action, "audit: could not record");
        }
    }

    /// `record`, at most once per actor and target every `VIEW_WINDOW_SECS`: for reads
    /// that poll, like an open log viewer.
    pub async fn record_view(&self, event: Event) {
        let now = unix_now();
        let key = (
            event.actor.clone(),
            format!("{}:{}", event.action, event.target.as_deref().unwrap_or("")),
        );
        {
            let mut views = self
                .inner
                .views
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            views.retain(|_, at| now.saturating_sub(*at) < VIEW_WINDOW_SECS);
            if views.contains_key(&key) {
                return;
            }
            views.insert(key, now);
        }
        self.record(event).await;
    }

    /// Newest first.
    pub async fn query(&self, filter: Filter) -> Result<Vec<Entry>, DbError> {
        let mut clauses: Vec<&str> = Vec::new();
        let mut args: Vec<Box<dyn ToSql + Send>> = Vec::new();
        if let Some(actor) = filter.actor {
            clauses.push("instr(lower(actor), lower(?)) > 0");
            args.push(Box::new(actor));
        }
        if let Some(action) = filter.action {
            clauses.push("action LIKE ? ESCAPE '\\'");
            let escaped = action
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            args.push(Box::new(format!("{escaped}%")));
        }
        if let Some(target) = filter.target {
            clauses.push("target = ?");
            args.push(Box::new(target));
        }
        if let Some(source) = filter.source {
            clauses.push("source = ?");
            args.push(Box::new(source));
        }
        if let Some(outcome) = filter.outcome {
            clauses.push("outcome = ?");
            args.push(Box::new(outcome));
        }
        if let Some(from) = filter.from {
            clauses.push("at >= ?");
            args.push(Box::new(sql_int(from)));
        }
        if let Some(until) = filter.until {
            clauses.push("at < ?");
            args.push(Box::new(sql_int(until)));
        }
        if let Some(before) = filter.before_id {
            clauses.push("id < ?");
            args.push(Box::new(before));
        }
        let mut sql = String::from(
            "SELECT id, at, source, actor, action, target, outcome, detail, ip FROM audit_log",
        );
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        let limit = filter.limit.clamp(1, MAX_ENTRIES);
        let _ = write!(sql, " ORDER BY id DESC LIMIT {limit}");
        self.inner
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(&sql)?;
                let entries = stmt
                    .query_map(
                        params_from_iter(args.iter().map(|a| a.as_ref() as &dyn ToSql)),
                        |row| {
                            let detail: Option<String> = row.get(7)?;
                            Ok(Entry {
                                id: row.get(0)?,
                                at: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                                source: row.get(2)?,
                                actor: row.get(3)?,
                                action: row.get(4)?,
                                target: row.get(5)?,
                                outcome: row.get(6)?,
                                detail: detail.and_then(|d| serde_json::from_str(&d).ok()),
                                ip: row.get(8)?,
                            })
                        },
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(entries)
            })
            .await
    }

    /// Deletes the entries past the retention; returns how many.
    pub async fn prune(&self, now: u64) -> Result<usize, DbError> {
        let Some(retention) = self.inner.retention else {
            return Ok(0);
        };
        let cutoff = sql_int(now.saturating_sub(retention.as_secs()));
        self.inner
            .db
            .write(move |tx| Ok(tx.execute("DELETE FROM audit_log WHERE at < ?1", [cutoff])?))
            .await
    }

    /// Prunes now and then every hour, in the background.
    pub fn spawn_pruning(&self) {
        let log = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_hours(1));
            loop {
                ticker.tick().await;
                match log.prune(unix_now()).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(deleted = n, "audit: old entries pruned"),
                    Err(e) => tracing::error!(error = %e, "audit: could not prune"),
                }
            }
        });
    }
}

/// Records `event` in the app's audit log, if one is registered (it always is outside
/// tests that don't care).
pub async fn record(req: &HttpRequest, event: Event) {
    if let Some(log) = req.app_data::<web::Data<AuditLog>>() {
        log.record(event).await;
    }
}

/// `AuditLog::record_view` through the request's app data.
pub async fn record_view(req: &HttpRequest, event: Event) {
    if let Some(log) = req.app_data::<web::Data<AuditLog>>() {
        log.record_view(event).await;
    }
}

/// An app's settings as the log keeps them: no embed token, header names without their
/// values, webhook URLs masked.
pub fn app_detail(app: &RegisteredApp) -> Value {
    let mut value = serde_json::to_value(app).unwrap_or(Value::Null);
    if let Some(fields) = value.as_object_mut() {
        fields.remove("embed_token");
        fields.insert(
            "headers".into(),
            json!(
                app.headers
                    .iter()
                    .map(|h| h.name.clone())
                    .collect::<Vec<_>>()
            ),
        );
        fields.insert(
            "alert_webhooks".into(),
            json!(
                app.alert_webhooks
                    .iter()
                    .map(|raw| url::Url::parse(raw)
                        .map_or_else(|_| "…".to_string(), |u| crate::alerts::mask(&u)))
                    .collect::<Vec<_>>()
            ),
        );
    }
    value
}

/// What changed between two versions of an app: `{field: {from, to}}`. A header whose value
/// alone changed shows as `"headers": "values changed"`.
pub fn app_changes(before: &RegisteredApp, after: &RegisteredApp) -> Value {
    let (old, new) = (app_detail(before), app_detail(after));
    let mut changes = serde_json::Map::new();
    if let (Some(old), Some(new)) = (old.as_object(), new.as_object()) {
        let keys: std::collections::BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        for key in keys {
            let (from, to) = (old.get(key), new.get(key));
            if from != to {
                changes.insert(
                    key.clone(),
                    json!({ "from": from.unwrap_or(&Value::Null), "to": to.unwrap_or(&Value::Null) }),
                );
            }
        }
    }
    if before.headers != after.headers && !changes.contains_key("headers") {
        changes.insert("headers".into(), json!("values changed"));
    }
    Value::Object(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::CheckHeader;

    fn app() -> RegisteredApp {
        serde_json::from_value(json!({
            "slug": "billing",
            "name": "Billing",
            "health_url": "https://api.example.com/health",
            "embed_token": "secret-embed-token",
            "headers": [{ "name": "Authorization", "value": "Bearer s3cret" }],
            "alert_webhooks": ["https://hooks.slack.com/services/T0/B0/XXXXXXXXXXXXXXXXabcd"],
        }))
        .unwrap()
    }

    #[test]
    fn app_details_leave_secrets_out() {
        let detail = app_detail(&app()).to_string();
        for secret in ["secret-embed-token", "s3cret", "XXXXXXXXXXXXXXXX"] {
            assert!(!detail.contains(secret), "{secret} leaked: {detail}");
        }
        assert!(detail.contains("Authorization"), "{detail}");
    }

    #[test]
    fn changes_list_only_what_differs() {
        let before = app();
        let mut after = before.clone();
        after.name = "Billing API".into();
        after.headers = vec![CheckHeader {
            name: "Authorization".into(),
            value: "Bearer other".into(),
        }];
        let changes = app_changes(&before, &after);
        assert_eq!(
            changes,
            json!({
                "name": { "from": "Billing", "to": "Billing API" },
                "headers": "values changed",
            })
        );
    }

    #[tokio::test]
    async fn entries_are_filtered_paged_and_pruned() {
        let log = AuditLog::new(Db::open_in_memory(), 30);
        log.record(Event::system("app.resume").target("billing"))
            .await;
        log.record(
            Event::chat(Source::Slack, "U0ADMIN", "chat.pause")
                .target("billing")
                .outcome(Outcome::Denied),
        )
        .await;
        log.record(Event::chat(Source::Discord, "42", "chat.status"))
            .await;

        let all = log
            .query(Filter {
                limit: 10,
                ..Filter::default()
            })
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].action, "chat.status", "newest first");

        let chat = log
            .query(Filter {
                action: Some("chat.".into()),
                target: Some("billing".into()),
                actor: Some("u0ad".into()),
                limit: 10,
                ..Filter::default()
            })
            .await
            .unwrap();
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].outcome, "denied");

        let page = log
            .query(Filter {
                before_id: Some(all[1].id),
                limit: 10,
                ..Filter::default()
            })
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].action, "app.resume");

        assert_eq!(log.prune(unix_now() + 31 * 86_400).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn views_are_recorded_once_per_window() {
        let log = AuditLog::new(Db::open_in_memory(), 0);
        for _ in 0..3 {
            log.record_view(
                Event::system("logs.view")
                    .actor("a@x.com")
                    .target("billing"),
            )
            .await;
        }
        log.record_view(Event::system("logs.view").actor("a@x.com").target("web"))
            .await;
        let entries = log
            .query(Filter {
                limit: 10,
                ..Filter::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            log.prune(u64::MAX / 2).await.unwrap(),
            0,
            "0 days keeps everything"
        );
    }
}
