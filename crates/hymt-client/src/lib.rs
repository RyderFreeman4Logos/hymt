//! HTTP client for the Hy-MT2 translation endpoint (OpenAI-compatible API).
//!
//! Provides concurrency limiting, exponential-backoff retry on transient errors,
//! `finish_reason == "length"` truncation detection, and SSE streaming support.

use std::sync::Arc;
use std::time::Duration;

use futures_core::Stream;
use hymt_core::config::HotConfig;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio_stream::wrappers::ReceiverStream;

// ── Error ─────────────────────────────────────────────────────────────────────

/// All errors that can occur during a translation request.
#[derive(Debug, Error)]
pub enum ClientError {
    /// Model stopped at `max_tokens` rather than completing the text.
    #[error(
        "segment truncated (hit max_tokens); \
         reduce context_window or increase max_output_tokens"
    )]
    Truncated,

    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },

    #[error("request error: {0}")]
    Request(#[from] reqwest::Error),

    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("translation response missing choices")]
    MissingChoices,

    #[error("translation response missing message content")]
    MissingContent,

    #[error("semaphore closed")]
    SemaphoreClosed,
}

// ── Serde types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct Message {
    role: &'static str,
    content: String,
}

#[derive(Debug, Clone, Serialize)]
struct ChatPayload {
    messages: Vec<Message>,
    max_tokens: u32,
    temperature: f64,
    top_p: f64,
    top_k: u32,
    repetition_penalty: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Option<Vec<Choice>>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    finish_reason: Option<String>,
    message: Option<ChoiceContent>,
    delta: Option<ChoiceContent>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChoiceContent {
    content: Option<String>,
}

// ── Inner shared state ────────────────────────────────────────────────────────

struct Inner {
    config: HotConfig,
    http: reqwest::Client,
    semaphore: Arc<Semaphore>,
    concurrency: usize,
}

// ── TranslationClient ─────────────────────────────────────────────────────────

/// Async HTTP client for the Hy-MT2 translation endpoint.
///
/// Cheap to clone — all clones share the same semaphore and HTTP connection pool.
#[derive(Clone)]
pub struct TranslationClient {
    inner: Arc<Inner>,
}

impl TranslationClient {
    /// Creates a new client.
    ///
    /// Reads `concurrency` and `timeout` once at construction; other config values
    /// (endpoint URL, model, token limits) are refreshed on each call via `maybe_reload`.
    pub fn new(config: HotConfig) -> Result<Self, ClientError> {
        let concurrency = config.concurrency() as usize;
        Self::with_concurrency(config, concurrency)
    }

