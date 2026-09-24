//! Session handling. There is no user database here -- the JWT returned by the upstream
//! login endpoint (already admin-checked there, see `routes::login`) is stored verbatim as
//! an HttpOnly cookie and forwarded as a Bearer token to every registered app's logs
//! endpoint. Each of those endpoints re-checks admin status itself (live, against its own
//! DB) before returning anything, so this app never needs to trust or even decode the
//! token's contents -- it only needs to hold it and hand it back.

use actix_web::cookie::{Cookie, SameSite, time::Duration as CookieDuration};
use actix_web::{HttpRequest, HttpResponse};

pub const SESSION_COOKIE: &str = "heartbeat_session";

/// The cookie only bounds how long the browser holds onto the token: if the token itself
/// expires sooner, every proxied logs call starts coming back 401 and the dashboard sends
/// the browser back to /login (see static/app.js's handling of a 401 response).
const SESSION_COOKIE_HOURS: i64 = 12;

pub fn session_token(req: &HttpRequest) -> Option<String> {
    req.cookie(SESSION_COOKIE).map(|c| c.value().to_string())
}

/// Guard for every page/API route that requires a logged-in admin. Returns the session
/// token on success, or a ready-to-return redirect-to-/login response otherwise -- callers
/// do `let token = match auth::require_session(&req) { Ok(t) => t, Err(resp) => return resp };`.
#[allow(clippy::result_large_err)] // HttpResponse IS the value on this path, not a hot loop
pub fn require_session(req: &HttpRequest) -> Result<String, HttpResponse> {
    session_token(req).ok_or_else(|| {
        HttpResponse::Found()
            .append_header(("Location", "/login"))
            .finish()
    })
}

pub fn build_session_cookie(token: String, secure: bool) -> Cookie<'static> {
    Cookie::build(SESSION_COOKIE, token)
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::hours(SESSION_COOKIE_HOURS))
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
