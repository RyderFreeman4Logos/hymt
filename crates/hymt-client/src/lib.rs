//! HTTP client for the Hy-MT2 translation endpoint (OpenAI-compatible API).
//!
//! Provides concurrency limiting, exponential-backoff retry on transient errors,
//! `finish_reason == "length"` truncation detection, and SSE streaming support.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_core::Stream;
use hymt_core::completeness::CompletionTermination;
use hymt_core::config::{GenerationBackend, GenerationSettings, HotConfig, Setting};
use hymt_core::runtime::{BackendRuntimeInfo, BackendVerificationStatus};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio_stream::wrappers::ReceiverStream;

// ── Error ─────────────────────────────────────────────────────────────────────

/// All errors that can occur during a translation request.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("invalid configuration: {0}")]
    Config(#[from] hymt_core::error::CoreError),

    /// Model stopped at `max_tokens` rather than completing the text.
    #[error(
        "segment truncated (hit max_tokens); \
         reduce per_request_context or increase max_output_tokens"
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

    #[error("strict backend preflight refused translation: {0}")]
    BackendPreflight(String),
}

/// A completed non-streaming translation with its provider termination signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationCompletion {
    pub text: String,
    pub termination: CompletionTermination,
}

/// An item emitted by a termination-aware translation stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslationStreamEvent {
    Token(String),
    Finished(CompletionTermination),
}

/// Configured values shown beside service-discovered values by `hymt backend inspect`.
/// API keys and endpoint credentials are deliberately absent.
#[derive(Debug, Clone)]
pub struct ConfiguredBackendInfo {
    pub backend: GenerationBackend,
    pub model: Option<String>,
    pub profile: String,
    pub total_context: u32,
    pub parallel_slots: u32,
    pub per_request_context: u32,
    pub max_output_tokens: u32,
    pub sampler_overrides: GenerationSettings,
}

/// Result of one cached or fresh backend preflight.
#[derive(Debug, Clone)]
pub struct BackendPreflight {
    pub configured: ConfiguredBackendInfo,
    pub runtime: BackendRuntimeInfo,
    pub warnings: Vec<String>,
}

// ── Serde types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct Message {
    role: &'static str,
    content: String,
}

/// The standard chat-completions fields shared by every endpoint adapter.
#[derive(Debug, Clone, Serialize)]
struct ChatPayloadCore {
    messages: Vec<Message>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

/// A backend-specific request body. Each variant owns only the extensions that
/// its endpoint documents, rather than exposing one pseudo-OpenAI sampler schema.
#[derive(Debug, Clone)]
enum ChatPayload {
    LlamaCpp(LlamaCppPayload),
    Vllm(VllmPayload),
    OpenAiCompatible(OpenAiCompatiblePayload),
}

impl Serialize for ChatPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::LlamaCpp(payload) => payload.serialize(serializer),
            Self::Vllm(payload) => payload.serialize(serializer),
            Self::OpenAiCompatible(payload) => payload.serialize(serializer),
        }
    }
}

impl ChatPayload {
    fn from_generation_settings(
        prompt: &str,
        max_tokens: u32,
        model: String,
        stream: bool,
        settings: &GenerationSettings,
        backend: GenerationBackend,
    ) -> Self {
        let core = ChatPayloadCore {
            messages: vec![Message {
                role: "user",
                content: prompt.to_owned(),
            }],
            max_tokens,
            model: (!model.is_empty()).then_some(model),
            stream: stream.then_some(true),
        };
        match backend {
            GenerationBackend::LlamaCpp => Self::LlamaCpp(LlamaCppPayload::new(core, settings)),
            GenerationBackend::Vllm => Self::Vllm(VllmPayload::new(core, settings)),
            GenerationBackend::OpenAiCompatible => {
                Self::OpenAiCompatible(OpenAiCompatiblePayload::new(core, settings))
            }
        }
    }
}

/// llama.cpp's documented native sampler extensions.
#[derive(Debug, Clone, Serialize)]
struct LlamaCppPayload {
    #[serde(flatten)]
    core: ChatPayloadCore,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repeat_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repeat_last_n: Option<i64>,
}

impl LlamaCppPayload {
    fn new(core: ChatPayloadCore, settings: &GenerationSettings) -> Self {
        Self {
            core,
            temperature: map_f64_setting(settings.temperature, 0.0),
            top_p: map_f64_setting(settings.top_p, 1.0),
            top_k: map_i32_setting(settings.top_k, 0),
            repeat_penalty: map_f64_setting(settings.repetition_penalty, 1.0),
            min_p: map_f64_setting(settings.min_p, 0.0),
            repeat_last_n: map_i64_setting(settings.repeat_last_n, 0),
        }
    }
}

