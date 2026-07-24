//! Hot-reloadable TOML configuration.
//!
//! Reads from `~/.config/hymt/config.toml`, creating it with embedded defaults
//! when absent. Call `maybe_reload()` to pick up on-disk changes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use crate::error::CoreError;
use crate::model_profile::ModelProfile;

pub const DEFAULT_CONFIG: &str = r#"[endpoint]
url = "http://127.0.0.1:8401/v1"
api_key = ""
model = ""
# Set this explicitly for a tested Hy-MT2 family member. When omitted, hymt
# uses generic mode and does not assume a tokenizer or sampler profile.
# profile = "hy_mt2_7b"

[translation]
context_window = 16384
max_output_tokens = 4096
concurrency = 1
stream = true
config_version = 1
timeout = 600
first_chunk_priority = false
# Hard cap on source tokens submitted per segment. Prevents oversized single-segment
# hangs when context_window/max_output_tokens alone still leave a multi-k budget.
# Set to 0 to disable the hard cap (budget is then only expansion/context-limited).
max_source_tokens_per_segment = 1024
debug_chunk_timing = false

[language]
primary = "zh"
secondary = "en"

[inference]
# Sampler values are omitted by default so the configured server selects them.
# `openai_compatible` accepts only temperature, top_p, and repetition_penalty;
# it sends the latter as `repetition_penalty`, unlike llama.cpp's repeat_penalty.
backend = "llama_cpp"
# Explicit overrides belong under `[inference.override]`; a numeric value is
# sent as-is and the string `"disabled"` turns off that sampler.

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
const OPENAI_COMPATIBLE_CAPABILITIES: &[GenerationSettingField] = &[
    GenerationSettingField::Temperature,
    GenerationSettingField::TopP,
    GenerationSettingField::RepetitionPenalty,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationBackend {
    /// llama.cpp's OpenAI-compatible server.
    LlamaCpp,
    /// A generic OpenAI-compatible server.
    ///
    /// This profile accepts `temperature`, `top_p`, and `repetition_penalty`.
    /// The repetition penalty is serialized as `repetition_penalty`.
    OpenAiCompatible,
}

impl GenerationBackend {
    const fn name(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama_cpp",
            Self::OpenAiCompatible => "openai_compatible",
        }
    }

    fn capabilities(self) -> &'static [GenerationSettingField] {
        match self {
            Self::LlamaCpp => LLAMA_CPP_CAPABILITIES,
            Self::OpenAiCompatible => OPENAI_COMPATIBLE_CAPABILITIES,
        }
    }

    fn validate_settings(self, settings: &GenerationSettings) -> Result<(), CoreError> {
        for field in GenerationSettingField::ALL {
            if settings.is_explicit(field) && !self.capabilities().contains(&field) {
                return Err(CoreError::Config(format!(
                    "inference.backend {} does not support inference.override.{}",
                    self.name(),
                    field.name(),
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

    /// Overlay explicit user overrides on top of a model profile's defaults.
    pub fn with_overrides(self, overrides: Self) -> Self {
        Self {
            temperature: setting_or_default(self.temperature, overrides.temperature),
            top_p: setting_or_default(self.top_p, overrides.top_p),
            top_k: setting_or_default(self.top_k, overrides.top_k),
            repetition_penalty: setting_or_default(
                self.repetition_penalty,
                overrides.repetition_penalty,
            ),
            min_p: setting_or_default(self.min_p, overrides.min_p),
            repeat_last_n: setting_or_default(self.repeat_last_n, overrides.repeat_last_n),
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

fn setting_or_default<T>(default: Setting<T>, override_value: Setting<T>) -> Setting<T> {
    match override_value {
        Setting::ServerDefault => default,
        value => value,
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
    let Some(inference) = data.get("inference") else {
        return Ok(GenerationBackend::LlamaCpp);
    };
    let table = inference
        .as_table()
        .ok_or_else(|| CoreError::Config("inference must be a TOML table".to_owned()))?;
    match table.get("backend") {
        None => Ok(GenerationBackend::LlamaCpp),
        Some(toml::Value::String(value)) => match value.as_str() {
            "llama_cpp" | "llama.cpp" => Ok(GenerationBackend::LlamaCpp),
            "openai_compatible" | "transformers" => Ok(GenerationBackend::OpenAiCompatible),
            _ => Err(CoreError::Config(format!(
                "inference.backend must be llama_cpp or openai_compatible, got {value:?}"
            ))),
        },
        Some(value) => Err(CoreError::Config(format!(
            "inference.backend must be llama_cpp or openai_compatible, got {value:?}"
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
}

impl ConfigState {
    fn empty() -> Self {
        Self {
            data: toml::Table::new(),
            mtime: None,
            profile: None,
            uses_legacy_generation_scalars: false,
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

    pub fn context_window(&self) -> u32 {
        self.get_positive_u32("translation", "context_window", 16384)
    }

    pub fn max_output_tokens(&self) -> u32 {
        let default = self
            .model_profile()
            .map(|profile| profile.recommended_max_output_tokens())
            .unwrap_or(4_096);
        self.get_positive_u32("translation", "max_output_tokens", default)
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

    // ── language ────────────────────────────────────────────────────────────

    pub fn primary_lang(&self) -> String {
        self.get_str("language", "primary", "zh")
    }

    pub fn secondary_lang(&self) -> String {
        self.get_str("language", "secondary", "en")
    }

    // ── inference ───────────────────────────────────────────────────────────

    /// Reads and validates the backend-neutral generation overrides.
    pub fn generation_settings(&self) -> Result<GenerationSettings, CoreError> {
        let state = self.state.read().unwrap();
        let profile = state.profile.unwrap_or(ModelProfile::Generic);
        let overrides = GenerationSettings::from_toml(&state.data)?;
        generation_backend_from_toml(&state.data)?.validate_settings(&overrides)?;
        let settings = profile.generation_defaults().with_overrides(overrides);
        settings.validate()?;
        Ok(settings)
    }

    /// Backend profile used to map [`GenerationSettings`] to request fields.
    pub fn generation_backend(&self) -> Result<GenerationBackend, CoreError> {
        let state = self.state.read().unwrap();
        generation_backend_from_toml(&state.data)
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
        let profile = self
            .state
            .read()
            .unwrap()
            .profile
            .map(Ok)
            .unwrap_or_else(|| model_profile_from_toml(&data))?;
        let overrides = GenerationSettings::from_toml(&data)?;
        generation_backend_from_toml(&data)?.validate_settings(&overrides)?;
        let settings = profile.generation_defaults().with_overrides(overrides);
        settings.validate()?;
        let uses_legacy_generation_scalars = uses_legacy_generation_scalars(&data);
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
    }

    #[test]
    fn creates_default_file_when_absent() {
        let path = temp_config_path("create");
        assert!(!path.exists());
        let _cfg = HotConfig::from_path(&path).unwrap();
        assert!(path.exists());
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[endpoint]"));
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
    fn endpoint_profile_selects_generation_defaults_and_rejects_unknown_values() {
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
            Setting::Value(0.7)
        );
        assert_eq!(
            config.generation_settings().unwrap().top_p,
            Setting::Value(0.8)
        );
        assert_eq!(
            config.generation_settings().unwrap().top_k,
            Setting::Disabled
        );
        assert_eq!(
            config.generation_settings().unwrap().repetition_penalty,
            Setting::Value(1.0)
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
    fn profiles_keep_semantic_defaults_when_openai_omits_unsupported_fields() {
        let path = temp_config_path("profile_openai_backend");
        fs::write(
            &path,
            r#"[endpoint]
profile = "hy_mt2_30b_a3b"

[inference]
backend = "openai_compatible""#,
        )
        .unwrap();

        let config = HotConfig::from_path(&path).unwrap();
        assert_eq!(
            config.generation_settings().unwrap().top_k,
            Setting::Disabled
        );
    }

    #[test]
    fn generation_settings_parse_semantic_override_states() {
        let path = temp_config_path("generation_states");
        fs::write(
            &path,
            r#"[inference.override]
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
            r#"[inference]
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
        fs::write(&path, "[inference]\nbackend = 7").unwrap();

        let error = HotConfig::from_path(&path).unwrap_err();
        assert!(error.to_string().contains("inference.backend"));
    }

    #[test]
    fn openai_compatible_rejects_unsupported_explicit_and_disabled_overrides_at_load() {
        for (tag, override_value, field) in [
            ("top_k_explicit", "top_k = 20", "top_k"),
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
                    "[inference]\nbackend = \"openai_compatible\"\n\n[inference.override]\n{override_value}"
                ),
            )
            .unwrap();

            let error = HotConfig::from_path(&path).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("openai_compatible"), "{tag}: {message}");
            assert!(message.contains(field), "{tag}: {message}");
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