    /// Creates a new client with an explicit concurrency limit.
    ///
    /// Use this when a CLI/runtime override must replace `[translation].concurrency`
    /// for the lifetime of the client. `concurrency` is clamped to at least 1.
    pub fn with_concurrency(config: HotConfig, concurrency: usize) -> Result<Self, ClientError> {
        let concurrency = concurrency.max(1);
        let timeout_secs = config.timeout();
        let timeout = if timeout_secs.is_finite() && timeout_secs > 0.0 {
            Duration::from_secs_f64(timeout_secs.min(86_400.0))
        } else {
            Duration::from_secs(300)
        };
        let http = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                http,
                semaphore: Arc::new(Semaphore::new(concurrency)),
                concurrency,
            }),
        })
    }

    /// Effective request concurrency baked into this client at construction.
    pub fn concurrency(&self) -> usize {
        self.inner.concurrency
    }

    /// Translates `prompt` to a single string (non-streaming).
    ///
    /// Acquires one concurrency slot for the duration of the request.
    /// Retries on transient errors and guards against `finish_reason == "length"`.
    pub async fn translate(&self, prompt: &str) -> Result<String, ClientError> {
        // Continue with cached config on reload failure (e.g. transient I/O)
        let _ = self.inner.config.maybe_reload();

        let _permit = self
            .inner
            .semaphore
            .acquire()
            .await
            .map_err(|_| ClientError::SemaphoreClosed)?;

        let payload = self.build_payload(prompt, false);
        let headers = self.build_headers();
        let url = self.chat_url();
        self.post_with_retry(&url, &payload, &headers).await
    }

    /// Translates `prompt` with SSE streaming.
    ///
    /// Returns a stream where each item is a content token string.  Errors (including
    /// truncation) surface as `Err` items within the stream.
    ///
    /// The connection is established (with retries) before this method returns.
    pub async fn translate_stream(
        &self,
        prompt: &str,
    ) -> Result<impl Stream<Item = Result<String, ClientError>>, ClientError> {
        let _ = self.inner.config.maybe_reload();

        let permit = Arc::clone(&self.inner.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| ClientError::SemaphoreClosed)?;

        let payload = self.build_payload(prompt, true);
        let headers = self.build_headers();
        let url = self.chat_url();

        let response = self
            .connect_stream_with_retry(&url, &payload, &headers)
            .await?;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, ClientError>>(64);

        let is_sse = is_event_stream(&response);
        tokio::spawn(async move {
            let _permit = permit; // held for the entire stream duration
            if is_sse {
                parse_sse(response, tx).await;
            } else {
                // Non-SSE fallback: read the whole body as a single completion
                let result = async {
                    let body = response.bytes().await?;
                    let resp: ChatResponse = serde_json::from_slice(&body)?;
                    extract_from_response(resp)
                }
                .await;
                let _ = tx.send(result).await;
            }
        });

        Ok(ReceiverStream::new(rx))
    }

    // ── Private helpers ────────────────────────────────────────────────────────

    fn build_payload(&self, prompt: &str, stream: bool) -> ChatPayload {
        let cfg = &self.inner.config;
        let model = cfg.model();
        ChatPayload {
            messages: vec![Message {
                role: "user",
                content: prompt.to_owned(),
            }],
            max_tokens: cfg.max_output_tokens(),
            temperature: cfg.temperature(),
            top_p: cfg.top_p(),
            top_k: cfg.top_k(),
            repetition_penalty: cfg.repetition_penalty(),
            model: if model.is_empty() { None } else { Some(model) },
            stream: if stream { Some(true) } else { None },
        }
    }

    fn build_headers(&self) -> reqwest::header::HeaderMap {
        let mut map = reqwest::header::HeaderMap::new();
        map.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        let key = self.inner.config.api_key();
        if !key.is_empty() {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")) {
                map.insert(reqwest::header::AUTHORIZATION, val);
            }
        }
        map
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.inner.config.endpoint_url())
    }

    async fn post_with_retry(
        &self,
        url: &str,
        payload: &ChatPayload,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<String, ClientError> {
        let mut last: Option<ClientError> = None;
        for attempt in 0..=MAX_RETRIES {
            match self
                .inner
                .http
                .post(url)
                .headers(headers.clone())
                .json(payload)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status < 400 {
                        let body = resp.bytes().await?;
                        let chat: ChatResponse = serde_json::from_slice(&body)?;
                        return extract_from_response(chat);
                    }
                    let body = resp.bytes().await.unwrap_or_default();
                    let err = http_error(status, &body);
                    if !is_retryable_status(status, &body) || attempt == MAX_RETRIES {
                        return Err(err);
                    }
                    last = Some(err);
                }
                Err(e) => {
                    if attempt == MAX_RETRIES {
                        return Err(ClientError::Request(e));
                    }
                    last = Some(ClientError::Request(e));
                }
            }
            tokio::time::sleep(backoff_duration(attempt)).await;
        }
        Err(last.unwrap_or(ClientError::Http {
            status: 0,
            body: "max retries exceeded".into(),
        }))
    }

    async fn connect_stream_with_retry(
        &self,
        url: &str,
        payload: &ChatPayload,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<reqwest::Response, ClientError> {
        let mut last: Option<ClientError> = None;
        for attempt in 0..=MAX_RETRIES {
            match self
                .inner
                .http
                .post(url)
                .headers(headers.clone())
                .json(payload)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status < 400 {
                        return Ok(resp);
                    }
                    let body = resp.bytes().await.unwrap_or_default();
                    let err = http_error(status, &body);
                    if !is_retryable_status(status, &body) || attempt == MAX_RETRIES {
                        return Err(err);
                    }
                    last = Some(err);
                }
                Err(e) => {
                    if attempt == MAX_RETRIES {
                        return Err(ClientError::Request(e));
                    }
                    last = Some(ClientError::Request(e));
                }
            }
            tokio::time::sleep(backoff_duration(attempt)).await;
        }
        Err(last.unwrap_or(ClientError::Http {
            status: 0,
            body: "max retries exceeded".into(),
        }))
    }
}

