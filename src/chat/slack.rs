//! `POST /slack/commands`: Slack's slash-command request. Signed with the app's signing
//! secret (HMAC-SHA256 of `v0:{timestamp}:{body}`), form-encoded, answered inline.

use actix_web::{HttpRequest, HttpResponse, web};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use super::{Caller, Chat, Command, decode_hex, is_fresh};
use crate::{config::Config, i18n::I18n, registry::AppRegistry, uptime::UptimeMonitor};

/// Whether `signature` (`v0=<hex>`) is the signing secret's HMAC of this request.
fn verify(secret: &str, timestamp: &str, signature: &str, body: &[u8], now: u64) -> bool {
    if !is_fresh(timestamp, now) {
        return false;
    }
    let Some(expected) = signature.strip_prefix("v0=").and_then(decode_hex) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(b"v0:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(body);
    // Constant-time comparison.
    mac.verify_slice(&expected).is_ok()
}

fn header<'a>(req: &'a HttpRequest, name: &str) -> &'a str {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// The manifest to create the Slack app from, pointing at this deployment.
pub fn manifest(base_url: &str, command: &str, i18n: &I18n) -> serde_json::Value {
    serde_json::json!({
        "display_information": { "name": "Heartbeat" },
        "features": {
            "bot_user": { "display_name": "heartbeat", "always_online": false },
            "slash_commands": [{
                "command": format!("/{command}"),
                "url": format!("{base_url}/slack/commands"),
                "description": i18n.text("chat.describe", &[]),
                "usage_hint": "status [app] | pause <app> [30m] | resume <app> | help",
                "should_escape": false,
            }],
        },
        "oauth_config": { "scopes": { "bot": ["commands"] } },
        "settings": {
            "org_deploy_enabled": false,
            "socket_mode_enabled": false,
            "token_rotation_enabled": false,
        },
    })
}

/// POST /slack/commands
pub async fn commands(
    req: HttpRequest,
    body: web::Bytes,
    cfg: web::Data<Config>,
    registry: web::Data<AppRegistry>,
    monitor: web::Data<UptimeMonitor>,
    i18n: web::Data<I18n>,
) -> HttpResponse {
    let Some(secret) = cfg.slack_signing_secret.as_deref() else {
        return HttpResponse::NotFound().finish();
    };
    let signed = verify(
        secret,
        header(&req, "X-Slack-Request-Timestamp"),
        header(&req, "X-Slack-Signature"),
        &body,
        crate::uptime::unix_now(),
    );
    if !signed {
        tracing::warn!("slack: rejected a request with a bad or stale signature");
        return HttpResponse::Unauthorized().finish();
    }
    let field = |name: &str| {
        url::form_urlencoded::parse(&body)
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default()
    };
    // Slack's periodic certificate check, when the app is configured to send it.
    if field("ssl_check") == "1" {
        return HttpResponse::Ok().finish();
    }
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
    let caller = Caller::Slack {
        user: field("user_id"),
    };
    let text = field("text");
    let context = serde_json::json!({
        "text": text,
        "team_id": field("team_id"),
        "channel_id": field("channel_id"),
        "channel_name": field("channel_name"),
        "user_name": field("user_name"),
    });
    let reply = chat.run(Command::parse(&text), &caller, context).await;
    HttpResponse::Ok().json(serde_json::json!({
        "response_type": if reply.public { "in_channel" } else { "ephemeral" },
        "text": reply.text,
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A request signature, as Slack computes it.
    pub(crate) fn sign(secret: &str, timestamp: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("v0:{timestamp}:").as_bytes());
        mac.update(body);
        format!(
            "v0={}",
            crate::chat::encode_hex(&mac.finalize().into_bytes())
        )
    }

    #[test]
    fn only_a_fresh_signature_from_the_secret_passes() {
        let body = b"command=%2Fpulse&text=status";
        let good = sign("s3cret", "1000", body);
        assert!(verify("s3cret", "1000", &good, body, 1100));
        assert!(!verify("other", "1000", &good, body, 1100), "wrong secret");
        assert!(
            !verify("s3cret", "1000", &good, b"text=pause", 1100),
            "tampered body"
        );
        assert!(
            !verify("s3cret", "1000", &good, body, 2000),
            "replayed later"
        );
        assert!(!verify("s3cret", "1000", "v0=zz", body, 1100));
        assert!(
            !verify("s3cret", "1000", &good[3..], body, 1100),
            "no v0= prefix"
        );
    }
}
