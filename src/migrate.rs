//! `heartbeat migrate`: moves a Heartbeat 0.2 install (JSON files under `data/`) into the
//! SQLite database, once.
//!
//! Everything is imported in one transaction and checked before it's committed; then the
//! old files are renamed to `*.migrated` (kept with `--keep`), so going back to 0.2 is a
//! rename away. `--dry-run` does the whole import into a throwaway in-memory database and
//! only prints what it would have done.
//!
//! The server refuses to start while there's 0.2 data next to an empty database, so an
//! upgrade can't come up looking like a fresh install.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use crate::alert_templates::{self, AlertTemplates, TemplateError, TemplateKind};
use crate::auth::{self, SessionError};
use crate::config::Config;
use crate::db::{Db, DbError};
use crate::notices::{self, Notice, NoticeError};
use crate::registry::{self, RegistryError};
use crate::theme::{self, ThemeError};
use crate::uptime::{self, Heartbeat};

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Sessions(#[from] SessionError),
    #[error(transparent)]
    Notices(#[from] NoticeError),
    #[error(transparent)]
    Templates(#[from] TemplateError),
    #[error(transparent)]
    Theme(#[from] ThemeError),
    #[error(
        "{0} already has apps: it was migrated before (use a new DATABASE_PATH to import again)"
    )]
    AlreadyMigrated(PathBuf),
    #[error("the import doesn't add up: {0}")]
    Mismatch(String),
    #[error(
        "found Heartbeat 0.2 data ({apps} apps in {file}) and an empty database: run `heartbeat migrate` before starting"
    )]
    Pending { apps: usize, file: String },
    #[error("could not rename {path}: {source}")]
    Rename {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Import into memory and report, touching nothing.
    pub dry_run: bool,
    /// Leave the 0.2 files where they are after a successful import.
    pub keep: bool,
}

impl Options {
    /// Parses the flags after `heartbeat migrate`.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self::default();
        for arg in args {
            match arg.as_str() {
                "--dry-run" => options.dry_run = true,
                "--keep" => options.keep = true,
                other => {
                    return Err(format!(
                        "unknown option {other} (expected --dry-run or --keep)"
                    ));
                }
            }
        }
        Ok(options)
    }
}

/// What an import brought over.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub apps: usize,
    /// Checks stored (within `UPTIME_RETENTION_DAYS`).
    pub checks: usize,
    /// Older checks, counted only in the daily uptime (within
    /// `UPTIME_DAILY_RETENTION_DAYS`).
    pub daily_only: usize,
    /// History lines that weren't a valid heartbeat (a torn write, usually), skipped.
    pub skipped_lines: usize,
    /// Checks older than both retentions, or repeating an earlier one's second, left out.
    pub dropped_checks: usize,
    /// Live sessions (nobody has to log in again).
    pub sessions: usize,
    pub notices: usize,
    /// Alert templates that had been customized.
    pub templates: usize,
    /// Whether there was a custom theme.
    pub theme: bool,
    /// 0.2 files renamed to `*.migrated` (none on a dry run or with `--keep`).
    pub renamed: Vec<PathBuf>,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} apps, {} checks, {} sessions, {} notices, {} custom alert templates{}",
            self.apps,
            self.checks,
            self.sessions,
            self.notices,
            self.templates,
            if self.theme { ", a custom theme" } else { "" }
        )?;
        if self.daily_only > 0 {
            write!(f, " (+{} older ones in the daily uptime)", self.daily_only)?;
        }
        if self.dropped_checks > 0 {
            write!(
                f,
                " ({} past both retentions or repeated, left out)",
                self.dropped_checks
            )?;
        }
        if self.skipped_lines > 0 {
            write!(
                f,
                " ({} unreadable history lines skipped)",
                self.skipped_lines
            )?;
        }
        Ok(())
    }
}

/// The 0.2 data found on disk, read and ready to import.
struct Legacy {
    apps: Vec<registry::RegisteredApp>,
    sessions: Vec<(String, auth::Session)>,
    notices: Vec<Notice>,
    templates: AlertTemplates,
    theme: String,
    /// The files it came from, renamed once the import is committed.
    files: Vec<PathBuf>,
}

