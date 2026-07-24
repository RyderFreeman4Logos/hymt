//! Hot-reloadable TOML configuration.
//!
//! Reads from `~/.config/hymt/config.toml`, creating it with embedded defaults
//! when absent. Call `maybe_reload()` to pick up on-disk changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::error::CoreError;
use crate::language::DocumentTranslationPolicy;
use crate::model_profile::{ModelProfile, UpstreamSource};
use crate::runtime::{BackendRuntimeInfo, BackendVerificationStatus};

pub const DEFAULT_CONFIG: &str = r#"[endpoint]
url = "http://127.0.0.1:8401/v1"
api_key = ""
model = ""
# Set this explicitly for a tested Hy-MT2 family member. When omitted, hymt
# uses generic mode and does not assume a tokenizer or sampler profile.
# profile = "hy_mt2_7b"
# Select the endpoint-specific sampler adapter. An omitted value is the strict
# openai_compatible adapter, which sends only common chat-completions fields.
backend = "llama_cpp"

[backend]
# `total_context` is the server-wide allocation (`llama-server -c`), while
# `per_request_context` is the guaranteed context for one request slot.
total_context = 16384
parallel_slots = 1
# Set per_request_context explicitly only when the backend guarantees a lower
# per-slot limit. When omitted, hymt derives total_context / parallel_slots.

[translation]
max_output_tokens = 4096
concurrency = 1
stream = true
config_version = 1
timeout = 600
first_chunk_priority = false
# Hard cap on source tokens submitted per segment. Prevents oversized single-segment
# hangs when the per-request context/max_output_tokens alone still leave a multi-k budget.
# Set to 0 to disable the hard cap (budget is then only expansion/context-limited).
max_source_tokens_per_segment = 1024
debug_chunk_timing = false
# Refuse planning when the active profile/tokenizer cannot count the final chat
# request locally. Default false keeps an explicitly warned conservative fallback.
strict_token_budget = false
# Refuse translation before cache lookup when backend runtime identity cannot be
# verified or materially differs from this configuration.
strict_backend_preflight = false
# Preserve high-confidence target-language paragraphs for Chinese-family targets.
language_detection = true
# Submit every non-code paragraph, including already-target-language paragraphs.
force_translate_all = false

[language]
primary = "zh"
secondary = "en"

[inference]
# Sampler values are omitted by default so the configured server selects them.
# A model profile is deployment guidance, not a source of request sampler values.
# Explicit overrides belong under `[inference.override]`; a numeric value is
# sent through the selected endpoint adapter and the string `"disabled"` turns
# off that sampler using the adapter's documented wire value.

[timing]
divergence_threshold = 2.0

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 2
# When false (default), top-level CLI translation exits non-zero after writing best
# attempt if any segment exhausted completeness retries. Set true (or pass
# --warn-only-completeness) to keep exit 0 with warnings only.
warn_only = false

[exec]
shared_cache_path = "/usr/local/share/hymt/cache.db"
translate_stderr = true
translate_stdout = true
skip_patterns = []
skip_commands = []

[exec.plugin]
blocklist = [
    "zstd", "gzip", "bzip2", "xz", "lz4", "rage", "age",
    "gpg", "openssl", "base64", "xxd", "od", "hexdump", "dd",
    "cp", "mv", "rsync", "docker", "podman", "hymt", "ssh", "scp"
]

[telegram]
enabled = false
bot_token = ""
claim_password = ""
owners = []
groups = []
mode = "owners"
"#;

const DEFAULT_BLOCKLIST: &[&str] = &[
    "zstd", "gzip", "bzip2", "xz", "lz4", "rage", "age", "gpg", "openssl", "base64", "xxd", "od",
    "hexdump", "dd", "cp", "mv", "rsync", "docker", "podman", "hymt", "ssh", "scp",
];

const GENERATION_SETTING_KEYS: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "repetition_penalty",
    "min_p",
    "repeat_last_n",
];

const BACKEND_CONTEXT_KEYS: &[&str] = &["total_context", "parallel_slots", "per_request_context"];

/// Canonical inference fingerprint schema. Increment this when its semantics change.
///
/// Version 1 writes `null` for quantization because the current configuration has
/// no quantization field; it never guesses an endpoint's loaded quant. Model and
/// tokenizer source identities are likewise `null` when generic mode cannot
/// provide them.
pub const INFERENCE_FINGERPRINT_SCHEMA_VERSION: u32 = 1;

const PROMPT_SCHEMA_VERSION: u32 = 1;

/// Stable inference identity used to isolate cache and history entries.
///
/// `canonical_json` is a recursively key-sorted JSON document and `hash` is its
/// SHA-256 hex digest. The raw JSON is intentionally retained for diagnostics and
/// schema audits, while callers should persist only `hash` in cache keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceFingerprint {
    canonical_json: String,
    hash: String,
    cache_verified: bool,
}

impl InferenceFingerprint {
    /// Canonical, versioned JSON used as the SHA-256 digest input.
    pub fn canonical_json(&self) -> &str {
        &self.canonical_json
    }

    /// Lowercase hexadecimal SHA-256 digest of [`Self::canonical_json`].
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Whether the served model and effective sampler identity are complete
    /// enough to safely reuse a segment-cache entry.
    pub fn is_cache_verified(&self) -> bool {
        self.cache_verified
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationSettingField {
    Temperature,
    TopP,
    TopK,
    RepetitionPenalty,
    MinP,
    RepeatLastN,
}

impl GenerationSettingField {
    const ALL: [Self; 6] = [
        Self::Temperature,
        Self::TopP,
        Self::TopK,
        Self::RepetitionPenalty,
        Self::MinP,
        Self::RepeatLastN,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Temperature => "temperature",
            Self::TopP => "top_p",
            Self::TopK => "top_k",
            Self::RepetitionPenalty => "repetition_penalty",
            Self::MinP => "min_p",
            Self::RepeatLastN => "repeat_last_n",
        }
    }
}

const LLAMA_CPP_CAPABILITIES: &[GenerationSettingField] = &GenerationSettingField::ALL;
const VLLM_CAPABILITIES: &[GenerationSettingField] = &[
    GenerationSettingField::Temperature,
    GenerationSettingField::TopP,
    GenerationSettingField::TopK,
    GenerationSettingField::RepetitionPenalty,
    GenerationSettingField::MinP,
];
const OPENAI_COMPATIBLE_CAPABILITIES: &[GenerationSettingField] = &[
    GenerationSettingField::Temperature,
    GenerationSettingField::TopP,
];

/// Semantic tri-state for a generation parameter.
///
/// `ServerDefault` deliberately differs from a numeric backend sentinel: it
/// means that the request must omit the field entirely.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Setting<T> {
    /// Omit the field and inherit the server or service default.
    #[default]
    ServerDefault,
    /// Explicitly disable the sampler through the selected backend adapter.
    Disabled,
    /// Send an explicit, backend-neutral value.
    Value(T),
}

/// Backend profile used only when converting semantic settings to wire values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationBackend {
    /// llama.cpp's OpenAI-compatible server.
    LlamaCpp,
    /// vLLM's OpenAI-compatible server and documented sampler extensions.
    Vllm,
    /// A generic OpenAI-compatible server.
    ///
    /// This conservative profile accepts only common `temperature` and `top_p`
    /// fields; it does not guess nonstandard extension names.
    OpenAiCompatible,
}

impl GenerationBackend {
    /// Stable configuration identifier for this endpoint adapter.
    pub const fn name(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama_cpp",
            Self::Vllm => "vllm",
            Self::OpenAiCompatible => "openai_compatible",
        }
    }

    fn capabilities(self) -> &'static [GenerationSettingField] {
        match self {
            Self::LlamaCpp => LLAMA_CPP_CAPABILITIES,
            Self::Vllm => VLLM_CAPABILITIES,
            Self::OpenAiCompatible => OPENAI_COMPATIBLE_CAPABILITIES,
        }
    }

    fn validate_settings(self, settings: &GenerationSettings) -> Result<(), CoreError> {
        for field in GenerationSettingField::ALL {
            if settings.is_explicit(field) && !self.capabilities().contains(&field) {
                return Err(CoreError::Config(format!(
                    "endpoint.backend {} cannot represent inference.override.{}; \
                     configured semantic value is {}; wire representation is unsupported",
                    self.name(),
                    field.name(),
                    settings.setting_description(field),
                )));
            }
        }
        Ok(())
    }
}

/// Backend-neutral generation configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationSettings {
    pub temperature: Setting<f64>,
    pub top_p: Setting<f64>,
    pub top_k: Setting<i32>,
    pub repetition_penalty: Setting<f64>,
    pub min_p: Setting<f64>,
    pub repeat_last_n: Setting<i64>,
}

impl GenerationSettings {
    /// Omit every sampler parameter so the server supplies its own default.
    pub const fn server_defaults() -> Self {
        Self {
            temperature: Setting::ServerDefault,
            top_p: Setting::ServerDefault,
            top_k: Setting::ServerDefault,
            repetition_penalty: Setting::ServerDefault,
            min_p: Setting::ServerDefault,
            repeat_last_n: Setting::ServerDefault,
        }
    }

    /// Whether every sampler is omitted from the request payload.
    pub fn uses_only_server_defaults(&self) -> bool {
        matches!(
            self,
            Self {
                temperature: Setting::ServerDefault,
                top_p: Setting::ServerDefault,
                top_k: Setting::ServerDefault,
                repetition_penalty: Setting::ServerDefault,
                min_p: Setting::ServerDefault,
                repeat_last_n: Setting::ServerDefault,
            }
        )
    }

    /// Whether any sampler remains omitted from the request payload.
    pub fn uses_any_server_defaults(&self) -> bool {
        matches!(self.temperature, Setting::ServerDefault)
            || matches!(self.top_p, Setting::ServerDefault)
            || matches!(self.top_k, Setting::ServerDefault)
            || matches!(self.repetition_penalty, Setting::ServerDefault)
            || matches!(self.min_p, Setting::ServerDefault)
            || matches!(self.repeat_last_n, Setting::ServerDefault)
    }

