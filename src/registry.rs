//! The list of registered apps (name + their /admin/logs-shaped endpoint URL + a health
//! check URL for uptime monitoring), persisted
//! to a JSON file. In-memory copy guarded by a lock so concurrent requests never see a
//! half-written file; every mutation rewrites the whole file (a handful of rows, no need
//! for anything more granular).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

/// Every way a registry operation can fail. The messages are shown to the admin as-is in
/// the /apps form, hence Spanish.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("El nombre no puede estar vacío")]
    EmptyName,
    #[error("El nombre debe tener al menos una letra o número")]
    NameWithoutAlphanumerics,
    #[error("La URL de {0} debe empezar con http:// o https://")]
    InvalidUrl(&'static str),
    #[error("Ya existe una app registrada con un nombre equivalente")]
    SlugTaken,
    #[error("No se encontró la app '{0}'")]
    NotFound(String),
    #[error("No se pudo generar el token: {0}")]
    Token(#[from] getrandom::Error),
    #[error("Error de E/S en {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} no es un registro de apps válido: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

impl RegistryError {
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        move |source| Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

type Result<T> = std::result::Result<T, RegistryError>;

impl crate::i18n::Localize for RegistryError {
    fn localize(&self, i18n: &crate::i18n::I18n) -> String {
        match self {
            Self::EmptyName => i18n.text("err.empty_name", &[]),
            Self::NameWithoutAlphanumerics => i18n.text("err.name_no_alnum", &[]),
            Self::InvalidUrl(label) => i18n.text("err.invalid_url", &[("label", label)]),
            Self::SlugTaken => i18n.text("err.slug_taken", &[]),
            Self::NotFound(slug) => i18n.text("err.not_found", &[("slug", slug)]),
            Self::Token(_) | Self::Io { .. } | Self::Json { .. } => {
                i18n.text("err.internal", &[("error", &self.to_string())])
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredApp {
    /// URL-safe identifier derived from `name` at registration time -- what routes address
    /// it by (`/api/apps/{slug}/logs`), since `name` itself may contain spaces/accents.
    pub slug: String,
    pub name: String,
    pub logs_url: String,
    /// Endpoint an uptime check can hit. `None` only for apps registered before this field
    /// existed -- `add` requires it for every new registration.
    #[serde(default)]
    pub health_url: Option<String>,
    /// Secret that authorizes the public, read-only status embed (`<heartbeat-status>`).
    /// Never empty after `AppRegistry::load` -- entries saved before this field existed get
    /// one generated there.
    #[serde(default)]
    pub embed_token: String,
    /// Paused apps aren't probed, so maintenance time doesn't count against their uptime.
    #[serde(default)]
    pub paused: bool,
    /// Listed on the public `/status` page.
    #[serde(default)]
    pub public: bool,
    /// Per-app "degraded" threshold; `None` = the global `UPTIME_DEGRADED_MS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_after_ms: Option<u32>,
    /// Text the health response body must contain to count as up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_body: Option<String>,
}

/// What an admin sets when registering or editing an app. The slug and embed token are
/// not part of it: they're assigned once and survive every edit.
#[derive(Debug, Clone, Default)]
pub struct AppSettings {
    pub name: String,
    pub logs_url: String,
    pub health_url: String,
    pub degraded_after_ms: Option<u32>,
    pub expect_body: Option<String>,
}

impl AppSettings {
    /// Trims every field, turns blank optionals into `None`, and validates.
    fn normalized(self) -> Result<Self> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err(RegistryError::EmptyName);
        }
        let logs_url = self.logs_url.trim().to_string();
        let health_url = self.health_url.trim().to_string();
        validate_url(&logs_url, "logs")?;
        validate_url(&health_url, "health")?;
        let expect_body = self
            .expect_body
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(Self {
            name,
            logs_url,
            health_url,
            degraded_after_ms: self.degraded_after_ms.filter(|&ms| ms > 0),
            expect_body,
        })
    }
}

impl RegisteredApp {
    /// Constant-time comparison, so response timing doesn't leak how much of a guessed
    /// token was right.
    #[must_use]
    pub fn embed_token_matches(&self, candidate: &str) -> bool {
        let expected = self.embed_token.as_bytes();
        let candidate = candidate.as_bytes();
        !expected.is_empty()
            && expected.len() == candidate.len()
            && expected
                .iter()
                .zip(candidate)
                .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                == 0
    }
}

/// 24 random bytes, hex-encoded (48 chars).
fn new_embed_token() -> Result<String> {
    Ok(crate::token::random_hex(24)?)
}

pub struct AppRegistry {
    path: PathBuf,
    apps: RwLock<Vec<RegisteredApp>>,
}

/// Lowercase, spaces/anything-not-alphanumeric collapsed to a single hyphen, trimmed --
/// "Billing API (staging)" -> "billing-api-staging".
fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_was_dash = true; // swallow a leading dash
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

fn validate_url(url: &str, label: &'static str) -> Result<()> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(RegistryError::InvalidUrl(label))
    }
}

impl AppRegistry {
    pub async fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut apps: Vec<RegisteredApp> = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => serde_json::from_str(&raw).map_err(|source| RegistryError::Json {
                path: path.clone(),
                source,
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(RegistryError::io(&path)(e)),
        };
        let mut backfilled = false;
        for app in apps.iter_mut().filter(|a| a.embed_token.is_empty()) {
            app.embed_token = new_embed_token()?;
            backfilled = true;
        }
        let registry = Self {
            path,
            apps: RwLock::new(apps),
        };
        if backfilled {
            registry.persist(&registry.apps.read().await).await?;
        }
        Ok(registry)
    }

    async fn persist(&self, apps: &[RegisteredApp]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(RegistryError::io(parent))?;
        }
        let raw = serde_json::to_string_pretty(apps).map_err(|source| RegistryError::Json {
            path: self.path.clone(),
            source,
        })?;
        tokio::fs::write(&self.path, raw)
            .await
            .map_err(RegistryError::io(&self.path))
    }

    pub async fn list(&self) -> Vec<RegisteredApp> {
        self.apps.read().await.clone()
    }

    pub async fn find(&self, slug: &str) -> Option<RegisteredApp> {
        self.apps
            .read()
            .await
            .iter()
            .find(|a| a.slug == slug)
            .cloned()
    }

    /// Registers an app, returns its new slug. Errors on a blank name/URL or a slug
    /// collision (two names that normalize the same way, e.g. "API Test" and "api-test")
    /// rather than silently overwriting the earlier registration.
    pub async fn add(&self, settings: AppSettings) -> Result<String> {
        let settings = settings.normalized()?;
        let slug = slugify(&settings.name);
        if slug.is_empty() {
            return Err(RegistryError::NameWithoutAlphanumerics);
        }

        let mut apps = self.apps.write().await;
        if apps.iter().any(|a| a.slug == slug) {
            return Err(RegistryError::SlugTaken);
        }
        apps.push(RegisteredApp {
            slug: slug.clone(),
            name: settings.name,
            logs_url: settings.logs_url,
            health_url: Some(settings.health_url),
            embed_token: new_embed_token()?,
            paused: false,
            public: false,
            degraded_after_ms: settings.degraded_after_ms,
            expect_body: settings.expect_body,
        });
        self.persist(&apps).await?;
        Ok(slug)
    }

    /// Applies `change` to one app and persists, or reports it missing.
    async fn modify(
        &self,
        slug: &str,
        change: impl FnOnce(&mut RegisteredApp) -> Result<()>,
    ) -> Result<()> {
        let mut apps = self.apps.write().await;
        let app = apps
            .iter_mut()
            .find(|a| a.slug == slug)
            .ok_or_else(|| RegistryError::NotFound(slug.to_string()))?;
        change(app)?;
        self.persist(&apps).await
    }

    /// Replaces an app's settings. Its slug, embed token and history stay the same.
    pub async fn update(&self, slug: &str, settings: AppSettings) -> Result<()> {
        let settings = settings.normalized()?;
        self.modify(slug, |app| {
            app.name = settings.name;
            app.logs_url = settings.logs_url;
            app.health_url = Some(settings.health_url);
            app.degraded_after_ms = settings.degraded_after_ms;
            app.expect_body = settings.expect_body;
            Ok(())
        })
        .await
    }

    pub async fn set_paused(&self, slug: &str, paused: bool) -> Result<()> {
        self.modify(slug, |app| {
            app.paused = paused;
            Ok(())
        })
        .await
    }

    pub async fn set_public(&self, slug: &str, public: bool) -> Result<()> {
        self.modify(slug, |app| {
            app.public = public;
            Ok(())
        })
        .await
    }

    /// Replaces the app's embed token, invalidating every embed that uses the old one.
    pub async fn rotate_embed_token(&self, slug: &str) -> Result<()> {
        self.modify(slug, |app| {
            app.embed_token = new_embed_token()?;
            Ok(())
        })
        .await
    }

    pub async fn remove(&self, slug: &str) -> Result<()> {
        let mut apps = self.apps.write().await;
        let before = apps.len();
        apps.retain(|a| a.slug != slug);
        if apps.len() == before {
            return Err(RegistryError::NotFound(slug.to_string()));
        }
        self.persist(&apps).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(name: &str, logs_url: &str, health_url: &str) -> AppSettings {
        AppSettings {
            name: name.to_string(),
            logs_url: logs_url.to_string(),
            health_url: health_url.to_string(),
            ..AppSettings::default()
        }
    }

    #[tokio::test]
    async fn update_keeps_slug_and_token_and_pause_persists() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("apps.json");
        let registry = AppRegistry::load(&file).await.unwrap();
        let slug = registry
            .add(settings(
                "Billing API",
                "https://x/logs",
                "https://x/health",
            ))
            .await
            .unwrap();
        let token = registry.find(&slug).await.unwrap().embed_token;

        let mut changed = settings("Billing API v2", "https://y/logs", "https://y/health");
        changed.degraded_after_ms = Some(1500);
        changed.expect_body = Some("  ok  ".to_string());
        registry.update(&slug, changed).await.unwrap();
        registry.set_paused(&slug, true).await.unwrap();
        registry.set_public(&slug, true).await.unwrap();

        let app = AppRegistry::load(&file)
            .await
            .unwrap()
            .find(&slug)
            .await
            .unwrap();
        assert_eq!(app.name, "Billing API v2");
        assert_eq!(app.embed_token, token);
        assert_eq!(app.degraded_after_ms, Some(1500));
        assert_eq!(app.expect_body.as_deref(), Some("ok"));
        assert!(app.paused && app.public);

        assert!(matches!(
            registry
                .update("missing", settings("a", "https://x", "https://x"))
                .await,
            Err(RegistryError::NotFound(_))
        ));
    }

    #[test]
    fn slugify_lowercases_and_collapses_punctuation() {
        assert_eq!(slugify("Billing API (staging)"), "billing-api-staging");
        assert_eq!(slugify("  leading/trailing  "), "leading-trailing");
        assert_eq!(slugify("já-acentuado"), "j-acentuado");
    }

    #[tokio::test]
    async fn add_then_find_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let registry = AppRegistry::load(dir.path().join("apps.json"))
            .await
            .unwrap();
        let slug = registry
            .add(settings(
                "Billing API",
                "https://x/logs",
                "https://x/health",
            ))
            .await
            .unwrap();
        assert_eq!(slug, "billing-api");
        let found = registry.find(&slug).await.unwrap();
        assert_eq!(found.name, "Billing API");
        assert_eq!(found.logs_url, "https://x/logs");
        assert_eq!(found.health_url.as_deref(), Some("https://x/health"));
    }

    #[tokio::test]
    async fn add_rejects_a_slug_collision() {
        let dir = tempfile::tempdir().unwrap();
        let registry = AppRegistry::load(dir.path().join("apps.json"))
            .await
            .unwrap();
        registry
            .add(settings(
                "Billing API",
                "https://x/logs",
                "https://x/health",
            ))
            .await
            .unwrap();
        let err = registry
            .add(settings(
                "billing api",
                "https://y/logs",
                "https://y/health",
            ))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn add_rejects_a_url_without_a_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let registry = AppRegistry::load(dir.path().join("apps.json"))
            .await
            .unwrap();
        let err = registry
            .add(settings("Bad", "not-a-url", "https://x/health"))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn add_rejects_a_health_url_without_a_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let registry = AppRegistry::load(dir.path().join("apps.json"))
            .await
            .unwrap();
        let err = registry.add(settings("Bad", "https://x/logs", "")).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn loads_entries_saved_before_health_url_existed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("apps.json");
        tokio::fs::write(
            &file,
            r#"[{"slug":"old","name":"Old","logs_url":"https://x/logs"}]"#,
        )
        .await
        .unwrap();
        let registry = AppRegistry::load(&file).await.unwrap();
        assert_eq!(registry.find("old").await.unwrap().health_url, None);
    }

    #[tokio::test]
    async fn add_generates_an_embed_token_and_rotate_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let registry = AppRegistry::load(dir.path().join("apps.json"))
            .await
            .unwrap();
        let slug = registry
            .add(settings(
                "Billing API",
                "https://x/logs",
                "https://x/health",
            ))
            .await
            .unwrap();
        let app = registry.find(&slug).await.unwrap();
        assert_eq!(app.embed_token.len(), 48);
        assert!(app.embed_token_matches(&app.embed_token.clone()));
        assert!(!app.embed_token_matches(""));
        assert!(!app.embed_token_matches(&"0".repeat(48)));

        registry.rotate_embed_token(&slug).await.unwrap();
        let rotated = registry.find(&slug).await.unwrap();
        assert_ne!(rotated.embed_token, app.embed_token);
        assert!(!rotated.embed_token_matches(&app.embed_token));
    }

    #[tokio::test]
    async fn load_backfills_missing_embed_tokens_and_persists_them() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("apps.json");
        tokio::fs::write(
            &file,
            r#"[{"slug":"old","name":"Old","logs_url":"https://x/logs"}]"#,
        )
        .await
        .unwrap();
        let token = AppRegistry::load(&file)
            .await
            .unwrap()
            .find("old")
            .await
            .unwrap()
            .embed_token;
        assert_eq!(token.len(), 48);
        let reloaded = AppRegistry::load(&file).await.unwrap();
        assert_eq!(reloaded.find("old").await.unwrap().embed_token, token);
    }

    #[tokio::test]
    async fn persists_across_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("apps.json");
        {
            let registry = AppRegistry::load(&file).await.unwrap();
            registry
                .add(settings(
                    "Billing API",
                    "https://x/logs",
                    "https://x/health",
                ))
                .await
                .unwrap();
        }
        let reloaded = AppRegistry::load(&file).await.unwrap();
        assert_eq!(reloaded.list().await.len(), 1);
    }

    #[tokio::test]
    async fn remove_then_reload_reflects_the_removal() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("apps.json");
        let registry = AppRegistry::load(&file).await.unwrap();
        let slug = registry
            .add(settings(
                "Billing API",
                "https://x/logs",
                "https://x/health",
            ))
            .await
            .unwrap();
        registry.remove(&slug).await.unwrap();
        assert!(registry.list().await.is_empty());
        let reloaded = AppRegistry::load(&file).await.unwrap();
        assert!(reloaded.list().await.is_empty());
    }
}
