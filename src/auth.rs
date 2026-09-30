//! Session handling. There is no user database here -- the login (see `routes::login`)
//! decides who is an admin, and a successful login gets a session in `SessionStore`, keyed
//! by a random ID. Only that ID goes in the (`HttpOnly`) cookie, so a cookie is worth
//! something only if this process issued it: forging or guessing one gets nothing, and
//! logging out really ends the session. In `AuthMode::Upstream` the session also holds the
//! login server's JWT, forwarded as a Bearer token to registered apps' logs endpoints.
//!
//! Sessions are stored in the database (the `sessions` table; the file is mode 600) so a
//! restart or deploy doesn't log everyone out, with a copy in memory for the lookup every
//! request does.
//!
//! A session is either an admin's or a viewer's (`Role`). Viewers see the dashboard and
//! the uptime API, read-only; every other page and every change needs `require_admin`.

use actix_web::cookie::{Cookie, SameSite, time::Duration as CookieDuration};
use actix_web::{HttpRequest, HttpResponse, web};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::{Db, DbError};
use crate::token;

pub const SESSION_COOKIE: &str = "heartbeat_session";

const SESSION_HOURS: u64 = 12;
/// 32 bytes = 256 bits: not guessable, so a `HashMap` lookup's timing leaks nothing useful.
const SESSION_ID_BYTES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("could not generate a session ID: {0}")]
    Token(#[from] getrandom::Error),
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a valid sessions file: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Db(#[from] DbError),
}

impl SessionError {
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        move |source| Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What a session may do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Everything. Sessions saved before roles existed are admins.
    #[default]
    Admin,
    /// The dashboard and uptime data, read-only: no logs, settings or changes.
    Viewer,
}

impl Role {
    /// How the `sessions.role` column spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Viewer => "viewer",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Session {
    /// The login server's JWT (`AuthMode::Upstream`); empty in `AuthMode::Password`.
    upstream_token: String,
    /// Unix seconds.
    expires_at: u64,
    #[serde(default)]
    role: Role,
    /// Who logged in (lowercased); empty for sessions from before it was kept.
    #[serde(default)]
    email: String,
}

/// A live session, as handlers see it.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// The upstream JWT; empty when there is none.
    pub token: String,
    pub role: Role,
    /// Who logged in; empty for a session started before Heartbeat kept it.
    pub email: String,
}