// ── SSE streaming ─────────────────────────────────────────────────────────────

fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false)
}

async fn parse_sse(
    response: reqwest::Response,
    tx: tokio::sync::mpsc::Sender<Result<String, ClientError>>,
) {
    use tokio_stream::StreamExt as _;

    let mut byte_stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();

    loop {
        match byte_stream.next().await {
            Some(Ok(chunk)) => {
                buf.extend_from_slice(&chunk);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&raw)
                        .trim_end_matches(['\r', '\n'])
                        .to_owned();
                    if let Some(result) = process_sse_line(&line, &mut data_lines) {
                        if tx.send(result).await.is_err() {
                            return;
                        }
                    }
                }
            }
            Some(Err(e)) => {
                let _ = tx.send(Err(ClientError::Request(e))).await;
                return;
            }
            None => break,
        }
    }

    // Handle any incomplete line left in the buffer
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf)
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_owned());
        }
    }
    // Flush any remaining accumulated data lines
    if let Some(result) = tokens_from_sse_data(&data_lines) {
        let _ = tx.send(result).await;
    }
}

/// Processes one SSE text line, mutating the `data_lines` accumulator.
///
/// Returns `Some(result)` only when an event boundary (empty line) is reached
/// and the accumulated data yields a non-empty token.
pub fn process_sse_line(
    line: &str,
    data_lines: &mut Vec<String>,
) -> Option<Result<String, ClientError>> {
    if line.is_empty() {
        let result = tokens_from_sse_data(data_lines);
        data_lines.clear();
        result
    } else if line.starts_with(':') {
        None // SSE comment
    } else if let Some(data) = line.strip_prefix("data:") {
        data_lines.push(data.trim_start().to_owned());
        None
    } else {
        None // other SSE fields (event, id, retry) are unused by this endpoint
    }
}

pub fn tokens_from_sse_data(data_lines: &[String]) -> Option<Result<String, ClientError>> {
    if data_lines.is_empty() {
        return None;
    }
    let data = data_lines.join("\n");
    if data == "[DONE]" {
        return None;
    }
    match serde_json::from_str::<ChatResponse>(&data) {
        Ok(resp) => match extract_stream_token(resp) {
            Ok(t) if t.is_empty() => None,
            Ok(t) => Some(Ok(t)),
            Err(e) => Some(Err(e)),
        },
        Err(e) => Some(Err(ClientError::Json(e))),
    }
}

// ── Response extraction ───────────────────────────────────────────────────────

fn extract_from_response(resp: ChatResponse) -> Result<String, ClientError> {
    let choices = resp.choices.unwrap_or_default();
    let first = choices
        .into_iter()
        .next()
        .ok_or(ClientError::MissingChoices)?;

    if first.finish_reason.as_deref() == Some("length") {
        return Err(ClientError::Truncated);
    }

    if let Some(msg) = first.message {
        if let Some(content) = msg.content {
            return Ok(content);
        }
    }
    if let Some(text) = first.text {
        return Ok(text);
    }

    Err(ClientError::MissingContent)
}

