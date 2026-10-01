//! `POST /discord/interactions`: Discord's interactions endpoint. Every request is signed
//! with the application's Ed25519 key (over `{timestamp}{body}`); Discord also sends a
//! `PING` when the URL is saved, and deliberately bad signatures to check they're refused.
//! The slash command itself is registered at startup (`register`), not from a manifest.

use std::time::Duration;

use actix_web::{HttpRequest, HttpResponse, web};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use serde_json::json;

use super::{Caller, Chat, Command, ParseError, decode_hex, is_fresh, suggestions};
use crate::{
    config::{Config, DiscordApp},
    i18n::I18n,
    registry::AppRegistry,
    uptime::UptimeMonitor,
};

const API: &str = "https://discord.com/api/v10";
/// Discord shows at most 25 autocomplete choices.
const MAX_CHOICES: usize = 25;
/// Message flag: only the person who ran the command sees it.
const EPHEMERAL: u64 = 1 << 6;

// Interaction and response types, from Discord's API.
const PING: u8 = 1;
const APPLICATION_COMMAND: u8 = 2;
const AUTOCOMPLETE: u8 = 4;
const PONG: u8 = 1;
const CHANNEL_MESSAGE: u8 = 4;
const AUTOCOMPLETE_RESULT: u8 = 8;

fn verify(public_key: &[u8; 32], timestamp: &str, signature: &str, body: &[u8], now: u64) -> bool {
    if !is_fresh(timestamp, now) {
        return false;
    }
    let Ok(key) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    let Some(signature) = decode_hex(signature).and_then(|b| Signature::from_slice(&b).ok()) else {
        return false;
    };
    let mut message = timestamp.as_bytes().to_vec();
    message.extend_from_slice(body);
    key.verify_strict(&message, &signature).is_ok()
}

#[derive(Deserialize)]
struct Interaction {
    #[serde(rename = "type")]
    kind: u8,
    #[serde(default)]
    data: Option<Data>,
    /// Set in a server; `user` is set instead in a DM.
    #[serde(default)]
    member: Option<Member>,
    #[serde(default)]
    user: Option<User>,
}

#[derive(Deserialize)]
struct Data {
    #[serde(default)]
    options: Vec<Opt>,
}

#[derive(Deserialize)]
struct Opt {
    name: String,
    #[serde(default)]
    value: Option<serde_json::Value>,
    #[serde(default)]
    options: Vec<Opt>,
    #[serde(default)]
    focused: bool,
}

#[derive(Deserialize)]
struct Member {
    user: User,
    #[serde(default)]
    roles: Vec<String>,
}

#[derive(Deserialize)]
struct User {
    id: String,
}

impl Interaction {
    fn caller(&self) -> Caller {
        match (&self.member, &self.user) {
            (Some(member), _) => Caller::Discord {
                user: member.user.id.clone(),
                roles: member.roles.clone(),
            },
            (None, user) => Caller::Discord {
                user: user.as_ref().map(|u| u.id.clone()).unwrap_or_default(),
                roles: Vec::new(),
            },
        }
    }

    /// The subcommand (`status`, `pause`...) and its options.
    fn subcommand(&self) -> Option<&Opt> {
        self.data.as_ref()?.options.first()
    }
}

impl Opt {
    fn string(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|o| o.name == name)
            .and_then(|o| o.value.as_ref()?.as_str())
    }

    fn command(&self) -> Result<Command, ParseError> {
        match self.name.as_str() {
            "help" => Ok(Command::Help),
            "status" => Ok(Command::Status(self.string("app").map(str::to_string))),
            "pause" => Command::pause(
                self.string("app").unwrap_or_default(),
                self.string("duration"),
            ),
            "resume" => Command::resume(self.string("app").unwrap_or_default()),
            other => Err(ParseError::Unknown(other.to_string())),
        }
    }
}