    /// Remove explicit sampler settings which the selected adapter cannot put
    /// on the wire. Explicit unsupported overrides are rejected before this
    /// normalization, so this keeps the fingerprint aligned with serialized
    /// request fields without inventing service-owned defaults.
    fn normalized_for_backend(&self, backend: GenerationBackend) -> Self {
        let supports = |field| backend.capabilities().contains(&field);
        Self {
            temperature: setting_if_supported(
                supports(GenerationSettingField::Temperature),
                self.temperature,
            ),
            top_p: setting_if_supported(supports(GenerationSettingField::TopP), self.top_p),
            top_k: setting_if_supported(supports(GenerationSettingField::TopK), self.top_k),
            repetition_penalty: setting_if_supported(
                supports(GenerationSettingField::RepetitionPenalty),
                self.repetition_penalty,
            ),
            min_p: setting_if_supported(supports(GenerationSettingField::MinP), self.min_p),
            repeat_last_n: setting_if_supported(
                supports(GenerationSettingField::RepeatLastN),
                self.repeat_last_n,
            ),
        }
    }

    fn setting_description(&self, field: GenerationSettingField) -> String {
        match field {
            GenerationSettingField::Temperature => format!("{:?}", self.temperature),
            GenerationSettingField::TopP => format!("{:?}", self.top_p),
            GenerationSettingField::TopK => format!("{:?}", self.top_k),
            GenerationSettingField::RepetitionPenalty => {
                format!("{:?}", self.repetition_penalty)
            }
            GenerationSettingField::MinP => format!("{:?}", self.min_p),
            GenerationSettingField::RepeatLastN => format!("{:?}", self.repeat_last_n),
        }
    }

    fn from_toml(data: &toml::Table) -> Result<Self, CoreError> {
        let settings = Self {
            temperature: parse_f64_setting(data, "temperature")?,
            top_p: parse_f64_setting(data, "top_p")?,
            top_k: parse_i32_setting(data, "top_k")?,
            repetition_penalty: parse_f64_setting(data, "repetition_penalty")?,
            min_p: parse_f64_setting(data, "min_p")?,
            repeat_last_n: parse_i64_setting(data, "repeat_last_n")?,
        };
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<(), CoreError> {
        validate_f64_range("temperature", &self.temperature, 0.0, 2.0)?;
        validate_f64_range("top_p", &self.top_p, 0.0, 1.0)?;
        validate_f64_range("min_p", &self.min_p, 0.0, 1.0)?;

        if let Setting::Value(value) = self.repetition_penalty {
            if !value.is_finite() || value <= 0.0 {
                return Err(CoreError::Config(
                    "inference.override.repetition_penalty must be finite and greater than 0"
                        .to_owned(),
                ));
            }
        }
        if let Setting::Value(value) = self.top_k {
            if value < -1 {
                return Err(CoreError::Config(
                    "inference.override.top_k must be at least -1".to_owned(),
                ));
            }
        }
        if let Setting::Value(value) = self.repeat_last_n {
            if value < -1 {
                return Err(CoreError::Config(
                    "inference.override.repeat_last_n must be at least -1".to_owned(),
                ));
            }
        }

        if matches!(self.temperature, Setting::Disabled)
            && (matches!(self.top_p, Setting::Value(_))
                || matches!(self.top_k, Setting::Value(_))
                || matches!(self.min_p, Setting::Value(_)))
        {
            return Err(CoreError::Config(
                "inference.override.temperature is disabled, so top_p, top_k, and min_p must not be explicit values"
                    .to_owned(),
            ));
        }
        if matches!(self.repetition_penalty, Setting::Disabled)
            && matches!(self.repeat_last_n, Setting::Value(_))
        {
            return Err(CoreError::Config(
                "inference.override.repetition_penalty is disabled, so repeat_last_n must not be an explicit value"
                    .to_owned(),
            ));
        }

        Ok(())
    }

    fn is_explicit(&self, field: GenerationSettingField) -> bool {
        match field {
            GenerationSettingField::Temperature => {
                !matches!(self.temperature, Setting::ServerDefault)
            }
            GenerationSettingField::TopP => !matches!(self.top_p, Setting::ServerDefault),
            GenerationSettingField::TopK => !matches!(self.top_k, Setting::ServerDefault),
            GenerationSettingField::RepetitionPenalty => {
                !matches!(self.repetition_penalty, Setting::ServerDefault)
            }
            GenerationSettingField::MinP => !matches!(self.min_p, Setting::ServerDefault),
            GenerationSettingField::RepeatLastN => {
                !matches!(self.repeat_last_n, Setting::ServerDefault)
            }
        }
    }
}

fn setting_if_supported<T>(supported: bool, setting: Setting<T>) -> Setting<T> {
    if supported {
        setting
    } else {
        Setting::ServerDefault
    }
}

fn canonical_object(
    fields: impl IntoIterator<Item = (String, serde_json::Value)>,
) -> serde_json::Value {
    let sorted: BTreeMap<String, serde_json::Value> = fields.into_iter().collect();
    serde_json::Value::Object(sorted.into_iter().collect())
}

fn string_or_null(value: String) -> serde_json::Value {
    if value.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(value)
    }
}

fn source_fingerprint_value(source: Option<&UpstreamSource>) -> serde_json::Value {
    match source {
        Some(source) => canonical_object([
            (
                "repo".to_owned(),
                serde_json::Value::String(source.repo.to_owned()),
            ),
            (
                "revision".to_owned(),
                serde_json::Value::String(source.revision.to_owned()),
            ),
        ]),
        None => serde_json::Value::Null,
    }
}

fn explicit_f64_setting(setting: Setting<f64>) -> Option<serde_json::Value> {
    match setting {
        Setting::ServerDefault => None,
        Setting::Disabled => Some(serde_json::Value::String("disabled".to_owned())),
        Setting::Value(value) => Some(serde_json::Value::from(value)),
    }
}

fn explicit_i32_setting(setting: Setting<i32>) -> Option<serde_json::Value> {
    match setting {
        Setting::ServerDefault => None,
        Setting::Disabled => Some(serde_json::Value::String("disabled".to_owned())),
        Setting::Value(value) => Some(serde_json::Value::from(value)),
    }
}

fn explicit_i64_setting(setting: Setting<i64>) -> Option<serde_json::Value> {
    match setting {
        Setting::ServerDefault => None,
        Setting::Disabled => Some(serde_json::Value::String("disabled".to_owned())),
        Setting::Value(value) => Some(serde_json::Value::from(value)),
    }
}

fn generation_fingerprint_value(settings: &GenerationSettings) -> serde_json::Value {
    let mut fields = BTreeMap::new();
    for (name, value) in [
        ("temperature", explicit_f64_setting(settings.temperature)),
        ("top_p", explicit_f64_setting(settings.top_p)),
        (
            "repetition_penalty",
            explicit_f64_setting(settings.repetition_penalty),
        ),
        ("min_p", explicit_f64_setting(settings.min_p)),
    ] {
        if let Some(value) = value {
            fields.insert(name.to_owned(), value);
        }
    }
    for (name, value) in [
        ("top_k", explicit_i32_setting(settings.top_k)),
        (
            "repeat_last_n",
            explicit_i64_setting(settings.repeat_last_n),
        ),
    ] {
        if let Some(value) = value {
            fields.insert(name.to_owned(), value);
        }
    }
    canonical_object(fields)
}

/// Runtime identity excludes probe-local timing and transport diagnostics: neither
/// changes a model response, while version/build/model/capability changes do.
fn runtime_fingerprint_value(info: &BackendRuntimeInfo) -> serde_json::Value {
    match serde_json::to_value(info) {
        Ok(serde_json::Value::Object(mut fields)) => {
            fields.remove("observed_at_unix_secs");
            fields.remove("verification_message");
            serde_json::Value::Object(fields)
        }
        Ok(value) => value,
        Err(_) => serde_json::Value::Null,
    }
}

fn validate_f64_range(
    field: &str,
    setting: &Setting<f64>,
    min: f64,
    max: f64,
) -> Result<(), CoreError> {
    if let Setting::Value(value) = setting {
        if !value.is_finite() || *value < min || *value > max {
            return Err(CoreError::Config(format!(
                "inference.override.{field} must be finite and in [{min}, {max}]"
            )));
        }
    }
    Ok(())
}

fn inference_value(data: &toml::Table, key: &str) -> Result<Option<toml::Value>, CoreError> {
    let Some(inference) = data.get("inference") else {
        return Ok(None);
    };
    let table = inference
        .as_table()
        .ok_or_else(|| CoreError::Config("inference must be a TOML table".to_owned()))?;
    if let Some(overrides) = table.get("override") {
        let overrides = overrides.as_table().ok_or_else(|| {
            CoreError::Config("inference.override must be a TOML table".to_owned())
        })?;
        if let Some(value) = overrides.get(key) {
            return Ok(Some(value.clone()));
        }
    }
    Ok(table.get(key).cloned())
}

fn uses_legacy_generation_scalars(data: &toml::Table) -> bool {
    data.get("inference")
        .and_then(toml::Value::as_table)
        .is_some_and(|table| {
            GENERATION_SETTING_KEYS
                .iter()
                .any(|key| table.contains_key(*key))
        })
}

fn uses_legacy_context_window(data: &toml::Table) -> bool {
    let uses_context_window = data
        .get("translation")
        .and_then(toml::Value::as_table)
        .is_some_and(|table| table.contains_key("context_window"));
    let has_backend_context = data
        .get("backend")
        .and_then(toml::Value::as_table)
        .is_some_and(|table| {
            BACKEND_CONTEXT_KEYS
                .iter()
                .any(|key| table.contains_key(*key))
        });
    uses_context_window && !has_backend_context
}

