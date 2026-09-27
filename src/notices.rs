//! Incident notices for the apps' public status pages (`/status/{slug}`): what admins tell
//! visitors about an outage ("investigating", "identified"...), next to the automatic
//! traffic lights. Each notice names the apps it affects; none named = every app.
//! Stored in the `notices` and `notice_apps` tables, with a copy in memory (a handful of
//! rows that every status page reads); a change is written first, then applied to the copy.

use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

use crate::db::{Db, DbError};
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
    #[error(transparent)]
    Db(#[from] DbError),
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
            Self::NotFound(_) | Self::Id(_) | Self::Io { .. } | Self::Json { .. } | Self::Db(_) => {
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

impl NoticeState {
    /// How the `notices.state` column spells it (the same as the JSON).
    fn as_str(self) -> &'static str {
        match self {
            Self::Investigating => "investigating",
            Self::Identified => "identified",
            Self::Monitoring => "monitoring",
            Self::Resolved => "resolved",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "identified" => Self::Identified,
            "monitoring" => Self::Monitoring,
            "resolved" => Self::Resolved,
            _ => Self::Investigating,
        }
    }
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
    db: Db,
    notices: RwLock<Vec<Notice>>,
}

fn sql_time(at: u64) -> i64 {
    i64::try_from(at).unwrap_or(i64::MAX)
}

/// Inserts or replaces one notice and the apps it names.
pub(crate) fn save_notice(tx: &rusqlite::Transaction<'_>, notice: &Notice) -> Result<(), DbError> {
    tx.prepare_cached(
        "INSERT INTO notices (id, title, body, state, created_at, updated_at, resolved_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT (id) DO UPDATE SET title = excluded.title, body = excluded.body, \
             state = excluded.state, updated_at = excluded.updated_at, \
             resolved_at = excluded.resolved_at",
    )?
    .execute(params![
        notice.id,
        notice.title,
        notice.body,
        notice.state.as_str(),
        sql_time(notice.created_at),
        sql_time(notice.updated_at),
        notice.resolved_at.map(sql_time),
    ])?;
    tx.prepare_cached("DELETE FROM notice_apps WHERE notice_id = ?1")?
        .execute([&notice.id])?;
    let mut add = tx.prepare_cached("INSERT INTO notice_apps (notice_id, slug) VALUES (?1, ?2)")?;
    for slug in &notice.apps {
        add.execute([&notice.id, slug])?;
    }
    Ok(())
}

/// Reads a 0.2 `notices.json` (for `heartbeat migrate`). A missing file is an empty list.
pub(crate) fn read_legacy_file(path: &Path) -> Result<Vec<Notice>, NoticeError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|source| NoticeError::Json {
            path: path.to_path_buf(),
            source,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(NoticeError::io(path)(e)),
    }
}

impl NoticeStore {
    pub async fn load(db: Db) -> Result<Self, NoticeError> {
        let notices = db
            .read(|conn| {
                let time = |v: i64| u64::try_from(v).unwrap_or(0);
                let mut stmt = conn.prepare(
                    "SELECT id, title, body, state, created_at, updated_at, resolved_at \
                     FROM notices ORDER BY created_at",
                )?;
                let mut notices = stmt
                    .query_map([], |row| {
                        Ok(Notice {
                            id: row.get(0)?,
                            title: row.get(1)?,
                            body: row.get(2)?,
                            state: NoticeState::parse(&row.get::<_, String>(3)?),
                            created_at: time(row.get(4)?),
                            updated_at: time(row.get(5)?),
                            resolved_at: row.get::<_, Option<i64>>(6)?.map(time),
                            apps: Vec::new(),
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut apps = conn
                    .prepare("SELECT slug FROM notice_apps WHERE notice_id = ?1 ORDER BY slug")?;
                for notice in &mut notices {
                    notice.apps = apps
                        .query_map([&notice.id], |row| row.get(0))?
                        .collect::<rusqlite::Result<_>>()?;
                }
                Ok(notices)
            })
            .await?;
        Ok(Self {
            db,
            notices: RwLock::new(notices),
        })
    }

    /// Writes `notice`; the caller updates the in-memory copy after.
    async fn save(&self, notice: Notice) -> Result<Notice, NoticeError> {
        Ok(self
            .db
            .write(move |tx| {
                save_notice(tx, &notice)?;
                Ok(notice)
            })
            .await?)
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
        let notice = self
            .save(Notice {
                id: id.clone(),
                title: draft.title,
                body: draft.body,
                state: draft.state,
                created_at: now,
                updated_at: now,
                resolved_at: (draft.state == NoticeState::Resolved).then_some(now),
                apps: draft.apps,
            })
            .await?;
        notices.push(notice);
        Ok(id)
    }

    pub async fn update(&self, id: &str, draft: NoticeDraft) -> Result<(), NoticeError> {
        let draft = draft.validated()?;
        let now = unix_now();
        let mut notices = self.notices.write().await;
        let slot = notices
            .iter_mut()
            .find(|n| n.id == id)
            .ok_or_else(|| NoticeError::NotFound(id.to_string()))?;
        let changed = Notice {
            resolved_at: match draft.state {
                NoticeState::Resolved => slot.resolved_at.or(Some(now)),
                NoticeState::Investigating | NoticeState::Identified | NoticeState::Monitoring => {
                    None
                }
            },
            title: draft.title,
            body: draft.body,
            state: draft.state,
            apps: draft.apps,
            updated_at: now,
            ..slot.clone()
        };
        *slot = self.save(changed).await?;
        Ok(())
    }

    pub async fn remove(&self, id: &str) -> Result<(), NoticeError> {
        let mut notices = self.notices.write().await;
        if !notices.iter().any(|n| n.id == id) {
            return Err(NoticeError::NotFound(id.to_string()));
        }
        let owned = id.to_string();
        self.db
            .write(move |tx| {
                tx.execute("DELETE FROM notices WHERE id = ?1", [owned])?;
                Ok(())
            })
            .await?;
        notices.retain(|n| n.id != id);
        Ok(())
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
        let db = Db::open_in_memory();
        let store = NoticeStore::load(db.clone()).await.unwrap();
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
        let reloaded = NoticeStore::load(db.clone()).await.unwrap();
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
        assert!(NoticeStore::load(db).await.unwrap().list().await.is_empty());
    }

    #[tokio::test]
    async fn notices_show_only_on_the_apps_they_name() {
        let db = Db::open_in_memory();
        let store = NoticeStore::load(db.clone()).await.unwrap();
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
        let reloaded = NoticeStore::load(db).await.unwrap();
        assert_eq!(
            reloaded.public_for("search", now).await.open.len(),
            1,
            "apps survive a reload"
        );
    }

    #[tokio::test]
    async fn drafts_are_validated() {
        let store = NoticeStore::load(Db::open_in_memory()).await.unwrap();
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
