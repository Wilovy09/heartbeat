//! /notices: incident notices shown on the apps' public status pages (see `notices`).

use actix_web::{HttpRequest, HttpResponse, web};
use tera::Tera;

use crate::{
    auth,
    i18n::{I18n, Localize},
    notices::{NoticeDraft, NoticeState, NoticeStore},
    registry::AppRegistry,
};

/// Parses the notice form. By hand, not with `web::Form`: each affected app arrives as its
/// own `apps=<slug>` pair, and the urlencoded deserializer rejects repeated keys.
fn draft_from(body: &[u8]) -> Option<NoticeDraft> {
    let (mut title, mut text, mut state, mut apps) = (None, String::new(), None, Vec::new());
    for (key, value) in url::form_urlencoded::parse(body) {
        match key.as_ref() {
            "title" => title = Some(value.into_owned()),
            "body" => text = value.into_owned(),
            "state" => {
                state = serde_json::from_value::<NoticeState>(serde_json::Value::String(
                    value.into_owned(),
                ))
                .ok();
            }
            "apps" => apps.push(value.into_owned()),
            _ => {}
        }
    }
    Some(NoticeDraft {
        title: title?,
        body: text,
        state: state?,
        apps,
    })
}

async fn render(
    tera: &Tera,
    store: &NoticeStore,
    registry: &AppRegistry,
    error: Option<&str>,
) -> HttpResponse {
    let mut ctx = tera::Context::new();
    ctx.insert("apps", &registry.list().await);
    ctx.insert("active", "notices");
    ctx.insert("is_admin", &true);
    ctx.insert("notices", &store.list().await);
    ctx.insert("error", &error);
    match tera.render("notices.html", &ctx) {
        Ok(html) => HttpResponse::Ok().content_type("text/html").body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}

fn back() -> HttpResponse {
    HttpResponse::Found()
        .append_header(("Location", "/notices"))
        .finish()
}

/// GET /notices
pub async fn show(
    req: HttpRequest,
    tera: web::Data<Tera>,
    store: web::Data<NoticeStore>,
    registry: web::Data<AppRegistry>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    render(&tera, &store, &registry, None).await
}

/// POST /notices -- publish a new notice.
pub async fn create(
    req: HttpRequest,
    tera: web::Data<Tera>,
    store: web::Data<NoticeStore>,
    registry: web::Data<AppRegistry>,
    i18n: web::Data<I18n>,
    body: web::Bytes,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    let Some(draft) = draft_from(&body) else {
        return HttpResponse::BadRequest().finish();
    };
    match store.create(draft).await {
        Ok(_) => back(),
        Err(e) => render(&tera, &store, &registry, Some(&e.localize(&i18n))).await,
    }
}

/// POST /notices/{id} -- edit a notice or move it to another state.
pub async fn update(
    req: HttpRequest,
    tera: web::Data<Tera>,
    store: web::Data<NoticeStore>,
    registry: web::Data<AppRegistry>,
    i18n: web::Data<I18n>,
    path: web::Path<String>,
    body: web::Bytes,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    let Some(draft) = draft_from(&body) else {
        return HttpResponse::BadRequest().finish();
    };
    match store.update(&path.into_inner(), draft).await {
        Ok(()) => back(),
        Err(e) => render(&tera, &store, &registry, Some(&e.localize(&i18n))).await,
    }
}

/// POST /notices/{id}/delete
pub async fn delete(
    req: HttpRequest,
    tera: web::Data<Tera>,
    store: web::Data<NoticeStore>,
    registry: web::Data<AppRegistry>,
    i18n: web::Data<I18n>,
    path: web::Path<String>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    match store.remove(&path.into_inner()).await {
        Ok(()) => back(),
        Err(e) => render(&tera, &store, &registry, Some(&e.localize(&i18n))).await,
    }
}