fn validate_backend_context(data: &toml::Table) -> Result<(), CoreError> {
    let Some(backend) = data.get("backend") else {
        return Ok(());
    };
    let backend = backend
        .as_table()
        .ok_or_else(|| CoreError::Config("backend must be a TOML table".to_owned()))?;

    for key in BACKEND_CONTEXT_KEYS {
        let Some(value) = backend.get(*key) else {
            continue;
        };
        let value = value.as_integer().ok_or_else(|| {
            CoreError::Config(format!(
                "backend.{key} must be an integer in 1..={}",
                u32::MAX
            ))
        })?;
        if !(1..=i64::from(u32::MAX)).contains(&value) {
            return Err(CoreError::Config(format!(
                "backend.{key} must be an integer in 1..={}",
                u32::MAX
            )));
        }
    }

    let total_context = backend
        .get("total_context")
        .and_then(toml::Value::as_integer)
        .map(|value| value as u32)
        .or_else(|| {
            data.get("translation")
                .and_then(toml::Value::as_table)
                .and_then(|translation| translation.get("context_window"))
                .and_then(toml::Value::as_integer)
                .filter(|&value| value > 0)
                .and_then(|value| u32::try_from(value).ok())
        })
        .unwrap_or(16_384);
    let parallel_slots = backend
        .get("parallel_slots")
        .and_then(toml::Value::as_integer)
        .map(|value| value as u32)
        .unwrap_or(1);
    let slot_capacity = total_context / parallel_slots;

    if parallel_slots > total_context {
        return Err(CoreError::Config(format!(
            "backend.parallel_slots ({parallel_slots}) must not exceed backend.total_context \
             ({total_context}); computed per-slot capacity is {slot_capacity}"
        )));
    }

    if let Some(per_request_context) = backend
        .get("per_request_context")
        .and_then(toml::Value::as_integer)
        .map(|value| value as u32)
    {
        if per_request_context > slot_capacity {
            return Err(CoreError::Config(format!(
                "backend.per_request_context ({per_request_context}) must not exceed computed \
                 per-slot capacity ({slot_capacity}) from backend.total_context ({total_context}) \
                 / backend.parallel_slots ({parallel_slots})"
            )));
        }
    }

    Ok(())
}
fn parse_f64_setting(data: &toml::Table, key: &str) -> Result<Setting<f64>, CoreError> {
    match inference_value(data, key)? {
        None => Ok(Setting::ServerDefault),
        Some(toml::Value::String(value)) if value == "disabled" => Ok(Setting::Disabled),
        Some(toml::Value::Float(value)) => Ok(Setting::Value(value)),
        Some(toml::Value::Integer(value)) => Ok(Setting::Value(value as f64)),
        Some(_) => Err(CoreError::Config(format!(
            "inference.override.{key} must be a number or \"disabled\""
        ))),
    }
}

fn parse_i32_setting(data: &toml::Table, key: &str) -> Result<Setting<i32>, CoreError> {
    match inference_value(data, key)? {
        None => Ok(Setting::ServerDefault),
        Some(toml::Value::String(value)) if value == "disabled" => Ok(Setting::Disabled),
        Some(toml::Value::Integer(value)) => {
            i32::try_from(value).map(Setting::Value).map_err(|_| {
                CoreError::Config(format!("inference.override.{key} is outside i32 range"))
            })
        }
        Some(_) => Err(CoreError::Config(format!(
            "inference.override.{key} must be an integer or \"disabled\""
        ))),
    }
}

fn parse_i64_setting(data: &toml::Table, key: &str) -> Result<Setting<i64>, CoreError> {
    match inference_value(data, key)? {
        None => Ok(Setting::ServerDefault),
        Some(toml::Value::String(value)) if value == "disabled" => Ok(Setting::Disabled),
        Some(toml::Value::Integer(value)) => Ok(Setting::Value(value)),
        Some(_) => Err(CoreError::Config(format!(
            "inference.override.{key} must be an integer or \"disabled\""
        ))),
    }
}

fn generation_backend_from_toml(data: &toml::Table) -> Result<GenerationBackend, CoreError> {
    if data
        .get("inference")
        .and_then(toml::Value::as_table)
        .is_some_and(|table| table.contains_key("backend"))
    {
        return Err(CoreError::Config(
            "inference.backend is no longer accepted; set endpoint.backend to \
             llama_cpp, vllm, or openai_compatible"
                .to_owned(),
        ));
    }

    let Some(endpoint) = data.get("endpoint") else {
        return Ok(GenerationBackend::OpenAiCompatible);
    };
    let table = endpoint
        .as_table()
        .ok_or_else(|| CoreError::Config("endpoint must be a TOML table".to_owned()))?;
    match table.get("backend") {
        None => Ok(GenerationBackend::OpenAiCompatible),
        Some(toml::Value::String(value)) => match value.as_str() {
            "llama_cpp" => Ok(GenerationBackend::LlamaCpp),
            "vllm" => Ok(GenerationBackend::Vllm),
            "openai_compatible" => Ok(GenerationBackend::OpenAiCompatible),
            _ => Err(CoreError::Config(format!(
                "endpoint.backend must be llama_cpp, vllm, or openai_compatible, got {value:?}"
            ))),
        },
        Some(value) => Err(CoreError::Config(format!(
            "endpoint.backend must be llama_cpp, vllm, or openai_compatible, got {value:?}"
        ))),
    }
}

fn model_profile_from_toml(data: &toml::Table) -> Result<ModelProfile, CoreError> {
    let Some(endpoint) = data.get("endpoint") else {
        return Ok(ModelProfile::Generic);
    };
    let table = endpoint
        .as_table()
        .ok_or_else(|| CoreError::Config("endpoint must be a TOML table".to_owned()))?;
    match table.get("profile") {
        None => Ok(ModelProfile::Generic),
        Some(toml::Value::String(value)) => ModelProfile::parse(value).ok_or_else(|| {
            CoreError::Config(format!(
                "endpoint.profile must be hy_mt2_1_8b, hy_mt2_7b, hy_mt2_30b_a3b, or generic, got {value:?}"
            ))
        }),
        Some(value) => Err(CoreError::Config(format!(
            "endpoint.profile must be a string, got {value:?}"
        ))),
    }
}

fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    home.join(".config").join("hymt").join("config.toml")
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(s)
    }
}

fn env_flag_enabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return false;
            }
            !matches!(
                trimmed.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        }
        Err(_) => false,
    }
}

#[derive(Debug)]
struct ConfigState {
    data: toml::Table,
    mtime: Option<SystemTime>,
    profile: Option<ModelProfile>,
    uses_legacy_generation_scalars: bool,
    uses_legacy_context_window: bool,
    backend_runtime_info: Option<BackendRuntimeInfo>,
}

impl ConfigState {
    fn empty() -> Self {
        Self {
            data: toml::Table::new(),
            mtime: None,
            profile: None,
            uses_legacy_generation_scalars: false,
            uses_legacy_context_window: false,
            backend_runtime_info: None,
        }
    }
}

/// Hot-reloadable TOML configuration.
///
/// `[endpoint].profile` is pinned when this object is created so a running
/// session cannot mix a startup tokenizer with reloaded model defaults.
#[derive(Debug, Clone)]
pub struct HotConfig {
    path: PathBuf,
    state: Arc<RwLock<ConfigState>>,
}

impl HotConfig {
    /// Opens the default config (`~/.config/hymt/config.toml`), creating it if absent.
    pub fn new() -> Result<Self, CoreError> {
        Self::from_path(default_config_path())
    }