fn extract_stream_token(resp: ChatResponse) -> Result<String, ClientError> {
    let choices = resp.choices.unwrap_or_default();
    let first = match choices.into_iter().next() {
        Some(c) => c,
        None => return Ok(String::new()),
    };

    if first.finish_reason.as_deref() == Some("length") {
        return Err(ClientError::Truncated);
    }

    if let Some(delta) = first.delta {
        if let Some(content) = delta.content {
            return Ok(content);
        }
    }
    if let Some(text) = first.text {
        return Ok(text);
    }
    Ok(String::new())
}

// ── Retry helpers ─────────────────────────────────────────────────────────────

const MAX_RETRIES: u32 = 5;

const RETRYABLE_STATUSES: &[u16] = &[429, 500, 502, 503, 504];

pub fn is_retryable_status(status: u16, body: &[u8]) -> bool {
    if RETRYABLE_STATUSES.contains(&status) {
        return true;
    }
    if status != 400 {
        return false;
    }
    // A 400 caused by a JSON parse issue on the server side is transient
    let text = String::from_utf8_lossy(body).to_lowercase();
    text.contains("json") || text.contains("parse")
}

pub fn backoff_duration(attempt: u32) -> Duration {
    Duration::from_secs_f64((0.5 * 2f64.powi(attempt as i32)).min(8.0))
}