impl SessionInfo {
    #[must_use]
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

pub struct SessionStore {
    db: Db,
    // std RwLock: every critical section is a short map operation, never held across .await.
    sessions: RwLock<HashMap<String, Session>>,
}

/// Stores one session (a new one, or one imported by `heartbeat migrate`).
pub(crate) fn save_session(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    session: &Session,
) -> Result<(), DbError> {
    tx.prepare_cached(
        "INSERT INTO sessions (id, upstream_token, role, expires_at, email) \
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (id) DO NOTHING",
    )?
    .execute(params![
        id,
        session.upstream_token,
        session.role.as_str(),
        i64::try_from(session.expires_at).unwrap_or(i64::MAX),
        session.email,
    ])?;
    Ok(())
}

/// Reads a 0.2 `sessions.json` (for `heartbeat migrate`), live sessions only. A missing
/// file is an empty list.
pub(crate) fn read_legacy_file(path: &Path) -> Result<Vec<(String, Session)>, SessionError> {
    let sessions: HashMap<String, Session> = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|source| SessionError::Json {
            path: path.to_path_buf(),
            source,
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(e) => return Err(SessionError::io(path)(e)),
    };
    let now = unix_now();
    Ok(sessions
        .into_iter()
        .filter(|(_, s)| s.expires_at > now)
        .collect())
}

impl SessionStore {
    /// Loads the live sessions; expired ones are deleted on the way.
    pub async fn load(db: Db) -> Result<Self, SessionError> {
        let now = i64::try_from(unix_now()).unwrap_or(i64::MAX);
        let sessions = db
            .write(move |tx| {
                tx.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now])?;
                let mut stmt =
                    tx.prepare("SELECT id, upstream_token, role, expires_at, email FROM sessions")?;
                let sessions = stmt
                    .query_map([], |row| {
                        let role: String = row.get(2)?;
                        Ok((
                            row.get::<_, String>(0)?,
                            Session {
                                upstream_token: row.get(1)?,
                                expires_at: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                                role: if role == "viewer" {
                                    Role::Viewer
                                } else {
                                    Role::Admin
                                },
                                email: row.get(4)?,
                            },
                        ))
                    })?
                    .collect::<rusqlite::Result<HashMap<_, _>>>()?;
                Ok(sessions)
            })
            .await?;
        Ok(Self {
            db,
            sessions: RwLock::new(sessions),
        })
    }

    /// Starts a session for an already verified user; returns its ID. `upstream_token` is
    /// the login server's JWT, or empty when there is none (`AuthMode::Password`).
    pub async fn create(
        &self,
        email: &str,
        upstream_token: String,
        role: Role,
    ) -> Result<String, SessionError> {
        let id = token::random_hex(SESSION_ID_BYTES)?;
        let now = unix_now();
        let session = Session {
            upstream_token,
            expires_at: now + SESSION_HOURS * 3600,
            role,
            email: email.trim().to_lowercase(),
        };
        let (stored_id, stored) = (id.clone(), session.clone());
        self.db
            .write(move |tx| {
                tx.execute(
                    "DELETE FROM sessions WHERE expires_at <= ?1",
                    [i64::try_from(now).unwrap_or(i64::MAX)],
                )?;
                save_session(tx, &stored_id, &stored)
            })
            .await?;
        let mut sessions = self
            .sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        sessions.retain(|_, s| s.expires_at > now);
        sessions.insert(id.clone(), session);
        Ok(id)
    }

    /// A live session, `None` if unknown or expired.
    fn get(&self, id: &str) -> Option<SessionInfo> {
        let sessions = self.sessions.read().unwrap_or_else(PoisonError::into_inner);
        sessions
            .get(id)
            .filter(|s| s.expires_at > unix_now())
            .map(|s| SessionInfo {
                token: s.upstream_token.clone(),
                role: s.role,
                email: s.email.clone(),
            })
    }

    /// Ends a session. Gone from memory first, so it stops working even if the database
    /// write then fails.
    pub async fn remove(&self, id: &str) -> Result<(), SessionError> {
        let removed = self
            .sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .is_some();
        if removed {
            let id = id.to_string();
            self.db
                .write(move |tx| {
                    tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
                    Ok(())
                })
                .await?;
        }
        Ok(())
    }
}

/// The live session behind the request's cookie, if any. Fails closed when the store
/// isn't registered as app data.
pub fn current_session(req: &HttpRequest) -> Option<SessionInfo> {
    let id = req.cookie(SESSION_COOKIE)?;
    req.app_data::<web::Data<SessionStore>>()?.get(id.value())
}

/// The upstream token of the request's session (admin or viewer).
pub fn session_token(req: &HttpRequest) -> Option<String> {
    current_session(req).map(|s| s.token)
}

fn to_login() -> HttpResponse {
    HttpResponse::Found()
        .append_header(("Location", "/login"))
        .finish()
}

/// Guard for pages any logged-in user may see; otherwise a ready-to-return redirect to
/// /login.
#[allow(clippy::result_large_err)] // HttpResponse IS the value on this path, not a hot loop
pub fn require_session(req: &HttpRequest) -> Result<SessionInfo, HttpResponse> {
    current_session(req).ok_or_else(to_login)
}

/// Guard for admin-only pages and form posts: no session redirects to /login, a viewer's
/// gets 403.
#[allow(clippy::result_large_err)]
pub fn require_admin(req: &HttpRequest) -> Result<SessionInfo, HttpResponse> {
    let session = require_session(req)?;
    if session.is_admin() {
        Ok(session)
    } else {
        Err(HttpResponse::Forbidden().body("Admins only / Solo administradores"))
    }
}

