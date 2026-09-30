//! Route tests: the real route table (`routes::configure`) and middleware, in process,
//! against temp storage and `AuthMode::Password` (no external login server needed).

use actix_web::middleware::from_fn;
use actix_web::{App, http::StatusCode, http::header, test, web};
use std::time::Duration;
use tera::Tera;

use crate::alerts::{AlertSettings, Alerter};
use crate::auth::SessionStore;
use crate::config::{AuthMode, Config, LocalAccount};
use crate::i18n::{I18n, Lang};
use crate::notices::NoticeStore;
use crate::outbound::Outbound;
use crate::registry::{AppRegistry, AppSettings};
use crate::security::{self, LoginLimiter};
use crate::uptime::{CheckPolicy, Notifiers, UptimeMonitor};
use crate::{password, routes};

const HOST: &str = "localhost:8090";
const ORIGIN: &str = "http://localhost:8090";
const ADMIN_EMAIL: &str = "admin@example.com";
const VIEWER_EMAIL: &str = "viewer@example.com";
const METRICS_TOKEN: &str = "scrape-me";
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
    alerter: web::Data<Alerter>,
    notices: web::Data<NoticeStore>,
    themes: web::Data<crate::theme::ThemeStore>,
    audit: web::Data<crate::audit::AuditLog>,
}

/// The configuration every route test runs with, files under `path(name)`.
pub(crate) fn test_config(path: &dyn Fn(&str) -> String) -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 0,
        auth: AuthMode::Password {
            admin: LocalAccount {
                email: ADMIN_EMAIL.into(),
                password_hash: password::hash(ADMIN_PASSWORD).unwrap(),
            },
            viewer: Some(LocalAccount {
                email: VIEWER_EMAIL.into(),
                password_hash: password::hash(ADMIN_PASSWORD).unwrap(),
            }),
        },
        app_lang: Lang::Es,
        database_path: path("heartbeat.db"),
        apps_file: path("apps.json"),
        uptime_dir: path("uptime"),
        uptime_interval_secs: 60,
        uptime_degraded_ms: 800,
        uptime_retries: 0,
        uptime_cert_warn_days: 14,
        uptime_retention_days: 30,
        uptime_daily_retention_days: 400,
        uptime_timeout_secs: 10,
        uptime_mass_down_pct: 50,
        alert_remind_mins: 60,
        alert_webhook_urls: Vec::new(),
        alert_on_degraded: false,
        alert_mentions: Vec::new(),
        alert_templates_file: path("alert_templates.json"),
        heartbeat_ping_url: None,
        public_url: None,
        sessions_file: path("sessions.json"),
        notices_file: path("notices.json"),
        theme_file: path("theme.css"),
        metrics_token: Some(METRICS_TOKEN.into()),
        allowed_hosts: "*.example.com".into(),
        cookie_secure: false,
        admin_logs_key: None,
        slack_signing_secret: None,
        discord: None,
        chat_command: "pulse".into(),
        chat_admins: Vec::new(),
        audit_retention_days: 365,
    }
}

async fn state() -> TestState {
    state_with(|_| {}).await
}