    /// Opens the config at `path`, creating it if absent.
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let path = path.into();
        let cfg = Self {
            path,
            state: Arc::new(RwLock::new(ConfigState::empty())),
        };
        cfg.ensure_exists()?;
        cfg.load_from_disk()?;
        Ok(cfg)
    }

    /// Path of the underlying config file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reloads from disk if the file's mtime changed since the last load.
    ///
    /// Returns `true` when a reload occurred.
    pub fn maybe_reload(&self) -> Result<bool, CoreError> {
        let current_mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        {
            let state = self.state.read().unwrap();
            if state.mtime == current_mtime {
                return Ok(false);
            }
        }
        self.load_from_disk()?;
        Ok(true)
    }

    /// Returns the raw text of the config file (reloads if modified).
    pub fn show(&self) -> Result<String, CoreError> {
        self.maybe_reload()?;
        std::fs::read_to_string(&self.path).map_err(CoreError::Io)
    }

    // ── endpoint ────────────────────────────────────────────────────────────

    pub fn endpoint_url(&self) -> String {
        let s = self.get_str("endpoint", "url", "http://127.0.0.1:8401/v1");
        s.trim_end_matches('/').to_owned()
    }

    pub fn api_key(&self) -> String {
        self.get_str("endpoint", "api_key", "")
    }

    pub fn model(&self) -> String {
        self.get_str("endpoint", "model", "")
    }

    /// Explicit Hy-MT2 profile selected when this config session started.
    ///
    /// Profile changes on disk require a new [`HotConfig`] and are intentionally
    /// ignored by hot reload so the tokenizer and generation defaults stay aligned.
    pub fn model_profile(&self) -> Result<ModelProfile, CoreError> {
        Ok(self
            .state
            .read()
            .unwrap()
            .profile
            .unwrap_or(ModelProfile::Generic))
    }

    // ── translation ─────────────────────────────────────────────────────────

    /// Legacy context setting retained as the fallback total backend context.
    pub fn context_window(&self) -> u32 {
        self.get_positive_u32("translation", "context_window", 16384)
    }

    /// Total server context allocation, such as llama.cpp's `-c` value.
    pub fn total_context(&self) -> u32 {
        self.get_positive_u32("backend", "total_context", self.context_window())
    }

    /// Number of server request slots that share [`Self::total_context`].
    pub fn parallel_slots(&self) -> u32 {
        self.get_positive_u32("backend", "parallel_slots", 1)
    }

    /// Guaranteed context available to one translation request.
    ///
    /// An explicit backend value wins; otherwise this is derived from the total
    /// server allocation divided by its parallel request slots.
    pub fn per_request_context(&self) -> u32 {
        self.section_value("backend", "per_request_context")
            .and_then(|value| value.as_integer())
            .filter(|&value| value > 0)
            .map(|value| value as u32)
            .unwrap_or_else(|| self.total_context() / self.parallel_slots())
    }

    /// Latest runtime snapshot recorded by a backend preflight, if one was run.
    ///
    /// This is in-memory process state only; it never persists service metadata or
    /// credentials into `config.toml`.
    pub fn backend_runtime_info(&self) -> Option<BackendRuntimeInfo> {
        self.state.read().unwrap().backend_runtime_info.clone()
    }

    /// Replace the runtime facts used by planner, request, and fingerprint resolution.
    pub fn set_backend_runtime_info(&self, info: BackendRuntimeInfo) {
        self.state.write().unwrap().backend_runtime_info = Some(info);
    }

    /// Clear stale runtime state after an endpoint/profile transition.
    pub fn clear_backend_runtime_info(&self) {
        self.state.write().unwrap().backend_runtime_info = None;
    }

    /// Context available to the planner after applying verified service limits.
    ///
    /// An unavailable preflight uses a deliberately conservative 4k context
    /// rather than promoting the configured maximum as verified service state.
    pub fn resolved_per_request_context(&self) -> u32 {
        let configured = self.per_request_context();
        let backend = self.generation_backend().ok();
        let runtime = self.backend_runtime_info();
        match runtime {
            Some(info) if Some(info.backend) == backend => match info.verification_status {
                BackendVerificationStatus::Verified => info
                    .per_slot_context
                    .map(|runtime_limit| configured.min(runtime_limit))
                    .unwrap_or(configured),
                BackendVerificationStatus::Unverified => configured.min(4_096),
            },
            _ => configured,
        }
    }

    /// Whether the deprecated `translation.context_window` needs migration to
    /// the separate backend context settings.
    pub fn uses_legacy_context_window(&self) -> bool {
        self.state.read().unwrap().uses_legacy_context_window
    }

    pub fn max_output_tokens(&self) -> u32 {
        let default = self
            .model_profile()
            .map(|profile| profile.recommended_max_output_tokens())
            .unwrap_or(4_096);
        self.get_positive_u32("translation", "max_output_tokens", default)
    }

    /// Output reservation after applying a verified server generation cap.
    pub fn resolved_max_output_tokens(&self) -> u32 {
        let configured = self.max_output_tokens();
        let backend = self.generation_backend().ok();
        let runtime = self.backend_runtime_info();
        match runtime {
            Some(info) if Some(info.backend) == backend => match info.verification_status {
                BackendVerificationStatus::Verified => info
                    .default_max_generation_tokens
                    .map(|runtime_limit| configured.min(runtime_limit))
                    .unwrap_or(configured),
                BackendVerificationStatus::Unverified => configured.min(1_024),
            },
            _ => configured,
        }
    }

    pub fn concurrency(&self) -> u32 {
        self.get_positive_u32("translation", "concurrency", 1)
    }

    pub fn stream(&self) -> bool {
        self.get_bool("translation", "stream", true)
    }

    pub fn config_version(&self) -> u32 {
        self.get_positive_u32("translation", "config_version", 1)
    }

    pub fn timeout(&self) -> f64 {
        self.get_number_as_f64("translation", "timeout", 600.0)
    }

    /// When true, chunk 0 is translated alone before remaining chunks run in parallel.
    ///
    /// This gives the first output segment priority GPU access so callers can
    /// display it immediately while the rest of the document is still being
    /// translated.  Defaults to `false` (all missing chunks are parallelised).
    pub fn first_chunk_priority(&self) -> bool {
        self.get_bool("translation", "first_chunk_priority", false)
    }

    /// Hard upper bound on source tokens per translation segment.
    ///
    /// Caps the expansion/context-derived budget so multi-k documents always
    /// split instead of hanging as one oversized request. Defaults to `1024`.
    /// `0` disables the hard cap (budget is only expansion/context-limited).
    pub fn max_source_tokens_per_segment(&self) -> u32 {
        self.get_non_negative_u32("translation", "max_source_tokens_per_segment", 1024)
    }

    /// When true, emit per-chunk pipeline timestamps on stderr.
    ///
    /// Also enabled when the environment variable `HYMT_DEBUG_CHUNK_TIMING` is
    /// set to a non-empty value other than `0`/`false`/`no`/`off` (case-insensitive).
    pub fn debug_chunk_timing(&self) -> bool {
        if env_flag_enabled("HYMT_DEBUG_CHUNK_TIMING") {
            return true;
        }
        self.get_bool("translation", "debug_chunk_timing", false)
    }

    /// Refuse request plans that cannot count the profiled chat template with a
    /// local tokenizer. This makes unknown/generic endpoints fail closed rather
    /// than use the explicitly warned conservative approximation.
    pub fn strict_token_budget(&self) -> bool {
        self.get_bool("translation", "strict_token_budget", false)
    }

    /// Refuse translation when preflight cannot attest the selected runtime.
    pub fn strict_backend_preflight(&self) -> bool {
        self.get_bool("translation", "strict_backend_preflight", false)
    }

    /// Selects whether document planning detects and preserves already-target text.
    pub fn document_translation_policy(&self) -> DocumentTranslationPolicy {
        if self.get_bool("translation", "force_translate_all", false)
            || !self.get_bool("translation", "language_detection", true)
        {
            DocumentTranslationPolicy::TranslateAll
        } else {
            DocumentTranslationPolicy::SkipHighConfidenceTargetParagraphs
        }
    }

    // ── language ────────────────────────────────────────────────────────────

    pub fn primary_lang(&self) -> String {
        self.get_str("language", "primary", "zh")
    }

    pub fn secondary_lang(&self) -> String {
        self.get_str("language", "secondary", "en")
    }

    // ── inference ───────────────────────────────────────────────────────────

    /// Reads and validates explicit backend-neutral generation overrides.
    ///
    /// An absent sampler key remains [`Setting::ServerDefault`], so it is not
    /// serialized and the selected inference service owns its default. Model
    /// profile sampling guidance is intentionally not overlaid here.
    pub fn generation_settings(&self) -> Result<GenerationSettings, CoreError> {
        let state = self.state.read().unwrap();
        let overrides = GenerationSettings::from_toml(&state.data)?;
        generation_backend_from_toml(&state.data)?.validate_settings(&overrides)?;
        Ok(overrides)
    }

    /// Backend profile used to map [`GenerationSettings`] to request fields.
    pub fn generation_backend(&self) -> Result<GenerationBackend, CoreError> {
        let state = self.state.read().unwrap();
        generation_backend_from_toml(&state.data)
    }

    /// Build the complete, normalized identity for a translation request.
    ///
    /// This includes every currently configured request field that can change a
    /// translation: endpoint, backend adapter, configured model/GGUF alias,
    /// profile model/tokenizer sources, effective non-default samplers, output
    /// limit, completeness retry/validation policy, and prompt template/options.
    /// API keys are deliberately excluded.
    /// Fields unavailable to the current configuration are represented as JSON
    /// `null` in schema version 1 rather than guessed.
    pub fn inference_fingerprint(
        &self,
        template_type: &str,
        options_hash: &str,
    ) -> Result<InferenceFingerprint, CoreError> {
        let profile = self.model_profile()?;
        let settings = self.generation_settings()?;
        let backend = self.generation_backend()?;
        let effective_settings = settings.normalized_for_backend(backend);
        let model = self.model();
        let runtime = self
            .backend_runtime_info()
            .filter(|info| info.backend == backend);
        let configured_model_matches_runtime = runtime.as_ref().is_none_or(|info| {
            model.is_empty()
                || info
                    .served_model
                    .as_deref()
                    .is_none_or(|served| served == model)
        });
        let service_defaults_are_known = !effective_settings.uses_any_server_defaults()
            || runtime
                .as_ref()
                .is_some_and(|info| info.sampler_defaults.is_complete());
        // A failed preflight explicitly marks the identity unverified. Existing
        // explicit-override callers retain their pre-preflight deterministic
        // namespace, while translation paths run preflight before cache lookup.
        let cache_verified = configured_model_matches_runtime
            && service_defaults_are_known
            && !runtime.as_ref().is_some_and(|info| {
                info.verification_status == BackendVerificationStatus::Unverified
            })
            && !(profile.is_generic()
                && model.is_empty()
                && runtime
                    .as_ref()
                    .and_then(|info| info.served_model.as_ref())
                    .is_none());

        let mut fields = BTreeMap::new();
        fields.insert(
            "backend".to_owned(),
            serde_json::Value::String(backend.name().to_owned()),
        );
        fields.insert(
            "completeness".to_owned(),
            canonical_object([
                (
                    "en_to_zh_min_ratio".to_owned(),
                    serde_json::Value::from(self.completeness_en_to_zh_min_ratio()),
                ),
                (
                    "max_retries".to_owned(),
                    serde_json::Value::from(self.completeness_max_retries()),
                ),
                (
                    "min_paragraph_ratio".to_owned(),
                    serde_json::Value::from(self.completeness_min_paragraph_ratio()),
                ),
                (
                    "zh_to_en_min_ratio".to_owned(),
                    serde_json::Value::from(self.completeness_zh_to_en_min_ratio()),
                ),
            ]),
        );
        fields.insert(
            "endpoint_url".to_owned(),
            serde_json::Value::String(self.endpoint_url()),
        );
        fields.insert(
            "generation".to_owned(),
            generation_fingerprint_value(&effective_settings),
        );
        fields.insert(
            "model".to_owned(),
            canonical_object([
                ("configured_alias".to_owned(), string_or_null(model)),
                (
                    "upstream_source".to_owned(),
                    source_fingerprint_value(profile.model()),
                ),
            ]),
        );
        fields.insert(
            "profile_id".to_owned(),
            serde_json::Value::String(profile.id().to_owned()),
        );
        fields.insert(
            "prompt".to_owned(),
            canonical_object([
                (
                    "options_hash".to_owned(),
                    serde_json::Value::String(options_hash.to_owned()),
                ),
                (
                    "schema_version".to_owned(),
                    serde_json::Value::from(PROMPT_SCHEMA_VERSION),
                ),
                (
                    "template_type".to_owned(),
                    serde_json::Value::String(template_type.to_owned()),
                ),
            ]),
        );
        fields.insert("quantization".to_owned(), serde_json::Value::Null);
        fields.insert(
            "request".to_owned(),
            canonical_object([(
                "max_output_tokens".to_owned(),
                serde_json::Value::from(self.resolved_max_output_tokens()),
            )]),
        );
        fields.insert(
            "runtime".to_owned(),
            runtime
                .as_ref()
                .map(runtime_fingerprint_value)
                .unwrap_or(serde_json::Value::Null),
        );
        fields.insert(
            "schema_version".to_owned(),
            serde_json::Value::from(INFERENCE_FINGERPRINT_SCHEMA_VERSION),
        );
        fields.insert(
            "tokenizer".to_owned(),
            source_fingerprint_value(profile.tokenizer()),
        );

        let canonical_json = serde_json::to_string(&fields).map_err(|error| {
            CoreError::Config(format!("serializing inference fingerprint: {error}"))
        })?;
        let hash = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
        Ok(InferenceFingerprint {
            canonical_json,
            hash,
            cache_verified,
        })
    }

    /// Whether this config uses deprecated scalar sampler keys in `[inference]`.
    pub fn uses_legacy_generation_scalars(&self) -> bool {
        self.state.read().unwrap().uses_legacy_generation_scalars
    }

    // ── timing ──────────────────────────────────────────────────────────────

    /// Minimum ratio threshold for divergence detection (must be > 1.0; defaults to 2.0).
    pub fn timing_divergence_threshold(&self) -> f64 {
        let v = self.get_number_as_f64("timing", "divergence_threshold", 2.0);
        if v > 1.0 {
            v
        } else {
            2.0
        }
    }

    // ── completeness ────────────────────────────────────────────────────────

    pub fn completeness_zh_to_en_min_ratio(&self) -> f64 {
        let v = self.get_number_as_f64("completeness", "zh_to_en_min_ratio", 0.3);
        if v > 0.0 {
            v
        } else {
            0.3
        }
    }

    pub fn completeness_en_to_zh_min_ratio(&self) -> f64 {
        let v = self.get_number_as_f64("completeness", "en_to_zh_min_ratio", 0.3);
        if v > 0.0 {
            v
        } else {
            0.3
        }
    }

    pub fn completeness_min_paragraph_ratio(&self) -> f64 {
        let v = self.get_number_as_f64("completeness", "min_paragraph_ratio", 0.5);
        if v > 0.0 {
            v
        } else {
            0.5
        }
    }

    pub fn completeness_max_retries(&self) -> u32 {
        self.get_non_negative_u32("completeness", "max_retries", 2)
    }

    /// When true, completeness best-attempt fallback only warns (exit 0).
    ///
    /// Default is `false`: top-level CLI translation reports non-success after
    /// writing the best-effort output so scripts can detect degraded results.
    pub fn completeness_warn_only(&self) -> bool {
        self.get_bool("completeness", "warn_only", false)
    }

    // ── exec ────────────────────────────────────────────────────────────────

    pub fn exec_shared_cache_path(&self) -> PathBuf {
        let s = self.get_str(
            "exec",
            "shared_cache_path",
            "/usr/local/share/hymt/cache.db",
        );
        expand_tilde(&s)
    }

    pub fn exec_translate_stderr(&self) -> bool {
        self.get_bool("exec", "translate_stderr", true)
    }

    /// Returns the `translate_stdout` setting; `"auto"` is treated as `true`.
    pub fn exec_translate_stdout(&self) -> bool {
        let state = self.state.read().unwrap();
        match state
            .data
            .get("exec")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("translate_stdout"))
        {
            Some(toml::Value::Boolean(b)) => *b,
            Some(toml::Value::String(s)) if s == "auto" => true,
            _ => true,
        }
    }

    pub fn exec_skip_patterns(&self) -> Vec<String> {
        self.get_string_vec("exec", "skip_patterns")
    }

    pub fn exec_skip_commands(&self) -> Vec<String> {
        self.get_string_vec("exec", "skip_commands")
    }

    pub fn exec_plugin_blocklist(&self) -> Vec<String> {
        let state = self.state.read().unwrap();
        state
            .data
            .get("exec")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("plugin"))
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("blocklist"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| DEFAULT_BLOCKLIST.iter().map(|s| s.to_string()).collect())
    }

    // ── telegram ────────────────────────────────────────────────────────────

    /// Whether the Telegram bot is enabled (`false` until the user opts in).
    pub fn telegram_enabled(&self) -> bool {
        self.get_bool("telegram", "enabled", false)
    }

    /// Bot token from config, or empty when unset.
    ///
    /// Callers should prefer [`Self::telegram_bot_token_resolved`] so the
    /// `HYMT_TELEGRAM_BOT_TOKEN` environment variable can override config.
    pub fn telegram_bot_token(&self) -> String {
        self.get_str("telegram", "bot_token", "")
    }

    /// Resolve the bot token from `HYMT_TELEGRAM_BOT_TOKEN` or config.
    pub fn telegram_bot_token_resolved(&self) -> String {
        if let Ok(env_token) = std::env::var("HYMT_TELEGRAM_BOT_TOKEN") {
            let trimmed = env_token.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
        self.telegram_bot_token()
    }

    /// Human-enterable claim password used for private-chat ownership claims.
    ///
    /// Treat as a secret: do not log the plaintext repeatedly.
    pub fn telegram_claim_password(&self) -> String {
        self.get_str("telegram", "claim_password", "")
    }

    /// Authorized private-chat owner ids after successful claim.
    pub fn telegram_owners(&self) -> Vec<i64> {
        self.get_i64_vec("telegram", "owners")
    }

    /// Authorized group chat ids used when mode is `groups`.
    pub fn telegram_groups(&self) -> Vec<i64> {
        self.get_i64_vec("telegram", "groups")
    }

    /// Authorization mode: `owners` (default) or `groups`.
    pub fn telegram_mode(&self) -> TelegramMode {
        match self
            .get_str("telegram", "mode", "owners")
            .to_ascii_lowercase()
            .as_str()
        {
            "groups" | "group" => TelegramMode::Groups,
            _ => TelegramMode::Owners,
        }
    }

    /// Ensure `[telegram]` exists and a claim password is present.
    ///
    /// When the password is missing/empty, generates one and writes it once.
    /// Returns whether a new password was generated (caller may print it once).
    pub fn ensure_telegram_claim_password(&self) -> Result<TelegramClaimBootstrap, CoreError> {
        self.maybe_reload()?;
        let existing = self.telegram_claim_password();
        if !existing.trim().is_empty() {
            return Ok(TelegramClaimBootstrap {
                claim_password: existing,
                newly_generated: false,
            });
        }
        let generated = generate_claim_password();
        self.set_telegram_string("claim_password", &generated)?;
        Ok(TelegramClaimBootstrap {
            claim_password: generated,
            newly_generated: true,
        })
    }

    /// Replace the claim password with a newly generated value and persist it.
    pub fn regenerate_telegram_claim_password(&self) -> Result<String, CoreError> {
        let generated = generate_claim_password();
        self.set_telegram_string("claim_password", &generated)?;
        Ok(generated)
    }

    /// Add `chat_id` to `owners` if absent and persist the config.
    pub fn add_telegram_owner(&self, chat_id: i64) -> Result<bool, CoreError> {
        self.append_telegram_i64("owners", chat_id)
    }

    /// Add `chat_id` to `groups` if absent and persist the config.
    pub fn add_telegram_group(&self, chat_id: i64) -> Result<bool, CoreError> {
        self.append_telegram_i64("groups", chat_id)
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn load_from_disk(&self) -> Result<(), CoreError> {
        let content = std::fs::read_to_string(&self.path)?;
        let data: toml::Table = toml::from_str(&content)
            .map_err(|e| CoreError::Config(format!("{}: {}", self.path.display(), e)))?;
        validate_backend_context(&data)?;
        let profile = self
            .state
            .read()
            .unwrap()
            .profile
            .map(Ok)
            .unwrap_or_else(|| model_profile_from_toml(&data))?;
        let overrides = GenerationSettings::from_toml(&data)?;
        generation_backend_from_toml(&data)?.validate_settings(&overrides)?;
        let uses_legacy_generation_scalars = uses_legacy_generation_scalars(&data);
        let uses_legacy_context_window = uses_legacy_context_window(&data);
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        let mut state = self.state.write().unwrap();
        if state.profile.is_none() {
            state.profile = Some(profile);
        }
        state.data = data;
        state.mtime = mtime;
        state.uses_legacy_generation_scalars = uses_legacy_generation_scalars;
        state.uses_legacy_context_window = uses_legacy_context_window;
        Ok(())
    }

    fn ensure_exists(&self) -> Result<(), CoreError> {
        if self.path.exists() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, DEFAULT_CONFIG)?;
        // Future secrets land in this file; never leave it group/world-readable.
        restrict_config_permissions(&self.path)?;
        Ok(())
    }

    fn section_value(&self, section: &str, key: &str) -> Option<toml::Value> {
        let state = self.state.read().unwrap();
        state
            .data
            .get(section)
            .and_then(|v| v.as_table())
            .and_then(|t| t.get(key))
            .cloned()
    }

    fn get_str(&self, section: &str, key: &str, default: &str) -> String {
        self.section_value(section, key)
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| default.to_owned())
    }

    fn get_bool(&self, section: &str, key: &str, default: bool) -> bool {
        self.section_value(section, key)
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    }

    fn get_positive_u32(&self, section: &str, key: &str, default: u32) -> u32 {
        self.section_value(section, key)
            .and_then(|v| v.as_integer())
            .filter(|&n| n > 0)
            .map(|n| n as u32)
            .unwrap_or(default)
    }

    fn get_non_negative_u32(&self, section: &str, key: &str, default: u32) -> u32 {
        self.section_value(section, key)
            .and_then(|v| v.as_integer())
            .filter(|&n| n >= 0)
            .map(|n| n as u32)
            .unwrap_or(default)
    }

    fn get_number_as_f64(&self, section: &str, key: &str, default: f64) -> f64 {
        match self.section_value(section, key) {
            Some(toml::Value::Float(f)) => f,
            Some(toml::Value::Integer(i)) => i as f64,
            _ => default,
        }
    }

    fn get_string_vec(&self, section: &str, key: &str) -> Vec<String> {
        self.section_value(section, key)
            .and_then(|v| v.as_array().cloned())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn get_i64_vec(&self, section: &str, key: &str) -> Vec<i64> {
        self.section_value(section, key)
            .and_then(|v| v.as_array().cloned())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        v.as_integer()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn set_telegram_string(&self, key: &str, value: &str) -> Result<(), CoreError> {
        self.mutate_telegram(|table| {
            table.insert(key.to_owned(), toml::Value::String(value.to_owned()));
        })
    }

    fn append_telegram_i64(&self, key: &str, value: i64) -> Result<bool, CoreError> {
        let mut added = false;
        self.mutate_telegram(|table| {
            let entry = table
                .entry(key.to_owned())
                .or_insert_with(|| toml::Value::Array(Vec::new()));
            let arr = match entry {
                toml::Value::Array(a) => a,
                _ => {
                    *entry = toml::Value::Array(Vec::new());
                    entry.as_array_mut().expect("array just inserted")
                }
            };
            let exists = arr.iter().any(|v| {
                v.as_integer() == Some(value)
                    || v.as_str().and_then(|s| s.parse::<i64>().ok()) == Some(value)
            });
            if !exists {
                arr.push(toml::Value::Integer(value));
                added = true;
            }
        })?;
        Ok(added)
    }

    fn mutate_telegram<F>(&self, f: F) -> Result<(), CoreError>
    where
        F: FnOnce(&mut toml::map::Map<String, toml::Value>),
    {
        self.maybe_reload()?;
        let mut root = {
            let state = self.state.read().unwrap();
            state.data.clone()
        };
        {
            let telegram = root
                .entry("telegram".to_owned())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            let table = match telegram {
                toml::Value::Table(t) => t,
                _ => {
                    *telegram = toml::Value::Table(toml::map::Map::new());
                    telegram.as_table_mut().expect("table just inserted")
                }
            };
            // Ensure stable defaults for missing keys so partial writes stay complete.
            table
                .entry("enabled".to_owned())
                .or_insert(toml::Value::Boolean(false));
            table
                .entry("bot_token".to_owned())
                .or_insert_with(|| toml::Value::String(String::new()));
            table
                .entry("claim_password".to_owned())
                .or_insert_with(|| toml::Value::String(String::new()));
            table
                .entry("owners".to_owned())
                .or_insert_with(|| toml::Value::Array(Vec::new()));
            table
                .entry("groups".to_owned())
                .or_insert_with(|| toml::Value::Array(Vec::new()));
            table
                .entry("mode".to_owned())
                .or_insert_with(|| toml::Value::String("owners".to_owned()));
            f(table);
        }
        let rendered = toml::to_string_pretty(&root)
            .map_err(|e| CoreError::Config(format!("serialize config: {e}")))?;
        atomic_write(&self.path, rendered.as_bytes())?;
        self.load_from_disk()?;
        Ok(())
    }
}

/// Telegram bot authorization mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramMode {
    /// Private chats that successfully claimed ownership.
    Owners,
    /// Messages inside configured group chat ids.
    Groups,
}

