//! The custom theme: CSS an admin saves from /settings (stored in the `kv` table), served
//! at `/theme/custom.css` and
//! applied on top of the dark tokens when a user picks "Custom" (the choice itself lives
//! in the browser, see `static/theme.js`). Meant for overriding the color tokens, so it's
//! size-capped and anything that would load external resources is refused -- the CSP
//! blocks those anyway, this makes the editor say why instead of failing silently.

use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

use rusqlite::OptionalExtension;

use crate::db::{Db, DbError};

/// The `kv` key the theme is stored under.
const KEY: &str = "theme.custom_css";

/// Largest custom theme accepted.
pub const MAX_THEME_BYTES: usize = 32 * 1024;

/// Constructs a theme may not contain, lowercase.
const FORBIDDEN: [&str; 4] = ["@import", "url(", "expression(", "</"];

#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("the theme is larger than {MAX_THEME_BYTES} bytes")]
    TooLarge,
    #[error("the theme may not contain `{0}`")]
    Forbidden(&'static str),
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Db(#[from] DbError),
}

impl ThemeError {
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        move |source| Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

impl crate::i18n::Localize for ThemeError {
    fn localize(&self, i18n: &crate::i18n::I18n) -> String {
        match self {
            Self::TooLarge => i18n.text(
                "theme.err_large",
                &[("kb", &(MAX_THEME_BYTES / 1024).to_string())],
            ),
            Self::Forbidden(what) => i18n.text("theme.err_forbidden", &[("what", what)]),
            Self::Io { .. } | Self::Db(_) => {
                i18n.text("err.internal", &[("error", &self.to_string())])
            }
        }
    }
}

/// Checks a theme before it's saved; returns it trimmed.
fn validated(css: &str) -> Result<String, ThemeError> {
    let css = css.trim();
    if css.len() > MAX_THEME_BYTES {
        return Err(ThemeError::TooLarge);
    }
    let lower = css.to_ascii_lowercase();
    if let Some(what) = FORBIDDEN.into_iter().find(|f| lower.contains(f)) {
        return Err(ThemeError::Forbidden(what));
    }
    Ok(css.to_string())
}

pub struct ThemeStore {
    db: Db,
    css: RwLock<String>,
}

/// Stores the theme (blank = none).
pub(crate) fn save_theme(tx: &rusqlite::Transaction<'_>, css: &str) -> Result<(), DbError> {
    if css.is_empty() {
        tx.execute("DELETE FROM kv WHERE key = ?1", [KEY])?;
    } else {
        tx.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [KEY, css],
        )?;
    }
    Ok(())
}

/// Reads a 0.2 `theme.css` (for `heartbeat migrate`), checked like a saved one; a
/// missing file is no theme.
pub(crate) fn read_legacy_file(path: &Path) -> Result<String, ThemeError> {
    match std::fs::read_to_string(path) {
        Ok(css) => validated(&css),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(ThemeError::io(path)(e)),
    }
}

impl ThemeStore {
    pub async fn load(db: Db) -> Result<Self, ThemeError> {
        let css = db
            .read(|conn| {
                Ok(conn
                    .query_row("SELECT value FROM kv WHERE key = ?1", [KEY], |row| {
                        row.get(0)
                    })
                    .optional()?
                    .unwrap_or_default())
            })
            .await?;
        Ok(Self {
            db,
            css: RwLock::new(css),
        })
    }

    #[must_use]
    pub fn css(&self) -> String {
        self.css
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Validates and stores the theme (blank = no custom theme).
    pub async fn save(&self, css: &str) -> Result<(), ThemeError> {
        let css = validated(css)?;
        let stored = css.clone();
        self.db.write(move |tx| save_theme(tx, &stored)).await?;
        *self.css.write().unwrap_or_else(PoisonError::into_inner) = css;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn themes_persist_and_blank_clears() {
        let db = Db::open_in_memory();
        let store = ThemeStore::load(db.clone()).await.unwrap();
        assert_eq!(store.css(), "");
        store.save("  :root { --glass: #101820; }  ").await.unwrap();
        let reloaded = ThemeStore::load(db.clone()).await.unwrap();
        assert_eq!(reloaded.css(), ":root { --glass: #101820; }");
        reloaded.save("   ").await.unwrap();
        assert_eq!(reloaded.css(), "");
        assert_eq!(ThemeStore::load(db).await.unwrap().css(), "");
    }

    #[test]
    fn external_loads_and_oversized_themes_are_refused() {
        for bad in [
            "@import 'x.css';",
            ":root { --x: URL(https://evil.example/a.png); }",
            "</style><script>",
        ] {
            assert!(
                matches!(validated(bad), Err(ThemeError::Forbidden(_))),
                "{bad}"
            );
        }
        let huge = format!(":root {{ --x: {}; }}", "a".repeat(MAX_THEME_BYTES));
        assert!(matches!(validated(&huge), Err(ThemeError::TooLarge)));
    }
}
