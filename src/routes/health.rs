//! Liveness endpoint for load balancers and external monitors -- public, no data.

use actix_web::HttpResponse;

/// GET /healthz -- 200 while the process is serving requests.
pub async fn healthz() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/plain; charset=utf-8")
        .body("ok")
}
