//! Request-level defenses that aren't about who you are (that's `auth`): cross-site request
//! forgery, browser hardening headers, and login brute-force throttling.

use actix_web::body::{BoxBody, EitherBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::{Method, header};
use actix_web::middleware::{DefaultHeaders, Next};
use actix_web::{Error, HttpRequest, HttpResponse};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Browser hardening for every response. Alpine.js evaluates its `x-*` expressions with
/// `new Function`, hence `'unsafe-eval'`; inline `<script>`/`onsubmit` need
/// `'unsafe-inline'`. What the policy still buys: no script, style, font or fetch from any
/// origin but this one, and no framing (clickjacking) at all.
pub fn headers() -> DefaultHeaders {
    DefaultHeaders::new()
        .add((
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; script-src 'self' 'unsafe-inline' 'unsafe-eval'; \
             style-src 'self' 'unsafe-inline'; font-src 'self'; img-src 'self' data:; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ))
        .add((header::X_FRAME_OPTIONS, "DENY"))
        .add((header::X_CONTENT_TYPE_OPTIONS, "nosniff"))
        .add((header::REFERRER_POLICY, "same-origin"))
}

/// Whether a state-changing request provably came from one of this app's own pages: its
/// `Origin` (or, failing that, `Referer`) must name the same host the request was sent to.
/// Browsers always attach `Origin` to cross-origin POSTs, so a forged form on another site
/// fails here even though `SameSite=Lax` would still let a same-site subdomain through.
fn is_same_origin(req: &ServiceRequest) -> bool {
    let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
    else {
        return false;
    };
    let source = req
        .headers()
        .get(header::ORIGIN)
        .or_else(|| req.headers().get(header::REFERER))
        .and_then(|h| h.to_str().ok())
        .and_then(|raw| reqwest::Url::parse(raw).ok());
    source.is_some_and(|url| {
        let authority = match (url.host_str(), url.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            (None, _) => return false,
        };
        authority.eq_ignore_ascii_case(host)
    })
}

/// Middleware: rejects any non-GET/HEAD request that isn't same-origin (see
/// `is_same_origin`) before it reaches a handler.
pub async fn csrf(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<EitherBody<impl MessageBody, BoxBody>>, Error> {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if safe || is_same_origin(&req) {
        return next
            .call(req)
            .await
            .map(ServiceResponse::map_into_left_body);
    }
    tracing::warn!(path = %req.path(), "csrf: rejected cross-origin request");
    let response = HttpResponse::Forbidden().body("Cross-origin request refused");
    Ok(req.into_response(response).map_into_right_body())
}

const LOGIN_MAX_FAILURES: usize = 5;
const LOGIN_WINDOW: Duration = Duration::from_mins(15);

/// Failed-login throttle: more than `LOGIN_MAX_FAILURES` failures within `LOGIN_WINDOW`
/// for the same client IP *or* the same email locks further attempts for that key until
/// the oldest failure ages out. Per-email stops a distributed guess at one account;
/// per-IP stops one client spraying many accounts. In memory: a restart resets it.
#[derive(Default)]
pub struct LoginLimiter {
    failures: Mutex<HashMap<String, Vec<Instant>>>,
}

impl LoginLimiter {
    fn keys(ip: &str, email: &str) -> [String; 2] {
        [
            format!("ip:{ip}"),
            format!("email:{}", email.trim().to_lowercase()),
        ]
    }

    /// `Some(wait)` if this IP or email is currently locked out.
    pub fn locked_for(&self, ip: &str, email: &str) -> Option<Duration> {
        let now = Instant::now();
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        Self::keys(ip, email)
            .iter()
            .filter_map(|key| {
                let times = failures.get_mut(key)?;
                times.retain(|t| now.duration_since(*t) < LOGIN_WINDOW);
                (times.len() >= LOGIN_MAX_FAILURES)
                    .then(|| LOGIN_WINDOW.saturating_sub(now.duration_since(times[0])))
            })
            .max()
    }

    pub fn record_failure(&self, ip: &str, email: &str) {
        let now = Instant::now();
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        for key in Self::keys(ip, email) {
            failures.entry(key).or_default().push(now);
        }
    }

    pub fn record_success(&self, ip: &str, email: &str) {
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        for key in Self::keys(ip, email) {
            failures.remove(&key);
        }
    }
}

/// The client's IP. Behind the local reverse proxy (connection from loopback) that's the
/// proxy's `X-Real-IP`; from anywhere else the header is ignored, since a direct client
/// could put anything in it.
pub fn client_ip(req: &HttpRequest) -> String {
    let peer = req.peer_addr().map(|a| a.ip());
    let forwarded = req
        .headers()
        .get("x-real-ip")
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    match (peer, forwarded) {
        (Some(p), Some(f)) if p.is_loopback() => f.to_string(),
        (Some(p), _) => p.to_string(),
        (None, _) => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test::TestRequest;

    #[test]
    fn same_origin_requires_a_matching_origin_or_referer() {
        let ok = TestRequest::post()
            .insert_header((header::HOST, "status.example.com"))
            .insert_header((header::ORIGIN, "https://status.example.com"))
            .to_srv_request();
        assert!(is_same_origin(&ok));

        let with_port = TestRequest::post()
            .insert_header((header::HOST, "localhost:8090"))
            .insert_header((header::REFERER, "http://localhost:8090/apps"))
            .to_srv_request();
        assert!(is_same_origin(&with_port));

        for (origin, why) in [
            (Some("https://evil.example.com"), "other host"),
            (Some("https://status.example.com.evil.com"), "prefix trick"),
            (Some("null"), "opaque origin"),
            (None, "no origin nor referer"),
        ] {
            let mut req = TestRequest::post().insert_header((header::HOST, "status.example.com"));
            if let Some(o) = origin {
                req = req.insert_header((header::ORIGIN, o));
            }
            assert!(!is_same_origin(&req.to_srv_request()), "{why}");
        }
    }

    #[test]
    fn limiter_locks_after_max_failures_per_ip_or_email() {
        let limiter = LoginLimiter::default();
        for _ in 0..LOGIN_MAX_FAILURES {
            assert!(limiter.locked_for("1.1.1.1", "a@b.c").is_none());
            limiter.record_failure("1.1.1.1", "a@b.c");
        }
        assert!(limiter.locked_for("1.1.1.1", "a@b.c").is_some());
        // Same email from another IP: still locked (per-email key).
        assert!(limiter.locked_for("2.2.2.2", "A@B.C ").is_some());
        // Same IP, another email: still locked (per-IP key).
        assert!(limiter.locked_for("1.1.1.1", "other@b.c").is_some());
        // Unrelated client and account: free.
        assert!(limiter.locked_for("3.3.3.3", "x@y.z").is_none());

        limiter.record_success("1.1.1.1", "a@b.c");
        assert!(limiter.locked_for("1.1.1.1", "a@b.c").is_none());
    }

    #[test]
    fn client_ip_trusts_x_real_ip_only_from_loopback() {
        let proxied = TestRequest::default()
            .peer_addr("127.0.0.1:5000".parse().unwrap())
            .insert_header(("x-real-ip", "203.0.113.7"))
            .to_http_request();
        assert_eq!(client_ip(&proxied), "203.0.113.7");

        let direct = TestRequest::default()
            .peer_addr("198.51.100.2:5000".parse().unwrap())
            .insert_header(("x-real-ip", "203.0.113.7"))
            .to_http_request();
        assert_eq!(client_ip(&direct), "198.51.100.2");
    }
}
