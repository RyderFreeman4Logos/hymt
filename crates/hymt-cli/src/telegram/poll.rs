//! Long-poll Bot API loop (feature-gated implementation).

use std::collections::BTreeSet;
use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hymt_cache::history::HistoryDB;
use hymt_client::TranslationClient;
use hymt_core::config::HotConfig;
use hymt_core::templates::{PromptOpts, TemplateType};
use hymt_segment::Segmenter;
use hymt_translate::{
    plan_translation, translate_text, translate_text_stream, StreamEvent, TranslationCtx,
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

use super::logic::{
    authorize_chat, cn_en_target_lang, denial_text, evaluate_text_message, help_text, AuthDecision,
    BotAction, ChatKind, IncomingTextMessage,
};

const API_BASE: &str = "https://api.telegram.org";
const LONG_POLL_TIMEOUT_SECS: u64 = 25;
const TELEGRAM_INLINE_LIMIT: usize = 4096;
const DOCUMENT_TOO_LARGE: &str = "document exceeds configured size";
const UNSUPPORTED_DOCUMENT_TEXT: &str =
    "Please send a .txt or .md document (or a text/plain or text/markdown file).";
const INVALID_DOCUMENT_TEXT: &str = "I can only translate UTF-8 text documents.";

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
    document: Option<Document>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: String,
}

#[derive(Debug, Deserialize)]
struct Document {
    file_id: String,
    file_name: Option<String>,
    mime_type: Option<String>,
    file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TelegramFile {
    file_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TranslationDocument {
    file_id: String,
    file_name: String,
}

#[derive(Debug, PartialEq, Eq)]
enum DocumentAction {
    Ignore,
    Unsupported,
    TooLarge,
    Accepted(TranslationDocument),
}

#[derive(Clone, Copy)]
enum DocumentKind {
    Text,
    Markdown,
}

impl DocumentKind {
    const fn extension(self) -> &'static str {
        match self {
            Self::Text => "txt",
            Self::Markdown => "md",
        }
    }

    const fn mime_type(self) -> &'static str {
        match self {
            Self::Text => "text/plain",
            Self::Markdown => "text/markdown",
        }
    }
}

const TELEGRAM_EDIT_INTERVAL: Duration = Duration::from_secs(1);

/// Minimal Bot API surface used by progressive delivery. Keeping it narrow makes
/// the ordered send/edit sequence directly testable without network traffic.
trait TelegramMessageApi {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<i64>;
    async fn edit_message(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()>;
}

/// File operations required for translating a Telegram document.
///
/// Keeping the Bot API calls behind this small trait makes the full
/// download → translate → reply sequence testable without network traffic.
trait TelegramDocumentApi: TelegramMessageApi {
    async fn download_document(&self, file_id: &str, max_size: u64) -> Result<Vec<u8>>;
    async fn send_document(&self, chat_id: i64, file_name: &str, text: &str) -> Result<()>;
}

struct TelegramHttpApi<'a> {
    http: &'a Client,
    token: &'a str,
}

impl<'a> TelegramHttpApi<'a> {
    const fn new(http: &'a Client, token: &'a str) -> Self {
        Self { http, token }
    }
}

impl TelegramMessageApi for TelegramHttpApi<'_> {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<i64> {
        send_message_id(self.http, self.token, chat_id, text).await
    }

    async fn edit_message(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        edit_message(self.http, self.token, chat_id, message_id, text).await
    }
}

impl TelegramDocumentApi for TelegramHttpApi<'_> {
    async fn download_document(&self, file_id: &str, max_size: u64) -> Result<Vec<u8>> {
        download_telegram_document(self.http, self.token, file_id, max_size).await
    }

    async fn send_document(&self, chat_id: i64, file_name: &str, text: &str) -> Result<()> {
        send_document_upload(self.http, self.token, chat_id, file_name, text).await
    }
}

#[derive(Default)]
struct TelegramStreamBatcher {
    text: String,
    completed_segments: BTreeSet<usize>,
    next_segment: usize,
}