impl Legacy {
    fn read(cfg: &Config) -> Result<Self, MigrateError> {
        let apps_file = PathBuf::from(&cfg.apps_file);
        let mut apps = registry::read_legacy_file(&apps_file)?;
        // Entries saved before embed tokens existed get one now, as 0.2 did on load.
        for app in apps.iter_mut().filter(|a| a.embed_token.is_empty()) {
            app.embed_token = registry::new_embed_token()?;
        }
        let sessions_file = PathBuf::from(&cfg.sessions_file);
        let notices_file = PathBuf::from(&cfg.notices_file);
        let templates_file = PathBuf::from(&cfg.alert_templates_file);
        let theme_file = PathBuf::from(&cfg.theme_file);
        Ok(Self {
            apps,
            sessions: auth::read_legacy_file(&sessions_file)?,
            notices: notices::read_legacy_file(&notices_file)?,
            templates: alert_templates::read_legacy_file(&templates_file)?,
            theme: theme::read_legacy_file(&theme_file)?,
            files: [
                apps_file,
                PathBuf::from(&cfg.uptime_dir),
                sessions_file,
                notices_file,
                templates_file,
                theme_file,
            ]
            .into_iter()
            .filter(|p| p.exists())
            .collect(),
        })
    }

    fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }
}

/// Runs the import described in the module docs.
pub async fn run(cfg: &Config, options: Options) -> Result<Report, MigrateError> {
    let legacy = Legacy::read(cfg)?;
    let db = if options.dry_run {
        Db::open_in_memory()
    } else {
        Db::open(&cfg.database_path).await?
    };
    if app_count(&db).await? > 0 {
        return Err(MigrateError::AlreadyMigrated(PathBuf::from(
            &cfg.database_path,
        )));
    }

    let expected = legacy.apps.len();
    let apps = legacy.apps;
    let others = Others {
        sessions: legacy.sessions,
        notices: legacy.notices,
        templates: legacy.templates,
        theme: legacy.theme,
    };
    let summary = others.summary();
    let uptime_dir = PathBuf::from(&cfg.uptime_dir);
    let now = uptime::unix_now();
    let cutoffs = Cutoffs {
        checks: now.saturating_sub(u64::from(cfg.uptime_retention_days) * DAY_SECS),
        days: now.saturating_sub(u64::from(cfg.uptime_daily_retention_days) * DAY_SECS) / DAY_SECS
            * DAY_SECS,
    };
    let (imported, history) = db
        .write(move |tx| {
            let mut history = HistoryImport::default();
            for app in &apps {
                registry::save_app(tx, app)?;
                history.import(
                    tx,
                    &uptime_dir.join(format!("{}.jsonl", app.slug)),
                    &app.slug,
                    cutoffs,
                )?;
            }
            others.import(tx)?;
            let count = |sql: &str| -> Result<usize, DbError> {
                let n: i64 = tx.query_row(sql, [], |row| row.get(0))?;
                Ok(usize::try_from(n).unwrap_or(usize::MAX))
            };
            let stored_apps = count("SELECT count(*) FROM apps")?;
            let stored_checks = count("SELECT count(*) FROM heartbeats")?;
            let counted = count("SELECT coalesce(sum(up + degraded + down), 0) FROM daily_uptime")?;
            if stored_checks != history.checks || counted != history.checks + history.daily_only {
                return Err(DbError::Integrity(format!(
                    "{} checks imported (+{} only per day), {stored_checks} stored, {counted} counted per day",
                    history.checks, history.daily_only
                )));
            }
            let stored_sessions = count("SELECT count(*) FROM sessions")?;
            let stored_notices = count("SELECT count(*) FROM notices")?;
            if stored_sessions < summary.sessions || stored_notices < summary.notices {
                return Err(DbError::Integrity(format!(
                    "{} sessions and {} notices read, {stored_sessions} and {stored_notices} stored",
                    summary.sessions, summary.notices
                )));
            }
            Ok((stored_apps, history))
        })
        .await?;
    if imported != expected {
        return Err(MigrateError::Mismatch(format!(
            "{expected} apps read, {imported} stored"
        )));
    }

    let mut report = Report {
        apps: imported,
        checks: history.checks,
        daily_only: history.daily_only,
        skipped_lines: history.skipped_lines,
        dropped_checks: history.dropped,
        sessions: summary.sessions,
        notices: summary.notices,
        templates: summary.templates,
        theme: summary.theme,
        renamed: Vec::new(),
    };
    if !options.dry_run && !options.keep {
        for path in legacy.files {
            let target = migrated_name(&path);
            std::fs::rename(&path, &target).map_err(|source| MigrateError::Rename {
                path: path.clone(),
                source,
            })?;
            report.renamed.push(target);
        }
    }
    Ok(report)
}

