//! The SQLite database (`DATABASE_PATH`) every store keeps its data in.
//!
//! One writer connection and a couple of readers, all used from `spawn_blocking` so a
//! query never stalls the async runtime. In WAL mode readers never wait for the writer.
//! `write` runs its closure in a transaction: `Ok` commits, an error (or a panic) rolls
//! back, so a store can't leave half an operation behind.
//!
//! The schema lives in `migrations/`, applied in order on open and tracked with
//! `PRAGMA user_version`. A database from a newer Heartbeat is refused rather than
//! half-understood.

// The stores move onto the database one by one (see SQLITE.md); until they all do, parts
// of this API have no caller outside the tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

/// Every schema migration, in order; the database's `user_version` is how many ran.
const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

/// Read connections next to the writer, for a file database.
const READERS: usize = 2;

/// How long a statement waits on a lock held by another connection before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("could not open the database {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error(
        "the database {path} is from a newer Heartbeat (schema {found}; this version knows up to {supported})"
    )]
    TooNew {
        path: PathBuf,
        found: usize,
        supported: usize,
    },
    #[error("could not apply database migration {version}: {source}")]
    Migration {
        version: usize,
        #[source]
        source: rusqlite::Error,
    },
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored data is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("a database task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl crate::i18n::Localize for DbError {
    fn localize(&self, i18n: &crate::i18n::I18n) -> String {
        i18n.text("err.internal", &[("error", &self.to_string())])
    }
}

#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    writer: Mutex<Connection>,
    /// Empty for an in-memory database: each connection to `:memory:` is its own
    /// database, so everything goes through the writer.
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
}

impl Db {
    /// Opens (or creates) the database at `path` and brings its schema up to date.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, DbError> {
        let path = path.into();
        tokio::task::spawn_blocking(move || Self::open_blocking(path)).await?
    }

    fn open_blocking(path: PathBuf) -> Result<Self, DbError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| DbError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let open_err = |source| DbError::Open {
            path: path.clone(),
            source,
        };
        let mut writer = Connection::open(&path).map_err(open_err)?;
        // Sessions live here: the file (and the WAL files SQLite derives from it) is for
        // this user only, like the old sessions.json.
        restrict_permissions(&path)?;
        configure(&writer).map_err(open_err)?;
        // `journal_mode` answers with the mode it ended up in, so it's a query, not an update.
        let mode: String = writer
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(open_err)?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(%mode, path = %path.display(), "database is not in WAL mode");
        }
        writer
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(open_err)?;
        migrate(&mut writer, &path)?;

        let readers = (0..READERS)
            .map(|_| {
                let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(open_err)?;
                configure(&conn).map_err(open_err)?;
                Ok(Mutex::new(conn))
            })
            .collect::<Result<_, DbError>>()?;
        Ok(Self::from_parts(path, writer, readers))
    }

    /// A private, empty database with the current schema, for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Self {
        let mut conn = Connection::open_in_memory().expect("in-memory database");
        configure(&conn).expect("configure in-memory database");
        let path = PathBuf::from(":memory:");
        migrate(&mut conn, &path).expect("migrate in-memory database");
        Self::from_parts(path, conn, Vec::new())
    }

    fn from_parts(path: PathBuf, writer: Connection, readers: Vec<Mutex<Connection>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                path,
                writer: Mutex::new(writer),
                readers,
                next_reader: AtomicUsize::new(0),
            }),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Runs `f` on a read-only connection.
    pub async fn read<T, F>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&Connection) -> Result<T, DbError> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let conn = if inner.readers.is_empty() {
                &inner.writer
            } else {
                let i = inner.next_reader.fetch_add(1, Ordering::Relaxed) % inner.readers.len();
                &inner.readers[i]
            };
            f(&conn.lock().unwrap_or_else(PoisonError::into_inner))
        })
        .await?
    }

    /// Runs `f` in a write transaction: committed if it returns `Ok`, rolled back
    /// otherwise.
    pub async fn write<T, F>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T, DbError> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut conn = inner.writer.lock().unwrap_or_else(PoisonError::into_inner);
            // IMMEDIATE takes the write lock up front, so a transaction that reads before
            // writing can't fail halfway with SQLITE_BUSY.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        })
        .await?
    }
}

/// Settings every connection needs, reader or writer.
fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", "ON")
}

