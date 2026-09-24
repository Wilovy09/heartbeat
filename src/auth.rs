//! Session handling. There is no user database here -- the JWT returned by the upstream
//! login endpoint (already admin-checked there, see `routes::login`) is kept server-side in
//! `SessionStore`, keyed by a random session ID. Only that ID goes in the (HttpOnly) cookie,
//! so a cookie is worth something only if this process issued it: forging or guessing one
//! gets nothing, and logging out really ends the session. The JWT is still forwarded as a
//! Bearer token to registered apps' logs endpoints, which re-check admin status themselves.
//!
//! Sessions live in memory: restarting the process logs everyone out.

use actix_web::cookie::{Cookie, SameSite, time::Duration as CookieDuration};
use actix_web::{HttpRequest, HttpResponse, web};
use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};
use std::time::{Duration, Instant};

use crate::token;

pub const SESSION_COOKIE: &str = "heartbeat_session";

const SESSION_HOURS: u64 = 12;
/// 32 bytes = 256 bits: not guessable, so a HashMap lookup's timing leaks nothing useful.
const SESSION_ID_BYTES: usize = 32;

struct Session {
    upstream_token: String,
    expires_at: Instant,
}

#[derive(Default)]
pub struct SessionStore {
    // std RwLock: every critical section is a short map operation, never held across .await.
    sessions: RwLock<HashMap<String, Session>>,
}

impl SessionStore {
    /// Starts a session for an already admin-verified upstream token; returns its ID.
    pub fn create(&self, upstream_token: String) -> Result<String, getrandom::Error> {
        let id = token::random_hex(SESSION_ID_BYTES)?;
        let now = Instant::now();
        let mut sessions = self
            .sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        sessions.retain(|_, s| s.expires_at > now);
        sessions.insert(
            id.clone(),
            Session {
                upstream_token,
                expires_at: now + Duration::from_secs(SESSION_HOURS * 3600),
            },
        );
        Ok(id)
    }

    /// The upstream token of a live session, `None` if unknown or expired.
    fn upstream_token(&self, id: &str) -> Option<String> {
        let sessions = self.sessions.read().unwrap_or_else(PoisonError::into_inner);
        sessions
            .get(id)
            .filter(|s| s.expires_at > Instant::now())
            .map(|s| s.upstream_token.clone())
    }

    pub fn remove(&self, id: &str) {
        self.sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
    }
}

/// The upstream token behind the request's session cookie, if it names a live session.
/// Fails closed when the store isn't registered as app data.
pub fn session_token(req: &HttpRequest) -> Option<String> {
    let id = req.cookie(SESSION_COOKIE)?;
    req.app_data::<web::Data<SessionStore>>()?
        .upstream_token(id.value())
}

/// Guard for every page route that requires a logged-in admin. Returns the session's
/// upstream token on success, or a ready-to-return redirect-to-/login response otherwise.
#[allow(clippy::result_large_err)] // HttpResponse IS the value on this path, not a hot loop
pub fn require_session(req: &HttpRequest) -> Result<String, HttpResponse> {
    session_token(req).ok_or_else(|| {
        HttpResponse::Found()
            .append_header(("Location", "/login"))
            .finish()
    })
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

    #[test]
    fn only_issued_live_sessions_resolve() {
        let store = SessionStore::default();
        let id = store.create("jwt".to_string()).unwrap();
        assert_eq!(id.len(), 64);
        assert_eq!(store.upstream_token(&id).as_deref(), Some("jwt"));
        assert_eq!(store.upstream_token("cualquier-cosa"), None);

        store.remove(&id);
        assert_eq!(store.upstream_token(&id), None);
    }

    #[test]
    fn expired_sessions_do_not_resolve() {
        let store = SessionStore::default();
        store.sessions.write().unwrap().insert(
            "old".to_string(),
            Session {
                upstream_token: "jwt".to_string(),
                expires_at: Instant::now(),
            },
        );
        assert_eq!(store.upstream_token("old"), None);
    }
}