const DAY_SECS: u64 = 24 * 3600;

/// Everything besides apps and history: small, imported as is.
struct Others {
    sessions: Vec<(String, auth::Session)>,
    notices: Vec<Notice>,
    templates: AlertTemplates,
    theme: String,
}

/// How much of each `Others` holds, for the report and the checks.
#[derive(Debug, Clone, Copy)]
struct OthersSummary {
    sessions: usize,
    notices: usize,
    templates: usize,
    theme: bool,
}

impl Others {
    fn summary(&self) -> OthersSummary {
        OthersSummary {
            sessions: self.sessions.len(),
            notices: self.notices.len(),
            templates: TemplateKind::ALL
                .into_iter()
                .filter(|k| self.templates.get(*k).is_some())
                .count(),
            theme: !self.theme.is_empty(),
        }
    }

    fn import(&self, tx: &rusqlite::Transaction<'_>) -> Result<(), DbError> {
        for (id, session) in &self.sessions {
            auth::save_session(tx, id, session)?;
        }
        for notice in &self.notices {
            notices::save_notice(tx, notice)?;
        }
        alert_templates::save_templates(tx, &self.templates)?;
        theme::save_theme(tx, &self.theme)
    }
}

/// What of a 0.2 history is kept: checks since `checks`, daily counts since `days`.
#[derive(Debug, Clone, Copy)]
struct Cutoffs {
    checks: u64,
    days: u64,
}

/// Running totals of the history import.
#[derive(Debug, Default)]
struct HistoryImport {
    checks: usize,
    daily_only: usize,
    skipped_lines: usize,
    dropped: usize,
}