/// Result of ensuring a claim password exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramClaimBootstrap {
    pub claim_password: String,
    pub newly_generated: bool,
}

fn generate_claim_password() -> String {
    // 10 chars from a no-ambiguous alphabet of length 32 (~50 bits of entropy).
    // Alphabet length is a power of two so byte→index mapping has no modulo bias.
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    debug_assert_eq!(ALPHABET.len(), 32);
    let mut bytes = [0u8; 10];
    // OS CSPRNG — never fall back to a process-seeded PRNG for claim secrets.
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable for claim password");
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    // Config may hold bot_token / claim_password / owners — keep owner-only.
    restrict_config_permissions(path)?;
    Ok(())
}

/// Best-effort owner-read/write-only mode for secret-bearing config files.
fn restrict_config_permissions(path: &Path) -> Result<(), CoreError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)?;
    let mut perms = meta.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(test)]
#[path = "config_backend_tests.rs"]
mod config_backend_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_config_path(tag: &str) -> PathBuf {
        let unique = format!(
            "{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let dir = std::env::temp_dir().join(format!("hymt-test-{}", unique));
        fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn defaults_when_file_absent() {
        let path = temp_config_path("defaults");
        let cfg = HotConfig::from_path(&path).unwrap();

        assert_eq!(cfg.endpoint_url(), "http://127.0.0.1:8401/v1");
        assert_eq!(cfg.total_context(), 16_384);
        assert_eq!(cfg.parallel_slots(), 1);
        assert_eq!(cfg.per_request_context(), 16_384);
        assert!(!cfg.uses_legacy_context_window());
        assert_eq!(cfg.concurrency(), 1);
        assert_eq!(cfg.primary_lang(), "zh");
        assert_eq!(cfg.secondary_lang(), "en");
        assert_eq!(cfg.stream(), true);
        assert!((cfg.timeout() - 600.0).abs() < f64::EPSILON);
        assert_eq!(cfg.max_source_tokens_per_segment(), 1024);
        assert!((cfg.completeness_zh_to_en_min_ratio() - 0.3).abs() < f64::EPSILON);
        assert!((cfg.completeness_en_to_zh_min_ratio() - 0.3).abs() < f64::EPSILON);
        assert!((cfg.completeness_min_paragraph_ratio() - 0.5).abs() < f64::EPSILON);
        assert_eq!(cfg.completeness_max_retries(), 2);
        assert!(!cfg.completeness_warn_only());
        assert!(!cfg.strict_token_budget());
    }

    #[test]
    fn strict_token_budget_reads_translation_setting() {
        let path = temp_config_path("strict_token_budget");
        fs::write(&path, "[translation]\nstrict_token_budget = true\n").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!(cfg.strict_token_budget());
    }

    #[test]
    fn generated_default_config_derives_per_request_context_after_backend_change() {
        let path = temp_config_path("generated_default_context");
        HotConfig::from_path(&path).unwrap();
        let defaults = fs::read_to_string(&path).unwrap();
        assert!(
            !defaults.contains("per_request_context ="),
            "generated defaults must not pin the derived per-request context"
        );

        let adjusted = defaults
            .replace("total_context = 16384", "total_context = 24576")
            .replace("parallel_slots = 1", "parallel_slots = 3");
        fs::write(&path, adjusted).unwrap();

        let cfg = HotConfig::from_path(&path).unwrap();
        assert_eq!(cfg.per_request_context(), 8_192);
    }

    #[test]
    fn creates_default_file_when_absent() {
        let path = temp_config_path("create");
        assert!(!path.exists());
        let _cfg = HotConfig::from_path(&path).unwrap();
        assert!(path.exists());
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[endpoint]"));
        assert!(contents.contains("[backend]"));
        assert!(!contents.contains("context_window"));
        assert!(!contents.contains("per_request_context ="));
    }

    #[test]
    fn backend_context_derives_per_request_limit_from_parallel_slots() {
        for (total_context, parallel_slots) in [(24_576, 3), (65_536, 8)] {
            let path = temp_config_path("derived_per_request_context");
            fs::write(
                &path,
                format!(
                    "[backend]\ntotal_context = {total_context}\nparallel_slots = {parallel_slots}\n"
                ),
            )
            .unwrap();

            let cfg = HotConfig::from_path(&path).unwrap();
            assert_eq!(cfg.total_context(), total_context);
            assert_eq!(cfg.parallel_slots(), parallel_slots);
            assert_eq!(cfg.per_request_context(), 8_192);
            assert!(!cfg.uses_legacy_context_window());
        }
    }

    #[test]
    fn backend_context_allows_explicit_per_request_limit() {
        let path = temp_config_path("explicit_per_request_context");
        fs::write(
            &path,
            "[backend]\ntotal_context = 65536\nparallel_slots = 8\nper_request_context = 7000\n",
        )
        .unwrap();

        let cfg = HotConfig::from_path(&path).unwrap();
        assert_eq!(cfg.total_context(), 65_536);
        assert_eq!(cfg.parallel_slots(), 8);
        assert_eq!(cfg.per_request_context(), 7_000);
    }

    #[test]
    fn rejects_inconsistent_backend_context_at_load() {
        for (tag, backend, expected_message) in [
            (
                "parallel_slots_exceed_total_context",
                "total_context = 1\nparallel_slots = 2",
                "backend.parallel_slots (2) must not exceed backend.total_context (1); computed per-slot capacity is 0",
            ),
            (
                "per_request_context_exceeds_slot_capacity",
                "total_context = 24576\nparallel_slots = 3\nper_request_context = 24576",
                "backend.per_request_context (24576) must not exceed computed per-slot capacity (8192)",
            ),
        ] {
            let path = temp_config_path(tag);
            fs::write(&path, format!("[backend]\n{backend}\n")).unwrap();

            let error = HotConfig::from_path(&path)
                .err()
                .expect("inconsistent backend context must fail at load");
            let message = match error {
                CoreError::Config(message) => message,
                error => panic!("{tag}: expected a configuration error, got {error}"),
            };
            assert!(message.contains(expected_message), "{tag}: {message}");
        }
    }

    #[test]
    fn rejects_invalid_backend_context_values_at_load_without_panicking() {
        for (tag, key, value) in [
            (
                "parallel_slots_above_u32_max",
                "parallel_slots",
                "4294967296",
            ),
            ("parallel_slots_zero", "parallel_slots", "0"),
            ("total_context_above_u32_max", "total_context", "4294967296"),
            ("total_context_zero", "total_context", "0"),
            ("per_request_context_zero", "per_request_context", "0"),
        ] {
            let path = temp_config_path(tag);
            fs::write(&path, format!("[backend]\n{key} = {value}\n")).unwrap();

            let result = std::panic::catch_unwind(|| {
                let config = HotConfig::from_path(&path)?;
                Ok::<u32, CoreError>(config.per_request_context())
            });
            let result = result.expect("invalid backend context must not panic");
            let error = result.expect_err("invalid backend context must fail at load");
            assert!(
                error.to_string().contains(&format!("backend.{key}")),
                "{tag}: {error}"
            );
        }
    }

    #[test]
    fn legacy_context_window_is_total_context_and_requests_migration_warning() {
        let path = temp_config_path("legacy_context_window");
        fs::write(&path, "[translation]\ncontext_window = 24576\n").unwrap();

        let cfg = HotConfig::from_path(&path).unwrap();
        assert_eq!(cfg.total_context(), 24_576);
        assert_eq!(cfg.parallel_slots(), 1);
        assert_eq!(cfg.per_request_context(), 24_576);
        assert!(cfg.uses_legacy_context_window());
    }

    #[test]
    fn profiles_define_distinct_tokenizers_and_upstream_defaults() {
        use crate::model_profile::{ArchitectureVariant, ModelProfile};

        let cases = [
            (
                ModelProfile::HyMt2_1_8b,
                "hy_mt2_1_8b",
                "tencent/Hy-MT2-1.8B",
                ArchitectureVariant::Dense1_8B,
                Setting::Value(0.6),
                Setting::Value(20),
                Setting::Value(1.05),
            ),
            (
                ModelProfile::HyMt2_7b,
                "hy_mt2_7b",
                "tencent/Hy-MT2-7B",
                ArchitectureVariant::Dense7B,
                Setting::Value(0.6),
                Setting::Value(20),
                Setting::Value(1.05),
            ),
            (
                ModelProfile::HyMt2_30bA3b,
                "hy_mt2_30b_a3b",
                "tencent/Hy-MT2-30B-A3B",
                ArchitectureVariant::MoE30BA3B,
                Setting::Value(1.0),
                Setting::Disabled,
                Setting::Value(1.0),
            ),
        ];

        for (profile, id, repo, architecture, top_p, top_k, repetition_penalty) in cases {
            assert_eq!(profile.id(), id);
            assert_eq!(ModelProfile::parse(id), Some(profile));
            assert_eq!(profile.architecture(), architecture);
            assert_eq!(profile.tokenizer().unwrap().repo, repo);
            assert_eq!(profile.tokenizer().unwrap().revision.len(), 40);
            assert!(profile
                .tokenizer()
                .unwrap()
                .revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()));
            assert_eq!(profile.max_context_tokens(), 262_144);
            assert_eq!(profile.recommended_max_output_tokens(), 4_096);
            assert!(!profile.gguf_aliases().is_empty());

            let defaults = profile.generation_defaults();
            assert_eq!(defaults.temperature, Setting::Value(0.7));
            assert_eq!(defaults.top_p, top_p);
            assert_eq!(defaults.top_k, top_k);
            assert_eq!(defaults.repetition_penalty, repetition_penalty);
        }

        assert_eq!(ModelProfile::Generic.tokenizer(), None);
        assert_eq!(
            ModelProfile::Generic.generation_defaults(),
            GenerationSettings::server_defaults()
        );
    }

