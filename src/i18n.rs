//! UI language (`APP_LANG`: `es` or `en`). Server-side strings live in
//! `locales/<lang>.json` (embedded in the binary); templates read them with
//! `{{ t(k="some.key") }}` and fill `{placeholders}` from extra keyword arguments, e.g.
//! `{{ t(k="apps.hint_health", secs=60) }}`. Client-side strings live in
//! `static/i18n/<lang>.js`, loaded by `base.html`.

use std::collections::HashMap;
use std::sync::Arc;
use tera::{Kwargs, State, Tera, TeraResult};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Lang {
    #[default]
    Es,
    En,
}

impl Lang {
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Es => "es",
            Self::En => "en",
        }
    }

    fn catalog_source(self) -> &'static str {
        match self {
            Self::Es => include_str!("../locales/es.json"),
            Self::En => include_str!("../locales/en.json"),
        }
    }
}

impl std::str::FromStr for Lang {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "es" => Ok(Self::Es),
            "en" => Ok(Self::En),
            _ => Err(()),
        }
    }
}

/// The active language's catalog. Cheap to clone.
#[derive(Clone)]
pub struct I18n {
    lang: Lang,
    strings: Arc<HashMap<String, String>>,
}

impl I18n {
    /// # Panics
    /// If the embedded catalog isn't a flat JSON object of strings -- a build-time asset,
    /// covered by `catalogs_are_valid_and_have_the_same_keys`.
    #[must_use]
    pub fn new(lang: Lang) -> Self {
        let strings: HashMap<String, String> = serde_json::from_str(lang.catalog_source())
            .unwrap_or_else(|e| panic!("locales/{}.json is invalid: {e}", lang.code()));
        Self {
            lang,
            strings: Arc::new(strings),
        }
    }

    #[must_use]
    pub fn lang(&self) -> Lang {
        self.lang
    }

    /// The string for `key` with each `{name}` replaced by its value from `args`. An
    /// unknown key renders as the key itself, so a missing translation is visible rather
    /// than blank.
    #[must_use]
    pub fn text(&self, key: &str, args: &[(&str, &str)]) -> String {
        let mut out = self
            .strings
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string());
        for (name, value) in args {
            out = out.replace(&format!("{{{name}}}"), value);
        }
        out
    }

    /// Makes `t(k=...)` and `lang()` available to every template.
    pub fn register(&self, tera: &mut Tera) {
        let catalog = self.clone();
        tera.register_function(
            "t",
            move |kwargs: Kwargs, _: &State| -> TeraResult<String> {
                let key: String = kwargs.must_get("k")?;
                let args: Vec<(String, String)> = kwargs
                    .iter()
                    .filter(|(name, _)| name.as_str() != Some("k"))
                    .filter_map(|(name, value)| {
                        Some((name.as_str()?.to_string(), value.to_string()))
                    })
                    .collect();
                let borrowed: Vec<(&str, &str)> =
                    args.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
                Ok(catalog.text(&key, &borrowed))
            },
        );
        let code = self.lang.code();
        tera.register_function("lang", move |_: Kwargs, _: &State| code);
    }
}

/// An error that can be shown to the admin in the UI's language. `Display` stays as the
/// (log-oriented) message; this is what the pages render.
pub trait Localize {
    fn localize(&self, i18n: &I18n) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn catalogs_are_valid_and_have_the_same_keys() {
        let spanish: BTreeSet<_> = I18n::new(Lang::Es).strings.keys().cloned().collect();
        let english: BTreeSet<_> = I18n::new(Lang::En).strings.keys().cloned().collect();
        let missing_in_english: Vec<_> = spanish.difference(&english).collect();
        let missing_in_spanish: Vec<_> = english.difference(&spanish).collect();
        assert!(
            missing_in_english.is_empty() && missing_in_spanish.is_empty(),
            "missing in en: {missing_in_english:?}, missing in es: {missing_in_spanish:?}"
        );
    }

    #[test]
    fn text_interpolates_and_falls_back_to_the_key() {
        let es = I18n::new(Lang::Es);
        assert_eq!(es.text("does.not.exist", &[]), "does.not.exist");
        let throttled = es.text("login.throttled", &[("minutes", "15")]);
        assert!(throttled.contains("15") && !throttled.contains("{minutes}"));
    }

    #[test]
    fn templates_can_call_t_and_lang() {
        // Tera validates function calls when a template is added: register first.
        let mut tera = Tera::new();
        I18n::new(Lang::En).register(&mut tera);
        tera.add_raw_template(
            "x",
            r#"{{ lang() }}|{{ t(k="login.throttled", minutes=3) }}"#,
        )
        .unwrap();
        let out = tera.render("x", &tera::Context::new()).unwrap();
        assert!(out.starts_with("en|") && out.contains('3'), "{out}");
    }
}