/// Applies the migrations the database hasn't seen yet, each in its own transaction
/// together with the `user_version` bump.
fn migrate(conn: &mut Connection, path: &Path) -> Result<(), DbError> {
    let found = schema_version(conn)?;
    if found > MIGRATIONS.len() {
        return Err(DbError::TooNew {
            path: path.to_path_buf(),
            found,
            supported: MIGRATIONS.len(),
        });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(found) {
        let version = i + 1;
        let apply = |conn: &mut Connection| -> rusqlite::Result<()> {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(
                None,
                "user_version",
                i64::try_from(version).unwrap_or(i64::MAX),
            )?;
            tx.commit()
        };
        apply(conn).map_err(|source| DbError::Migration { version, source })?;
        tracing::info!(version, "applied database migration");
    }
    Ok(())
}

/// The schema version (`PRAGMA user_version`): how many migrations the database has.
fn schema_version(conn: &Connection) -> rusqlite::Result<usize> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    // Negative only if something else wrote it: treat it as unknown, i.e. too new.
    Ok(usize::try_from(version).unwrap_or(usize::MAX))
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), DbError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        DbError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), DbError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[tokio::test]
    async fn a_new_database_gets_the_whole_schema() {
        let db = Db::open_in_memory();
        let (names, version) = db
            .read(|conn| Ok((tables(conn), schema_version(conn)?)))
            .await
            .unwrap();
        assert_eq!(version, MIGRATIONS.len());
        for table in [
            "apps",
            "heartbeats",
            "daily_uptime",
            "sessions",
            "notices",
            "kv",
        ] {
            assert!(
                names.iter().any(|n| n == table),
                "{table} missing: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn reopening_a_file_keeps_its_data_and_uses_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/heartbeat.db");
        let db = Db::open(&path).await.unwrap();
        db.write(|tx| {
            tx.execute("INSERT INTO kv (key, value) VALUES ('k', 'v')", [])?;
            Ok(())
        })
        .await
        .unwrap();
        drop(db);

        let db = Db::open(&path).await.unwrap();
        let (value, mode) = db
            .read(|conn| {
                let value: String =
                    conn.query_row("SELECT value FROM kv WHERE key = 'k'", [], |r| r.get(0))?;
                let mode: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
                Ok((value, mode))
            })
            .await
            .unwrap();
        assert_eq!(value, "v");
        assert_eq!(mode, "wal");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn a_failed_write_is_rolled_back() {
        let db = Db::open_in_memory();
        let result = db
            .write(|tx| {
                tx.execute("INSERT INTO kv (key, value) VALUES ('k', 'v')", [])?;
                // A constraint violation after the first insert: the whole transaction goes.
                tx.execute("INSERT INTO kv (key, value) VALUES ('k', 'again')", [])?;
                Ok(())
            })
            .await;
        assert!(matches!(result, Err(DbError::Sqlite(_))), "{result:?}");
        let count: i64 = db
            .read(|conn| Ok(conn.query_row("SELECT count(*) FROM kv", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn deleting_an_app_deletes_its_history() {
        let db = Db::open_in_memory();
        db.write(|tx| {
            tx.execute(
                "INSERT INTO apps (slug, position, name, embed_token) VALUES ('api', 0, 'API', 't')",
                [],
            )?;
            tx.execute(
                "INSERT INTO heartbeats (slug, at, status) VALUES ('api', 1, 0)",
                [],
            )?;
            tx.execute(
                "INSERT INTO daily_uptime (slug, day, up) VALUES ('api', 0, 1)",
                [],
            )?;
            tx.execute("DELETE FROM apps WHERE slug = 'api'", [])?;
            Ok(())
        })
        .await
        .unwrap();
        let left: i64 = db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT (SELECT count(*) FROM heartbeats) + (SELECT count(*) FROM daily_uptime)",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn a_database_from_a_newer_heartbeat_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heartbeat.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 99_i64).unwrap();
        }
        let err = Db::open(&path).await.err().expect("newer schema refused");
        assert!(
            matches!(err, DbError::TooNew { found: 99, supported, .. } if supported == MIGRATIONS.len()),
            "{err}"
        );
    }

    #[tokio::test]
    async fn readers_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("heartbeat.db")).await.unwrap();
        let result = db
            .read(|conn| {
                conn.execute("INSERT INTO kv (key, value) VALUES ('k', 'v')", [])?;
                Ok(())
            })
            .await;
        assert!(result.is_err());
    }
}