impl TelegramStreamBatcher {
    fn push_token(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// Return true only when a newly contiguous prefix has completed.
    fn mark_segment_complete(&mut self, segment: usize) -> bool {
        self.completed_segments.insert(segment);
        let previous_next = self.next_segment;
        while self.completed_segments.remove(&self.next_segment) {
            self.next_segment += 1;
        }
        self.next_segment != previous_next
    }

    fn replace_with_final(&mut self, text: String) {
        self.text = text;
    }

    fn text(&self) -> &str {
        &self.text
    }
}

struct EditRateLimiter {
    minimum_interval: Duration,
    last_edit: Option<Instant>,
}

impl EditRateLimiter {
    fn new(minimum_interval: Duration) -> Self {
        Self {
            minimum_interval,
            last_edit: None,
        }
    }

    fn remaining_delay(&self, now: Instant) -> Option<Duration> {
        self.last_edit.and_then(|last_edit| {
            self.minimum_interval
                .checked_sub(now.saturating_duration_since(last_edit))
                .filter(|delay| !delay.is_zero())
        })
    }

    fn record_edit(&mut self, now: Instant) {
        self.last_edit = Some(now);
    }

    async fn wait_before_edit(&mut self) {
        if let Some(delay) = self.remaining_delay(Instant::now()) {
            tokio::time::sleep(delay).await;
        }
    }
}

fn should_stream_telegram(stream_enabled: bool, segment_count: usize) -> bool {
    stream_enabled && segment_count > 1
}

async fn publish_stream_batch<A: TelegramMessageApi>(
    api: &A,
    chat_id: i64,
    text: &str,
    sent_message: &mut Option<(i64, String)>,
    edit_rate_limiter: &mut EditRateLimiter,
) {
    let rendered_text = telegram_message_text(text);
    if rendered_text.is_empty() {
        return;
    }

    let Some((message_id, rendered_prefix)) = sent_message.as_ref() else {
        match api.send_message(chat_id, &rendered_text).await {
            Ok(message_id) => *sent_message = Some((message_id, rendered_text)),
            Err(error) => eprintln!(
                "hymt telegram: sendMessage failed while streaming; will retry on the next batch: {error:#}"
            ),
        }
        return;
    };

    if rendered_prefix == &rendered_text {
        return;
    }
    let message_id = *message_id;
    edit_rate_limiter.wait_before_edit().await;
    match api.edit_message(chat_id, message_id, &rendered_text).await {
        Ok(()) => {
            edit_rate_limiter.record_edit(Instant::now());
            if let Some((_, rendered_prefix)) = sent_message.as_mut() {
                *rendered_prefix = rendered_text;
            }
        }
        Err(error) => eprintln!(
            "hymt telegram: editMessageText failed while streaming; keeping the previous partial response: {error:#}"
        ),
    }
}

async fn deliver_stream_events<A: TelegramMessageApi>(
    api: &A,
    chat_id: i64,
    mut events: tokio::sync::mpsc::Receiver<StreamEvent>,
    edit_interval: Duration,
) -> Result<()> {
    let mut batch = TelegramStreamBatcher::default();
    let mut sent_message = None;
    let mut edit_rate_limiter = EditRateLimiter::new(edit_interval);

    while let Some(event) = events.recv().await {
        match event {
            StreamEvent::Token(text) => batch.push_token(&text),
            StreamEvent::SegmentDone(segment) => {
                if batch.mark_segment_complete(segment) {
                    publish_stream_batch(
                        api,
                        chat_id,
                        batch.text(),
                        &mut sent_message,
                        &mut edit_rate_limiter,
                    )
                    .await;
                }
            }
            StreamEvent::AllDone(text) => {
                batch.replace_with_final(text);
                publish_stream_batch(
                    api,
                    chat_id,
                    batch.text(),
                    &mut sent_message,
                    &mut edit_rate_limiter,
                )
                .await;
                break;
            }
        }
    }
    Ok(())
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
    let segmenter = make_segmenter(config)?;
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
    let chat_id = message.chat.id;
    let chat_kind = match message.chat.chat_type.as_str() {
        "private" => ChatKind::Private,
        "group" | "supergroup" => ChatKind::Group,
        _ => ChatKind::Other,
    };

    if let Some(text) = message.text {
        let incoming = IncomingTextMessage {
            chat_id,
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

        return match action {
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
                let plan = plan_translation(
                    &text,
                    &target_lang,
                    config,
                    segmenter,
                    &TemplateType::Default,
                    &opts,
                )
                .map_err(|e| anyhow::anyhow!("translation failed: {e}"))?;
                let stream = should_stream_telegram(
                    config.telegram_streaming_enabled(),
                    plan.segment_count(),
                );
                let outcome = if stream {
                    let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
                    let api = TelegramHttpApi::new(http, token);
                    let translate = translate_text_stream(
                        &text,
                        &target_lang,
                        &TemplateType::Default,
                        &opts,
                        &ctx,
                        event_tx,
                    );
                    let deliver = deliver_stream_events(
                        &api,
                        incoming.chat_id,
                        event_rx,
                        TELEGRAM_EDIT_INTERVAL,
                    );
                    let (outcome, ()) = tokio::try_join!(translate, deliver)
                        .map_err(|e| anyhow::anyhow!("translation failed: {e}"))?;
                    outcome
                } else {
                    translate_text(&text, &target_lang, &TemplateType::Default, &opts, &ctx)
                        .await
                        .map_err(|e| anyhow::anyhow!("translation failed: {e}"))?
                };
                if outcome.is_completeness_degraded() {
                    outcome.report_completeness_degraded();
                }
                if stream {
                    Ok(())
                } else {
                    send_message(http, token, incoming.chat_id, &outcome.text).await
                }
            }
        };
    }

    let Some(document) = message.document else {
        return Ok(());
    };
    handle_document_update(
        config, http, token, client, segmenter, history, chat_id, chat_kind, document,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn handle_document_update(
    config: &HotConfig,
    http: &Client,
    token: &str,
    client: &TranslationClient,
    segmenter: &Segmenter,
    history: &HistoryDB,
    chat_id: i64,
    chat_kind: ChatKind,
    document: Document,
) -> Result<()> {
    let auth = authorize_chat(
        chat_kind,
        chat_id,
        config.telegram_mode(),
        &config.telegram_owners(),
        &config.telegram_groups(),
    );
    let api = TelegramHttpApi::new(http, token);
    if !config.telegram_accept_documents() {
        return Ok(());
    }
    if auth == AuthDecision::Denied {
        api.send_message(chat_id, denial_text()).await?;
        return Ok(());
    }
    match classify_document(&document, true, config.telegram_max_document_size()) {
        DocumentAction::Ignore => Ok(()),
        DocumentAction::Unsupported => {
            api.send_message(chat_id, UNSUPPORTED_DOCUMENT_TEXT).await?;
            Ok(())
        }
        DocumentAction::TooLarge => {
            api.send_message(
                chat_id,
                &document_too_large_message(config.telegram_max_document_size()),
            )
            .await?;
            Ok(())
        }
        DocumentAction::Accepted(document) => process_document_message(
            &api,
            chat_id,
            auth,
            true,
            &document,
            config.telegram_max_document_size(),
            |source| async move {
                let target_lang =
                    cn_en_target_lang(&source, &config.primary_lang(), &config.secondary_lang());
                let opts = PromptOpts::default();
                let ctx = TranslationCtx {
                    config,
                    client,
                    segmenter,
                    history,
                };
                let outcome =
                    translate_text(&source, &target_lang, &TemplateType::Default, &opts, &ctx)
                        .await
                        .map_err(|_| anyhow::anyhow!("document translation failed"))?;
                if outcome.is_completeness_degraded() {
                    outcome.report_completeness_degraded();
                }
                Ok(outcome.text)
            },
        )
        .await
        .map_err(|_| anyhow::anyhow!("document handling failed")),
    }
}

async fn process_document_message<A, F, Fut>(
    api: &A,
    chat_id: i64,
    auth: AuthDecision,
    accept_documents: bool,
    document: &TranslationDocument,
    max_size: u64,
    translate: F,
) -> Result<()>
where
    A: TelegramDocumentApi,
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    if !accept_documents {
        return Ok(());
    }
    if auth == AuthDecision::Denied {
        api.send_message(chat_id, denial_text()).await?;
        return Ok(());
    }

    let bytes = match api.download_document(&document.file_id, max_size).await {
        Ok(bytes) => bytes,
        Err(error) if error.to_string() == DOCUMENT_TOO_LARGE => {
            api.send_message(chat_id, &document_too_large_message(max_size))
                .await?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if bytes.len() as u64 > max_size {
        api.send_message(chat_id, &document_too_large_message(max_size))
            .await?;
        return Ok(());
    }
    let source = match String::from_utf8(bytes) {
        Ok(source) => source,
        Err(_) => {
            api.send_message(chat_id, INVALID_DOCUMENT_TEXT).await?;
            return Ok(());
        }
    };
    let translated = translate(source).await?;
    if should_send_document(&translated) {
        api.send_document(chat_id, &document.file_name, &translated)
            .await
    } else {
        api.send_message(chat_id, &translated).await.map(|_| ())
    }
}

fn classify_document(document: &Document, accept_documents: bool, max_size: u64) -> DocumentAction {
    if !accept_documents {
        return DocumentAction::Ignore;
    }
    if document.file_size.is_some_and(|size| size > max_size) {
        return DocumentAction::TooLarge;
    }
    let Some(kind) = document_kind(document.file_name.as_deref(), document.mime_type.as_deref())
    else {
        return DocumentAction::Unsupported;
    };
    DocumentAction::Accepted(TranslationDocument {
        file_id: document.file_id.clone(),
        file_name: output_document_name(document.file_name.as_deref(), kind),
    })
}

fn document_kind(file_name: Option<&str>, mime_type: Option<&str>) -> Option<DocumentKind> {
    document_kind_from_filename(file_name).or_else(|| document_kind_from_mime(mime_type))
}

fn document_kind_from_filename(file_name: Option<&str>) -> Option<DocumentKind> {
    let extension = file_name?.rsplit_once('.')?.1;
    if extension.eq_ignore_ascii_case("txt") {
        Some(DocumentKind::Text)
    } else if extension.eq_ignore_ascii_case("md") {
        Some(DocumentKind::Markdown)
    } else {
        None
    }
}

fn document_kind_from_mime(mime_type: Option<&str>) -> Option<DocumentKind> {
    match mime_type?.split(';').next()?.trim() {
        value if value.eq_ignore_ascii_case("text/plain") => Some(DocumentKind::Text),
        value if value.eq_ignore_ascii_case("text/markdown") => Some(DocumentKind::Markdown),
        _ => None,
    }
}

fn output_document_name(file_name: Option<&str>, kind: DocumentKind) -> String {
    let safe_name = file_name
        .and_then(|name| name.rsplit(['/', '\\']).next())
        .filter(|name| !name.is_empty())
        .filter(|name| document_kind_from_filename(Some(name)).is_some());
    safe_name
        .map(str::to_owned)
        .unwrap_or_else(|| format!("translated.{}", kind.extension()))
}

fn should_send_document(text: &str) -> bool {
    text.chars().count() >= TELEGRAM_INLINE_LIMIT
}

fn document_too_large_message(max_size: u64) -> String {
    format!("This document is too large. Maximum accepted size is {max_size} bytes.")
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

async fn telegram_file_path(http: &Client, token: &str, file_id: &str) -> Result<String> {
    // Token lives in the Bot API URL. Keep all failure messages URL-free.
    let url = format!("{API_BASE}/bot{token}/getFile");
    let resp = match http
        .post(&url)
        .json(&json!({ "file_id": file_id }))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(error) => bail!("{}", safe_reqwest_error("getFile request", token, &error)),
    };
    let status = resp.status();
    let body = match resp.text().await {
        Ok(body) => body,
        Err(error) => bail!("{}", safe_reqwest_error("getFile body", token, &error)),
    };
    if !status.is_success() {
        bail!(
            "getFile HTTP {status}: {}",
            redact_token_in_text(token, &body)
        );
    }
    let parsed: ApiResponse<TelegramFile> =
        serde_json::from_str(&body).map_err(|_| anyhow::anyhow!("parse getFile json"))?;
    if !parsed.ok {
        bail!(
            "getFile not ok: {}",
            redact_token_in_text(
                token,
                &parsed.description.unwrap_or_else(|| "unknown".into())
            )
        );
    }
    parsed
        .result
        .and_then(|file| file.file_path)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| anyhow::anyhow!("getFile succeeded without a file path"))
}

async fn download_telegram_document(
    http: &Client,
    token: &str,
    file_id: &str,
    max_size: u64,
) -> Result<Vec<u8>> {
    let file_path = telegram_file_path(http, token, file_id).await?;
    // `file_path` comes from Telegram's getFile response. Do not log this URL:
    // it includes the bot token by protocol definition.
    let url = format!("{API_BASE}/file/bot{token}/{file_path}");
    let mut resp = match http.get(&url).send().await {
        Ok(resp) => resp,
        Err(error) => bail!(
            "{}",
            safe_reqwest_error("document download request", token, &error)
        ),
    };
    if resp.content_length().is_some_and(|size| size > max_size) {
        bail!(DOCUMENT_TOO_LARGE);
    }
    let status = resp.status();
    if !status.is_success() {
        bail!("document download HTTP {status}");
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = match resp.chunk().await {
        Ok(chunk) => chunk,
        Err(error) => bail!(
            "{}",
            safe_reqwest_error("document download body", token, &error)
        ),
    } {
        let chunk_size = chunk.len() as u64;
        if chunk_size > max_size.saturating_sub(bytes.len() as u64) {
            bail!(DOCUMENT_TOO_LARGE);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn send_document_upload(
    http: &Client,
    token: &str,
    chat_id: i64,
    file_name: &str,
    text: &str,
) -> Result<()> {
    let kind = document_kind_from_filename(Some(file_name)).unwrap_or(DocumentKind::Text);
    let part = reqwest::multipart::Part::text(text.to_owned())
        .file_name(file_name.to_owned())
        .mime_str(kind.mime_type())
        .map_err(|_| anyhow::anyhow!("build document upload"))?;
    let form = reqwest::multipart::Form::new()
        .text("chat_id", chat_id.to_string())
        .part("document", part);
    let url = format!("{API_BASE}/bot{token}/sendDocument");
    let resp = match http.post(&url).multipart(form).send().await {
        Ok(resp) => resp,
        Err(error) => bail!(
            "{}",
            safe_reqwest_error("sendDocument request", token, &error)
        ),
    };
    let status = resp.status();
    let body: Value = match resp.json().await {
        Ok(body) => body,
        Err(error) => bail!("{}", safe_reqwest_error("sendDocument json", token, &error)),
    };
    if !status.is_success() || body.get("ok").and_then(Value::as_bool) != Some(true) {
        let description = body
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        bail!(
            "sendDocument failed HTTP {status}: {}",
            redact_token_in_text(token, description)
        );
    }
    Ok(())
}

fn telegram_message_text(text: &str) -> String {
    // Telegram accepts at most 4096 characters in a text message.
    if text.chars().count() > TELEGRAM_INLINE_LIMIT {
        let truncated: String = text.chars().take(TELEGRAM_INLINE_LIMIT - 1).collect();
        format!("{truncated}…")
    } else {
        text.to_owned()
    }
}

async fn send_message(http: &Client, token: &str, chat_id: i64, text: &str) -> Result<()> {
    send_message_id(http, token, chat_id, text)
        .await
        .map(|_| ())
}

async fn send_message_id(http: &Client, token: &str, chat_id: i64, text: &str) -> Result<i64> {
    let url = format!("{API_BASE}/bot{token}/sendMessage");
    let payload = json!({
        "chat_id": chat_id,
        "text": telegram_message_text(text),
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
    body.pointer("/result/message_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("sendMessage succeeded without a message id"))
}

async fn edit_message(
    http: &Client,
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
) -> Result<()> {
    let url = format!("{API_BASE}/bot{token}/editMessageText");
    let payload = json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": telegram_message_text(text),
        "disable_web_page_preview": true,
    });

    for attempt in 0..=1 {
        let resp = match http.post(&url).json(&payload).send().await {
            Ok(r) => r,
            Err(e) => bail!(
                "{}",
                safe_reqwest_error("editMessageText request", token, &e)
            ),
        };
        let status = resp.status();
        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => bail!("{}", safe_reqwest_error("editMessageText json", token, &e)),
        };
        if status.is_success() && body.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            return Ok(());
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
            if let Some(retry_after) = body
                .pointer("/parameters/retry_after")
                .and_then(Value::as_u64)
            {
                eprintln!(
                    "hymt telegram: editMessageText rate-limited; retrying after {retry_after}s"
                );
                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                continue;
            }
        }
        let desc = body
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        bail!(
            "editMessageText failed HTTP {status}: {}",
            redact_token_in_text(token, desc)
        );
    }
    unreachable!("the bounded Telegram edit retry returns or fails")
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

    #[test]
    fn telegram_streaming_requires_opt_in_and_multiple_segments() {
        assert!(!should_stream_telegram(false, 2));
        assert!(!should_stream_telegram(true, 1));
        assert!(should_stream_telegram(true, 2));
    }

    #[test]
    fn stream_batcher_flushes_only_after_contiguous_segments_complete() {
        let mut batcher = TelegramStreamBatcher::default();
        batcher.push_token("first");
        assert!(!batcher.mark_segment_complete(1));
        assert!(batcher.mark_segment_complete(0));
        assert_eq!(batcher.text(), "first");
    }

    #[test]
    fn edit_rate_limiter_waits_one_second_between_edits() {
        let mut limiter = EditRateLimiter::new(Duration::from_secs(1));
        let now = Instant::now();
        assert_eq!(limiter.remaining_delay(now), None);
        limiter.record_edit(now);
        assert_eq!(
            limiter.remaining_delay(now + Duration::from_millis(400)),
            Some(Duration::from_millis(600))
        );
        assert_eq!(limiter.remaining_delay(now + Duration::from_secs(1)), None);
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum TelegramCall {
        Send {
            chat_id: i64,
            text: String,
        },
        Edit {
            chat_id: i64,
            message_id: i64,
            text: String,
        },
    }

    #[derive(Default)]
    struct MockTelegramApi {
        calls: std::sync::Mutex<Vec<TelegramCall>>,
        edit_failures_remaining: std::sync::Mutex<usize>,
    }

    impl MockTelegramApi {
        fn with_edit_failures(edit_failures: usize) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                edit_failures_remaining: std::sync::Mutex::new(edit_failures),
            }
        }

        fn calls(&self) -> Vec<TelegramCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl TelegramMessageApi for MockTelegramApi {
        async fn send_message(&self, chat_id: i64, text: &str) -> Result<i64> {
            self.calls.lock().unwrap().push(TelegramCall::Send {
                chat_id,
                text: text.to_owned(),
            });
            Ok(777)
        }

        async fn edit_message(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
            self.calls.lock().unwrap().push(TelegramCall::Edit {
                chat_id,
                message_id,
                text: text.to_owned(),
            });
            let mut failures = self.edit_failures_remaining.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(anyhow::anyhow!("mock edit failure"));
            }
            Ok(())
        }
    }

    struct RetryingEditMockTelegramApi {
        retry_after: Duration,
        edit_count: std::sync::Mutex<usize>,
        retry_succeeded_at: std::sync::Mutex<Option<Instant>>,
        next_edit_started_at: std::sync::Mutex<Option<Instant>>,
    }

    impl RetryingEditMockTelegramApi {
        fn new(retry_after: Duration) -> Self {
            Self {
                retry_after,
                edit_count: std::sync::Mutex::new(0),
                retry_succeeded_at: std::sync::Mutex::new(None),
                next_edit_started_at: std::sync::Mutex::new(None),
            }
        }

        fn retry_succeeded_at(&self) -> Instant {
            self.retry_succeeded_at
                .lock()
                .unwrap()
                .expect("first edit should retry successfully")
        }

        fn next_edit_started_at(&self) -> Instant {
            self.next_edit_started_at
                .lock()
                .unwrap()
                .expect("next batch should edit the message")
        }
    }

    impl TelegramMessageApi for RetryingEditMockTelegramApi {
        async fn send_message(&self, _chat_id: i64, _text: &str) -> Result<i64> {
            Ok(777)
        }

        async fn edit_message(&self, _chat_id: i64, _message_id: i64, _text: &str) -> Result<()> {
            let edit_count = {
                let mut edit_count = self.edit_count.lock().unwrap();
                *edit_count += 1;
                *edit_count
            };

            if edit_count == 1 {
                tokio::time::sleep(self.retry_after).await;
                *self.retry_succeeded_at.lock().unwrap() = Some(Instant::now());
            } else {
                *self.next_edit_started_at.lock().unwrap() = Some(Instant::now());
            }
            Ok(())
        }
    }

    async fn deliver_mock_events(api: &MockTelegramApi, events: Vec<StreamEvent>) -> Result<()> {
        let (tx, rx) = tokio::sync::mpsc::channel(events.len());
        for event in events {
            tx.send(event).await.unwrap();
        }
        drop(tx);
        deliver_stream_events(api, 42, rx, Duration::ZERO).await
    }

    #[tokio::test]
    async fn streaming_delivery_sends_initial_chunk_then_edits_progressively() {
        let api = MockTelegramApi::default();
        deliver_mock_events(
            &api,
            vec![
                StreamEvent::Token("first".into()),
                StreamEvent::SegmentDone(0),
                StreamEvent::Token(" second".into()),
                StreamEvent::SegmentDone(1),
                StreamEvent::AllDone("first second".into()),
            ],
        )
        .await
        .unwrap();

        assert_eq!(
            api.calls(),
            vec![
                TelegramCall::Send {
                    chat_id: 42,
                    text: "first".into(),
                },
                TelegramCall::Edit {
                    chat_id: 42,
                    message_id: 777,
                    text: "first second".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn streaming_delivery_continues_after_an_edit_failure() {
        let api = MockTelegramApi::with_edit_failures(1);
        deliver_mock_events(
            &api,
            vec![
                StreamEvent::Token("first".into()),
                StreamEvent::SegmentDone(0),
                StreamEvent::Token(" second".into()),
                StreamEvent::SegmentDone(1),
                StreamEvent::AllDone("first second".into()),
            ],
        )
        .await
        .unwrap();

        assert_eq!(
            api.calls()
                .iter()
                .filter(|call| matches!(call, TelegramCall::Edit { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn streaming_delivery_throttles_after_a_retrying_edit_succeeds() {
        let retry_after = Duration::from_millis(75);
        let edit_interval = Duration::from_millis(100);
        let api = RetryingEditMockTelegramApi::new(retry_after);
        let (tx, rx) = tokio::sync::mpsc::channel(5);
        for event in [
            StreamEvent::Token("first".into()),
            StreamEvent::SegmentDone(0),
            StreamEvent::Token(" second".into()),
            StreamEvent::SegmentDone(1),
            StreamEvent::AllDone("first second final".into()),
        ] {
            tx.send(event).await.unwrap();
        }
        drop(tx);

        deliver_stream_events(&api, 42, rx, edit_interval)
            .await
            .unwrap();

        let elapsed_since_retry_success = api
            .next_edit_started_at()
            .saturating_duration_since(api.retry_succeeded_at());
        assert!(
            elapsed_since_retry_success >= Duration::from_millis(80),
            "next edit started only {elapsed_since_retry_success:?} after the retried edit succeeded"
        );
    }

    #[test]
    fn documents_accept_text_mime_or_extension_and_enforce_size_limit() {
        let text_by_extension = Document {
            file_id: "txt-file".into(),
            file_name: Some("notes.TXT".into()),
            mime_type: Some("application/octet-stream".into()),
            file_size: Some(1024),
        };
        let markdown_by_mime = Document {
            file_id: "markdown-file".into(),
            file_name: Some("upload".into()),
            mime_type: Some("text/markdown; charset=utf-8".into()),
            file_size: Some(1024),
        };
        let unsupported = Document {
            file_id: "image-file".into(),
            file_name: Some("photo.png".into()),
            mime_type: Some("image/png".into()),
            file_size: Some(1024),
        };
        let too_large = Document {
            file_id: "large-file".into(),
            file_name: Some("large.txt".into()),
            mime_type: Some("text/plain".into()),
            file_size: Some(1025),
        };

        assert_eq!(
            classify_document(&text_by_extension, true, 1024),
            DocumentAction::Accepted(TranslationDocument {
                file_id: "txt-file".into(),
                file_name: "notes.TXT".into(),
            })
        );
        assert_eq!(
            classify_document(&markdown_by_mime, true, 1024),
            DocumentAction::Accepted(TranslationDocument {
                file_id: "markdown-file".into(),
                file_name: "translated.md".into(),
            })
        );
        assert_eq!(
            classify_document(&unsupported, true, 1024),
            DocumentAction::Unsupported
        );
        assert_eq!(
            classify_document(&too_large, true, 1024),
            DocumentAction::TooLarge
        );
        assert_eq!(
            classify_document(&text_by_extension, false, 1024),
            DocumentAction::Ignore
        );
    }

    #[test]
    fn documents_use_a_document_at_the_inline_limit() {
        assert!(!should_send_document(
            &"a".repeat(TELEGRAM_INLINE_LIMIT - 1)
        ));
        assert!(should_send_document(&"a".repeat(TELEGRAM_INLINE_LIMIT)));
        assert!(should_send_document(&"🦀".repeat(TELEGRAM_INLINE_LIMIT)));
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum DocumentCall {
        Send(String),
        Download(String),
        Translate(String),
        Upload { file_name: String, text: String },
    }

    struct DocumentMockApi {
        calls: std::sync::Mutex<Vec<DocumentCall>>,
        downloaded: Vec<u8>,
    }

    impl DocumentMockApi {
        fn new(downloaded: impl Into<Vec<u8>>) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                downloaded: downloaded.into(),
            }
        }

        fn record_translation(&self, source: &str) {
            self.calls
                .lock()
                .unwrap()
                .push(DocumentCall::Translate(source.to_owned()));
        }

        fn calls(&self) -> Vec<DocumentCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl TelegramMessageApi for DocumentMockApi {
        async fn send_message(&self, _chat_id: i64, text: &str) -> Result<i64> {
            self.calls
                .lock()
                .unwrap()
                .push(DocumentCall::Send(text.to_owned()));
            Ok(777)
        }

        async fn edit_message(&self, _chat_id: i64, _message_id: i64, _text: &str) -> Result<()> {
            Ok(())
        }
    }

    impl TelegramDocumentApi for DocumentMockApi {
        async fn download_document(&self, file_id: &str, _max_size: u64) -> Result<Vec<u8>> {
            self.calls
                .lock()
                .unwrap()
                .push(DocumentCall::Download(file_id.to_owned()));
            Ok(self.downloaded.clone())
        }

        async fn send_document(&self, _chat_id: i64, file_name: &str, text: &str) -> Result<()> {
            self.calls.lock().unwrap().push(DocumentCall::Upload {
                file_name: file_name.to_owned(),
                text: text.to_owned(),
            });
            Ok(())
        }
    }

    #[tokio::test]
    async fn authorized_document_is_downloaded_translated_then_uploaded() {
        let api = DocumentMockApi::new("English source");
        let document = TranslationDocument {
            file_id: "file-id".into(),
            file_name: "source.md".into(),
        };

        process_document_message(
            &api,
            42,
            AuthDecision::Allowed,
            true,
            &document,
            1024,
            |source| {
                api.record_translation(&source);
                std::future::ready(Ok("x".repeat(TELEGRAM_INLINE_LIMIT)))
            },
        )
        .await
        .unwrap();

        assert_eq!(
            api.calls(),
            vec![
                DocumentCall::Download("file-id".into()),
                DocumentCall::Translate("English source".into()),
                DocumentCall::Upload {
                    file_name: "source.md".into(),
                    text: "x".repeat(TELEGRAM_INLINE_LIMIT),
                },
            ]
        );
    }

    #[tokio::test]
    async fn unauthorized_document_is_denied_without_downloading() {
        let api = DocumentMockApi::new("must not download");
        let document = TranslationDocument {
            file_id: "file-id".into(),
            file_name: "source.txt".into(),
        };
        process_document_message(
            &api,
            99,
            AuthDecision::Denied,
            true,
            &document,
            1024,
            |_| std::future::ready(Ok(String::new())),
        )
        .await
        .unwrap();
        assert_eq!(api.calls(), vec![DocumentCall::Send(denial_text().into())]);
    }
}

fn make_segmenter(config: &HotConfig) -> Result<Segmenter> {
    let profile = config.model_profile()?;
    hymt_segment::create_segmenter(profile).map_err(|error| anyhow::anyhow!("{error}"))
}