/// vLLM's documented OpenAI-server sampler extensions.
#[derive(Debug, Clone, Serialize)]
struct VllmPayload {
    #[serde(flatten)]
    core: ChatPayloadCore,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repetition_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_p: Option<f64>,
}

impl VllmPayload {
    fn new(core: ChatPayloadCore, settings: &GenerationSettings) -> Self {
        Self {
            core,
            temperature: map_f64_setting(settings.temperature, 0.0),
            top_p: map_f64_setting(settings.top_p, 1.0),
            // vLLM uses -1 for disabled top-k, unlike llama.cpp's 0.
            top_k: map_i32_setting(settings.top_k, -1),
            repetition_penalty: map_f64_setting(settings.repetition_penalty, 1.0),
            min_p: map_f64_setting(settings.min_p, 0.0),
        }
    }
}

/// Strict generic mode sends only common chat-completions sampling fields.
#[derive(Debug, Clone, Serialize)]
struct OpenAiCompatiblePayload {
    #[serde(flatten)]
    core: ChatPayloadCore,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
}

impl OpenAiCompatiblePayload {
    fn new(core: ChatPayloadCore, settings: &GenerationSettings) -> Self {
        Self {
            core,
            temperature: map_f64_setting(settings.temperature, 0.0),
            top_p: map_f64_setting(settings.top_p, 1.0),
        }
    }
}

fn map_f64_setting(setting: Setting<f64>, disabled_value: f64) -> Option<f64> {
    match setting {
        Setting::ServerDefault => None,
        Setting::Disabled => Some(disabled_value),
        Setting::Value(value) => Some(value),
    }
}

fn map_i32_setting(setting: Setting<i32>, disabled_value: i32) -> Option<i32> {
    match setting {
        Setting::ServerDefault => None,
        Setting::Disabled => Some(disabled_value),
        Setting::Value(value) => Some(value),
    }
}

