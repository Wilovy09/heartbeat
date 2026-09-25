//! Route tests: the real route table (`routes::configure`) and middleware, in process,
//! against temp storage and `AuthMode::Password` (no external login server needed).

use actix_web::middleware::from_fn;
use actix_web::{App, http::StatusCode, http::header, test, web};
use std::time::Duration;
use tera::Tera;

use crate::alerts::Alerter;
use crate::auth::SessionStore;
use crate::config::{AuthMode, Config};
use crate::i18n::{I18n, Lang};
use crate::outbound::Outbound;
use crate::registry::{AppRegistry, AppSettings};
use crate::security::{self, LoginLimiter};
use crate::uptime::{CheckPolicy, Notifiers, UptimeMonitor};
use crate::{password, routes};

const HOST: &str = "localhost:8090";
const ORIGIN: &str = "http://localhost:8090";
const ADMIN_EMAIL: &str = "admin@example.com";
const ADMIN_PASSWORD: &str = "correct horse";

struct TestState {
    _dir: tempfile::TempDir,
    cfg: web::Data<Config>,
    tera: web::Data<Tera>,
    i18n: web::Data<I18n>,
    registry: web::Data<AppRegistry>,
    sessions: web::Data<SessionStore>,
    limiter: web::Data<LoginLimiter>,
    outbound: web::Data<Outbound>,
    monitor: web::Data<UptimeMonitor>,
    alerter: web::Data<Option<Alerter>>,
}

async fn state() -> TestState {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
    let cfg = Config {
        host: "127.0.0.1".into(),
        port: 0,
        auth: AuthMode::Password {
            email: ADMIN_EMAIL.into(),
            password_hash: password::hash(ADMIN_PASSWORD).unwrap(),
        },
        app_lang: Lang::Es,
        apps_file: path("apps.json"),
        uptime_dir: path("uptime"),
        uptime_interval_secs: 60,
        uptime_degraded_ms: 800,
        uptime_retries: 0,
        uptime_cert_warn_days: 14,
        uptime_retention_days: 30,
        alert_webhook_urls: Vec::new(),
        alert_on_degraded: false,
        alert_mentions: Vec::new(),
        alert_templates_file: path("alert_templates.json"),
        heartbeat_ping_url: None,
        public_url: None,
        sessions_file: path("sessions.json"),
        allowed_hosts: "*.example.com".into(),
        cookie_secure: false,
        admin_logs_key: None,
    };
    let i18n = I18n::new(cfg.app_lang);
    let mut tera = Tera::new();
    i18n.register(&mut tera);
    tera.load_from_glob("templates/**/*.html").unwrap();
    let outbound = Outbound::new(&cfg.allowed_hosts).unwrap();
    let policy = CheckPolicy {
        interval: Duration::from_secs(60),
        degraded_after_ms: 800,
        retries: 0,
        cert_warn_days: 14,
        retention: Duration::from_hours(720),
    };
    let monitor = UptimeMonitor::load(
        &cfg.uptime_dir,
        policy,
        outbound.clone(),
        Notifiers::default(),
    )
    .await
    .unwrap();
    TestState {
        registry: web::Data::new(AppRegistry::load(&cfg.apps_file).await.unwrap()),
        sessions: web::Data::new(SessionStore::load(&cfg.sessions_file).await.unwrap()),
        limiter: web::Data::new(LoginLimiter::default()),
        outbound: web::Data::new(outbound),
        monitor: web::Data::new(monitor),
        alerter: web::Data::new(None),
        tera: web::Data::new(tera),
        i18n: web::Data::new(i18n),
        cfg: web::Data::new(cfg),
        _dir: dir,
    }
}

macro_rules! app {
    ($s:expr) => {
        test::init_service(
            App::new()
                .wrap(from_fn(security::csrf))
                .wrap(security::headers())
                .app_data($s.cfg.clone())
                .app_data($s.tera.clone())
                .app_data($s.i18n.clone())
                .app_data($s.registry.clone())
                .app_data($s.sessions.clone())
                .app_data($s.limiter.clone())
                .app_data($s.outbound.clone())
                .app_data($s.monitor.clone())
                .app_data($s.alerter.clone())
                .configure(routes::configure),
        )
        .await
    };
}

fn login_request(password: &str) -> test::TestRequest {
    test::TestRequest::post()
        .uri("/login")
        .insert_header((header::HOST, HOST))
        .insert_header((header::ORIGIN, ORIGIN))
        .set_form([("email", ADMIN_EMAIL), ("password", password)])
}

fn location<B>(resp: &actix_web::dev::ServiceResponse<B>) -> &str {
    resp.headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

#[actix_web::test]
async fn pages_redirect_to_login_without_a_real_session() {
    let s = state().await;
    let app = app!(s);
    for cookie in [None, Some("forged-value")] {
        let mut req = test::TestRequest::get().uri("/apps");
        if let Some(value) = cookie {
            req = req.cookie(actix_web::cookie::Cookie::new(
                crate::auth::SESSION_COOKIE,
                value,
            ));
        }
        let resp = test::call_service(&app, req.to_request()).await;
        assert_eq!(resp.status(), StatusCode::FOUND, "cookie {cookie:?}");
        assert_eq!(location(&resp), "/login");
    }
    let api = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/uptime").to_request(),
    )
    .await;
    assert_eq!(api.status(), StatusCode::UNAUTHORIZED);
}

