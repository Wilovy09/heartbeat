//! Editable alert message templates, one per kind (down, reminder, degraded, recovered). Admins
//! change them from /settings; they're stored in the `alert_templates` table and take
//! effect immediately. A kind without a custom template uses the default for `APP_LANG`
//! (`locales/<lang>.json`, keys `alert.default_*`).
//!
//! Placeholders: `{app}`, `{message}`, `{latency}` (e.g. `412 ms`), `{link}` (the app's
//! dashboard page, when `PUBLIC_URL` is set), `{mentions}` (only filled on down and
//! reminders) and `{duration}` (how long the outage has lasted, on reminders and
//! recoveries).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

use crate::db::{Db, DbError};
use crate::i18n::I18n;

/// Longest template accepted from the settings page.
pub const MAX_TEMPLATE_CHARS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TemplateKind {
    Down,
    /// Still down, every `ALERT_REMIND_MINS`.
    Reminder,
    Degraded,
    Recovered,
}

impl TemplateKind {
    pub const ALL: [Self; 4] = [Self::Down, Self::Reminder, Self::Degraded, Self::Recovered];

    /// How the `alert_templates.kind` column spells it (the same as the JSON).
    fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Reminder => "reminder",
            Self::Degraded => "degraded",
            Self::Recovered => "recovered",
        }
    }
}

/// Custom templates as stored; `None` = use the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertTemplates {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub down: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered: Option<String>,
}

impl AlertTemplates {
    fn slot(&mut self, kind: TemplateKind) -> &mut Option<String> {
        match kind {
            TemplateKind::Down => &mut self.down,
            TemplateKind::Reminder => &mut self.reminder,
            TemplateKind::Degraded => &mut self.degraded,
            TemplateKind::Recovered => &mut self.recovered,
        }
    }

    pub(crate) fn get(&self, kind: TemplateKind) -> Option<&String> {
        match kind {
            TemplateKind::Down => self.down.as_ref(),
            TemplateKind::Reminder => self.reminder.as_ref(),
            TemplateKind::Degraded => self.degraded.as_ref(),
            TemplateKind::Recovered => self.recovered.as_ref(),
        }
    }

    /// Trims every template and turns blank ones into `None` (= back to the default).
    #[must_use]
    pub fn normalized(self) -> Self {
        let clean = |t: Option<String>| t.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Self {
            down: clean(self.down),
            reminder: clean(self.reminder),
            degraded: clean(self.degraded),
            recovered: clean(self.recovered),
        }
    }

    /// The first template longer than `MAX_TEMPLATE_CHARS`, if any.
    #[must_use]
    pub fn too_long(&self) -> Option<TemplateKind> {
        TemplateKind::ALL.into_iter().find(|k| {
            self.get(*k)
                .is_some_and(|t| t.chars().count() > MAX_TEMPLATE_CHARS)
        })
    }
}

/// The values a template is filled with.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TemplateVars {
    pub app: String,
    pub message: String,
    /// `412 ms`, or empty when the check got no response.
    pub latency: String,
    pub link: String,
    pub mentions: String,
    /// `25 min`, or empty when there's no outage to measure.
    pub duration: String,
}

/// Fills `template` and tidies what empty placeholders leave behind: leading/trailing
/// spaces on each line, empty `()`, and blank lines at the end (e.g. an empty `{link}`).
/// `static/settings.js` mirrors this for the live preview.
#[must_use]
pub fn render(template: &str, vars: &TemplateVars) -> String {
    let filled = template
        .replace("{app}", &vars.app)
        .replace("{message}", &vars.message)
        .replace("{latency}", &vars.latency)
        .replace("{link}", &vars.link)
        .replace("{mentions}", &vars.mentions)
        .replace("{duration}", &vars.duration)
        .replace("( )", "")
        .replace("()", "");
    let lines: Vec<String> = filled
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    let end = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(0, |i| i + 1);
    lines[..end].join("\n")
}

/// Custom templates plus the language defaults, shared by the monitor (sending) and the
/// settings page (editing).
#[derive(Debug)]
pub struct TemplateStore {
    /// `None`: nothing is stored (tests).
    db: Option<Db>,
    custom: RwLock<AlertTemplates>,
    defaults: AlertTemplates,
}

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a valid templates file: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Db(#[from] DbError),
}

/// Replaces every stored template with `templates` (already normalized).
pub(crate) fn save_templates(
    tx: &rusqlite::Transaction<'_>,
    templates: &AlertTemplates,
) -> Result<(), DbError> {
    tx.execute("DELETE FROM alert_templates", [])?;
    let mut insert =
        tx.prepare_cached("INSERT INTO alert_templates (kind, text) VALUES (?1, ?2)")?;
    for kind in TemplateKind::ALL {
        if let Some(text) = templates.get(kind) {
            insert.execute([kind.as_str(), text])?;
        }
    }
    Ok(())
}

/// Reads a 0.2 `alert_templates.json` (for `heartbeat migrate`); a missing file is no
/// custom templates.
pub(crate) fn read_legacy_file(path: &Path) -> Result<AlertTemplates, TemplateError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(serde_json::from_str::<AlertTemplates>(&raw)
            .map_err(|source| TemplateError::Json {
                path: path.to_path_buf(),
                source,
            })?
            .normalized()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AlertTemplates::default()),
        Err(e) => Err(TemplateError::io(path)(e)),
    }
}

