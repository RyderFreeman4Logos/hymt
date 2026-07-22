//! Long-poll Bot API loop (feature-gated implementation).

use std::time::Duration;

use anyhow::{bail, Context, Result};
use hymt_cache::history::HistoryDB;
use hymt_client::TranslationClient;
use hymt_core::config::HotConfig;
use hymt_core::templates::{PromptOpts, TemplateType};
use hymt_segment::Segmenter;
use hymt_translate::{translate_text, TranslationCtx};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

use super::logic::{
    denial_text, evaluate_text_message, help_text, BotAction, ChatKind, IncomingTextMessage,
};

const API_BASE: &str = "https://api.telegram.org";
const LONG_POLL_TIMEOUT_SECS: u64 = 25;

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    description: Option<String>,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    chat: Chat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: String,
}

/// Run the Telegram long-poll loop until SIGINT / process exit.
pub async fn run_telegram_bot(config: &HotConfig) -> Result<()> {
    config.maybe_reload()?;
    if !config.telegram_enabled() {
        bail!(
            "telegram is disabled; set [telegram].enabled = true in {} after configuring bot_token",
            config.path().display()
        );
    }
    let token = config.telegram_bot_token_resolved();
    if token.is_empty() {
        bail!("telegram bot_token is empty; set [telegram].bot_token or HYMT_TELEGRAM_BOT_TOKEN");
    }

    let bootstrap = config.ensure_telegram_claim_password()?;
    if bootstrap.newly_generated {
        // Print once on generation; do not log on every subsequent run.
        eprintln!(
            "hymt telegram: generated claim password (store securely): {}",
            bootstrap.claim_password
        );
    } else {
        eprintln!(
            "hymt telegram: claim password is set in config (not re-printed). Mode={:?}.",
            config.telegram_mode()
        );
    }
    eprintln!(
        "hymt telegram: long-polling (Ctrl+C to stop). Config: {}",
        config.path().display()
    );

    let http = Client::builder()
        .timeout(Duration::from_secs(LONG_POLL_TIMEOUT_SECS + 15))
        .build()
        .context("build reqwest client")?;
    let segmenter = make_segmenter();
    let history = HistoryDB::default();
    let client = TranslationClient::new(config.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut offset: i64 = 0;

    loop {
        // Hot-reload token/owners/mode between polls.
        let _ = config.maybe_reload();
        if !config.telegram_enabled() {
            eprintln!("hymt telegram: disabled in config; exiting");
            break;
        }
        let token = config.telegram_bot_token_resolved();
        if token.is_empty() {
            bail!("telegram bot_token became empty");
        }

        let updates = match get_updates(&http, &token, offset).await {
            Ok(u) => u,
            Err(e) => {
                eprintln!("hymt telegram: getUpdates error: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        for update in updates {
            offset = update.update_id + 1;
            if let Err(e) =
                handle_update(config, &http, &token, &client, &segmenter, &history, update).await
            {
                eprintln!("hymt telegram: update error: {e:#}");
            }
        }
    }
    Ok(())
}

async fn handle_update(
    config: &HotConfig,
    http: &Client,
    token: &str,
    client: &TranslationClient,
    segmenter: &Segmenter,
    history: &HistoryDB,
    update: Update,
) -> Result<()> {
    let Some(message) = update.message else {
        return Ok(());
    };
    let Some(text) = message.text else {
        return Ok(());
    };
    let chat_kind = match message.chat.chat_type.as_str() {
        "private" => ChatKind::Private,
        "group" | "supergroup" => ChatKind::Group,
        _ => ChatKind::Other,
    };
    let incoming = IncomingTextMessage {
        chat_id: message.chat.id,
        chat_kind,
        text,
    };

    let action = evaluate_text_message(
        &incoming,
        &config.telegram_claim_password(),
        config.telegram_mode(),
        &config.telegram_owners(),
        &config.telegram_groups(),
        &config.primary_lang(),
        &config.secondary_lang(),
    );

    match action {
        BotAction::Ignore => Ok(()),
        BotAction::Help => send_message(http, token, incoming.chat_id, help_text()).await,
        BotAction::Denied => send_message(http, token, incoming.chat_id, denial_text()).await,
        BotAction::AlreadyOwner => {
            send_message(http, token, incoming.chat_id, "You are already an owner.").await
        }
        BotAction::Claimed { chat_id } => {
            config.add_telegram_owner(chat_id)?;
            send_message(
                http,
                token,
                chat_id,
                "Ownership claimed. You can now send text for Chinese↔English translation.",
            )
            .await
        }
        BotAction::Translate { text, target_lang } => {
            let opts = PromptOpts::default();
            let ctx = TranslationCtx {
                config,
                client,
                segmenter,
                history,
            };
            let outcome = translate_text(&text, &target_lang, &TemplateType::Default, &opts, &ctx)
                .await
                .map_err(|e| anyhow::anyhow!("translation failed: {e}"))?;
            if outcome.is_completeness_degraded() {
                outcome.report_completeness_degraded();
            }
            send_message(http, token, incoming.chat_id, &outcome.text).await
        }
    }
}

async fn get_updates(http: &Client, token: &str, offset: i64) -> Result<Vec<Update>> {
    // Token lives in the URL path (Bot API contract). Never surface that URL
    // via reqwest's Display/Debug/context chains — they embed the full URL.
    let url = format!("{API_BASE}/bot{token}/getUpdates");
    let resp = match http
        .get(&url)
        .query(&[
            ("offset", offset.to_string()),
            ("timeout", LONG_POLL_TIMEOUT_SECS.to_string()),
            ("allowed_updates", json!(["message"]).to_string()),
        ])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => bail!("{}", safe_reqwest_error("getUpdates request", token, &e)),
    };
    let status = resp.status();
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => bail!("{}", safe_reqwest_error("getUpdates body", token, &e)),
    };
    if !status.is_success() {
        bail!(
            "getUpdates HTTP {status}: {}",
            redact_token_in_text(token, &body)
        );
    }
    let parsed: ApiResponse<Vec<Update>> = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => bail!("parse getUpdates json: {e}"),
    };
    if !parsed.ok {
        bail!(
            "getUpdates not ok: {}",
            redact_token_in_text(
                token,
                &parsed.description.unwrap_or_else(|| "unknown".into())
            )
        );
    }
    Ok(parsed.result.unwrap_or_default())
}