#[actix_web::test]
async fn password_login_grants_a_session_that_opens_pages() {
    let s = state().await;
    let app = app!(s);
    let resp = test::call_service(&app, login_request(ADMIN_PASSWORD).to_request()).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(location(&resp), "/");
    let cookie = resp
        .response()
        .cookies()
        .find(|c| c.name() == crate::auth::SESSION_COOKIE)
        .expect("session cookie")
        .into_owned();
    assert!(cookie.http_only().unwrap_or(false));

    let page = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/apps")
            .cookie(cookie)
            .to_request(),
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
}

#[actix_web::test]
async fn cross_origin_posts_are_rejected_before_any_handler() {
    let s = state().await;
    let app = app!(s);
    for origin in [None, Some("https://evil.example.com")] {
        let mut req = test::TestRequest::post()
            .uri("/login")
            .insert_header((header::HOST, HOST))
            .set_form([("email", ADMIN_EMAIL), ("password", ADMIN_PASSWORD)]);
        if let Some(o) = origin {
            req = req.insert_header((header::ORIGIN, o));
        }
        let resp = test::call_service(&app, req.to_request()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "origin {origin:?}");
    }
}

#[actix_web::test]
async fn repeated_failed_logins_are_throttled_even_with_the_right_password() {
    let s = state().await;
    let app = app!(s);
    for _ in 0..5 {
        let resp = test::call_service(&app, login_request("wrong").to_request()).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a failure re-renders the form"
        );
    }
    let resp = test::call_service(&app, login_request(ADMIN_PASSWORD).to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK, "locked out: no redirect");
    let body = test::read_body(resp).await;
    assert!(String::from_utf8_lossy(&body).contains("Demasiados intentos"));
}

#[actix_web::test]
async fn every_response_carries_the_security_headers() {
    let s = state().await;
    let app = app!(s);
    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/healthz").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers();
    let csp = headers
        .get(header::CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(csp.contains("frame-ancestors 'none'"));
    assert_eq!(headers.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    assert_eq!(
        headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
}

#[actix_web::test]
async fn embed_needs_the_right_token_and_never_leaks_urls() {
    let s = state().await;
    let slug = s
        .registry
        .add(AppSettings {
            name: "Billing".into(),
            logs_url: "https://api.example.com/logs".into(),
            health_url: "https://api.example.com/health".into(),
            ..AppSettings::default()
        })
        .await
        .unwrap();
    let token = s.registry.find(&slug).await.unwrap().embed_token;
    let app = app!(s);

    for (who, uri) in [
        ("wrong token", format!("/embed/{slug}?token=nope")),
        ("unknown app", format!("/embed/missing?token={token}")),
    ] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(&uri).to_request()).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{who}");
    }

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/embed/{slug}?token={token}"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "*"
    );
    let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("\"name\":\"Billing\""));
    assert!(
        !body.contains("api.example.com"),
        "embed must not expose URLs: {body}"
    );
}

#[actix_web::test]
async fn status_page_lists_only_public_apps() {
    let s = state().await;
    for (name, public) in [("Visible API", true), ("Internal API", false)] {
        let slug = s
            .registry
            .add(AppSettings {
                name: name.into(),
                logs_url: "https://api.example.com/logs".into(),
                health_url: "https://api.example.com/health".into(),
                ..AppSettings::default()
            })
            .await
            .unwrap();
        s.registry.set_public(&slug, public).await.unwrap();
    }
    let app = app!(s);
    let resp = test::call_service(&app, test::TestRequest::get().uri("/status").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("Visible API"));
    assert!(!body.contains("Internal API"));
}

#[actix_web::test]
async fn settings_page_and_alert_tests_require_a_session() {
    let s = state().await;
    let app = app!(s);
    let page =
        test::call_service(&app, test::TestRequest::get().uri("/settings").to_request()).await;
    assert_eq!(page.status(), StatusCode::FOUND);

    let test_req = || {
        test::TestRequest::post()
            .uri("/settings/alerts/test")
            .insert_header((header::HOST, HOST))
            .insert_header((header::ORIGIN, ORIGIN))
            .set_json(serde_json::json!({ "kind": "ping" }))
    };
    let resp = test::call_service(&app, test_req().to_request()).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let login = test::call_service(&app, login_request(ADMIN_PASSWORD).to_request()).await;
    let cookie = login
        .response()
        .cookies()
        .find(|c| c.name() == crate::auth::SESSION_COOKIE)
        .unwrap()
        .into_owned();
    let page = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/settings")
            .cookie(cookie.clone())
            .to_request(),
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
    let body = String::from_utf8(test::read_body(page).await.to_vec()).unwrap();
    assert!(
        body.contains("ALERT_WEBHOOK_URLS"),
        "empty state explains how to enable alerts"
    );

    let resp = test::call_service(&app, test_req().cookie(cookie).to_request()).await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "no webhooks configured"
    );
}
