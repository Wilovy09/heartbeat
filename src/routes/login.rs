//! Login: proxies credentials to the real login endpoint (see `Config::login_url`), and
//! only lets the browser in if that endpoint's response says `is_admin: true`. That field
//! is computed there by a live DB lookup (see pulso-backend's `enrich_with_profile`), not
//! by decoding the returned JWT's claims -- this app never re-implements that check, it
//! just reads the verdict.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;
use tera::{Context, Tera};

use crate::{
    auth::{self, SessionStore},
    config::Config,
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

pub async fn submit_login(
    tera: web::Data<Tera>,
    cfg: web::Data<Config>,
    sessions: web::Data<SessionStore>,
    form: web::Form<LoginForm>,
) -> HttpResponse {
    let client = reqwest::Client::new();
    let resp = client
        .post(&cfg.login_url)
        .json(&serde_json::json!({ "email": form.email, "password": form.password }))
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "login: no se pudo contactar el endpoint de login");
            return render_login(
                &tera,
                Some("No se pudo contactar el servidor de login. Intenta de nuevo."),
                &form.email,
            );
        }
    };

    let status = resp.status();
    let body: serde_json::Value = match resp.json().await {
        Ok(b) => b,
        Err(_) => {
            return render_login(
                &tera,
                Some("El servidor de login regresó una respuesta inválida."),
                &form.email,
            );
        }
    };

    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Credenciales inválidas.");
        return render_login(&tera, Some(msg), &form.email);
    }

    let is_admin = body
        .get("is_admin")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !is_admin {
        tracing::info!(email = %form.email, "login: usuario válido pero no es admin, acceso denegado");
        return render_login(
            &tera,
            Some("Tu cuenta no tiene permisos de administrador."),
            &form.email,
        );
    }

    let Some(token) = body.get("access_token").and_then(|v| v.as_str()) else {
        return render_login(
            &tera,
            Some("El login no regresó un token de sesión válido."),
            &form.email,
        );
    };

    let session_id = match sessions.create(token.to_string()) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "login: could not generate a session id");
            return render_login(&tera, Some("No se pudo iniciar la sesión."), &form.email);
        }
    };

    tracing::info!(email = %form.email, "login: acceso de admin concedido");
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
    if let Some(cookie) = req.cookie(auth::SESSION_COOKIE) {
        sessions.remove(cookie.value());
    }
    HttpResponse::Found()
        .append_header(("Location", "/login"))
        .cookie(auth::build_logout_cookie(cfg.cookie_secure))
        .finish()
}