/// `require_admin` for JSON endpoints: 401 without a session, 403 for viewers.
#[allow(clippy::result_large_err)]
pub fn require_admin_json(req: &HttpRequest) -> Result<SessionInfo, HttpResponse> {
    match current_session(req) {
        None => {
            Err(HttpResponse::Unauthorized().json(serde_json::json!({ "error": "unauthorized" })))
        }
        Some(s) if !s.is_admin() => {
            Err(HttpResponse::Forbidden().json(serde_json::json!({ "error": "admins only" })))
        }
        Some(s) => Ok(s),
    }
}

/// A page's base template context: which nav entry is active and whether the user is an
/// admin (viewers get a read-only nav).
#[must_use]
pub fn page_context(session: &SessionInfo, active: &str) -> tera::Context {
    let mut ctx = tera::Context::new();
    ctx.insert("active", active);
    ctx.insert("is_admin", &session.is_admin());
    ctx
}

pub fn build_session_cookie(session_id: String, secure: bool) -> Cookie<'static> {
    Cookie::build(SESSION_COOKIE, session_id)
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::hours(
            i64::try_from(SESSION_HOURS).unwrap_or(12),
        ))
        .finish()
}

pub fn build_logout_cookie(secure: bool) -> Cookie<'static> {
    Cookie::build(SESSION_COOKIE, "")
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::ZERO)
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_issued_live_sessions_resolve_and_survive_a_reload() {
        let db = Db::open_in_memory();
        let store = SessionStore::load(db.clone()).await.unwrap();
        let id = store
            .create(" Admin@Example.com", "jwt".to_string(), Role::Admin)
            .await
            .unwrap();
        assert_eq!(id.len(), 64);
        assert_eq!(store.get(&id).map(|s| s.token).as_deref(), Some("jwt"));
        assert_eq!(
            store.get(&id).map(|s| s.email).as_deref(),
            Some("admin@example.com")
        );
        assert_eq!(store.get("cualquier-cosa").map(|s| s.token), None);

        let viewer = store
            .create("viewer@example.com", String::new(), Role::Viewer)
            .await
            .unwrap();
        let reloaded = SessionStore::load(db.clone()).await.unwrap();
        assert_eq!(reloaded.get(&viewer).map(|s| s.role), Some(Role::Viewer));
        assert_eq!(
            reloaded.get(&id).map(|s| s.role),
            Some(Role::Admin),
            "roles survive a reload"
        );
        assert_eq!(reloaded.get(&id).map(|s| s.token).as_deref(), Some("jwt"));
        assert_eq!(
            reloaded.get(&viewer).map(|s| s.email).as_deref(),
            Some("viewer@example.com"),
            "emails survive a reload"
        );

        reloaded.remove(&id).await.unwrap();
        assert_eq!(reloaded.get(&id).map(|s| s.token), None);
        let after_logout = SessionStore::load(db).await.unwrap();
        assert_eq!(after_logout.get(&id).map(|s| s.token), None);
    }

    #[tokio::test]
    async fn expired_sessions_do_not_resolve_and_are_deleted_on_load() {
        let db = Db::open_in_memory();
        db.write(|tx| {
            save_session(
                tx,
                "old",
                &Session {
                    upstream_token: "jwt".into(),
                    expires_at: 1,
                    role: Role::Admin,
                    email: String::new(),
                },
            )
        })
        .await
        .unwrap();
        let store = SessionStore::load(db.clone()).await.unwrap();
        assert_eq!(store.get("old").map(|s| s.token), None);
        let left: i64 = db
            .read(|conn| Ok(conn.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn legacy_files_keep_only_live_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sessions.json");
        let live = unix_now() + 3600;
        std::fs::write(
            &file,
            format!(
                r#"{{"old":{{"upstream_token":"a","expires_at":1}},"live":{{"upstream_token":"b","expires_at":{live},"role":"viewer"}}}}"#
            ),
        )
        .unwrap();
        let sessions = read_legacy_file(&file).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "live");
        assert_eq!(sessions[0].1.role, Role::Viewer);
    }
}
