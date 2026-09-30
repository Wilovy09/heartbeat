//! /audit: the audit log, newest first, filtered by who, what, which app, where from and
//! when; `/audit/export` downloads the same selection as CSV.

use actix_web::{HttpRequest, HttpResponse, http::header, web};
use serde::{Deserialize, Serialize};
use tera::{Context, Tera};

use crate::audit::{self, AuditLog, Entry, Event, Filter, MAX_ENTRIES};
use crate::auth;
use crate::i18n::I18n;

/// Entries per page on /audit.
const PAGE: usize = 100;
/// Action families for the filter, in the order the menu lists them.
const GROUPS: [&str; 10] = [
    "login",
    "logout",
    "app.",
    "chat.",
    "logs.",
    "uptime.",
    "notice.",
    "templates.",
    "theme.",
    "settings.",
];

/// The filter form, as submitted (every field optional; blank = no filter).
#[derive(Deserialize, Serialize, Default)]
pub struct AuditQuery {
    #[serde(default)]
    actor: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    app: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    outcome: String,
    /// `YYYY-MM-DD`, UTC.
    #[serde(default)]
    from: String,
    /// `YYYY-MM-DD`, UTC, inclusive.
    #[serde(default)]
    to: String,
    before: Option<i64>,
}

fn non_blank(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Unix seconds at 00:00 UTC of a `YYYY-MM-DD` date.
fn day_start(date: &str) -> Option<u64> {
    let mut parts = date.trim().splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm), proleptic Gregorian.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400).ok()
}

impl AuditQuery {
    fn filter(&self, limit: usize) -> Filter {
        Filter {
            actor: non_blank(&self.actor),
            action: non_blank(&self.action),
            target: non_blank(&self.app),
            source: non_blank(&self.source),
            outcome: non_blank(&self.outcome),
            from: day_start(&self.from),
            until: day_start(&self.to).map(|start| start + 86_400),
            before_id: self.before,
            limit,
        }
    }

    /// The query string for the next page: the same filters, older entries.
    fn older(&self, before: i64) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in [
            ("actor", &self.actor),
            ("action", &self.action),
            ("app", &self.app),
            ("source", &self.source),
            ("outcome", &self.outcome),
            ("from", &self.from),
            ("to", &self.to),
        ] {
            if !value.trim().is_empty() {
                query.append_pair(key, value);
            }
        }
        query.append_pair("before", &before.to_string());
        query.finish()
    }

    /// The current filters, without the page, for the export link.
    fn filters_only(&self) -> String {
        let mut query = self.older(0);
        if let Some(at) = query.rfind("before=") {
            query.truncate(at.saturating_sub(1));
        }
        query
    }
}

/// One row of the table: the entry, with its detail pretty-printed.
#[derive(Serialize)]
struct Row {
    #[serde(flatten)]
    entry: Entry,
    detail_json: Option<String>,
}

/// GET /audit
pub async fn show(
    req: HttpRequest,
    tera: web::Data<Tera>,
    i18n: web::Data<I18n>,
    log: web::Data<AuditLog>,
    query: web::Query<AuditQuery>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    // One more than a page, to know whether there's an older one.
    let mut entries = match log.query(query.filter(PAGE + 1)).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::error!(error = %e, "audit: could not read the log");
            return HttpResponse::InternalServerError().body(e.to_string());
        }
    };
    let older = (entries.len() > PAGE).then(|| {
        entries.truncate(PAGE);
        query.older(entries.last().map_or(0, |e| e.id))
    });
    let rows: Vec<Row> = entries
        .into_iter()
        .map(|entry| Row {
            detail_json: entry
                .detail
                .as_ref()
                .filter(|d| !d.is_null())
                .and_then(|d| serde_json::to_string_pretty(d).ok()),
            entry,
        })
        .collect();
    let mut ctx = Context::new();
    ctx.insert("active", "audit");
    ctx.insert("is_admin", &true);
    ctx.insert("rows", &rows);
    ctx.insert("query", &*query);
    let groups: Vec<_> = GROUPS
        .iter()
        .map(|prefix| {
            let key = format!("audit.group_{}", prefix.trim_end_matches('.'));
            serde_json::json!({ "prefix": prefix, "label": i18n.text(&key, &[]) })
        })
        .collect();
    ctx.insert("groups", &groups);
    ctx.insert("older", &older);
    ctx.insert("export_query", &query.filters_only());
    ctx.insert("paged", &query.before.is_some());
    match tera.render("audit.html", &ctx) {
        Ok(html) => HttpResponse::Ok().content_type("text/html").body(html),
        Err(e) => HttpResponse::InternalServerError().body(format!("template error: {e}")),
    }
}

/// A CSV field, quoted when it holds a comma, quote or newline.
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// GET /audit/export -- the filtered entries (up to `MAX_ENTRIES`) as CSV.
pub async fn export(
    req: HttpRequest,
    log: web::Data<AuditLog>,
    query: web::Query<AuditQuery>,
) -> HttpResponse {
    if let Err(resp) = auth::require_admin(&req) {
        return resp;
    }
    let entries = match log.query(query.filter(MAX_ENTRIES)).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::error!(error = %e, "audit: could not read the log");
            return HttpResponse::InternalServerError().body(e.to_string());
        }
    };
    let event = Event::web(&req, "audit.export").detail(serde_json::json!({
        "filters": query.filters_only(),
        "entries": entries.len(),
    }));
    audit::record(&req, event).await;
    let mut body = String::from("id,at,source,actor,action,target,outcome,detail,ip\n");
    for e in &entries {
        let detail = e
            .detail
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        let fields = [
            e.id.to_string(),
            e.at.to_string(),
            e.source.clone(),
            e.actor.clone(),
            e.action.clone(),
            e.target.clone().unwrap_or_default(),
            e.outcome.clone(),
            detail,
            e.ip.clone().unwrap_or_default(),
        ];
        let line: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
        body.push_str(&line.join(","));
        body.push('\n');
    }
    HttpResponse::Ok()
        .content_type("text/csv; charset=utf-8")
        .insert_header((
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"heartbeat-audit.csv\"",
        ))
        .body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc_day_starts() {
        assert_eq!(day_start("1970-01-01"), Some(0));
        assert_eq!(day_start("2026-09-30"), Some(1_790_726_400));
        assert_eq!(day_start("2024-02-29"), Some(1_709_164_800));
        for bad in ["", "2026-13-01", "2026-09", "yesterday"] {
            assert_eq!(day_start(bad), None, "{bad}");
        }
    }

    #[test]
    fn paging_keeps_the_filters() {
        let query = AuditQuery {
            actor: "ana@example.com".into(),
            action: "app.".into(),
            ..AuditQuery::default()
        };
        assert_eq!(
            query.older(42),
            "actor=ana%40example.com&action=app.&before=42"
        );
        assert_eq!(query.filters_only(), "actor=ana%40example.com&action=app.");
        assert_eq!(AuditQuery::default().filters_only(), "");
    }
}
