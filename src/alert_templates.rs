//! Editable alert message templates, one per kind (down, reminder, degraded, recovered). Admins
//! change them from /settings; they're stored in `ALERT_TEMPLATES_FILE` and take effect
//! immediately. A kind without a custom template uses the default for `APP_LANG`
//! (`locales/<lang>.json`, keys `alert.default_*`).
//!
//! Placeholders: `{app}`, `{message}`, `{latency}` (e.g. `412 ms`), `{link}` (the app's
//! dashboard page, when `PUBLIC_URL` is set), `{mentions}` (only filled on down and
//! reminders) and `{duration}` (how long the outage has lasted, on reminders and
//! recoveries).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

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
    fn get(&self, kind: TemplateKind) -> Option<&String> {
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
    path: Option<PathBuf>,
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

    /// Loads the custom templates from `path` (a missing file = none customized).
    pub async fn load(path: impl Into<PathBuf>, i18n: &I18n) -> Result<Self, TemplateError> {
        let path = path.into();
        let custom = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => serde_json::from_str::<AlertTemplates>(&raw)
                .map_err(|source| TemplateError::Json {
                    path: path.clone(),
                    source,
                })?
                .normalized(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => AlertTemplates::default(),
            Err(e) => return Err(TemplateError::io(&path)(e)),
        };
        Ok(Self {
            path: Some(path),
            custom: RwLock::new(custom),
            defaults: Self::defaults(i18n),
        })
    }

    /// Defaults only, nothing persisted (tests).
    #[must_use]
    pub fn in_memory(i18n: &I18n) -> Self {
        Self {
            path: None,
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

    /// Replaces the custom templates (blank = default) and persists them: temp file +
    /// rename, so a crash never leaves a half-written file.
    pub async fn save(&self, templates: AlertTemplates) -> Result<(), TemplateError> {
        let templates = templates.normalized();
        if let Some(path) = &self.path {
            let raw =
                serde_json::to_string_pretty(&templates).map_err(|source| TemplateError::Json {
                    path: path.clone(),
                    source,
                })?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(TemplateError::io(parent))?;
            }
            let tmp = path.with_extension("json.tmp");
            tokio::fs::write(&tmp, raw)
                .await
                .map_err(TemplateError::io(&tmp))?;
            tokio::fs::rename(&tmp, path)
                .await
                .map_err(TemplateError::io(path))?;
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
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("alert_templates.json");
        let i18n = I18n::new(Lang::Es);
        let store = TemplateStore::load(&file, &i18n).await.unwrap();
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

        let reloaded = TemplateStore::load(&file, &i18n).await.unwrap();
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
