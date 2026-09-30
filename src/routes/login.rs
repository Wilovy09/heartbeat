//! Login, in one of two modes (`config::AuthMode`):
//! - `Upstream`: credentials go to an external login endpoint, and the browser gets in only
//!   if its response says `is_admin: true` -- that verdict is the login server's own
//!   decision, this app never re-implements it.
//! - `Password`: a local admin (and optionally a viewer), checked against argon2 hashes.
//!
//! Failed attempts are throttled per IP and per email (`security::LoginLimiter`).

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;
use tera::{Context, Tera};

/// How long the login server gets to answer before the attempt fails.
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

use crate::{
    audit::{self, Event, Outcome},
    auth::{self, Role, SessionStore},
    config::{AuthMode, Config, LocalAccount},
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

    fn key(&self) -> &'static str {
        match self {
            Self::Rejected { key, .. } | Self::Unavailable(key) => key,
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
/// the mode has none) and the session's role.
async fn authenticate(cfg: &Config, form: &LoginForm) -> Result<(String, Role), LoginFailure> {
    match &cfg.auth {
        AuthMode::Upstream {
            login_url,
            allow_viewers,
        } => authenticate_upstream(login_url, *allow_viewers, form).await,
        AuthMode::Password { admin, viewer } => {
            authenticate_password(admin, viewer.as_ref(), form).await
        }
    }
}

async fn authenticate_password(
    admin: &LocalAccount,
    viewer: Option<&LocalAccount>,
    form: &LoginForm,
) -> Result<(String, Role), LoginFailure> {
    let email = form.email.trim();
    let (account, role) = match viewer {
        Some(v) if email.eq_ignore_ascii_case(&v.email) => (v, Role::Viewer),
        _ => (admin, Role::Admin),
    };
    let hash = account.password_hash.clone();
    let password = form.password.clone();
    // argon2 is deliberately slow CPU work: keep it off the async executor. The password is
    // verified even when the email is wrong, so timing doesn't reveal which one failed.
    let password_ok =
        tokio::task::spawn_blocking(move || crate::password::verify(&password, &hash))
            .await
            .unwrap_or(false);
    if password_ok && email.eq_ignore_ascii_case(&account.email) {
        Ok((String::new(), role))
    } else {
        Err(LoginFailure::rejected("login.invalid"))
    }
}

async fn authenticate_upstream(
    login_url: &str,
    allow_viewers: bool,
    form: &LoginForm,
) -> Result<(String, Role), LoginFailure> {
    let resp = reqwest::Client::new()
        .post(login_url)
        .timeout(LOGIN_TIMEOUT)
        .json(&serde_json::json!({ "email": form.email, "password": form.password }))
        .send()
        .await
        .map_err(|e| {
            // The whole cause chain (DNS, refused, TLS, timeout): reqwest's own message
            // alone doesn't say which.
            tracing::warn!(
                url = %login_url,
                error = %crate::uptime::error_chain(&e),
                "login: could not reach the login endpoint"
            );
            LoginFailure::Unavailable("login.unreachable")
        })?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| {
        tracing::warn!(
            url = %login_url,
            %status,
            error = %crate::uptime::error_chain(&e),
            "login: the login endpoint's answer isn't JSON"
        );
        LoginFailure::Unavailable("login.bad_response")
    })?;

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
    let role = match (is_admin, allow_viewers) {
        (true, _) => Role::Admin,
        (false, true) => Role::Viewer,
        (false, false) => {
            tracing::info!(email = %form.email, "login: valid user without admin rights, denied");
            return Err(LoginFailure::rejected("login.not_admin"));
        }
    };

    body.get("access_token")
        .and_then(|v| v.as_str())
        .map(|token| (token.to_string(), role))
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
    // As typed, for the audit log: an attempt is recorded under the email it tried.
    let typed: String = form.email.trim().to_lowercase().chars().take(200).collect();
    if let Some(wait) = limiter.locked_for(&ip, &form.email) {
        tracing::warn!(%ip, email = %form.email, "login: throttled");
        audit::record(
            &req,
            Event::web(&req, "login.throttled")
                .actor(&typed)
                .outcome(Outcome::Denied),
        )
        .await;
        let minutes = wait.as_secs().div_ceil(60).max(1).to_string();
        let msg = i18n.text("login.throttled", &[("minutes", &minutes)]);
        return render_login(&tera, Some(&msg), &form.email);
    }

    let (token, role) = match authenticate(&cfg, &form).await {
        Ok(granted) => granted,
        Err(failure) => {
            let rejected = matches!(failure, LoginFailure::Rejected { .. });
            if rejected {
                limiter.record_failure(&ip, &form.email);
            }
            let event = Event::web(&req, "login.failed")
                .actor(&typed)
                .outcome(if rejected {
                    Outcome::Denied
                } else {
                    Outcome::Error
                })
                .detail(serde_json::json!({ "reason": failure.key() }));
            audit::record(&req, event).await;
            return render_login(&tera, Some(&failure.message(&i18n)), &form.email);
        }
    };
    limiter.record_success(&ip, &form.email);

    let session_id = match sessions.create(&typed, token, role).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "login: could not start a session");
            let msg = i18n.text("login.session_failed", &[]);
            return render_login(&tera, Some(&msg), &form.email);
        }
    };

    tracing::info!(email = %form.email, ?role, "login: access granted");
    let event = Event::web(&req, "login.ok")
        .actor(&typed)
        .detail(serde_json::json!({ "role": role.as_str() }));
    audit::record(&req, event).await;
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
    if auth::current_session(&req).is_some() {
        audit::record(&req, Event::web(&req, "logout")).await;
    }
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