impl HistoryImport {
    /// Streams one app's `<slug>.jsonl` into the database, line by line: a whole 0.2
    /// history never has to fit in memory. A missing file is an app without history.
    fn import(
        &mut self,
        tx: &rusqlite::Transaction<'_>,
        path: &Path,
        slug: &str,
        cutoffs: Cutoffs,
    ) -> Result<(), DbError> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(DbError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        for line in std::io::BufReader::new(file).lines() {
            let line = line.map_err(|source| DbError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(beat) = serde_json::from_str::<Heartbeat>(&line) else {
                self.skipped_lines += 1;
                continue;
            };
            if beat.at >= cutoffs.checks {
                if uptime::store_heartbeat(tx, slug, &beat)? {
                    self.checks += 1;
                } else {
                    self.dropped += 1;
                }
            } else if beat.at >= cutoffs.days {
                uptime::count_heartbeat_day(tx, slug, &beat)?;
                self.daily_only += 1;
            } else {
                self.dropped += 1;
            }
        }
        Ok(())
    }
}

/// Fails if 0.2 data sits next to a database that has none: the server must not start
/// as an empty install on top of an unmigrated one.
pub async fn ensure_nothing_pending(cfg: &Config, db: &Db) -> Result<(), MigrateError> {
    if app_count(db).await? > 0 {
        return Ok(());
    }
    let legacy = Legacy::read(cfg)?;
    if legacy.is_empty() {
        return Ok(());
    }
    Err(MigrateError::Pending {
        apps: legacy.apps.len(),
        file: cfg.apps_file.clone(),
    })
}

async fn app_count(db: &Db) -> Result<i64, DbError> {
    db.read(|conn| Ok(conn.query_row("SELECT count(*) FROM apps", [], |row| row.get(0))?))
        .await
}

/// `apps.json` -> `apps.json.migrated`, or `apps.json.migrated.2` (and so on) when an
/// earlier import already left one there.
fn migrated_name(path: &Path) -> PathBuf {
    let with_suffix = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    let first = with_suffix(".migrated");
    if !first.exists() {
        return first;
    }
    (2..10_000)
        .map(|n| with_suffix(&format!(".migrated.{n}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path) -> Config {
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let mut cfg = crate::app_tests::test_config(&path);
        cfg.database_path = path("heartbeat.db");
        cfg
    }

    fn write_legacy_apps(dir: &Path) {
        std::fs::write(
            dir.join("apps.json"),
            r#"[
              {"slug":"api","name":"API","health_url":"https://api.example.com/health",
               "embed_token":"t1","public":true,"expect_status":[200,401]},
              {"slug":"old","name":"Old","logs_url":"https://old.example.com/logs"}
            ]"#,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn imports_apps_in_order_and_renames_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        write_legacy_apps(dir.path());

        let report = run(&cfg, Options::default()).await.unwrap();
        assert_eq!(report.apps, 2);
        assert_eq!(report.renamed, [dir.path().join("apps.json.migrated")]);
        assert!(!dir.path().join("apps.json").exists());

        let db = Db::open(&cfg.database_path).await.unwrap();
        let apps = registry::AppRegistry::load(db).await.unwrap().list().await;
        assert_eq!(apps[0].slug, "api");
        assert_eq!(apps[0].embed_token, "t1");
        assert!(apps[0].public);
        assert_eq!(apps[0].expect_status, [200, 401]);
        assert_eq!(apps[1].slug, "old");
        assert_eq!(
            apps[1].embed_token.len(),
            48,
            "a missing token is generated"
        );
    }

    #[tokio::test]
    async fn a_second_run_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        write_legacy_apps(dir.path());
        run(
            &cfg,
            Options {
                keep: true,
                ..Options::default()
            },
        )
        .await
        .unwrap();
        assert!(
            dir.path().join("apps.json").exists(),
            "--keep leaves the file"
        );
        let again = run(&cfg, Options::default()).await;
        assert!(
            matches!(again, Err(MigrateError::AlreadyMigrated(_))),
            "{again:?}"
        );
    }

    #[tokio::test]
    async fn a_dry_run_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        write_legacy_apps(dir.path());
        let report = run(
            &cfg,
            Options {
                dry_run: true,
                ..Options::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(report.apps, 2);
        assert_eq!(report.renamed, Vec::<PathBuf>::new());
        assert!(dir.path().join("apps.json").exists());
        assert!(!dir.path().join("heartbeat.db").exists());
    }

    #[tokio::test]
    async fn the_server_waits_for_a_pending_migration() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let db = Db::open(&cfg.database_path).await.unwrap();
        // Fresh install: nothing to migrate.
        ensure_nothing_pending(&cfg, &db).await.unwrap();

        write_legacy_apps(dir.path());
        assert!(matches!(
            ensure_nothing_pending(&cfg, &db).await,
            Err(MigrateError::Pending { apps: 2, .. })
        ));

        drop(db);
        run(&cfg, Options::default()).await.unwrap();
        let db = Db::open(&cfg.database_path).await.unwrap();
        ensure_nothing_pending(&cfg, &db).await.unwrap();
    }

    #[tokio::test]
    async fn imports_each_apps_history_and_counts_it_per_day() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        write_legacy_apps(dir.path());
        let now = uptime::unix_now();
        let old = now - 40 * 24 * 3600; // past the 30-day retention, within the 400-day one
        let ancient = now - 500 * 24 * 3600; // past both
        let uptime_dir = dir.path().join("uptime");
        std::fs::create_dir_all(&uptime_dir).unwrap();
        std::fs::write(
            uptime_dir.join("api.jsonl"),
            format!(
                "{{\"at\":{ancient},\"status\":\"up\",\"latency_ms\":10,\"message\":\"\"}}\n\
                 {{\"at\":{old},\"status\":\"up\",\"latency_ms\":10,\"message\":\"\"}}\n\
                 {{\"at\":{a},\"status\":\"up\",\"latency_ms\":20,\"message\":\"\"}}\n\
                 {{\"at\":{a},\"status\":\"up\",\"latency_ms\":20,\"message\":\"\"}}\n\
                 {{\"at\":{b},\"status\":\"down\",\"latency_ms\":null,\"message\":\"HTTP 502\"}}\n\
                 {{\"at\":{c},\"sta\n",
                a = now - 120,
                b = now - 60,
                c = now,
            ),
        )
        .unwrap();
        // History for an app that's no longer registered: not imported.
        std::fs::write(uptime_dir.join("gone.jsonl"), "{}\n").unwrap();

        let report = run(&cfg, Options::default()).await.unwrap();
        assert_eq!(report.apps, 2);
        assert_eq!(report.checks, 2);
        assert_eq!(
            report.dropped_checks, 2,
            "the old one and the repeated second"
        );
        assert_eq!(report.skipped_lines, 1, "the torn last line");
        assert!(report.renamed.contains(&dir.path().join("uptime.migrated")));

        let db = Db::open(&cfg.database_path).await.unwrap();
        let (latest, uptime_today) = db
            .read(move |conn| {
                let latest: (i64, i64, Option<String>) = conn.query_row(
                    "SELECT at, status, message FROM heartbeats WHERE slug = 'api' \
                     ORDER BY at DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                let today: (i64, i64) = conn.query_row(
                    "SELECT up, down FROM daily_uptime WHERE slug = 'api' AND day = ?1",
                    [i64::try_from(now / 86_400 * 86_400).unwrap()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                Ok((latest, today))
            })
            .await
            .unwrap();
        assert_eq!(
            latest,
            (i64::try_from(now - 60).unwrap(), 2, Some("HTTP 502".into()))
        );
        // Both checks are from today unless the test runs right after midnight UTC.
        if (now - 120) / 86_400 == now / 86_400 {
            assert_eq!(uptime_today, (1, 1));
        }
    }

    #[tokio::test]
    async fn imports_sessions_notices_templates_and_the_theme() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        write_legacy_apps(dir.path());
        let live = uptime::unix_now() + 3600;
        std::fs::write(
            dir.path().join("sessions.json"),
            format!(
                r#"{{"s1":{{"upstream_token":"jwt","expires_at":{live}}},
                     "gone":{{"upstream_token":"x","expires_at":1}}}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("notices.json"),
            r#"[{"id":"n1","title":"Slow","body":"","state":"identified",
                 "created_at":10,"updated_at":20,"apps":["api"]}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("alert_templates.json"),
            r#"{"down":"ALERTA {app}","degraded":"   "}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("theme.css"), ":root { --glass: #101820; }").unwrap();

        let report = run(&cfg, Options::default()).await.unwrap();
        assert_eq!(
            (
                report.sessions,
                report.notices,
                report.templates,
                report.theme
            ),
            (1, 1, 1, true)
        );
        assert_eq!(
            report.renamed.len(),
            5,
            "apps, sessions, notices, templates, theme"
        );

        let db = Db::open(&cfg.database_path).await.unwrap();
        let notices = notices::NoticeStore::load(db.clone())
            .await
            .unwrap()
            .list()
            .await;
        assert_eq!(
            (notices[0].id.as_str(), notices[0].apps.as_slice()),
            ("n1", &["api".to_string()][..])
        );
        let i18n = crate::i18n::I18n::new(crate::i18n::Lang::Es);
        let templates = alert_templates::TemplateStore::load(db.clone(), &i18n)
            .await
            .unwrap();
        assert_eq!(templates.effective(TemplateKind::Down), "ALERTA {app}");
        assert!(!templates.is_custom(TemplateKind::Degraded));
        let theme = theme::ThemeStore::load(db.clone()).await.unwrap();
        assert!(theme.css().contains("#101820"));
        let sessions: i64 = db
            .read(|conn| {
                Ok(
                    conn.query_row("SELECT count(*) FROM sessions WHERE id = 's1'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(sessions, 1, "only the live session");
    }

    #[test]
    fn an_earlier_migrated_copy_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("apps.json");
        assert_eq!(migrated_name(&apps), dir.path().join("apps.json.migrated"));
        std::fs::write(dir.path().join("apps.json.migrated"), "").unwrap();
        std::fs::create_dir(dir.path().join("apps.json.migrated.2")).unwrap();
        assert_eq!(
            migrated_name(&apps),
            dir.path().join("apps.json.migrated.3")
        );
    }

    #[test]
    fn options_parse() {
        let parse = |args: &[&str]| Options::from_args(args.iter().map(ToString::to_string));
        let both = parse(&["--dry-run", "--keep"]).unwrap();
        assert!(both.dry_run && both.keep);
        assert!(parse(&["--force"]).is_err());
    }
}