fn map_i64_setting(setting: Setting<i64>, disabled_value: i64) -> Option<i64> {
    match setting {
        Setting::ServerDefault => None,
        Setting::Disabled => Some(disabled_value),
        Setting::Value(value) => Some(value),
    }
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

const BACKEND_PREFLIGHT_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeCacheKey {
    endpoint_url: String,
    backend: GenerationBackend,
    profile: String,
}

#[derive(Debug, Clone)]
struct RuntimeCacheEntry {
    key: RuntimeCacheKey,
    checked_at: Instant,
    runtime: BackendRuntimeInfo,
    warnings: Vec<String>,
}

struct Inner {
    config: HotConfig,
    http: reqwest::Client,
    semaphore: Arc<Semaphore>,
    concurrency: usize,
    runtime_cache: Mutex<Option<RuntimeCacheEntry>>,
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
        config.generation_settings()?;
        config.generation_backend()?;
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
                runtime_cache: Mutex::new(None),
            }),
        })
    }

    /// Effective request concurrency baked into this client at construction.
    pub fn concurrency(&self) -> usize {
        self.inner.concurrency
    }

    /// Discover runtime state once per endpoint/backend/profile TTL.
    ///
    /// Normal mode always returns a report: an unavailable or malformed endpoint
    /// becomes explicitly unverified and callers plan conservatively. Strict mode
    /// refuses before its caller can reach cache lookup or model invocation.
    pub async fn preflight_backend(&self) -> Result<BackendPreflight, ClientError> {
        self.preflight_backend_inner(false, true).await
    }

    /// Bypass the preflight TTL, for inspection and restart/profile-change checks.
    pub async fn refresh_backend_preflight(&self) -> Result<BackendPreflight, ClientError> {
        self.preflight_backend_inner(true, true).await
    }

    /// Return a fresh resolved-state report even when strict translation policy
    /// would refuse work. This keeps `hymt backend inspect` actionable.
    pub async fn inspect_backend(&self) -> Result<BackendPreflight, ClientError> {
        self.preflight_backend_inner(true, false).await
    }

    async fn preflight_backend_inner(
        &self,
        force_refresh: bool,
        enforce_strict: bool,
    ) -> Result<BackendPreflight, ClientError> {
        let _ = self.inner.config.maybe_reload();
        let key = self.runtime_cache_key()?;
        if !force_refresh {
            if let Some(entry) = self.cached_runtime(&key) {
                self.inner
                    .config
                    .set_backend_runtime_info(entry.runtime.clone());
                return self.backend_preflight_report(
                    entry.runtime,
                    entry.warnings,
                    enforce_strict,
                );
            }
        }

        // A changed endpoint/backend/profile must never continue planning against
        // a previous server's runtime state while this request is in flight.
        self.inner.config.clear_backend_runtime_info();
        let observed_at = unix_timestamp_secs();
        let runtime = self.fetch_backend_runtime(key.backend, observed_at).await;
        let warnings = self.preflight_warnings(&runtime)?;
        self.inner.config.set_backend_runtime_info(runtime.clone());
        if let Ok(mut cache) = self.inner.runtime_cache.lock() {
            *cache = Some(RuntimeCacheEntry {
                key,
                checked_at: Instant::now(),
                runtime: runtime.clone(),
                warnings: warnings.clone(),
            });
        }
        self.backend_preflight_report(runtime, warnings, enforce_strict)
    }

    fn runtime_cache_key(&self) -> Result<RuntimeCacheKey, ClientError> {
        Ok(RuntimeCacheKey {
            endpoint_url: self.inner.config.endpoint_url(),
            backend: self.inner.config.generation_backend()?,
            profile: self.inner.config.model_profile()?.id().to_owned(),
        })
    }

    fn cached_runtime(&self, key: &RuntimeCacheKey) -> Option<RuntimeCacheEntry> {
        let cache = self.inner.runtime_cache.lock().ok()?;
        let entry = cache.as_ref()?;
        (entry.key == *key && entry.checked_at.elapsed() < BACKEND_PREFLIGHT_TTL)
            .then(|| entry.clone())
    }

    async fn fetch_backend_runtime(
        &self,
        backend: GenerationBackend,
        observed_at_unix_secs: u64,
    ) -> BackendRuntimeInfo {
        let url = match backend {
            GenerationBackend::LlamaCpp => {
                Self::llama_cpp_props_url(&self.inner.config.endpoint_url())
            }
            GenerationBackend::Vllm | GenerationBackend::OpenAiCompatible => {
                Self::models_url(&self.inner.config.endpoint_url())
            }
        };

        match self
            .inner
            .http
            .get(&url)
            .headers(self.build_headers())
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => match response.json().await {
                Ok(body) => {
                    let parsed = match backend {
                        GenerationBackend::LlamaCpp => {
                            BackendRuntimeInfo::from_llama_cpp_props(&body, observed_at_unix_secs)
                        }
                        GenerationBackend::Vllm | GenerationBackend::OpenAiCompatible => {
                            BackendRuntimeInfo::from_openai_models(
                                backend,
                                &body,
                                observed_at_unix_secs,
                            )
                        }
                    };
                    match parsed {
                        Ok(runtime) => runtime,
                        Err(error) => BackendRuntimeInfo::unverified(
                            backend,
                            observed_at_unix_secs,
                            format!("malformed backend response: {error}"),
                        ),
                    }
                }
                Err(_) => BackendRuntimeInfo::unverified(
                    backend,
                    observed_at_unix_secs,
                    "backend returned invalid JSON",
                ),
            },
            Ok(response) => BackendRuntimeInfo::unverified(
                backend,
                observed_at_unix_secs,
                format!("backend endpoint returned HTTP {}", response.status()),
            ),
            Err(_) => BackendRuntimeInfo::unverified(
                backend,
                observed_at_unix_secs,
                "backend endpoint is unavailable",
            ),
        }
    }

    fn backend_preflight_report(
        &self,
        runtime: BackendRuntimeInfo,
        warnings: Vec<String>,
        enforce_strict: bool,
    ) -> Result<BackendPreflight, ClientError> {
        let configured = self.configured_backend_info()?;
        let identity_verified = runtime.is_verified() && runtime.served_model.is_some();
        if enforce_strict
            && self.inner.config.strict_backend_preflight()
            && (!identity_verified || !warnings.is_empty())
        {
            let reasons = if warnings.is_empty() {
                "runtime model identity is unavailable".to_owned()
            } else {
                warnings.join("; ")
            };
            return Err(ClientError::BackendPreflight(reasons));
        }
        Ok(BackendPreflight {
            configured,
            runtime,
            warnings,
        })
    }

    fn configured_backend_info(&self) -> Result<ConfiguredBackendInfo, ClientError> {
        let model = self.inner.config.model();
        Ok(ConfiguredBackendInfo {
            backend: self.inner.config.generation_backend()?,
            model: (!model.is_empty()).then_some(model),
            profile: self.inner.config.model_profile()?.id().to_owned(),
            total_context: self.inner.config.total_context(),
            parallel_slots: self.inner.config.parallel_slots(),
            per_request_context: self.inner.config.per_request_context(),
            max_output_tokens: self.inner.config.max_output_tokens(),
            sampler_overrides: self.inner.config.generation_settings()?,
        })
    }

    fn preflight_warnings(&self, runtime: &BackendRuntimeInfo) -> Result<Vec<String>, ClientError> {
        let configured = self.configured_backend_info()?;
        let mut warnings = Vec::new();
        if runtime.verification_status == BackendVerificationStatus::Unverified {
            warnings.push(format!(
                "runtime preflight is unverified: {}",
                runtime
                    .verification_message
                    .as_deref()
                    .unwrap_or("backend did not provide verifiable metadata")
            ));
            return Ok(warnings);
        }
        if let Some(total_context) = runtime.total_context {
            if total_context != configured.total_context {
                warnings.push(format!(
                    "context mismatch: configured total_context={} but service reports {}",
                    configured.total_context, total_context
                ));
            }
        }
        if let Some(per_slot_context) = runtime.per_slot_context {
            if per_slot_context != configured.per_request_context {
                warnings.push(format!(
                    "context mismatch: configured per_request_context={} but service reports {}",
                    configured.per_request_context, per_slot_context
                ));
            }
        }
        if let (Some(configured_model), Some(served_model)) =
            (configured.model.as_deref(), runtime.served_model.as_deref())
        {
            if configured_model != served_model {
                warnings.push(format!(
                    "profile/model mismatch: configured model {configured_model:?} but service reports {served_model:?}"
                ));
            }
        }
        let profile = self.inner.config.model_profile()?;
        if configured.model.is_none() && !profile.is_generic() {
            if let Some(served_model) = runtime.served_model.as_deref() {
                let normalized_served = served_model.to_ascii_lowercase();
                let recognized = profile
                    .gguf_aliases()
                    .iter()
                    .any(|alias| normalized_served == alias.to_ascii_lowercase())
                    || profile.model().is_some_and(|source| {
                        source
                            .repo
                            .rsplit('/')
                            .next()
                            .is_some_and(|name| normalized_served == name.to_ascii_lowercase())
                    });
                if !recognized {
                    warnings.push(format!(
                        "profile/model mismatch: profile {} does not recognize service model {served_model:?}",
                        profile.id()
                    ));
                }
            }
        }
        if let Some(discovered_wire_key) = runtime
            .sampler_defaults
            .repetition_penalty_wire_key
            .as_deref()
        {
            let expected_wire_key = match configured.backend {
                GenerationBackend::LlamaCpp => Some("repeat_penalty"),
                GenerationBackend::Vllm => Some("repetition_penalty"),
                GenerationBackend::OpenAiCompatible => None,
            };
            if let Some(expected_wire_key) = expected_wire_key {
                if discovered_wire_key != expected_wire_key {
                    warnings.push(format!(
                        "sampler wire-key mismatch: configured backend expects {expected_wire_key}, service advertises {discovered_wire_key}"
                    ));
                }
            }
        }
        if configured.sampler_overrides.uses_any_server_defaults()
            && !runtime.sampler_defaults.is_complete()
        {
            warnings.push(
                "unexpected sampler state: service-owned sampler defaults are incomplete in runtime metadata"
                    .to_owned(),
            );
        }
        Ok(warnings)
    }

    /// Query llama.cpp's `/props` endpoint for its service-owned sampler defaults.
    ///
    /// This is diagnostic-only: failures never alter a request or block
    /// translation. Normal requests still omit sampler fields unless an explicit
    /// configuration override supplies one.
    pub async fn llama_cpp_props_diagnostic(&self) -> Option<String> {
        let _ = self.inner.config.maybe_reload();
        match self.inner.config.generation_backend() {
            Ok(GenerationBackend::LlamaCpp) => {}
            Ok(_) => return None,
            Err(error) => {
                return Some(format!(
                    "Warning: cannot determine backend for llama.cpp /props diagnostics ({error}); \
                     client will continue omitting sampler fields."
                ));
            }
        }

        let url = Self::llama_cpp_props_url(&self.inner.config.endpoint_url());
        match self
            .inner
            .http
            .get(&url)
            .headers(self.build_headers())
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => match response.json().await {
                Ok(props) => Some(llama_cpp_props_default_message(&url, &props)),
                Err(error) => Some(format!(
                    "Warning: llama.cpp GET {url} returned invalid /props JSON ({error}); \
                     client will continue omitting sampler fields."
                )),
            },
            Ok(response) => Some(format!(
                "Warning: llama.cpp GET {url} returned HTTP {}; client will continue omitting \
                 sampler fields.",
                response.status()
            )),
            Err(error) => Some(format!(
                "Warning: llama.cpp GET {url} is unavailable ({error}); client will continue \
                 omitting sampler fields."
            )),
        }
    }

    /// Translates `prompt` to a single string (non-streaming).
    ///
    /// Acquires one concurrency slot for the duration of the request.
    /// Retries on transient errors and returns an error for `finish_reason == "length"`.
    ///
    /// Call [`Self::translate_with_completion`] when the caller needs the partial
    /// output and termination signal for completeness handling.
    pub async fn translate(&self, prompt: &str) -> Result<String, ClientError> {
        let completion = self.translate_with_completion(prompt).await?;
        if completion.termination == CompletionTermination::Length {
            return Err(ClientError::Truncated);
        }
        Ok(completion.text)
    }

    /// Translates `prompt`, preserving partial output and the provider termination signal.
    pub async fn translate_with_completion(
        &self,
        prompt: &str,
    ) -> Result<TranslationCompletion, ClientError> {
        // Continue with cached config on reload failure (e.g. transient I/O)
        let _ = self.inner.config.maybe_reload();

        let _permit = self
            .inner
            .semaphore
            .acquire()
            .await
            .map_err(|_| ClientError::SemaphoreClosed)?;

        let payload = self.build_payload(prompt, false)?;
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

        let payload = self.build_payload(prompt, true)?;
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

    /// Translates `prompt` with SSE streaming while preserving termination events.
    ///
    /// A `finish_reason == "length"` response emits [`TranslationStreamEvent::Finished`]
    /// with [`CompletionTermination::Length`] after all available partial tokens.
    pub async fn translate_stream_with_completion(
        &self,
        prompt: &str,
    ) -> Result<impl Stream<Item = Result<TranslationStreamEvent, ClientError>>, ClientError> {
        let _ = self.inner.config.maybe_reload();

        let permit = Arc::clone(&self.inner.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| ClientError::SemaphoreClosed)?;

        let payload = self.build_payload(prompt, true)?;
        let headers = self.build_headers();
        let url = self.chat_url();

        let response = self
            .connect_stream_with_retry(&url, &payload, &headers)
            .await?;
        let (tx, rx) =
            tokio::sync::mpsc::channel::<Result<TranslationStreamEvent, ClientError>>(64);

        let is_sse = is_event_stream(&response);
        tokio::spawn(async move {
            let _permit = permit; // held for the entire stream duration
            if is_sse {
                parse_sse_with_completion(response, tx).await;
            } else {
                // Non-SSE fallback: emit its text before the terminal signal.
                let result = async {
                    let body = response.bytes().await?;
                    let resp: ChatResponse = serde_json::from_slice(&body)?;
                    extract_completion_from_response(resp)
                }
                .await;
                match result {
                    Ok(completion) => {
                        if !completion.text.is_empty()
                            && tx
                                .send(Ok(TranslationStreamEvent::Token(completion.text)))
                                .await
                                .is_err()
                        {
                            return;
                        }
                        let _ = tx
                            .send(Ok(TranslationStreamEvent::Finished(completion.termination)))
                            .await;
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error)).await;
                    }
                }
            }
        });

        Ok(ReceiverStream::new(rx))
    }

    // ── Private helpers ────────────────────────────────────────────────────────

    fn build_payload(&self, prompt: &str, stream: bool) -> Result<ChatPayload, ClientError> {
        let cfg = &self.inner.config;
        Ok(ChatPayload::from_generation_settings(
            prompt,
            cfg.resolved_max_output_tokens(),
            cfg.model(),
            stream,
            &cfg.generation_settings()?,
            cfg.generation_backend()?,
        ))
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

    fn llama_cpp_props_url(endpoint_url: &str) -> String {
        let endpoint_url = endpoint_url.trim_end_matches('/');
        let server_url = endpoint_url.strip_suffix("/v1").unwrap_or(endpoint_url);
        format!("{server_url}/props")
    }

    fn models_url(endpoint_url: &str) -> String {
        format!("{}/models", endpoint_url.trim_end_matches('/'))
    }

    async fn post_with_retry(
        &self,
        url: &str,
        payload: &ChatPayload,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<TranslationCompletion, ClientError> {
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
                        return extract_completion_from_response(chat);
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

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

async fn parse_sse_with_completion(
    response: reqwest::Response,
    tx: tokio::sync::mpsc::Sender<Result<TranslationStreamEvent, ClientError>>,
) {
    use tokio_stream::StreamExt as _;

    let mut byte_stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    let mut saw_terminal = false;

    loop {
        match byte_stream.next().await {
            Some(Ok(chunk)) => {
                buf.extend_from_slice(&chunk);
                while let Some(pos) = buf.iter().position(|&byte| byte == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&raw)
                        .trim_end_matches(['\r', '\n'])
                        .to_owned();
                    if line.is_empty() {
                        if !flush_completion_sse_data(&mut data_lines, &tx, &mut saw_terminal).await
                        {
                            return;
                        }
                    } else if !line.starts_with(':') {
                        if let Some(data) = line.strip_prefix("data:") {
                            data_lines.push(data.trim_start().to_owned());
                        }
                    }
                }
            }
            Some(Err(error)) => {
                let _ = tx.send(Err(ClientError::Request(error))).await;
                return;
            }
            None => break,
        }
    }

    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf)
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_owned());
        }
    }
    if !flush_completion_sse_data(&mut data_lines, &tx, &mut saw_terminal).await {
        return;
    }
    if !saw_terminal {
        let _ = tx
            .send(Ok(TranslationStreamEvent::Finished(
                CompletionTermination::Unknown,
            )))
            .await;
    }
}