fn header<'a>(req: &'a HttpRequest, name: &str) -> &'a str {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// POST /discord/interactions
pub async fn interactions(
    req: HttpRequest,
    body: web::Bytes,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
    i18n: web::Data<I18n>,
) -> HttpResponse {
    let Some(discord) = &cfg.discord else {
        return HttpResponse::NotFound().finish();
    };
    let signed = verify(
        &discord.public_key,
        header(&req, "X-Signature-Timestamp"),
        header(&req, "X-Signature-Ed25519"),
        &body,
        crate::uptime::unix_now(),
    );
    if !signed {
        // Discord sends these on purpose when the URL is saved: refusing them is expected.
        tracing::debug!("discord: rejected a request with a bad or stale signature");
        return HttpResponse::Unauthorized().finish();
    }
    let Ok(interaction) = serde_json::from_slice::<Interaction>(&body) else {
        return HttpResponse::BadRequest().finish();
    };
    match interaction.kind {
        PING => HttpResponse::Ok().json(json!({ "type": PONG })),
        AUTOCOMPLETE => {
            let typed = interaction
                .subcommand()
                .and_then(|sub| sub.options.iter().find(|o| o.focused))
                .and_then(|o| o.value.as_ref()?.as_str())
                .unwrap_or_default();
            let choices: Vec<_> = suggestions(&registry.list().await, typed)
                .into_iter()
                .take(MAX_CHOICES)
                .map(|(name, slug)| json!({ "name": name, "value": slug }))
                .collect();
            HttpResponse::Ok().json(json!({
                "type": AUTOCOMPLETE_RESULT,
                "data": { "choices": choices },
            }))
        }
        APPLICATION_COMMAND => {
            let chat = Chat {
                registry: &registry,
                monitor: &monitor,
                i18n: &i18n,
                admins: &cfg.chat_admins,
                command: &cfg.chat_command,
                public_url: cfg.public_url.as_deref(),
                audit: req
                    .app_data::<web::Data<crate::audit::AuditLog>>()
                    .map(web::Data::get_ref),
                system: req
                    .app_data::<web::Data<crate::system::SystemMonitor>>()
                    .map(web::Data::get_ref),
            };
            let parsed = interaction
                .subcommand()
                .map_or(Ok(Command::Status(None)), Opt::command);
            let raw: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let context = json!({
                "guild_id": raw["guild_id"],
                "channel_id": raw["channel_id"],
                "options": raw["data"]["options"],
            });
            let reply = chat.run(parsed, &interaction.caller(), context).await;
            HttpResponse::Ok().json(json!({
                "type": CHANNEL_MESSAGE,
                "data": {
                    "content": reply.text,
                    "flags": if reply.public { 0 } else { EPHEMERAL },
                    // Naming who paused an app shouldn't ping them.
                    "allowed_mentions": { "parse": [] },
                },
            }))
        }
        _ => HttpResponse::BadRequest().finish(),
    }
}

/// The slash command as Discord registers it: one command, a subcommand per action.
fn definition(command: &str, i18n: &I18n) -> serde_json::Value {
    let app = |required: bool| {
        json!({
            "type": 3,
            "name": "app",
            "description": i18n.text("chat.opt_app", &[]),
            "required": required,
            "autocomplete": true,
        })
    };
    let sub = |name: &str, key: &str, options: Vec<serde_json::Value>| {
        json!({
            "type": 1,
            "name": name,
            "description": i18n.text(key, &[]),
            "options": options,
        })
    };
    json!({
        "name": command,
        "type": 1,
        "description": i18n.text("chat.describe", &[]),
        // Servers only (`GUILD_INSTALL`, `GUILD`): there's nothing to monitor in a DM.
        "integration_types": [0],
        "contexts": [0],
        "options": [
            sub("status", "chat.sub_status", vec![app(false)]),
            sub("pause", "chat.sub_pause", vec![
                app(true),
                json!({
                    "type": 3,
                    "name": "duration",
                    "description": i18n.text("chat.opt_duration", &[]),
                    "required": false,
                }),
            ]),
            sub("resume", "chat.sub_resume", vec![app(true)]),
            sub("help", "chat.sub_help", vec![]),
        ],
    })
}

