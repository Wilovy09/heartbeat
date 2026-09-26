//! Bundle check for single-page apps. An SPA's server answers 200 with the same
//! `index.html` for every path -- often even for a missing `/assets/index-abc.js`, which
//! then arrives as HTML. So a broken deploy (a bundle that never uploaded, a stale hash)
//! still looks "up" to a plain health check while users get a blank page. This reads the
//! page's `<script src>` / `<link rel="stylesheet|modulepreload">` references and checks
//! that each one really loads as JS or CSS.

use reqwest::Url;
use std::time::Duration;

use crate::outbound::Outbound;

/// Most bundles checked per page; a typical Vite/webpack page has a handful.
const MAX_BUNDLES: usize = 12;
const BUNDLE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleKind {
    Script,
    Stylesheet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub url: Url,
    pub kind: BundleKind,
}

/// The inside of every `<name ...>` tag (ASCII case-insensitive), without the brackets.
fn tags<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
    let lower = html.to_ascii_lowercase(); // same byte offsets as `html`
    let open = format!("<{name}");
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(&open) {
        let start = from + pos + open.len();
        // `<scriptx` isn't a <script> tag.
        if !lower[start..].starts_with(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/') {
            from = start;
            continue;
        }
        let Some(len) = lower[start..].find('>') else {
            break;
        };
        found.push(&html[start..start + len]);
        from = start + len;
    }
    found
}

/// Value of attribute `name` in a tag's inside: quoted with `"`/`'`, or bare.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut from = 0;
    while let Some(pos) = lower[from..].find(&needle) {
        let at = from + pos;
        let is_whole_name = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = &tag[at + needle.len()..];
        from = at + needle.len();
        if !is_whole_name {
            continue;
        }
        return match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].find(q).map(|end| rest[1..=end].to_string()),
            Some(_) => Some(
                rest.split(|c: char| c.is_ascii_whitespace() || c == '/')
                    .next()?
                    .to_string(),
            ),
            None => None,
        };
    }
    None
}

/// The JS/CSS bundles `html` references, resolved against `base` (the page's URL).
#[must_use]
pub fn extract(html: &str, base: &Url) -> Vec<Bundle> {
    let mut found: Vec<(String, BundleKind)> = tags(html, "script")
        .into_iter()
        .filter_map(|tag| attr(tag, "src").map(|src| (src, BundleKind::Script)))
        .collect();
    for tag in tags(html, "link") {
        let rel = attr(tag, "rel").unwrap_or_default().to_ascii_lowercase();
        let kind = if rel.split_whitespace().any(|r| r == "stylesheet") {
            BundleKind::Stylesheet
        } else if rel.split_whitespace().any(|r| r == "modulepreload") {
            BundleKind::Script
        } else {
            continue;
        };
        if let Some(href) = attr(tag, "href") {
            found.push((href, kind));
        }
    }
    let mut bundles: Vec<Bundle> = Vec::new();
    for (reference, kind) in found {
        let Ok(url) = base.join(reference.trim()) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") || bundles.iter().any(|b| b.url == url) {
            continue;
        }
        bundles.push(Bundle { url, kind });
        if bundles.len() == MAX_BUNDLES {
            break;
        }
    }
    bundles
}

/// Why a bundle isn't the JS/CSS it should be. Rendered in the UI's language by the
/// monitor (`Localize`), since it becomes the check's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleProblem {
    Status {
        path: String,
        status: u16,
    },
    /// `content_type` is `None` when the response had none.
    WrongType {
        path: String,
        content_type: Option<String>,
    },
    Unreachable {
        path: String,
        error: String,
    },
}

impl crate::i18n::Localize for BundleProblem {
    fn localize(&self, i18n: &crate::i18n::I18n) -> String {
        match self {
            Self::Status { path, status } => i18n.text(
                "check.bundle_status",
                &[("path", path), ("status", &status.to_string())],
            ),
            Self::WrongType {
                path,
                content_type: Some(ct),
            } => i18n.text("check.bundle_type", &[("path", path), ("type", ct)]),
            Self::WrongType {
                path,
                content_type: None,
            } => i18n.text("check.bundle_no_type", &[("path", path)]),
            Self::Unreachable { path, error } => i18n.text(
                "check.bundle_unreachable",
                &[("path", path), ("error", error)],
            ),
        }
    }
}

/// Why a bundle response isn't the JS/CSS it should be, or `None` if it's fine.
#[must_use]
pub fn problem(bundle: &Bundle, status: u16, content_type: &str) -> Option<BundleProblem> {
    let path = bundle.url.path().to_string();
    if !(200..300).contains(&status) {
        return Some(BundleProblem::Status { path, status });
    }
    let ct = content_type.to_ascii_lowercase();
    let ok = match bundle.kind {
        BundleKind::Script => ct.contains("javascript") || ct.contains("ecmascript"),
        BundleKind::Stylesheet => ct.contains("text/css"),
    };
    if ok {
        return None;
    }
    let got = ct.split(';').next().unwrap_or_default().trim();
    Some(BundleProblem::WrongType {
        path,
        content_type: (!got.is_empty()).then(|| got.to_string()),
    })
}