impl TemplateError {
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        move |source| Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

impl TemplateStore {
    fn defaults(i18n: &I18n) -> AlertTemplates {
        AlertTemplates {
            down: Some(i18n.text("alert.default_down", &[])),
            reminder: Some(i18n.text("alert.default_reminder", &[])),
            degraded: Some(i18n.text("alert.default_degraded", &[])),
            recovered: Some(i18n.text("alert.default_recovered", &[])),
        }
    }

    /// Loads the custom templates (none stored = none customized).
    pub async fn load(db: Db, i18n: &I18n) -> Result<Self, TemplateError> {
        let custom = db
            .read(|conn| {
                let mut custom = AlertTemplates::default();
                let mut stmt = conn.prepare("SELECT kind, text FROM alert_templates")?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let kind: String = row.get(0)?;
                    if let Some(kind) = TemplateKind::ALL.into_iter().find(|k| k.as_str() == kind) {
                        *custom.slot(kind) = Some(row.get(1)?);
                    }
                }
                Ok(custom.normalized())
            })
            .await?;
        Ok(Self {
            db: Some(db),
            custom: RwLock::new(custom),
            defaults: Self::defaults(i18n),
        })
    }

    /// Defaults only, nothing persisted (tests).
    #[must_use]
    pub fn in_memory(i18n: &I18n) -> Self {
        Self {
            db: None,
            custom: RwLock::new(AlertTemplates::default()),
            defaults: Self::defaults(i18n),
        }
    }

    #[must_use]
    pub fn custom(&self) -> AlertTemplates {
        self.custom
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn default_for(&self, kind: TemplateKind) -> String {
        self.defaults.get(kind).cloned().unwrap_or_default()
    }

    /// Whether `kind` has a custom template (vs. the language default).
    #[must_use]
    pub fn is_custom(&self, kind: TemplateKind) -> bool {
        self.custom().get(kind).is_some()
    }

    /// The template in effect for `kind`: the custom one, else the default.
    #[must_use]
    pub fn effective(&self, kind: TemplateKind) -> String {
        self.custom()
            .get(kind)
            .cloned()
            .unwrap_or_else(|| self.default_for(kind))
    }

    /// Replaces the custom templates (blank = default) and stores them.
    pub async fn save(&self, templates: AlertTemplates) -> Result<(), TemplateError> {
        let templates = templates.normalized();
        if let Some(db) = &self.db {
            let stored = templates.clone();
            db.write(move |tx| save_templates(tx, &stored)).await?;
        }
        *self.custom.write().unwrap_or_else(PoisonError::into_inner) = templates;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;

    fn vars() -> TemplateVars {
        TemplateVars {
            app: "Billing".into(),
            message: "HTTP 503".into(),
            latency: "412 ms".into(),
            link: "https://status.example.com/#billing".into(),
            mentions: "<@U1>".into(),
            duration: "25 min".into(),
        }
    }

    #[test]
    fn render_fills_every_placeholder() {
        let out = render("{mentions} {app}: {message} ({latency})\n{link}", &vars());
        assert_eq!(
            out,
            "<@U1> Billing: HTTP 503 (412 ms)\nhttps://status.example.com/#billing"
        );
    }

    #[test]
    fn render_tidies_what_empty_placeholders_leave() {
        let empty = TemplateVars {
            app: "Billing".into(),
            ..TemplateVars::default()
        };
        assert_eq!(
            render("{mentions} 🔴 {app} ({latency})\n{link}", &empty),
            "🔴 Billing"
        );
    }

    #[test]
    fn defaults_follow_the_language() {
        let es = TemplateStore::in_memory(&I18n::new(Lang::Es));
        let en = TemplateStore::in_memory(&I18n::new(Lang::En));
        assert!(es.effective(TemplateKind::Down).contains("está caída"));
        assert!(en.effective(TemplateKind::Down).contains("is down"));
    }

    #[tokio::test]
    async fn custom_templates_persist_and_blank_means_default() {
        let db = Db::open_in_memory();
        let i18n = I18n::new(Lang::Es);
        let store = TemplateStore::load(db.clone(), &i18n).await.unwrap();
        store
            .save(AlertTemplates {
                down: Some("  ALERTA {app}  ".into()),
                degraded: Some("   ".into()),
                ..AlertTemplates::default()
            })
            .await
            .unwrap();
        assert_eq!(store.effective(TemplateKind::Down), "ALERTA {app}");
        assert_eq!(
            store.effective(TemplateKind::Degraded),
            store.default_for(TemplateKind::Degraded)
        );

        let reloaded = TemplateStore::load(db, &i18n).await.unwrap();
        assert_eq!(reloaded.effective(TemplateKind::Down), "ALERTA {app}");
        assert_eq!(reloaded.custom().degraded, None);
    }

    #[test]
    fn too_long_is_reported() {
        let t = AlertTemplates {
            recovered: Some("x".repeat(MAX_TEMPLATE_CHARS + 1)),
            ..AlertTemplates::default()
        };
        assert_eq!(t.too_long(), Some(TemplateKind::Recovered));
    }
}
