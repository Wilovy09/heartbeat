//! Incident notices for the apps' public status pages (`/status/{slug}`): what admins tell
//! visitors about an outage ("investigating", "identified"...), next to the automatic
//! traffic lights. Each notice names the apps it affects; none named = every app.
//! Persisted to one JSON file (`NOTICES_FILE`); a handful of entries, rewritten whole on
//! every change like the app registry.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

use crate::uptime::unix_now;

/// Longest title and body accepted from the form.
const MAX_TITLE_CHARS: usize = 120;
const MAX_BODY_CHARS: usize = 2000;
/// How long a resolved notice stays on /status under "past incidents".
pub const RESOLVED_VISIBLE_SECS: u64 = 7 * 24 * 3600;

#[derive(Debug, thiserror::Error)]
pub enum NoticeError {
    #[error("the title can't be empty")]
    EmptyTitle,
    #[error("the title or text is too long")]
    TooLong,
    #[error("notice '{0}' not found")]
    NotFound(String),
    #[error("could not generate an ID: {0}")]
    Id(#[from] getrandom::Error),
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a valid notices file: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

impl NoticeError {
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        move |source| Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

impl crate::i18n::Localize for NoticeError {
    fn localize(&self, i18n: &crate::i18n::I18n) -> String {
        match self {
            Self::EmptyTitle => i18n.text("notices.err_title", &[]),
            Self::TooLong => i18n.text(
                "notices.err_long",
                &[
                    ("title", &MAX_TITLE_CHARS.to_string()),
                    ("body", &MAX_BODY_CHARS.to_string()),
                ],
            ),
            Self::NotFound(_) | Self::Id(_) | Self::Io { .. } | Self::Json { .. } => {
                i18n.text("err.internal", &[("error", &self.to_string())])
            }
        }
    }
}

/// Where an incident stands, in the usual status-page vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeState {
    Investigating,
    Identified,
    Monitoring,
    Resolved,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notice {
    pub id: String,
    pub title: String,
    pub body: String,
    pub state: NoticeState,
    pub created_at: u64,
    pub updated_at: u64,
    /// Set when the state first becomes `Resolved`; cleared if it's reopened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<u64>,
    /// Slugs of the affected apps; empty = all of them (and every notice saved before
    /// notices were per app).
    #[serde(default)]
    pub apps: Vec<String>,
}

impl Notice {
    #[must_use]
    pub fn affects(&self, slug: &str) -> bool {
        self.apps.is_empty() || self.apps.iter().any(|a| a == slug)
    }
}

/// What the admin form submits.
#[derive(Debug, Clone)]
pub struct NoticeDraft {
    pub title: String,
    pub body: String,
    pub state: NoticeState,
    pub apps: Vec<String>,
}

impl NoticeDraft {
    fn validated(self) -> Result<Self, NoticeError> {
        let title = self.title.trim().to_string();
        let body = self.body.trim().to_string();
        if title.is_empty() {
            return Err(NoticeError::EmptyTitle);
        }
        if title.chars().count() > MAX_TITLE_CHARS || body.chars().count() > MAX_BODY_CHARS {
            return Err(NoticeError::TooLong);
        }
        let mut apps: Vec<String> = self
            .apps
            .into_iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
        apps.sort();
        apps.dedup();
        Ok(Self {
            title,
            body,
            apps,
            ..self
        })
    }
}

/// The notices an app's status page shows: open ones, and those resolved recently.
#[derive(Debug, Default, Serialize)]
pub struct PublicNotices {
    pub open: Vec<Notice>,
    pub recent: Vec<Notice>,
}

pub struct NoticeStore {
    path: PathBuf,
    notices: RwLock<Vec<Notice>>,
}

impl NoticeStore {
    /// Loads `path`; a missing file is an empty store.
    pub async fn load(path: impl Into<PathBuf>) -> Result<Self, NoticeError> {
        let path = path.into();
        let notices = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => serde_json::from_str(&raw).map_err(|source| NoticeError::Json {
                path: path.clone(),
                source,
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(NoticeError::io(&path)(e)),
        };
        Ok(Self {
            path,
            notices: RwLock::new(notices),
        })
    }

    async fn persist(&self, notices: &[Notice]) -> Result<(), NoticeError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(NoticeError::io(parent))?;
        }
        let raw = serde_json::to_string_pretty(notices).map_err(|source| NoticeError::Json {
            path: self.path.clone(),
            source,
        })?;
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, raw)
            .await
            .map_err(NoticeError::io(&tmp))?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(NoticeError::io(&self.path))
    }

    /// Newest first.
    pub async fn list(&self) -> Vec<Notice> {
        let mut notices = self.notices.read().await.clone();
        notices.sort_by_key(|n| std::cmp::Reverse(n.created_at));
        notices
    }

    /// What `slug`'s status page shows.
    pub async fn public_for(&self, slug: &str, now: u64) -> PublicNotices {
        let (open, recent) = self
            .list()
            .await
            .into_iter()
            .filter(|n| n.affects(slug))
            .filter(|n| {
                n.resolved_at
                    .is_none_or(|at| now.saturating_sub(at) < RESOLVED_VISIBLE_SECS)
            })
            .partition(|n| n.state != NoticeState::Resolved);
        PublicNotices { open, recent }
    }

    pub async fn create(&self, draft: NoticeDraft) -> Result<String, NoticeError> {
        let draft = draft.validated()?;
        let now = unix_now();
        let id = crate::token::random_hex(8)?;
        let mut notices = self.notices.write().await;
        notices.push(Notice {
            id: id.clone(),
            title: draft.title,
            body: draft.body,
            state: draft.state,
            created_at: now,
            updated_at: now,
            resolved_at: (draft.state == NoticeState::Resolved).then_some(now),
            apps: draft.apps,
        });
        self.persist(&notices).await?;
        Ok(id)
    }

    pub async fn update(&self, id: &str, draft: NoticeDraft) -> Result<(), NoticeError> {
        let draft = draft.validated()?;
        let now = unix_now();
        let mut notices = self.notices.write().await;
        let notice = notices
            .iter_mut()
            .find(|n| n.id == id)
            .ok_or_else(|| NoticeError::NotFound(id.to_string()))?;
        notice.resolved_at = match draft.state {
            NoticeState::Resolved => notice.resolved_at.or(Some(now)),
            NoticeState::Investigating | NoticeState::Identified | NoticeState::Monitoring => None,
        };
        notice.title = draft.title;
        notice.body = draft.body;
        notice.state = draft.state;
        notice.apps = draft.apps;
        notice.updated_at = now;
        self.persist(&notices).await
    }

    pub async fn remove(&self, id: &str) -> Result<(), NoticeError> {
        let mut notices = self.notices.write().await;
        let before = notices.len();
        notices.retain(|n| n.id != id);
        if notices.len() == before {
            return Err(NoticeError::NotFound(id.to_string()));
        }
        self.persist(&notices).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(title: &str, state: NoticeState) -> NoticeDraft {
        NoticeDraft {
            title: title.into(),
            body: "  We're on it.  ".into(),
            state,
            apps: Vec::new(),
        }
    }

    #[tokio::test]
    async fn notices_persist_and_resolving_moves_them_to_recent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notices.json");
        let store = NoticeStore::load(&file).await.unwrap();
        let id = store
            .create(draft(" Slow checkout ", NoticeState::Investigating))
            .await
            .unwrap();
        let public = store.public_for("any", unix_now()).await;
        assert_eq!(public.open.len(), 1);
        assert_eq!(public.open[0].title, "Slow checkout");
        assert_eq!(public.open[0].body, "We're on it.");

        store
            .update(&id, draft("Slow checkout", NoticeState::Resolved))
            .await
            .unwrap();
        let reloaded = NoticeStore::load(&file).await.unwrap();
        let public = reloaded.public_for("any", unix_now()).await;
        assert!(public.open.is_empty());
        assert_eq!(public.recent.len(), 1);
        assert!(public.recent[0].resolved_at.is_some());

        let later = unix_now() + RESOLVED_VISIBLE_SECS + 1;
        assert!(
            reloaded.public_for("any", later).await.recent.is_empty(),
            "old ones drop off"
        );

        reloaded.remove(&id).await.unwrap();
        assert!(reloaded.list().await.is_empty());
    }

    #[tokio::test]
    async fn notices_show_only_on_the_apps_they_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoticeStore::load(dir.path().join("n.json")).await.unwrap();
        let mut billing = draft("Billing down", NoticeState::Investigating);
        billing.apps = vec![" billing ".into(), "billing".into()];
        store.create(billing).await.unwrap();
        store
            .create(draft("Everything slow", NoticeState::Identified))
            .await
            .unwrap();
        let now = unix_now();
        assert_eq!(store.public_for("billing", now).await.open.len(), 2);
        let other = store.public_for("search", now).await.open;
        assert_eq!(other.len(), 1, "a notice naming no app applies to all");
        assert_eq!(other[0].title, "Everything slow");
        let named: Vec<_> = store
            .list()
            .await
            .into_iter()
            .filter(|n| !n.apps.is_empty())
            .collect();
        assert_eq!(named[0].apps, ["billing"], "trimmed and deduplicated");
    }

    #[tokio::test]
    async fn drafts_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoticeStore::load(dir.path().join("n.json")).await.unwrap();
        assert!(matches!(
            store.create(draft("   ", NoticeState::Identified)).await,
            Err(NoticeError::EmptyTitle)
        ));
        assert!(matches!(
            store
                .create(draft(
                    &"x".repeat(MAX_TITLE_CHARS + 1),
                    NoticeState::Identified
                ))
                .await,
            Err(NoticeError::TooLong)
        ));
    }
}
