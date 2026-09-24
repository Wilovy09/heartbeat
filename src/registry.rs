//! The list of registered apps (name + their /admin/logs-shaped endpoint URL + a health
//! check URL for uptime monitoring), persisted
//! to a JSON file. In-memory copy guarded by a lock so concurrent requests never see a
//! half-written file; every mutation rewrites the whole file (a handful of rows, no need
//! for anything more granular).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::RwLock;

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
fn new_embed_token() -> anyhow::Result<String> {
    crate::token::random_hex(24).map_err(|e| anyhow::anyhow!("no se pudo generar el token: {e}"))
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

fn validate_url(url: &str, label: &str) -> anyhow::Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        anyhow::bail!("La URL de {label} debe empezar con http:// o https://");
    }
    Ok(())
}

impl AppRegistry {
    pub async fn load(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let mut apps: Vec<RegisteredApp> = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => serde_json::from_str(&raw)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
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

    async fn persist(&self, apps: &[RegisteredApp]) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let raw = serde_json::to_string_pretty(apps)?;
        tokio::fs::write(&self.path, raw).await?;
        Ok(())
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

    /// Adds `name`/`logs_url`/`health_url`, returns the new entry's slug. Errors on a blank
    /// name/URL or a slug collision (two names that normalize the same way, e.g. "API Test"
    /// and "api-test") rather than silently overwriting the earlier registration.
    pub async fn add(
        &self,
        name: &str,
        logs_url: &str,
        health_url: &str,
    ) -> anyhow::Result<String> {
        let name = name.trim();
        let logs_url = logs_url.trim();
        let health_url = health_url.trim();
        if name.is_empty() {
            anyhow::bail!("El nombre no puede estar vacío");
        }
        validate_url(logs_url, "logs")?;
        validate_url(health_url, "health")?;
        let slug = slugify(name);
        if slug.is_empty() {
            anyhow::bail!("El nombre debe tener al menos una letra o número");
        }

        let mut apps = self.apps.write().await;
        if apps.iter().any(|a| a.slug == slug) {
            anyhow::bail!("Ya existe una app registrada con un nombre equivalente");
        }
        apps.push(RegisteredApp {
            slug: slug.clone(),
            name: name.to_string(),
            logs_url: logs_url.to_string(),
            health_url: Some(health_url.to_string()),
            embed_token: new_embed_token()?,
        });
        self.persist(&apps).await?;
        Ok(slug)
    }

    /// Replaces the app's embed token, invalidating every embed that uses the old one.
    pub async fn rotate_embed_token(&self, slug: &str) -> anyhow::Result<()> {
        let mut apps = self.apps.write().await;
        let Some(app) = apps.iter_mut().find(|a| a.slug == slug) else {
            anyhow::bail!("No se encontró la app '{slug}'");
        };
        app.embed_token = new_embed_token()?;
        self.persist(&apps).await
    }

    pub async fn remove(&self, slug: &str) -> anyhow::Result<()> {
        let mut apps = self.apps.write().await;
        let before = apps.len();
        apps.retain(|a| a.slug != slug);
        if apps.len() == before {
            anyhow::bail!("No se encontró la app '{slug}'");
        }
        self.persist(&apps).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .add("Billing API", "https://x/logs", "https://x/health")
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
            .add("Billing API", "https://x/logs", "https://x/health")
            .await
            .unwrap();
        let err = registry
            .add("billing api", "https://y/logs", "https://y/health")
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn add_rejects_a_url_without_a_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let registry = AppRegistry::load(dir.path().join("apps.json"))
            .await
            .unwrap();
        let err = registry.add("Bad", "not-a-url", "https://x/health").await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn add_rejects_a_health_url_without_a_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let registry = AppRegistry::load(dir.path().join("apps.json"))
            .await
            .unwrap();
        let err = registry.add("Bad", "https://x/logs", "").await;
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
            .add("Billing API", "https://x/logs", "https://x/health")
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
                .add("Billing API", "https://x/logs", "https://x/health")
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
            .add("Billing API", "https://x/logs", "https://x/health")
            .await
            .unwrap();
        registry.remove(&slug).await.unwrap();
        assert!(registry.list().await.is_empty());
        let reloaded = AppRegistry::load(&file).await.unwrap();
        assert!(reloaded.list().await.is_empty());
    }
}