async fn send_message(http: &Client, token: &str, chat_id: i64, text: &str) -> Result<()> {
    // Telegram hard limit ~4096; truncate politely for long translations.
    let text = if text.chars().count() > 4000 {
        let truncated: String = text.chars().take(3990).collect();
        format!("{truncated}…")
    } else {
        text.to_owned()
    };
    let url = format!("{API_BASE}/bot{token}/sendMessage");
    let payload = json!({
        "chat_id": chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });
    let resp = match http.post(&url).json(&payload).send().await {
        Ok(r) => r,
        Err(e) => bail!("{}", safe_reqwest_error("sendMessage request", token, &e)),
    };
    let status = resp.status();
    let body: Value = match resp.json().await {
        Ok(b) => b,
        Err(e) => bail!("{}", safe_reqwest_error("sendMessage json", token, &e)),
    };
    if !status.is_success() || body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let desc = body
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        bail!(
            "sendMessage failed HTTP {status}: {}",
            redact_token_in_text(token, desc)
        );
    }
    Ok(())
}

/// Map a reqwest error to a log-safe message that never includes the bot token.
///
/// `reqwest::Error` Display often embeds the request URL (`/bot{token}/...`).
/// Callers must not append `err.to_string()` / `{:?}` — only this helper.
fn safe_reqwest_error(op: &str, _token: &str, err: &reqwest::Error) -> String {
    // Prefer structured fields over Display/Debug, which may contain the URL.
    let kind = if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else if err.is_request() {
        "request"
    } else if err.is_body() {
        "body"
    } else if err.is_decode() {
        "decode"
    } else if err.is_redirect() {
        "redirect"
    } else {
        "transport"
    };
    let status = err
        .status()
        .map(|s| format!(" HTTP {s}"))
        .unwrap_or_default();
    format!("{op} failed ({kind}{status})")
}

fn redact_token_in_text(token: &str, text: &str) -> String {
    if token.is_empty() {
        return text.to_owned();
    }
    text.replace(token, "<redacted>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_token_in_text_strips_secret() {
        let token = "123456:ABC-DEF_secret";
        let raw = format!("error for https://api.telegram.org/bot{token}/getUpdates");
        let scrubbed = redact_token_in_text(token, &raw);
        assert!(!scrubbed.contains(token));
        assert!(scrubbed.contains("<redacted>"));
        assert!(!scrubbed.contains("ABC-DEF"));
    }
}

fn make_segmenter() -> Segmenter {
    let tokenizer_path = hymt_segment::tokenizer_path();
    if hymt_segment::has_tokenizer_support() && tokenizer_path.exists() {
        Segmenter::new(Some(tokenizer_path)).unwrap_or_else(|_| Segmenter::fallback())
    } else {
        Segmenter::fallback()
    }
}
