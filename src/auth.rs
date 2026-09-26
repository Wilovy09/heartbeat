//! Session handling. There is no user database here -- the login (see `routes::login`)
//! decides who is an admin, and a successful login gets a session in `SessionStore`, keyed
//! by a random ID. Only that ID goes in the (`HttpOnly`) cookie, so a cookie is worth
//! something only if this process issued it: forging or guessing one gets nothing, and
//! logging out really ends the session. In `AuthMode::Upstream` the session also holds the
//! login server's JWT, forwarded as a Bearer token to registered apps' logs endpoints.
//!
//! Sessions are mirrored to `SESSIONS_FILE` (mode 600) so a restart or deploy doesn't log
//! everyone out.
//!
//! A session is either an admin's or a viewer's (`Role`). Viewers see the dashboard and
//! the uptime API, read-only; every other page and every change needs `require_admin`.

use actix_web::cookie::{Cookie, SameSite, time::Duration as CookieDuration};
use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

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

#[derive(Clone, Serialize, Deserialize)]
struct Session {
    /// The login server's JWT (`AuthMode::Upstream`); empty in `AuthMode::Password`.
    upstream_token: String,
    /// Unix seconds.
    expires_at: u64,
    #[serde(default)]
    role: Role,
}

/// A live session, as handlers see it.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// The upstream JWT; empty when there is none.
    pub token: String,
    pub role: Role,
}

impl SessionInfo {
    #[must_use]
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

pub struct SessionStore {
    path: PathBuf,
    // std RwLock: every critical section is a short map operation, never held across .await.
    sessions: RwLock<HashMap<String, Session>>,
    /// Serializes file writes so two concurrent logins can't persist out of order.
    write_lock: tokio::sync::Mutex<()>,
}

impl SessionStore {
    /// Loads the persisted sessions (dropping expired ones); a missing file is an empty store.
    pub async fn load(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let mut sessions: HashMap<String, Session> = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => serde_json::from_str(&raw).map_err(|source| SessionError::Json {
                path: path.clone(),
                source,
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(SessionError::io(&path)(e)),
        };
        let now = unix_now();
        sessions.retain(|_, s| s.expires_at > now);
        Ok(Self {
            path,
            sessions: RwLock::new(sessions),
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Writes the current sessions to disk: temp file (mode 600) + rename, so a crash never
    /// leaves a half-written file and the tokens are never world-readable.
    async fn persist(&self) -> Result<(), SessionError> {
        let _guard = self.write_lock.lock().await;
        let raw = {
            let sessions = self.sessions.read().unwrap_or_else(PoisonError::into_inner);
            serde_json::to_vec(&*sessions).map_err(|source| SessionError::Json {
                path: self.path.clone(),
                source,
            })?
        };
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(SessionError::io(parent))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&tmp).await.map_err(SessionError::io(&tmp))?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &raw)
            .await
            .map_err(SessionError::io(&tmp))?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(SessionError::io(&self.path))
    }

    /// Starts a session for an already verified user; returns its ID. `upstream_token` is
    /// the login server's JWT, or empty when there is none (`AuthMode::Password`).
    pub async fn create(&self, upstream_token: String, role: Role) -> Result<String, SessionError> {
        let id = token::random_hex(SESSION_ID_BYTES)?;
        let now = unix_now();
        {
            let mut sessions = self
                .sessions
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            sessions.retain(|_, s| s.expires_at > now);
            sessions.insert(
                id.clone(),
                Session {
                    upstream_token,
                    expires_at: now + SESSION_HOURS * 3600,
                    role,
                },
            );
        }
        self.persist().await?;
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
            })
    }

    pub async fn remove(&self, id: &str) -> Result<(), SessionError> {
        let removed = self
            .sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .is_some();
        if removed {
            self.persist().await?;
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
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sessions.json");
        let store = SessionStore::load(&file).await.unwrap();
        let id = store.create("jwt".to_string(), Role::Admin).await.unwrap();
        assert_eq!(id.len(), 64);
        assert_eq!(store.get(&id).map(|s| s.token).as_deref(), Some("jwt"));
        assert_eq!(store.get("cualquier-cosa").map(|s| s.token), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "session file must not be world-readable"
            );
        }

        let viewer = store.create(String::new(), Role::Viewer).await.unwrap();
        let reloaded = SessionStore::load(&file).await.unwrap();
        assert_eq!(reloaded.get(&viewer).map(|s| s.role), Some(Role::Viewer));
        assert_eq!(
            reloaded.get(&id).map(|s| s.role),
            Some(Role::Admin),
            "roles survive a reload"
        );
        assert_eq!(reloaded.get(&id).map(|s| s.token).as_deref(), Some("jwt"));

        reloaded.remove(&id).await.unwrap();
        assert_eq!(reloaded.get(&id).map(|s| s.token), None);
        let after_logout = SessionStore::load(&file).await.unwrap();
        assert_eq!(after_logout.get(&id).map(|s| s.token), None);
    }

    #[tokio::test]
    async fn expired_sessions_do_not_resolve_nor_load() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sessions.json");
        std::fs::write(&file, r#"{"old":{"upstream_token":"jwt","expires_at":1}}"#).unwrap();
        let store = SessionStore::load(&file).await.unwrap();
        assert_eq!(store.get("old").map(|s| s.token), None);
    }
}
