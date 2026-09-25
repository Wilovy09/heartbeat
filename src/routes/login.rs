//! Login, in one of two modes (`config::AuthMode`):
//! - `Upstream`: credentials go to an external login endpoint, and the browser gets in only
//!   if its response says `is_admin: true` -- that verdict is the login server's own
//!   decision, this app never re-implements it.
//! - `Password`: one local admin, checked against an argon2 hash.
//!
//! Failed attempts are throttled per IP and per email (`security::LoginLimiter`).

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;
use tera::{Context, Tera};

use crate::{
    auth::{self, SessionStore},
    config::{AuthMode, Config},
    i18n::I18n,
    security::{self, LoginLimiter},
};

fn render_login(tera: &Tera, error: Option<&str>, email: &str) -> HttpResponse {
    let mut ctx = Context::new();
    ctx.insert("error", &error);
    ctx.insert("email", email);
    match tera.render("login.html", &ctx) {
        Ok(html) => HttpResponse::Ok().content_type("text/html").body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}

pub async fn show_login(tera: web::Data<Tera>) -> HttpResponse {
    render_login(&tera, None, "")
}

#[derive(Deserialize)]
pub struct LoginForm {
    email: String,
    password: String,
}

/// Why a login attempt didn't produce a session. Only `Rejected` counts toward the
/// brute-force throttle: an unreachable login server says nothing about the password.
/// Both carry a catalog key; `Rejected` may also carry the login server's own message,
/// shown as-is when present.
enum LoginFailure {
    Rejected {
        key: &'static str,
        upstream: Option<String>,
    },
    Unavailable(&'static str),
}

impl LoginFailure {
    fn rejected(key: &'static str) -> Self {
        Self::Rejected {
            key,
            upstream: None,
        }
    }

    fn message(&self, i18n: &I18n) -> String {
        match self {
            Self::Rejected {
                upstream: Some(msg),
                ..
            } => msg.clone(),
            Self::Rejected { key, .. } | Self::Unavailable(key) => i18n.text(key, &[]),
        }
    }
}

/// Checks the credentials; on success returns the token to keep in the session (empty when
/// the mode has none).
async fn authenticate(cfg: &Config, form: &LoginForm) -> Result<String, LoginFailure> {
    match &cfg.auth {
        AuthMode::Upstream { login_url } => authenticate_upstream(login_url, form).await,
        AuthMode::Password {
            email,
            password_hash,
        } => authenticate_password(email, password_hash, form).await,
    }
}

async fn authenticate_password(
    admin_email: &str,
    password_hash: &str,
    form: &LoginForm,
) -> Result<String, LoginFailure> {
    let hash = password_hash.to_string();
    let password = form.password.clone();
    // argon2 is deliberately slow CPU work: keep it off the async executor. The password is
    // verified even when the email is wrong, so timing doesn't reveal which one failed.
    let password_ok =
        tokio::task::spawn_blocking(move || crate::password::verify(&password, &hash))
            .await
            .unwrap_or(false);
    if password_ok && form.email.trim().eq_ignore_ascii_case(admin_email) {
        Ok(String::new())
    } else {
        Err(LoginFailure::rejected("login.invalid"))
    }
}

async fn authenticate_upstream(login_url: &str, form: &LoginForm) -> Result<String, LoginFailure> {
    let resp = reqwest::Client::new()
        .post(login_url)
        .json(&serde_json::json!({ "email": form.email, "password": form.password }))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "login: could not reach the login endpoint");
            LoginFailure::Unavailable("login.unreachable")
        })?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|_| LoginFailure::Unavailable("login.bad_response"))?;

    if !status.is_success() {
        return Err(LoginFailure::Rejected {
            key: "login.invalid",
            upstream: body
                .get("error")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        });
    }

    let is_admin = body
        .get("is_admin")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !is_admin {
        tracing::info!(email = %form.email, "login: valid user without admin rights, denied");
        return Err(LoginFailure::rejected("login.not_admin"));
    }

    body.get("access_token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or(LoginFailure::Unavailable("login.no_token"))
}

pub async fn submit_login(
    req: HttpRequest,
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    i18n: web::Data<I18n>,
    sessions: web::Data<SessionStore>,
    limiter: web::Data<LoginLimiter>,
    form: web::Form<LoginForm>,
) -> HttpResponse {
    let ip = security::client_ip(&req);
    if let Some(wait) = limiter.locked_for(&ip, &form.email) {
        tracing::warn!(%ip, email = %form.email, "login: throttled");
        let minutes = wait.as_secs().div_ceil(60).max(1).to_string();
        let msg = i18n.text("login.throttled", &[("minutes", &minutes)]);
        return render_login(&tera, Some(&msg), &form.email);
    }

    let token = match authenticate(&cfg, &form).await {
        Ok(token) => token,
        Err(failure) => {
            if matches!(failure, LoginFailure::Rejected { .. }) {
                limiter.record_failure(&ip, &form.email);
            }
            return render_login(&tera, Some(&failure.message(&i18n)), &form.email);
        }
    };
    limiter.record_success(&ip, &form.email);

    let session_id = match sessions.create(token).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "login: could not start a session");
            let msg = i18n.text("login.session_failed", &[]);
            return render_login(&tera, Some(&msg), &form.email);
        }
    };

    tracing::info!(email = %form.email, "login: admin access granted");
    HttpResponse::Found()
        .append_header(("Location", "/"))
        .cookie(auth::build_session_cookie(session_id, cfg.cookie_secure))
        .finish()
}

pub async fn logout(
    req: HttpRequest,
    cfg: web::Data<Config>,
    sessions: web::Data<SessionStore>,
) -> HttpResponse {
    if let Some(cookie) = req.cookie(auth::SESSION_COOKIE)
        && let Err(e) = sessions.remove(cookie.value()).await
    {
        tracing::error!(error = %e, "logout: could not persist the session removal");
    }
    HttpResponse::Found()
        .append_header(("Location", "/login"))
        .cookie(auth::build_logout_cookie(cfg.cookie_secure))
        .finish()
}
