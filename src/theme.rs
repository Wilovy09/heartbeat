//! The custom theme: CSS an admin saves from /settings, served at `/theme/custom.css` and
//! applied on top of the dark tokens when a user picks "Custom" (the choice itself lives
//! in the browser, see `static/theme.js`). Meant for overriding the color tokens, so it's
//! size-capped and anything that would load external resources is refused -- the CSP
//! blocks those anyway, this makes the editor say why instead of failing silently.

use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

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
            Self::Io { .. } => i18n.text("err.internal", &[("error", &self.to_string())]),
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
    path: PathBuf,
    css: RwLock<String>,
}

impl ThemeStore {
    /// Loads `path`; a missing file is an empty theme.
    pub async fn load(path: impl Into<PathBuf>) -> Result<Self, ThemeError> {
        let path = path.into();
        let css = match tokio::fs::read_to_string(&path).await {
            Ok(css) => css,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(ThemeError::io(&path)(e)),
        };
        Ok(Self {
            path,
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

    /// Validates and saves the theme (blank = no custom theme): temp file + rename.
    pub async fn save(&self, css: &str) -> Result<(), ThemeError> {
        let css = validated(css)?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(ThemeError::io(parent))?;
        }
        let tmp = self.path.with_extension("css.tmp");
        tokio::fs::write(&tmp, &css)
            .await
            .map_err(ThemeError::io(&tmp))?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(ThemeError::io(&self.path))?;
        *self.css.write().unwrap_or_else(PoisonError::into_inner) = css;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn themes_persist_and_blank_clears() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("theme.css");
        let store = ThemeStore::load(&file).await.unwrap();
        assert_eq!(store.css(), "");
        store.save("  :root { --glass: #101820; }  ").await.unwrap();
        let reloaded = ThemeStore::load(&file).await.unwrap();
        assert_eq!(reloaded.css(), ":root { --glass: #101820; }");
        reloaded.save("   ").await.unwrap();
        assert_eq!(reloaded.css(), "");
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