/// Link that adds the application's commands to a server.
pub fn invite_url(discord: &DiscordApp) -> String {
    format!(
        "https://discord.com/oauth2/authorize?client_id={}&scope=applications.commands",
        discord.application_id
    )
}

/// Registers the slash command in the background, when Discord is configured.
pub fn spawn_registration(cfg: &Config, i18n: &I18n) {
    if let Some(discord) = cfg.discord.clone() {
        tokio::spawn(register(discord, cfg.chat_command.clone(), i18n.clone()));
    }
}

/// Registers (or updates) the slash command. Overwriting the whole list is idempotent,
/// so it runs on every start; a failure is logged and the rest of Heartbeat carries on.
async fn register(discord: DiscordApp, command: String, i18n: I18n) {
    let url = format!("{API}/applications/{}/commands", discord.application_id);
    let result = async {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?
            .put(&url)
            .header("Authorization", format!("Bot {}", discord.bot_token))
            .json(&[definition(&command, &i18n)])
            .send()
            .await?
            .error_for_status()
    }
    .await;
    match result {
        Ok(_) => tracing::info!(command = %command, "discord: slash command registered"),
        Err(e) => tracing::warn!(error = %e, "discord: could not register the slash command"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub(crate) fn key() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    /// A request signature, as Discord computes it.
    pub(crate) fn sign(timestamp: &str, body: &[u8]) -> String {
        let mut message = timestamp.as_bytes().to_vec();
        message.extend_from_slice(body);
        crate::chat::encode_hex(&key().sign(&message).to_bytes())
    }

    #[test]
    fn only_a_fresh_signature_from_the_key_passes() {
        let public = key().verifying_key().to_bytes();
        let body = br#"{"type":1}"#;
        let good = sign("1000", body);
        assert!(verify(&public, "1000", &good, body, 1100));
        assert!(
            !verify(&public, "1000", &good, br#"{"type":2}"#, 1100),
            "tampered body"
        );
        assert!(
            !verify(&public, "1001", &good, body, 1100),
            "other timestamp"
        );
        assert!(
            !verify(&public, "1000", &good, body, 5000),
            "replayed later"
        );
        assert!(!verify(&[0; 32], "1000", &good, body, 1100), "other key");
        assert!(!verify(&public, "1000", "abcd", body, 1100));
    }

    #[test]
    fn subcommand_options_become_commands() {
        let interaction: Interaction = serde_json::from_value(json!({
            "type": 2,
            "data": { "name": "pulse", "options": [{
                "name": "pause", "type": 1,
                "options": [
                    { "name": "app", "type": 3, "value": "billing" },
                    { "name": "duration", "type": 3, "value": "2h" },
                ],
            }]},
            "member": { "user": { "id": "42" }, "roles": ["7"] },
        }))
        .unwrap();
        assert_eq!(
            interaction.subcommand().unwrap().command(),
            Ok(Command::Pause {
                app: "billing".into(),
                for_secs: Some(7200)
            })
        );
        assert_eq!(
            interaction.caller(),
            Caller::Discord {
                user: "42".into(),
                roles: vec!["7".into()]
            }
        );
    }

    #[test]
    fn the_definition_fits_discords_limits() {
        for lang in [crate::i18n::Lang::Es, crate::i18n::Lang::En] {
            let def = definition("pulse", &I18n::new(lang));
            let mut descriptions = vec![def["description"].as_str().unwrap().to_string()];
            for sub in def["options"].as_array().unwrap() {
                descriptions.push(sub["description"].as_str().unwrap().to_string());
                for opt in sub["options"].as_array().unwrap() {
                    descriptions.push(opt["description"].as_str().unwrap().to_string());
                }
            }
            for d in descriptions {
                assert!((1..=100).contains(&d.chars().count()), "{lang:?}: {d:?}");
                assert!(!d.starts_with("chat."), "missing translation: {d}");
            }
        }
    }
}
