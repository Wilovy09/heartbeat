//! Templates, static files and vendored libraries, embedded in the binary so a release is
//! one file. In debug builds rust-embed reads them from disk instead, so editing a template
//! or a script needs no rebuild while developing.

use actix_web::http::header;
use actix_web::{HttpRequest, HttpResponse, web};
use rust_embed::RustEmbed;
use std::fmt::Write as _;
use tera::Tera;

#[derive(RustEmbed)]
#[folder = "templates/"]
struct Templates;

#[derive(RustEmbed)]
#[folder = "static/"]
struct Static;

#[derive(RustEmbed)]
#[folder = "libs/"]
struct Libs;

/// Adds every embedded template to `tera` (at once, so `extends` resolves).
pub fn load_templates(tera: &mut Tera) -> tera::TeraResult<()> {
    let templates: Vec<(String, String)> = Templates::iter()
        .filter_map(|name| {
            let file = Templates::get(&name)?;
            let source = String::from_utf8_lossy(&file.data).into_owned();
            Some((name.into_owned(), source))
        })
        .collect();
    tera.add_raw_templates(templates)
}

/// An embedded file as an HTTP response, with a content hash `ETag` so browsers
/// revalidate cheaply (the names carry no fingerprint, hence the short max-age).
fn serve(req: &HttpRequest, file: Option<rust_embed::EmbeddedFile>) -> HttpResponse {
    let Some(file) = file else {
        return HttpResponse::NotFound().finish();
    };
    let etag =
        file.metadata
            .sha256_hash()
            .iter()
            .take(12)
            .fold(String::from("\""), |mut tag, b| {
                // Writing into a String can't fail.
                let _ = write!(tag, "{b:02x}");
                tag
            })
            + "\"";
    let fresh = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag);
    if fresh {
        return HttpResponse::NotModified()
            .insert_header((header::ETAG, etag))
            .finish();
    }
    HttpResponse::Ok()
        .content_type(file.metadata.mimetype())
        .insert_header((header::ETAG, etag))
        .insert_header((header::CACHE_CONTROL, "public, max-age=300"))
        .body(file.data.into_owned())
}

/// GET /static/{path}
pub async fn static_file(req: HttpRequest, path: web::Path<String>) -> HttpResponse {
    serve(&req, Static::get(&path))
}

/// GET /libs/{path}
pub async fn lib_file(req: HttpRequest, path: web::Path<String>) -> HttpResponse {
    serve(&req, Libs::get(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_is_embedded_and_parses() {
        let mut tera = Tera::new();
        crate::i18n::I18n::new(crate::i18n::Lang::Es).register(&mut tera);
        load_templates(&mut tera).unwrap();
        assert!(Templates::get("dashboard.html").is_some());
        assert!(Static::get("embed.js").is_some());
        assert!(Libs::get("alpinejs/alpine.js").is_some());
    }
}