/// `state()` with the configuration adjusted first.
async fn state_with(adjust: impl FnOnce(&mut Config)) -> TestState {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
    let mut cfg = test_config(&path);
    adjust(&mut cfg);
    let i18n = I18n::new(cfg.app_lang);
    let mut tera = Tera::new();
    i18n.register(&mut tera);
    crate::assets::load_templates(&mut tera).unwrap();
    let outbound = Outbound::new(&cfg.allowed_hosts).unwrap();
    let policy = CheckPolicy {
        interval: Duration::from_secs(60),
        degraded_after_ms: 800,
        retries: 0,
        cert_warn_days: 14,
        retention: Duration::from_hours(720),
        daily_retention: Duration::from_hours(400 * 24),
        timeout: Duration::from_secs(10),
        mass_down_pct: 50,
        lang: Lang::Es,
    };
    let db = crate::db::Db::open_in_memory();
    let monitor = UptimeMonitor::load(db.clone(), policy, outbound.clone(), Notifiers::default())
        .await
        .unwrap();
    TestState {
        registry: web::Data::new(AppRegistry::load(db.clone()).await.unwrap()),
        sessions: web::Data::new(SessionStore::load(db.clone()).await.unwrap()),
        limiter: web::Data::new(LoginLimiter::default()),
        outbound: web::Data::new(outbound),
        monitor: web::Data::new(monitor),
        alerter: web::Data::new(Alerter::new(AlertSettings::default()).unwrap()),
        notices: web::Data::new(NoticeStore::load(db.clone()).await.unwrap()),
        themes: web::Data::new(crate::theme::ThemeStore::load(db.clone()).await.unwrap()),
        audit: web::Data::new(crate::audit::AuditLog::new(db, 365)),
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
                .app_data($s.notices.clone())
                .app_data($s.themes.clone())
                .app_data($s.audit.clone())
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
async fn status_pages_exist_only_for_public_apps_and_hide_urls() {
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
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/status/visible-api")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("Visible API"));
    assert!(
        !body.contains("api.example.com"),
        "never the health or logs URL"
    );
    let internal = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/status/internal-api")
            .to_request(),
    )
    .await;
    assert_eq!(internal.status(), StatusCode::NOT_FOUND);
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

/// Logs in as `email` and evaluates to the session cookie.
macro_rules! login_as {
    ($app:expr, $email:expr) => {
        test::call_service(
            &$app,
            test::TestRequest::post()
                .uri("/login")
                .insert_header((header::HOST, HOST))
                .insert_header((header::ORIGIN, ORIGIN))
                .set_form([("email", $email), ("password", ADMIN_PASSWORD)])
                .to_request(),
        )
        .await
        .response()
        .cookies()
        .find(|c| c.name() == crate::auth::SESSION_COOKIE)
        .expect("session cookie")
        .into_owned()
    };
}

fn post(uri: &str) -> test::TestRequest {
    test::TestRequest::post()
        .uri(uri)
        .insert_header((header::HOST, HOST))
        .insert_header((header::ORIGIN, ORIGIN))
}

#[actix_web::test]
async fn viewers_see_the_dashboard_but_nothing_else() {
    let s = state().await;
    let slug = s
        .registry
        .add(AppSettings {
            name: "Billing".into(),
            health_url: "https://billing.example.com/health".into(),
            logs_url: "https://billing.example.com/logs".into(),
            ..AppSettings::default()
        })
        .await
        .unwrap();
    let app = app!(s);
    let viewer = login_as!(app, VIEWER_EMAIL);

    for uri in ["/", "/api/uptime", &format!("/api/uptime/{slug}")] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(uri)
                .cookie(viewer.clone())
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    }
    let dashboard = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/")
            .cookie(viewer.clone())
            .to_request(),
    )
    .await;
    let body = String::from_utf8(test::read_body(dashboard).await.to_vec()).unwrap();
    assert!(
        !body.contains("href=\"/settings\""),
        "no admin nav for viewers"
    );

    for uri in [
        "/apps",
        "/settings",
        "/notices",
        "/audit",
        "/audit/export",
        "/logs",
        &format!("/logs/{slug}"),
        &format!("/api/apps/{slug}/logs"),
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(uri)
                .cookie(viewer.clone())
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "GET {uri}");
    }
    for uri in [
        &format!("/apps/{slug}/delete"),
        &format!("/apps/{slug}/pause"),
        "/notices",
    ] {
        // A well-formed body, so the handler (not the form extractor) is what refuses.
        let form = [("value", "true"), ("title", "x"), ("state", "resolved")];
        let resp = test::call_service(
            &app,
            post(uri).cookie(viewer.clone()).set_form(form).to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "POST {uri}");
    }
    let json = test::call_service(
        &app,
        post("/settings/alerts/test")
            .cookie(viewer)
            .set_json(serde_json::json!({ "kind": "ping" }))
            .to_request(),
    )
    .await;
    assert_eq!(json.status(), StatusCode::FORBIDDEN);
    assert!(
        s.registry.find(&slug).await.is_some(),
        "nothing was deleted"
    );
}