async fn flush_completion_sse_data(
    data_lines: &mut Vec<String>,
    tx: &tokio::sync::mpsc::Sender<Result<TranslationStreamEvent, ClientError>>,
    saw_terminal: &mut bool,
) -> bool {
    if data_lines.is_empty() {
        return true;
    }
    let data = data_lines.join("\n");
    data_lines.clear();
    if data == "[DONE]" {
        if !*saw_terminal {
            *saw_terminal = true;
            return tx
                .send(Ok(TranslationStreamEvent::Finished(
                    CompletionTermination::Stop,
                )))
                .await
                .is_ok();
        }
        return true;
    }

    let events = match serde_json::from_str::<ChatResponse>(&data) {
        Ok(response) => match extract_stream_completion_events(response) {
            Ok(events) => events,
            Err(error) => return tx.send(Err(error)).await.is_ok(),
        },
        Err(error) => return tx.send(Err(ClientError::Json(error))).await.is_ok(),
    };
    for event in events {
        if matches!(event, TranslationStreamEvent::Finished(_)) {
            *saw_terminal = true;
        }
        if tx.send(Ok(event)).await.is_err() {
            return false;
        }
    }
    true
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

fn termination_from_finish_reason(finish_reason: Option<&str>) -> CompletionTermination {
    match finish_reason {
        Some("stop") => CompletionTermination::Stop,
        Some("length") => CompletionTermination::Length,
        Some(_) | None => CompletionTermination::Unknown,
    }
}

fn extract_completion_from_response(
    resp: ChatResponse,
) -> Result<TranslationCompletion, ClientError> {
    let choices = resp.choices.unwrap_or_default();
    let first = choices
        .into_iter()
        .next()
        .ok_or(ClientError::MissingChoices)?;
    let termination = termination_from_finish_reason(first.finish_reason.as_deref());

    if let Some(msg) = first.message {
        if let Some(content) = msg.content {
            return Ok(TranslationCompletion {
                text: content,
                termination,
            });
        }
    }
    if let Some(text) = first.text {
        return Ok(TranslationCompletion { text, termination });
    }

    Err(ClientError::MissingContent)
}

fn extract_from_response(resp: ChatResponse) -> Result<String, ClientError> {
    let completion = extract_completion_from_response(resp)?;
    if completion.termination == CompletionTermination::Length {
        return Err(ClientError::Truncated);
    }
    Ok(completion.text)
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

fn extract_stream_completion_events(
    resp: ChatResponse,
) -> Result<Vec<TranslationStreamEvent>, ClientError> {
    let choices = resp.choices.unwrap_or_default();
    let Some(first) = choices.into_iter().next() else {
        return Ok(Vec::new());
    };
    let termination = first
        .finish_reason
        .as_deref()
        .map(|finish_reason| termination_from_finish_reason(Some(finish_reason)));
    let content = first
        .delta
        .and_then(|delta| delta.content)
        .or(first.text)
        .unwrap_or_default();
    let mut events = Vec::new();
    if !content.is_empty() {
        events.push(TranslationStreamEvent::Token(content));
    }
    if let Some(termination) = termination {
        events.push(TranslationStreamEvent::Finished(termination));
    }
    Ok(events)
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

fn llama_cpp_props_default_message(url: &str, props: &serde_json::Value) -> String {
    match props.get("default_generation_settings") {
        Some(defaults) if defaults.is_object() => {
            format!("llama.cpp /props server sampling defaults from {url}: {defaults}")
        }
        _ => format!(
            "Warning: llama.cpp GET {url} did not expose default_generation_settings; \
             client will continue omitting sampler fields."
        ),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "backend_payload_tests.rs"]
mod backend_payload_tests;

#[cfg(test)]
#[path = "backend_preflight_tests.rs"]
mod backend_preflight_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use hymt_core::config::{GenerationBackend, GenerationSettings, Setting};

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
    fn llama_cpp_maps_disabled_samplers() {
        let payload = ChatPayload::from_generation_settings(
            "test",
            1,
            String::new(),
            false,
            &GenerationSettings {
                temperature: Setting::Value(0.7),
                top_p: Setting::ServerDefault,
                top_k: Setting::Disabled,
                repetition_penalty: Setting::Disabled,
                min_p: Setting::Disabled,
                repeat_last_n: Setting::Disabled,
            },
            GenerationBackend::LlamaCpp,
        );
        let object = serde_json::to_value(payload).unwrap();

        assert_eq!(object["temperature"], 0.7);
        assert!(object.get("top_p").is_none());
        assert_eq!(object["top_k"], 0);
        assert_eq!(object["repeat_penalty"], 1.0);
        assert_eq!(object["min_p"], 0.0);
        assert_eq!(object["repeat_last_n"], 0);
    }

    #[test]
    fn openai_compatible_omits_unsupported_profile_sampler_fields() {
        let settings = hymt_core::model_profile::ModelProfile::HyMt2_30bA3b.generation_defaults();
        let payload = ChatPayload::from_generation_settings(
            "test",
            1,
            String::new(),
            false,
            &settings,
            GenerationBackend::OpenAiCompatible,
        );
        let object = serde_json::to_value(payload).unwrap();

        assert_eq!(object["temperature"], 0.7);
        assert_eq!(object["top_p"], 1.0);
        for field in [
            "top_k",
            "repeat_penalty",
            "repetition_penalty",
            "min_p",
            "repeat_last_n",
        ] {
            assert!(object.get(field).is_none(), "{field} should be omitted");
        }
    }

    #[test]
    fn llama_cpp_payload_uses_repeat_penalty_wire_key() {
        let payload = ChatPayload::from_generation_settings(
            "test",
            1,
            String::new(),
            false,
            &GenerationSettings {
                temperature: Setting::ServerDefault,
                top_p: Setting::ServerDefault,
                top_k: Setting::ServerDefault,
                repetition_penalty: Setting::Value(1.05),
                min_p: Setting::ServerDefault,
                repeat_last_n: Setting::ServerDefault,
            },
            GenerationBackend::LlamaCpp,
        );

        let object = serde_json::to_value(payload).unwrap();
        assert_eq!(object["repeat_penalty"], 1.05);
        assert!(object.get("repetition_penalty").is_none());
    }

    #[test]
    fn vllm_payload_uses_repetition_penalty_wire_key() {
        let payload = ChatPayload::from_generation_settings(
            "test",
            1,
            String::new(),
            false,
            &GenerationSettings {
                temperature: Setting::ServerDefault,
                top_p: Setting::ServerDefault,
                top_k: Setting::ServerDefault,
                repetition_penalty: Setting::Value(1.05),
                min_p: Setting::ServerDefault,
                repeat_last_n: Setting::ServerDefault,
            },
            GenerationBackend::Vllm,
        );

        let object = serde_json::to_value(payload).unwrap();
        assert_eq!(object["repetition_penalty"], 1.05);
        assert!(object.get("repeat_penalty").is_none());
    }

    #[test]
    fn server_default_generation_settings_are_omitted_from_payload_json() {
        let payload = ChatPayload::from_generation_settings(
            "test",
            1,
            String::new(),
            false,
            &GenerationSettings::server_defaults(),
            GenerationBackend::LlamaCpp,
        );

        let json = serde_json::to_value(payload).unwrap();
        let object = json.as_object().unwrap();
        for field in [
            "temperature",
            "top_p",
            "top_k",
            "repeat_penalty",
            "repetition_penalty",
            "min_p",
            "repeat_last_n",
        ] {
            assert!(!object.contains_key(field), "{field} should be omitted");
        }
    }

    #[test]
    fn payload_omits_model_and_stream_when_none() {
        let payload = ChatPayload::from_generation_settings(
            "test",
            100,
            String::new(),
            false,
            &GenerationSettings::server_defaults(),
            GenerationBackend::LlamaCpp,
        );
        let json = serde_json::to_value(&payload).unwrap();
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("model"), "model should be absent");
        assert!(!obj.contains_key("stream"), "stream should be absent");
    }

    #[test]
    fn payload_includes_model_and_stream_when_set() {
        let payload = ChatPayload::from_generation_settings(
            "test",
            4096,
            "hy-mt2".into(),
            true,
            &GenerationSettings::server_defaults(),
            GenerationBackend::LlamaCpp,
        );
        let json = serde_json::to_value(&payload).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj["model"].as_str().unwrap(), "hy-mt2");
        assert!(obj["stream"].as_bool().unwrap());
    }

    #[test]
    fn payload_message_structure() {
        let payload = ChatPayload::from_generation_settings(
            "translate this",
            4096,
            String::new(),
            false,
            &GenerationSettings::server_defaults(),
            GenerationBackend::LlamaCpp,
        );
        let json = serde_json::to_value(&payload).unwrap();
        let messages = json["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"].as_str().unwrap(), "user");
        assert_eq!(messages[0]["content"].as_str().unwrap(), "translate this");
    }

    #[test]
    fn translation_chat_contract_has_one_user_message_and_generation_prefix() {
        let prompt = hymt_core::templates::build_prompt(
            "contract source",
            "zh",
            &hymt_core::templates::TemplateType::Default,
            &hymt_core::templates::PromptOpts::default(),
        )
        .unwrap();
        let payload = ChatPayload::from_generation_settings(
            &prompt,
            4096,
            "hy-mt2".into(),
            false,
            &GenerationSettings::server_defaults(),
            GenerationBackend::LlamaCpp,
        );
        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(
            json["messages"],
            serde_json::json!([{ "role": "user", "content": prompt }]),
            "translation must not add a default system message or mutate the prompt"
        );

        let rendered = hymt_core::model_profile::ModelProfile::HyMt2_7b
            .render_chat_user_prompt(json["messages"][0]["content"].as_str().unwrap())
            .expect("the Hy-MT2 7B chat template is pinned");
        assert_eq!(
            rendered,
            format!("<|startoftext|>{prompt}<|extra_0|>"),
            "the rendered request must retain both special tokens and the assistant generation prefix"
        );
        assert_eq!(rendered.matches("<|").count(), 2);
    }

    #[test]
    fn llama_cpp_props_diagnostics_use_the_server_root_and_report_literal_defaults() {
        assert_eq!(
            TranslationClient::llama_cpp_props_url("http://127.0.0.1:8401/v1/"),
            "http://127.0.0.1:8401/props"
        );

        let message = llama_cpp_props_default_message(
            "http://127.0.0.1:8401/props",
            &serde_json::json!({
                "default_generation_settings": {
                    "temperature": 0.7,
                    "top_p": 0.6,
                    "top_k": 20,
                    "repeat_penalty": 1.05
                }
            }),
        );
        assert!(message.contains("server sampling defaults"));
        assert!(message.contains("\"temperature\":0.7"));
        assert!(message.contains("\"repeat_penalty\":1.05"));
    }

    #[test]
    fn llama_cpp_props_diagnostics_fail_open_when_defaults_are_unavailable() {
        let message = llama_cpp_props_default_message(
            "http://127.0.0.1:8401/props",
            &serde_json::json!({"build": "old-server"}),
        );
        assert!(message.contains("did not expose default_generation_settings"));
        assert!(message.contains("continue omitting sampler fields"));
    }

    #[tokio::test]
    async fn llama_cpp_props_diagnostic_queries_props_without_affecting_requests() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let count = socket.read(&mut request).await.unwrap();
            let request = std::str::from_utf8(&request[..count]).unwrap();
            assert!(request.starts_with("GET /props HTTP/1.1"));
            assert!(request
                .lines()
                .any(|line| { line.eq_ignore_ascii_case("authorization: Bearer props-test-key") }));
            assert!(request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("content-type: application/json")));

            let body = r#"{"default_generation_settings":{"temperature":0.7,"top_p":0.6,"top_k":20,"repeat_penalty":1.05}}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let path = std::env::temp_dir().join(format!(
            "hymt-client-props-{}-{}.toml",
            std::process::id(),
            address.port()
        ));
        std::fs::write(
            &path,
            format!(
                "[endpoint]\nurl = \"http://{address}/v1\"\nbackend = \"llama_cpp\"\napi_key = \"props-test-key\"\n"
            ),
        )
        .unwrap();
        let config = HotConfig::from_path(&path).unwrap();
        let client = TranslationClient::new(config).unwrap();
        let diagnostic = client
            .llama_cpp_props_diagnostic()
            .await
            .expect("llama.cpp diagnostics");

        server.await.unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(diagnostic.contains("\"temperature\":0.7"));
        assert!(diagnostic.contains("\"repeat_penalty\":1.05"));
    }
}