/// Checks every bundle; the first problem found, if any. Bundles on hosts outside
/// `ALLOWED_HOSTS` (e.g. a third-party CDN) are skipped: they can't be requested.
pub async fn verify(outbound: &Outbound, bundles: &[Bundle]) -> Option<BundleProblem> {
    for bundle in bundles {
        let Ok(url) = outbound.check(bundle.url.as_str()) else {
            continue;
        };
        let resp = match outbound
            .client()
            .get(url)
            .timeout(BUNDLE_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                return Some(BundleProblem::Unreachable {
                    path: bundle.url.path().to_string(),
                    error: e.to_string(),
                });
            }
        };
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if let Some(why) = problem(bundle, resp.status().as_u16(), &content_type) {
            return Some(why);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const VITE_INDEX: &str = r#"<!doctype html>
<html lang="es">
  <head>
    <link rel="icon" href="/favicon.ico">
    <title>Pulso | Adquiere.co</title>
    <script type="module" crossorigin src="/assets/index-BmLipbGa.js"></script>
    <link rel="modulepreload" crossorigin href="/assets/vendor-X1.js">
    <LINK REL=stylesheet crossorigin href='/assets/index-Bf41gqzV.css'>
    <script>window.inline = true</script>
    <script src="https://cdn.example.com/lib.js"></script>
    <script src="/assets/index-BmLipbGa.js"></script>
  </head>
  <body><div id="app"></div></body>
</html>"#;

    fn base() -> Url {
        Url::parse("https://app.example.com/dashboard").unwrap()
    }

    #[test]
    fn extracts_scripts_preloads_and_stylesheets_once() {
        let bundles = extract(VITE_INDEX, &base());
        let found: Vec<(&str, BundleKind)> =
            bundles.iter().map(|b| (b.url.as_str(), b.kind)).collect();
        assert_eq!(
            found,
            [
                (
                    "https://app.example.com/assets/index-BmLipbGa.js",
                    BundleKind::Script
                ),
                ("https://cdn.example.com/lib.js", BundleKind::Script),
                (
                    "https://app.example.com/assets/vendor-X1.js",
                    BundleKind::Script
                ),
                (
                    "https://app.example.com/assets/index-Bf41gqzV.css",
                    BundleKind::Stylesheet
                ),
            ]
        );
    }

    #[test]
    fn ignores_non_bundles() {
        let html = r#"<scripty src="/x.js"><link rel="icon" href="/i.png"><script data-src="/y.js"></script>
            <script src="data:text/javascript,1"></script>"#;
        assert_eq!(extract(html, &base()), Vec::new());
    }

    #[test]
    fn a_bundle_served_as_html_or_missing_is_a_problem() {
        let js = Bundle {
            url: Url::parse("https://app.example.com/assets/index-X.js").unwrap(),
            kind: BundleKind::Script,
        };
        let css = Bundle {
            url: Url::parse("https://app.example.com/assets/index-X.css").unwrap(),
            kind: BundleKind::Stylesheet,
        };
        assert_eq!(
            problem(&js, 200, "application/javascript; charset=utf-8"),
            None
        );
        assert_eq!(problem(&js, 200, "text/javascript"), None);
        assert_eq!(problem(&css, 200, "text/css"), None);
        assert_eq!(
            problem(&js, 200, "text/html; charset=utf-8"),
            Some(BundleProblem::WrongType {
                path: "/assets/index-X.js".into(),
                content_type: Some("text/html".into()),
            })
        );
        assert_eq!(
            problem(&css, 404, "text/html"),
            Some(BundleProblem::Status {
                path: "/assets/index-X.css".into(),
                status: 404,
            })
        );
        assert_eq!(
            problem(&js, 200, ""),
            Some(BundleProblem::WrongType {
                path: "/assets/index-X.js".into(),
                content_type: None,
            })
        );
    }

    #[test]
    fn problems_read_in_the_ui_language() {
        use crate::i18n::{I18n, Lang, Localize};
        let p = BundleProblem::WrongType {
            path: "/assets/x.js".into(),
            content_type: Some("text/html".into()),
        };
        assert_eq!(
            p.localize(&I18n::new(Lang::Es)),
            "Bundle roto: /assets/x.js respondió text/html"
        );
        assert_eq!(
            p.localize(&I18n::new(Lang::En)),
            "Broken bundle: /assets/x.js answered text/html"
        );
    }
}