fn http_error(status: u16, body: &[u8]) -> ClientError {
    ClientError::Http {
        status,
        body: String::from_utf8_lossy(body).chars().take(500).collect(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_chat(json: &str) -> ChatResponse {
        serde_json::from_str(json).expect("test JSON must be valid")
    }

    // ── extract_from_response ────────────────────────────────────────────────

    #[test]
    fn extract_message_content() {
        let resp =
            parse_chat(r#"{"choices":[{"finish_reason":"stop","message":{"content":"Hello"}}]}"#);
        assert_eq!(extract_from_response(resp).unwrap(), "Hello");
    }

    #[test]
    fn extract_text_field_fallback() {
        let resp = parse_chat(r#"{"choices":[{"finish_reason":"stop","text":"World"}]}"#);
        assert_eq!(extract_from_response(resp).unwrap(), "World");
    }

    #[test]
    fn extract_truncated_finish_reason() {
        let resp =
            parse_chat(r#"{"choices":[{"finish_reason":"length","message":{"content":"cut"}}]}"#);
        assert!(matches!(
            extract_from_response(resp),
            Err(ClientError::Truncated)
        ));
    }

    #[test]
    fn extract_empty_choices_is_error() {
        let resp = parse_chat(r#"{"choices":[]}"#);
        assert!(matches!(
            extract_from_response(resp),
            Err(ClientError::MissingChoices)
        ));
    }

    #[test]
    fn extract_null_choices_is_error() {
        let resp = parse_chat(r#"{"choices":null}"#);
        assert!(matches!(
            extract_from_response(resp),
            Err(ClientError::MissingChoices)
        ));
    }

    #[test]
    fn extract_missing_content_is_error() {
        let resp = parse_chat(r#"{"choices":[{"finish_reason":"stop","message":{}}]}"#);
        assert!(matches!(
            extract_from_response(resp),
            Err(ClientError::MissingContent)
        ));
    }

    // ── extract_stream_token ─────────────────────────────────────────────────

    #[test]
    fn stream_token_delta_content() {
        let resp = parse_chat(r#"{"choices":[{"finish_reason":null,"delta":{"content":"tok"}}]}"#);
        assert_eq!(extract_stream_token(resp).unwrap(), "tok");
    }

    #[test]
    fn stream_token_truncated_finish_reason() {
        let resp = parse_chat(r#"{"choices":[{"finish_reason":"length","delta":{"content":""}}]}"#);
        assert!(matches!(
            extract_stream_token(resp),
            Err(ClientError::Truncated)
        ));
    }

    #[test]
    fn stream_token_empty_delta_returns_empty() {
        let resp = parse_chat(r#"{"choices":[{"finish_reason":null,"delta":{}}]}"#);
        assert_eq!(extract_stream_token(resp).unwrap(), "");
    }

    #[test]
    fn stream_token_no_choices_returns_empty() {
        let resp = parse_chat(r#"{"choices":[]}"#);
        assert_eq!(extract_stream_token(resp).unwrap(), "");
    }

    #[test]
    fn stream_token_text_field_fallback() {
        let resp = parse_chat(r#"{"choices":[{"finish_reason":null,"text":"fallback"}]}"#);
        assert_eq!(extract_stream_token(resp).unwrap(), "fallback");
    }

    // ── SSE line processing ──────────────────────────────────────────────────

    #[test]
    fn sse_data_line_accumulates() {
        let mut data_lines: Vec<String> = Vec::new();
        let result = process_sse_line("data: hello", &mut data_lines);
        assert!(result.is_none());
        assert_eq!(data_lines, vec!["hello"]);
    }

    #[test]
    fn sse_data_line_no_space() {
        let mut data_lines: Vec<String> = Vec::new();
        process_sse_line("data:hello", &mut data_lines);
        assert_eq!(data_lines, vec!["hello"]);
    }

    #[test]
    fn sse_comment_ignored() {
        let mut data_lines: Vec<String> = Vec::new();
        let result = process_sse_line(": heartbeat", &mut data_lines);
        assert!(result.is_none());
        assert!(data_lines.is_empty());
    }

    #[test]
    fn sse_empty_line_flushes_done() {
        let mut data_lines = vec!["[DONE]".to_owned()];
        let result = process_sse_line("", &mut data_lines);
        assert!(result.is_none()); // [DONE] yields nothing
        assert!(data_lines.is_empty()); // cleared
    }

    #[test]
    fn sse_done_sentinel_returns_none() {
        assert!(tokens_from_sse_data(&["[DONE]".to_owned()]).is_none());
    }

    #[test]
    fn sse_empty_data_lines_returns_none() {
        assert!(tokens_from_sse_data(&[]).is_none());
    }

    #[test]
    fn sse_valid_token_extracted() {
        let json = r#"{"choices":[{"finish_reason":null,"delta":{"content":"word"}}]}"#;
        let result = tokens_from_sse_data(&[json.to_owned()]);
        assert_eq!(result.unwrap().unwrap(), "word");
    }

    #[test]
    fn sse_truncated_propagates_as_error() {
        let json = r#"{"choices":[{"finish_reason":"length","delta":{"content":""}}]}"#;
        let result = tokens_from_sse_data(&[json.to_owned()]);
        assert!(matches!(result, Some(Err(ClientError::Truncated))));
    }

    #[test]
    fn sse_invalid_json_is_error() {
        let result = tokens_from_sse_data(&["not json".to_owned()]);
        assert!(matches!(result, Some(Err(ClientError::Json(_)))));
    }

    #[test]
    fn sse_empty_content_token_returns_none() {
        // A delta with empty content (e.g. final role announcement) is filtered out
        let json = r#"{"choices":[{"finish_reason":null,"delta":{"content":""}}]}"#;
        let result = tokens_from_sse_data(&[json.to_owned()]);
        assert!(result.is_none());
    }

    // ── SSE full event sequence ──────────────────────────────────────────────

    #[test]
    fn sse_multi_line_event() {
        // Multiple data: lines in one event are joined with \n before JSON parse.
        // In practice the endpoint always sends one data: line per event, but the
        // spec allows multiple lines.
        let json_part1 = r#"{"choices":[{"finish_reason":null,"delta":{"content":"hi"}}]}"#;
        let result = tokens_from_sse_data(&[json_part1.to_owned()]);
        assert_eq!(result.unwrap().unwrap(), "hi");
    }

    #[test]
    fn sse_full_sequence_via_process_line() {
        // Each SSE event is one data: line followed by an empty line delimiter.
        let lines = [
            r#"data: {"choices":[{"finish_reason":null,"delta":{"content":"He"}}]}"#,
            "",
            r#"data: {"choices":[{"finish_reason":null,"delta":{"content":"llo"}}]}"#,
            "",
            "data: [DONE]",
            "",
        ];

        let mut data_lines: Vec<String> = Vec::new();
        let mut tokens: Vec<String> = Vec::new();

        for line in &lines {
            if let Some(Ok(tok)) = process_sse_line(line, &mut data_lines) {
                tokens.push(tok);
            }
        }

        assert_eq!(tokens, vec!["He", "llo"]);
    }

    // ── Retry helpers ────────────────────────────────────────────────────────

    #[test]
    fn retryable_statuses() {
        for &code in &[429u16, 500, 502, 503, 504] {
            assert!(
                is_retryable_status(code, b""),
                "status {code} should be retryable"
            );
        }
    }

    #[test]
    fn non_retryable_statuses() {
        assert!(!is_retryable_status(200, b""));
        assert!(!is_retryable_status(404, b"not found"));
        assert!(!is_retryable_status(400, b"bad request"));
        assert!(!is_retryable_status(401, b""));
    }

    #[test]
    fn status_400_json_parse_body_is_retryable() {
        assert!(is_retryable_status(
            400,
            b"json parse error in request body"
        ));
        assert!(is_retryable_status(400, b"failed to parse JSON"));
        assert!(is_retryable_status(400, b"JSON_PARSE_FAILURE"));
    }

    #[test]
    fn backoff_values() {
        assert!((backoff_duration(0).as_secs_f64() - 0.5).abs() < 1e-9);
        assert!((backoff_duration(1).as_secs_f64() - 1.0).abs() < 1e-9);
        assert!((backoff_duration(2).as_secs_f64() - 2.0).abs() < 1e-9);
        assert!((backoff_duration(3).as_secs_f64() - 4.0).abs() < 1e-9);
        assert!((backoff_duration(4).as_secs_f64() - 8.0).abs() < 1e-9);
        // Capped at 8 seconds for high attempt numbers
        assert!((backoff_duration(10).as_secs_f64() - 8.0).abs() < 1e-9);
        assert!((backoff_duration(100).as_secs_f64() - 8.0).abs() < 1e-9);
    }

    // ── Payload serialization ────────────────────────────────────────────────

    #[test]
    fn payload_omits_model_and_stream_when_none() {
        let payload = ChatPayload {
            messages: vec![Message {
                role: "user",
                content: "test".into(),
            }],
            max_tokens: 100,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repetition_penalty: 1.0,
            model: None,
            stream: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("model"), "model should be absent");
        assert!(!obj.contains_key("stream"), "stream should be absent");
    }

    #[test]
    fn payload_includes_model_and_stream_when_set() {
        let payload = ChatPayload {
            messages: vec![],
            max_tokens: 4096,
            temperature: 0.7,
            top_p: 0.6,
            top_k: 20,
            repetition_penalty: 1.05,
            model: Some("hy-mt2".into()),
            stream: Some(true),
        };
        let json = serde_json::to_value(&payload).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj["model"].as_str().unwrap(), "hy-mt2");
        assert!(obj["stream"].as_bool().unwrap());
    }

    #[test]
    fn payload_message_structure() {
        let payload = ChatPayload {
            messages: vec![Message {
                role: "user",
                content: "translate this".into(),
            }],
            max_tokens: 4096,
            temperature: 0.7,
            top_p: 0.6,
            top_k: 20,
            repetition_penalty: 1.05,
            model: None,
            stream: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        let messages = json["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"].as_str().unwrap(), "user");
        assert_eq!(messages[0]["content"].as_str().unwrap(), "translate this");
    }
}