#[actix_web::test]
async fn metrics_need_the_bearer_token() {
    let s = state().await;
    s.registry
        .add(AppSettings {
            name: "Billing".into(),
            health_url: "https://billing.example.com/health".into(),
            ..AppSettings::default()
        })
        .await
        .unwrap();
    let app = app!(s);
    for auth in [None, Some("Bearer wrong")] {
        let mut req = test::TestRequest::get().uri("/metrics");
        if let Some(value) = auth {
            req = req.insert_header((header::AUTHORIZATION, value));
        }
        let resp = test::call_service(&app, req.to_request()).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{auth:?}");
    }
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/metrics")
            .insert_header((header::AUTHORIZATION, format!("Bearer {METRICS_TOKEN}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("heartbeat_paused{app=\"billing\",name=\"Billing\"} 0"),
        "{body}"
    );
}

#[actix_web::test]
async fn badges_need_the_embed_token_and_exports_a_session() {
    let s = state().await;
    let slug = s
        .registry
        .add(AppSettings {
            name: "Billing".into(),
            health_url: "https://billing.example.com/health".into(),
            ..AppSettings::default()
        })
        .await
        .unwrap();
    let token = s.registry.find(&slug).await.unwrap().embed_token;
    let app = app!(s);

    let bad = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/badge/{slug}.svg?token=nope"))
            .to_request(),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::NOT_FOUND);
    let badge = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/badge/{slug}.svg?token={token}&label=API"))
            .to_request(),
    )
    .await;
    assert_eq!(badge.status(), StatusCode::OK);
    assert_eq!(
        badge.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/svg+xml"
    );
    let svg = String::from_utf8(test::read_body(badge).await.to_vec()).unwrap();
    assert!(svg.contains(">API<") && svg.contains("Sin datos"), "{svg}");

    let export_uri = format!("/api/uptime/{slug}/export?format=csv");
    let anonymous =
        test::call_service(&app, test::TestRequest::get().uri(&export_uri).to_request()).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let viewer = login_as!(app, VIEWER_EMAIL);
    let csv = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&export_uri)
            .cookie(viewer)
            .to_request(),
    )
    .await;
    assert_eq!(csv.status(), StatusCode::OK);
    let body = String::from_utf8(test::read_body(csv).await.to_vec()).unwrap();
    assert!(body.starts_with("at,status,latency_ms,message\n"));
}