    #[test]
    fn inference_fingerprint_is_stable_and_changes_with_inference_identity() {
        fn fingerprint(
            config: &str,
            template_type: &str,
            options_hash: &str,
        ) -> InferenceFingerprint {
            let path = temp_config_path("inference_fingerprint");
            fs::write(&path, config).unwrap();
            HotConfig::from_path(&path)
                .unwrap()
                .inference_fingerprint(template_type, options_hash)
                .unwrap()
        }

        let q4 = fingerprint(
            r#"[endpoint]
url = "http://localhost:8401/v1"
model = "hy-mt2-7b-q4_k_m.gguf"
backend = "llama_cpp"

[inference]

[inference.override]
temperature = 0.7"#,
            "default",
            "",
        );
        let profiled = fingerprint(
            r#"[endpoint]
url = "http://localhost:8401/v1"
profile = "hy_mt2_7b""#,
            "default",
            "",
        );
        let same_q4 = fingerprint(
            r#"[endpoint]
url = "http://localhost:8401/v1"
model = "hy-mt2-7b-q4_k_m.gguf"
backend = "llama_cpp"

[inference]

[inference.override]
temperature = 0.7"#,
            "default",
            "",
        );
        let q6 = fingerprint(
            r#"[endpoint]
url = "http://localhost:8401/v1"
model = "hy-mt2-7b-q6_k.gguf"
backend = "llama_cpp"

[inference]

[inference.override]
temperature = 0.7"#,
            "default",
            "",
        );
        let openai = fingerprint(
            r#"[endpoint]
url = "http://localhost:8401/v1"
model = "hy-mt2-7b-q4_k_m.gguf"
backend = "openai_compatible"

[inference]

[inference.override]
temperature = 0.7"#,
            "default",
            "",
        );
        let hotter = fingerprint(
            r#"[endpoint]
url = "http://localhost:8401/v1"
model = "hy-mt2-7b-q4_k_m.gguf"
backend = "llama_cpp"

[inference]

[inference.override]
temperature = 0.8"#,
            "default",
            "",
        );
        let no_completeness_retries = fingerprint(
            r#"[endpoint]
url = "http://localhost:8401/v1"
model = "hy-mt2-7b-q4_k_m.gguf"
backend = "llama_cpp"

[inference]

[inference.override]
temperature = 0.7

[completeness]
max_retries = 0"#,
            "default",
            "",
        );
        let prompted = fingerprint(
            r#"[endpoint]
url = "http://localhost:8401/v1"
model = "hy-mt2-7b-q4_k_m.gguf"
backend = "llama_cpp"

[inference]

[inference.override]
temperature = 0.7"#,
            "style",
            "prompt-options-sha256",
        );

        assert_eq!(q4, same_q4);
        assert!(profiled.canonical_json().contains(
            "\"tokenizer\":{\"repo\":\"tencent/Hy-MT2-7B\",\"revision\":\"9b0eb4e8f001def3e5ff6469a0ac96fdb39ec223\"}"
        ));
        assert_ne!(q4, q6, "a configured GGUF alias must scope the cache");
        assert_ne!(q4, openai, "the request backend must scope the cache");
        assert_ne!(q4, hotter, "sampling overrides must scope the cache");
        assert_ne!(
            q4, no_completeness_retries,
            "retry policy can construct a different follow-up inference request"
        );
        assert_ne!(q4, prompted, "prompt identity must scope the cache");
        assert_eq!(q4.hash().len(), 64);
        assert!(q4.canonical_json().contains("\"quantization\":null"));
        assert!(q4.canonical_json().contains("\"schema_version\":1"));
    }