#[actix_web::test]
async fn status_pages_are_per_published_app_with_their_notices() {
    let s = state().await;
    for name in ["Billing", "Search"] {
        s.registry
            .add(AppSettings {
                name: name.into(),
                health_url: format!("https://{}.example.com/health", name.to_lowercase()),
                ..AppSettings::default()
            })
            .await
            .unwrap();
    }
    s.registry.set_public("billing", true).await.unwrap();
    let app = app!(s);
    let admin = login_as!(app, ADMIN_EMAIL);
    let resp = test::call_service(
        &app,
        post("/notices")
            .cookie(admin)
            .insert_header((header::CONTENT_TYPE, "application/x-www-form-urlencoded"))
            .set_payload("title=Pagos+lentos&body=Revisando.&state=investigating&apps=billing")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);

    let page = test::call_service(
        &app,
        test::TestRequest::get().uri("/status/billing").to_request(),
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
    let body = String::from_utf8(test::read_body(page).await.to_vec()).unwrap();
    assert!(
        body.contains("Pagos lentos") && body.contains("Billing"),
        "{body}"
    );

    for hidden in ["/status/search", "/status/nope", "/status"] {
        let resp =
            test::call_service(&app, test::TestRequest::get().uri(hidden).to_request()).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{hidden}");
    }
}

#[actix_web::test]
async fn the_custom_theme_is_public_to_read_and_admin_only_to_write() {
    let s = state().await;
    let app = app!(s);
    let save = |css: &str| post("/settings/theme").set_json(serde_json::json!({ "css": css }));
    let viewer = login_as!(app, VIEWER_EMAIL);
    let resp = test::call_service(&app, save(":root{}").cookie(viewer).to_request()).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let admin = login_as!(app, ADMIN_EMAIL);
    let refused = test::call_service(
        &app,
        save("@import 'https://evil.example/x.css';")
            .cookie(admin.clone())
            .to_request(),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let ok = test::call_service(
        &app,
        save(":root { --glass: #101820; }")
            .cookie(admin)
            .to_request(),
    )
    .await;
    assert_eq!(ok.status(), StatusCode::OK);

    let css = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/theme/custom.css")
            .to_request(),
    )
    .await;
    assert_eq!(css.status(), StatusCode::OK);
    assert!(
        css.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/css")
    );
    let body = String::from_utf8(test::read_body(css).await.to_vec()).unwrap();
    assert_eq!(body, ":root { --glass: #101820; }");
}

const SLACK_SECRET: &str = "slack-signing-secret";
const SLACK_ADMIN: &str = "U0ADMIN1";

async fn chat_state() -> TestState {
    let s = state_with(|cfg| {
        cfg.slack_signing_secret = Some(SLACK_SECRET.into());
        cfg.discord = Some(crate::config::DiscordApp {
            application_id: "1234".into(),
            public_key: crate::chat::discord::tests::key()
                .verifying_key()
                .to_bytes(),
            bot_token: "unused".into(),
        });
        cfg.chat_admins = vec![SLACK_ADMIN.into()];
    })
    .await;
    s.registry
        .add(AppSettings {
            name: "Billing API".into(),
            logs_url: String::new(),
            health_url: "https://api.example.com/health".into(),
            ..AppSettings::default()
        })
        .await
        .unwrap();
    s
}

/// A slash command as Slack sends it: no `Origin`, signed with the signing secret.
fn slack_command(user: &str, text: &str) -> test::TestRequest {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("command", "/pulse")
        .append_pair("user_id", user)
        .append_pair("text", text)
        .finish();
    let now = crate::uptime::unix_now().to_string();
    let signature = crate::chat::slack::tests::sign(SLACK_SECRET, &now, body.as_bytes());
    test::TestRequest::post()
        .uri("/slack/commands")
        .insert_header((header::HOST, HOST))
        .insert_header((header::CONTENT_TYPE, "application/x-www-form-urlencoded"))
        .insert_header(("X-Slack-Request-Timestamp", now))
        .insert_header(("X-Slack-Signature", signature))
        .set_payload(body)
}

fn discord_interaction(body: &serde_json::Value) -> test::TestRequest {
    let body = body.to_string();
    let now = crate::uptime::unix_now().to_string();
    let signature = crate::chat::discord::tests::sign(&now, body.as_bytes());
    test::TestRequest::post()
        .uri("/discord/interactions")
        .insert_header((header::HOST, HOST))
        .insert_header((header::CONTENT_TYPE, "application/json"))
        .insert_header(("X-Signature-Timestamp", now))
        .insert_header(("X-Signature-Ed25519", signature))
        .set_payload(body)
}

#[actix_web::test]
async fn chat_endpoints_are_off_until_configured() {
    let s = state().await;
    let app = app!(s);
    for uri in ["/slack/commands", "/discord/interactions"] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(uri)
                .insert_header((header::HOST, HOST))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[actix_web::test]
async fn slack_commands_need_a_valid_signature_but_no_origin() {
    let s = chat_state().await;
    let app = app!(s);
    let forged = slack_command(SLACK_ADMIN, "status")
        .insert_header(("X-Slack-Signature", "v0=00"))
        .to_request();
    let resp = test::call_service(&app, forged).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = test::call_service(&app, slack_command("U0SOMEONE", "status").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let reply: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(reply["response_type"], "in_channel");
    assert!(
        reply["text"].as_str().unwrap().contains("*Billing API*"),
        "{reply}"
    );
}

#[actix_web::test]
async fn only_chat_admins_can_pause_and_resume() {
    let s = chat_state().await;
    let app = app!(s);
    let resp = test::call_service(
        &app,
        slack_command("U0SOMEONE", "pause billing 2h").to_request(),
    )
    .await;
    let reply: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(reply["response_type"], "ephemeral");
    assert!(!s.registry.find("billing-api").await.unwrap().paused);

    let resp = test::call_service(
        &app,
        slack_command(SLACK_ADMIN, "pause billing 2h").to_request(),
    )
    .await;
    let reply: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(reply["response_type"], "in_channel", "{reply}");
    let paused = s.registry.find("billing-api").await.unwrap();
    assert!(paused.paused);
    let left = paused.paused_until.unwrap() - crate::uptime::unix_now();
    assert!((7190..=7200).contains(&left), "{left}");

    test::call_service(
        &app,
        slack_command(SLACK_ADMIN, "resume billing").to_request(),
    )
    .await;
    assert!(!s.registry.find("billing-api").await.unwrap().paused);
}

#[actix_web::test]
async fn discord_pings_autocompletes_and_answers_commands() {
    let s = chat_state().await;
    let app = app!(s);
    let unsigned = test::TestRequest::post()
        .uri("/discord/interactions")
        .insert_header((header::HOST, HOST))
        .set_payload(r#"{"type":1}"#)
        .to_request();
    assert_eq!(
        test::call_service(&app, unsigned).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let resp = test::call_service(
        &app,
        discord_interaction(&serde_json::json!({ "type": 1 })).to_request(),
    )
    .await;
    let pong: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(pong, serde_json::json!({ "type": 1 }));

    let autocomplete = serde_json::json!({
        "type": 4,
        "data": { "name": "pulse", "options": [{ "name": "status", "type": 1, "options": [
            { "name": "app", "type": 3, "value": "bill", "focused": true },
        ]}]},
    });
    let resp = test::call_service(&app, discord_interaction(&autocomplete).to_request()).await;
    let choices: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(
        choices["data"]["choices"],
        serde_json::json!([{ "name": "Billing API", "value": "billing-api" }])
    );

    // A Discord user can't borrow a Slack admin's ID.
    let pause = serde_json::json!({
        "type": 2,
        "data": { "name": "pulse", "options": [{ "name": "pause", "type": 1, "options": [
            { "name": "app", "type": 3, "value": "billing-api" },
        ]}]},
        "member": { "user": { "id": "999" }, "roles": [] },
    });
    let resp = test::call_service(&app, discord_interaction(&pause).to_request()).await;
    let reply: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["flags"], 64, "{reply}");
    assert!(!s.registry.find("billing-api").await.unwrap().paused);
}

/// Every entry, newest first.
async fn audit_entries(s: &TestState) -> Vec<crate::audit::Entry> {
    s.audit
        .query(crate::audit::Filter {
            limit: 100,
            ..crate::audit::Filter::default()
        })
        .await
        .unwrap()
}

#[actix_web::test]
async fn logins_and_app_changes_are_audited_under_the_email() {
    let s = state().await;
    let app = app!(s);
    test::call_service(&app, login_request("wrong").to_request()).await;
    let admin = login_as!(app, ADMIN_EMAIL);

    let form = [
        ("name", "Billing"),
        ("logs_url", ""),
        ("health_url", "https://api.example.com/health"),
        ("headers", "Authorization: Bearer s3cret"),
    ];
    test::call_service(
        &app,
        post("/apps")
            .cookie(admin.clone())
            .set_form(form)
            .to_request(),
    )
    .await;
    let mut edited = form;
    edited[0].1 = "Billing API";
    test::call_service(
        &app,
        post("/apps/billing/edit")
            .cookie(admin.clone())
            .set_form(edited)
            .to_request(),
    )
    .await;
    test::call_service(&app, post("/logout").cookie(admin).to_request()).await;

    let entries = audit_entries(&s).await;
    let summary: Vec<(&str, &str, &str)> = entries
        .iter()
        .rev()
        .map(|e| (e.action.as_str(), e.actor.as_str(), e.outcome.as_str()))
        .collect();
    assert_eq!(
        summary,
        [
            ("login.failed", ADMIN_EMAIL, "denied"),
            ("login.ok", ADMIN_EMAIL, "ok"),
            ("app.add", ADMIN_EMAIL, "ok"),
            ("app.update", ADMIN_EMAIL, "ok"),
            ("logout", ADMIN_EMAIL, "ok"),
        ]
    );
    let update = &entries[1];
    assert_eq!(update.target.as_deref(), Some("billing"));
    assert_eq!(
        update.detail.as_ref().unwrap()["name"],
        serde_json::json!({ "from": "Billing", "to": "Billing API" })
    );
    let everything = serde_json::to_string(&entries).unwrap();
    assert!(
        !everything.contains("s3cret"),
        "header values stay out: {everything}"
    );
    assert!(!everything.contains(ADMIN_PASSWORD));
}

#[actix_web::test]
async fn every_chat_command_is_audited_even_when_refused() {
    let s = chat_state().await;
    let app = app!(s);
    test::call_service(&app, slack_command("U0SOMEONE", "status").to_request()).await;
    test::call_service(
        &app,
        slack_command("U0SOMEONE", "pause billing").to_request(),
    )
    .await;
    test::call_service(
        &app,
        slack_command(SLACK_ADMIN, "pause billing 1h").to_request(),
    )
    .await;
    test::call_service(&app, slack_command(SLACK_ADMIN, "reboot").to_request()).await;

    let entries = audit_entries(&s).await;
    let summary: Vec<(&str, &str, &str, Option<&str>)> = entries
        .iter()
        .rev()
        .map(|e| {
            (
                e.action.as_str(),
                e.actor.as_str(),
                e.outcome.as_str(),
                e.target.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("chat.status", "U0SOMEONE", "ok", None),
            ("chat.pause", "U0SOMEONE", "denied", None),
            ("chat.pause", SLACK_ADMIN, "ok", Some("billing-api")),
            ("chat.invalid", SLACK_ADMIN, "error", None),
        ]
    );
    assert!(entries.iter().all(|e| e.source == "slack"));
    assert_eq!(
        entries[1].detail.as_ref().unwrap()["text"],
        "pause billing 1h"
    );
}

#[actix_web::test]
async fn the_audit_page_filters_and_exports() {
    let s = state().await;
    let app = app!(s);
    let admin = login_as!(app, ADMIN_EMAIL);
    s.audit
        .record(crate::audit::Event::system("app.resume").target("billing"))
        .await;

    let page = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/audit?action=app.")
            .cookie(admin.clone())
            .to_request(),
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
    let body = String::from_utf8(test::read_body(page).await.to_vec()).unwrap();
    assert!(body.contains("app.resume"), "{body}");
    assert!(!body.contains("login.ok"), "filtered out");

    let csv = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/audit/export")
            .cookie(admin)
            .to_request(),
    )
    .await;
    assert_eq!(csv.status(), StatusCode::OK);
    let body = String::from_utf8(test::read_body(csv).await.to_vec()).unwrap();
    assert!(body.starts_with("id,at,source,actor,action"), "{body}");
    assert!(
        body.contains(",login.ok,") && body.contains(",app.resume,"),
        "{body}"
    );
    let exported = audit_entries(&s).await;
    assert_eq!(
        exported[0].action, "audit.export",
        "the export itself is audited"
    );
}