    #[test]
    fn incomplete_inference_identity_is_not_cache_verified() {
        fn fingerprint(config: &str) -> InferenceFingerprint {
            let path = temp_config_path("inference_fingerprint_verification");
            fs::write(&path, config).unwrap();
            HotConfig::from_path(&path)
                .unwrap()
                .inference_fingerprint("default", "")
                .unwrap()
        }

        let incomplete = fingerprint(
            r#"[endpoint]
url = "http://localhost:8401/v1""#,
        );
        let profiled = fingerprint(
            r#"[endpoint]
url = "http://localhost:8401/v1"
profile = "hy_mt2_7b""#,
        );
        let explicit_generic = fingerprint(
            r#"[endpoint]
url = "http://localhost:8401/v1"
model = "stable-served-model"
backend = "llama_cpp"

[inference.override]
temperature = 0.7
top_p = 0.6
top_k = 20
repetition_penalty = 1.05
min_p = 0.1
repeat_last_n = 64"#,
        );

        assert!(
            !incomplete.is_cache_verified(),
            "generic model identity plus server-default sampling cannot safely reuse cache"
        );
        assert!(
            !profiled.is_cache_verified(),
            "a profile alone cannot verify service-owned sampling for cache reuse"
        );
        assert!(
            explicit_generic.is_cache_verified(),
            "a configured served model and explicit sampling establish a usable identity"
        );
    }

    #[test]
    fn endpoint_profile_keeps_sampling_server_owned_without_explicit_overrides() {
        use crate::model_profile::ModelProfile;

        let path = temp_config_path("profile_defaults");
        fs::write(
            &path,
            r#"[endpoint]
profile = "hy_mt2_30b_a3b"

[inference.override]
top_p = 0.8"#,
        )
        .unwrap();

        let config = HotConfig::from_path(&path).unwrap();
        assert_eq!(config.model_profile().unwrap(), ModelProfile::HyMt2_30bA3b);
        assert_eq!(
            config.generation_settings().unwrap().temperature,
            Setting::ServerDefault
        );
        assert_eq!(
            config.generation_settings().unwrap().top_p,
            Setting::Value(0.8)
        );
        assert_eq!(
            config.generation_settings().unwrap().top_k,
            Setting::ServerDefault
        );
        assert_eq!(
            config.generation_settings().unwrap().repetition_penalty,
            Setting::ServerDefault
        );

        let generic_path = temp_config_path("generic_profile");
        fs::write(&generic_path, "[endpoint]\nmodel = \"untested-model\"").unwrap();
        let generic = HotConfig::from_path(&generic_path).unwrap();
        assert_eq!(generic.model_profile().unwrap(), ModelProfile::Generic);
        assert_eq!(
            generic.generation_settings().unwrap(),
            GenerationSettings::server_defaults()
        );

        let unknown_path = temp_config_path("unknown_profile");
        fs::write(&unknown_path, "[endpoint]\nprofile = \"hy_mt2_99b\"").unwrap();
        let error = HotConfig::from_path(&unknown_path).unwrap_err();
        assert!(error.to_string().contains("endpoint.profile"));
    }

    #[test]
    fn profiles_without_explicit_overrides_keep_openai_sampling_server_owned() {
        let path = temp_config_path("profile_openai_backend");
        fs::write(
            &path,
            r#"[endpoint]
profile = "hy_mt2_30b_a3b"
backend = "openai_compatible""#,
        )
        .unwrap();

        let config = HotConfig::from_path(&path).unwrap();
        assert_eq!(
            config.generation_settings().unwrap().top_k,
            Setting::ServerDefault
        );
    }

    #[test]
    fn generation_settings_parse_semantic_override_states() {
        let path = temp_config_path("generation_states");
        fs::write(
            &path,
            r#"[endpoint]
backend = "llama_cpp"

[inference.override]
temperature = 0.7
top_k = "disabled"
repeat_last_n = -1"#,
        )
        .unwrap();

        let settings = HotConfig::from_path(&path)
            .unwrap()
            .generation_settings()
            .unwrap();
        assert_eq!(settings.temperature, Setting::Value(0.7));
        assert_eq!(settings.top_p, Setting::ServerDefault);
        assert_eq!(settings.top_k, Setting::Disabled);
        assert_eq!(settings.repeat_last_n, Setting::Value(-1));
    }

    #[test]
    fn legacy_generation_scalars_remain_explicit_values() {
        let path = temp_config_path("legacy_generation");
        fs::write(
            &path,
            r#"[endpoint]
backend = "llama_cpp"

[inference]
temperature = 0.7
top_p = 0.6
top_k = -1
repetition_penalty = 1.05
min_p = 0.2
repeat_last_n = -1"#,
        )
        .unwrap();

        let config = HotConfig::from_path(&path).unwrap();
        assert!(config.uses_legacy_generation_scalars());
        let settings = config.generation_settings().unwrap();
        assert_eq!(settings.temperature, Setting::Value(0.7));
        assert_eq!(settings.top_p, Setting::Value(0.6));
        assert_eq!(settings.top_k, Setting::Value(-1));
        assert_eq!(settings.repetition_penalty, Setting::Value(1.05));
        assert_eq!(settings.min_p, Setting::Value(0.2));
        assert_eq!(settings.repeat_last_n, Setting::Value(-1));
    }

    #[test]
    fn invalid_generation_overrides_name_the_invalid_field() {
        for (tag, value, field) in [
            ("temperature", "temperature = nan", "temperature"),
            ("top_p", "top_p = 1.1", "top_p"),
            ("min_p", "min_p = -0.1", "min_p"),
            ("repetition", "repetition_penalty = 0", "repetition_penalty"),
            ("top_k", "top_k = -2", "top_k"),
            ("repeat", "repeat_last_n = -2", "repeat_last_n"),
        ] {
            let path = temp_config_path(tag);
            fs::write(&path, format!("[inference.override]\n{value}")).unwrap();
            let error = HotConfig::from_path(&path).unwrap_err();
            assert!(error.to_string().contains(field), "{tag}: {error}");
        }
    }

    #[test]
    fn contradictory_disabled_generation_settings_are_rejected() {
        let path = temp_config_path("contradictory_generation");
        fs::write(
            &path,
            r#"[inference.override]
repetition_penalty = "disabled"
repeat_last_n = -1"#,
        )
        .unwrap();

        let error = HotConfig::from_path(&path).unwrap_err();
        assert!(error.to_string().contains("repetition_penalty"));
        assert!(error.to_string().contains("repeat_last_n"));
    }

    #[test]
    fn invalid_generation_backend_is_rejected() {
        let path = temp_config_path("invalid_generation_backend");
        fs::write(&path, "[endpoint]\nbackend = 7").unwrap();

        let error = HotConfig::from_path(&path).unwrap_err();
        assert!(error.to_string().contains("endpoint.backend"));
    }

    #[test]
    fn openai_compatible_rejects_unsupported_explicit_and_disabled_overrides_at_load() {
        for (tag, override_value, field) in [
            ("top_k_explicit", "top_k = 20", "top_k"),
            (
                "repetition_explicit",
                "repetition_penalty = 1.05",
                "repetition_penalty",
            ),
            ("min_p_disabled", "min_p = \"disabled\"", "min_p"),
            (
                "repeat_last_n_explicit",
                "repeat_last_n = -1",
                "repeat_last_n",
            ),
        ] {
            let path = temp_config_path(tag);
            fs::write(
                &path,
                format!(
                    "[endpoint]\nbackend = \"openai_compatible\"\n\n[inference.override]\n{override_value}"
                ),
            )
            .unwrap();

            let error = HotConfig::from_path(&path).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("openai_compatible"), "{tag}: {message}");
            assert!(message.contains(field), "{tag}: {message}");
            assert!(
                message.contains("configured semantic value"),
                "{tag}: {message}"
            );
            assert!(message.contains("wire representation"), "{tag}: {message}");
        }
    }

    #[test]
    fn reads_custom_values() {
        let path = temp_config_path("custom");
        fs::write(
            &path,
            r#"
[endpoint]
url = "http://example.com:9000/v1/"
model = "hy-mt2"

[translation]
concurrency = 4
timeout = 120

[language]
primary = "en"
secondary = "zh"
"#,
        )
        .unwrap();

        let cfg = HotConfig::from_path(&path).unwrap();
        assert_eq!(cfg.endpoint_url(), "http://example.com:9000/v1");
        assert_eq!(cfg.model(), "hy-mt2");
        assert_eq!(cfg.concurrency(), 4);
        assert!((cfg.timeout() - 120.0).abs() < f64::EPSILON);
        assert_eq!(cfg.primary_lang(), "en");
        assert_eq!(cfg.secondary_lang(), "zh");
        // Unset fields keep defaults
        assert_eq!(cfg.max_output_tokens(), 4096);
    }

    #[test]
    fn endpoint_url_strips_trailing_slash() {
        let path = temp_config_path("slash");
        fs::write(
            &path,
            r#"[endpoint]
url = "http://localhost:8401/v1/""#,
        )
        .unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert_eq!(cfg.endpoint_url(), "http://localhost:8401/v1");
    }

    #[test]
    fn integer_timeout_parsed_as_float() {
        let path = temp_config_path("int_timeout");
        fs::write(&path, "[translation]\ntimeout = 300").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!((cfg.timeout() - 300.0).abs() < f64::EPSILON);
    }

    #[test]
    fn invalid_toml_returns_config_error() {
        let path = temp_config_path("invalid");
        fs::write(&path, "not valid toml = = =").unwrap();
        let result = HotConfig::from_path(&path);
        assert!(matches!(result, Err(CoreError::Config(_))));
    }

    #[test]
    fn maybe_reload_detects_changes() {
        let path = temp_config_path("reload");
        fs::write(&path, "[translation]\nconcurrency = 1").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert_eq!(cfg.concurrency(), 1);

        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, "[translation]\nconcurrency = 8").unwrap();
        cfg.maybe_reload().unwrap();
        assert_eq!(cfg.concurrency(), 8);
    }

    #[test]
    fn model_profile_is_pinned_when_config_reloads() {
        use crate::model_profile::ModelProfile;

        let path = temp_config_path("pinned_profile");
        fs::write(&path, "[endpoint]\nprofile = \"hy_mt2_7b\"").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert_eq!(cfg.model_profile().unwrap(), ModelProfile::HyMt2_7b);

        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, "[endpoint]\nprofile = \"hy_mt2_30b_a3b\"").unwrap();
        assert!(cfg.maybe_reload().unwrap());

        assert_eq!(
            cfg.model_profile().unwrap(),
            ModelProfile::HyMt2_7b,
            "profile changes require a new HotConfig/session"
        );
    }

    #[test]
    fn positive_int_rejects_zero_and_negative() {
        let path = temp_config_path("pos_int");
        fs::write(&path, "[translation]\nconcurrency = -1").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        // -1 is not positive, falls back to default
        assert_eq!(cfg.concurrency(), 1);
    }

    #[test]
    fn timing_divergence_threshold_min_enforced() {
        let path = temp_config_path("timing");
        fs::write(&path, "[timing]\ndivergence_threshold = 0.5").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        // 0.5 ≤ 1.0, falls back to default 2.0
        assert!((cfg.timing_divergence_threshold() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn first_chunk_priority_defaults_false() {
        let path = temp_config_path("fcp_default");
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!(!cfg.first_chunk_priority());
    }

    #[test]
    fn first_chunk_priority_reads_true() {
        let path = temp_config_path("fcp_true");
        fs::write(&path, "[translation]\nfirst_chunk_priority = true").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!(cfg.first_chunk_priority());
    }

    #[test]
    fn debug_chunk_timing_defaults_false() {
        let path = temp_config_path("debug_timing_default");
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!(!cfg.debug_chunk_timing());
    }

    #[test]
    fn debug_chunk_timing_reads_true() {
        let path = temp_config_path("debug_timing_true");
        fs::write(&path, "[translation]\ndebug_chunk_timing = true").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!(cfg.debug_chunk_timing());
    }

    #[test]
    fn exec_plugin_blocklist_defaults() {
        let path = temp_config_path("blocklist");
        let cfg = HotConfig::from_path(&path).unwrap();
        let bl = cfg.exec_plugin_blocklist();
        assert!(bl.contains(&"docker".to_owned()));
        assert!(bl.contains(&"hymt".to_owned()));
        assert!(bl.contains(&"ssh".to_owned()));
    }

    #[test]
    fn exec_plugin_blocklist_custom() {
        let path = temp_config_path("blocklist_custom");
        fs::write(
            &path,
            r#"[exec.plugin]
blocklist = ["foo", "bar"]"#,
        )
        .unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        let bl = cfg.exec_plugin_blocklist();
        assert_eq!(bl, vec!["foo".to_owned(), "bar".to_owned()]);
    }

    #[test]
    fn telegram_defaults_when_absent() {
        let path = temp_config_path("tg_defaults");
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!(!cfg.telegram_enabled());
        assert!(cfg.telegram_bot_token().is_empty());
        assert!(cfg.telegram_claim_password().is_empty());
        assert!(cfg.telegram_owners().is_empty());
        assert!(cfg.telegram_groups().is_empty());
        assert_eq!(cfg.telegram_mode(), TelegramMode::Owners);
    }

    #[test]
    fn telegram_reads_custom_values() {
        let path = temp_config_path("tg_custom");
        fs::write(
            &path,
            r#"
[telegram]
enabled = true
bot_token = "token-from-file"
claim_password = "CLAIM-ME"
owners = [111, 222]
groups = [333]
mode = "groups"
"#,
        )
        .unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!(cfg.telegram_enabled());
        assert_eq!(cfg.telegram_bot_token(), "token-from-file");
        assert_eq!(cfg.telegram_claim_password(), "CLAIM-ME");
        assert_eq!(cfg.telegram_owners(), vec![111, 222]);
        assert_eq!(cfg.telegram_groups(), vec![333]);
        assert_eq!(cfg.telegram_mode(), TelegramMode::Groups);
    }

    #[test]
    fn ensure_claim_password_generates_once() {
        let path = temp_config_path("tg_claim_gen");
        let cfg = HotConfig::from_path(&path).unwrap();
        let first = cfg.ensure_telegram_claim_password().unwrap();
        assert!(first.newly_generated);
        assert_eq!(first.claim_password.len(), 10);
        assert!(first
            .claim_password
            .bytes()
            .all(|b| b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(&b)));
        let second = cfg.ensure_telegram_claim_password().unwrap();
        assert!(!second.newly_generated);
        assert_eq!(second.claim_password, first.claim_password);
        assert_eq!(cfg.telegram_claim_password(), first.claim_password);
    }

    #[test]
    fn claim_password_generation_is_unpredictable_across_calls() {
        // Weak process-seeded PRNGs often emit identical streams in-process;
        // CSPRNG should almost never collide across independent generations.
        let a = generate_claim_password();
        let b = generate_claim_password();
        let c = generate_claim_password();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn add_telegram_owner_is_idempotent_and_multi() {
        let path = temp_config_path("tg_owners");
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!(cfg.add_telegram_owner(42).unwrap());
        assert!(!cfg.add_telegram_owner(42).unwrap());
        assert!(cfg.add_telegram_owner(99).unwrap());
        assert_eq!(cfg.telegram_owners(), vec![42, 99]);
    }

    #[test]
    fn regenerate_claim_password_changes_value() {
        let path = temp_config_path("tg_regen");
        let cfg = HotConfig::from_path(&path).unwrap();
        let first = cfg.ensure_telegram_claim_password().unwrap().claim_password;
        let second = cfg.regenerate_telegram_claim_password().unwrap();
        assert_ne!(first, second);
        assert_eq!(cfg.telegram_claim_password(), second);
    }

    #[test]
    fn telegram_secret_writes_set_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_config_path("tg_mode_600");
        let cfg = HotConfig::from_path(&path).unwrap();
        let _ = cfg.ensure_telegram_claim_password().unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "claim password write must set 0o600");
        let _ = cfg.add_telegram_owner(1).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "owner write must set 0o600");
    }
}
