//! Normalized, capability-oriented facts discovered from a running inference service.
//!
//! Values remain optional unless the backend explicitly reported them.  This keeps a
//! partially upgraded or unavailable service from being mistaken for a verified one.

use crate::config::GenerationBackend;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Whether a backend response was sufficiently well formed to be used as runtime evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendVerificationStatus {
    /// The service returned a backend-specific response that parsed successfully.
    Verified,
    /// No trustworthy response is available; all missing values must remain unknown.
    Unverified,
}

/// Sampler defaults advertised by a backend.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackendSamplerDefaults {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i32>,
    pub min_p: Option<f64>,
    pub repetition_penalty: Option<f64>,
    pub repeat_last_n: Option<i64>,
    /// The server field that carried [`Self::repetition_penalty`].
    ///
    /// This makes an adapter/service wire-key mismatch observable without changing
    /// the request adapter's established serialization policy.
    pub repetition_penalty_wire_key: Option<String>,
}

impl BackendSamplerDefaults {
    /// Whether every sampler that hymt delegates to llama.cpp is explicitly known.
    pub fn is_complete(&self) -> bool {
        self.temperature.is_some()
            && self.top_p.is_some()
            && self.top_k.is_some()
            && self.min_p.is_some()
            && self.repetition_penalty.is_some()
            && self.repeat_last_n.is_some()
    }
}

/// A backend-neutral snapshot of service state.
///
/// It is deliberately a value object: callers can serialize it into diagnostics
/// and fingerprints without carrying credentials, HTTP clients, or endpoint URLs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendRuntimeInfo {
    pub backend: GenerationBackend,
    pub version: Option<String>,
    pub build: Option<String>,
    pub served_model: Option<String>,
    pub model_metadata: Option<Value>,
    pub total_context: Option<u32>,
    pub per_slot_context: Option<u32>,
    pub active_slots: Option<u32>,
    pub max_parallel_slots: Option<u32>,
    pub default_max_generation_tokens: Option<u32>,
    pub sampler_defaults: BackendSamplerDefaults,
    pub supports_streaming: Option<bool>,
    pub supports_tokenization: Option<bool>,
    /// A single ASCII character that llama.cpp incorrectly advertises as EOS.
    ///
    /// The client resolves this candidate through `/tokenize` during preflight.
    pub false_eos_token: Option<String>,
    /// Token ID resolved from [`Self::false_eos_token`] and suppressed in requests.
    pub false_eos_token_id: Option<u32>,
    pub supports_templates: Option<bool>,
    pub supports_structured_output: Option<bool>,
    pub observed_at_unix_secs: u64,
    pub verification_status: BackendVerificationStatus,
    pub verification_message: Option<String>,
    /// Stable facts available from this response, used to notice a service restart
    /// or profile switch when a fresh probe is made.
    pub server_identity: Option<String>,
}

impl BackendRuntimeInfo {
    /// Create an honest failure result without fabricating endpoint metadata.
    pub fn unverified(
        backend: GenerationBackend,
        observed_at_unix_secs: u64,
        message: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            version: None,
            build: None,
            served_model: None,
            model_metadata: None,
            total_context: None,
            per_slot_context: None,
            active_slots: None,
            max_parallel_slots: None,
            default_max_generation_tokens: None,
            sampler_defaults: BackendSamplerDefaults::default(),
            supports_streaming: None,
            supports_tokenization: None,
            false_eos_token: None,
            false_eos_token_id: None,
            supports_templates: None,
            supports_structured_output: None,
            observed_at_unix_secs,
            verification_status: BackendVerificationStatus::Unverified,
            verification_message: Some(message.into()),
            server_identity: None,
        }
    }

    /// Parse llama.cpp's documented, version-tolerant `/props` response.
    ///
    /// Older llama.cpp versions omit many keys.  Omission is not an error, while a
    /// present key with the wrong type is rejected so callers cannot trust a
    /// malformed response.
    pub fn from_llama_cpp_props(props: &Value, observed_at_unix_secs: u64) -> Result<Self, String> {
        let object = props
            .as_object()
            .ok_or_else(|| "/props response must be a JSON object".to_owned())?;
        let version = optional_string(object, "version")?;
        let build = optional_string_any(object, &["build", "build_info"])?;
        let served_model = optional_string_any(object, &["model_alias", "model"])?;
        let model_path = optional_string(object, "model_path")?;
        let total_context = optional_u32_any(object, &["n_ctx", "total_context"])?;
        let per_slot_context = optional_u32_any(object, &["n_ctx_per_seq", "n_ctx_slot"])?;
        let advertised_slots = optional_u32_any(object, &["n_parallel", "n_slots"])?;
        let (active_slots, slot_count) = slots(object)?;
        let max_parallel_slots = advertised_slots.or(slot_count);
        let defaults = optional_object(object, "default_generation_settings")?;
        let sampler_defaults = defaults
            .map(parse_sampler_defaults)
            .transpose()?
            .unwrap_or_default();
        let default_max_generation_tokens = defaults
            .map(|settings| {
                optional_u32_any(settings, &["n_predict", "max_tokens", "max_new_tokens"])
            })
            .transpose()?
            .flatten();
        let supports_streaming = optional_bool_any(object, &["supports_streaming", "streaming"])?;
        let supports_tokenization =
            optional_bool_any(object, &["supports_tokenization", "tokenization"])?;
        let false_eos_token = optional_string(object, "eos_token")?.filter(|token| {
            token.len() == 1
                && token.is_ascii()
                && !(token.starts_with("<|") && token.ends_with("|>"))
        });
        let supports_structured_output = optional_bool_any(
            object,
            &["supports_structured_output", "structured_output", "grammar"],
        )?;
        let supports_templates = match object.get("chat_template") {
            Some(Value::String(template)) => Some(!template.is_empty()),
            Some(Value::Null) | None => None,
            Some(_) => return Err("chat_template must be a string".to_owned()),
        };
        let model_metadata = model_path.map(|path| {
            Value::Object(Map::from_iter([(
                String::from("model_path"),
                Value::String(path),
            )]))
        });
        let server_identity = identity(&[
            version.as_deref(),
            build.as_deref(),
            served_model.as_deref(),
        ]);

        Ok(Self {
            backend: GenerationBackend::LlamaCpp,
            version,
            build,
            served_model,
            model_metadata,
            total_context,
            per_slot_context,
            active_slots,
            max_parallel_slots,
            default_max_generation_tokens,
            sampler_defaults,
            supports_streaming,
            supports_tokenization,
            false_eos_token,
            false_eos_token_id: None,
            supports_templates,
            supports_structured_output,
            observed_at_unix_secs,
            verification_status: BackendVerificationStatus::Verified,
            verification_message: None,
            server_identity,
        })
    }

    /// Parse the standard OpenAI `/v1/models` response used by vLLM and generic
    /// OpenAI-compatible backends.  It establishes model identity but intentionally
    /// leaves unavailable capabilities and sampler defaults unknown.
    pub fn from_openai_models(
        backend: GenerationBackend,
        models: &Value,
        observed_at_unix_secs: u64,
    ) -> Result<Self, String> {
        let object = models
            .as_object()
            .ok_or_else(|| "/models response must be a JSON object".to_owned())?;
        let data = object
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| "/models response must contain a data array".to_owned())?;
        let served_model = data
            .iter()
            .find_map(|entry| entry.get("id").and_then(Value::as_str))
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| "/models response did not expose a model id".to_owned())?;
        let model_metadata = data.first().cloned();
        let server_identity = identity(&[Some(&served_model)]);

        Ok(Self {
            backend,
            version: None,
            build: None,
            served_model: Some(served_model),
            model_metadata,
            total_context: None,
            per_slot_context: None,
            active_slots: None,
            max_parallel_slots: None,
            default_max_generation_tokens: None,
            sampler_defaults: BackendSamplerDefaults::default(),
            supports_streaming: None,
            supports_tokenization: None,
            false_eos_token: None,
            false_eos_token_id: None,
            supports_templates: None,
            supports_structured_output: None,
            observed_at_unix_secs,
            verification_status: BackendVerificationStatus::Verified,
            verification_message: None,
            server_identity,
        })
    }

    pub fn is_verified(&self) -> bool {
        self.verification_status == BackendVerificationStatus::Verified
    }
}

fn parse_sampler_defaults(settings: &Map<String, Value>) -> Result<BackendSamplerDefaults, String> {
    let (repetition_penalty, repetition_penalty_wire_key) = match settings.get("repeat_penalty") {
        Some(value) => (
            Some(number_as_f64(
                value,
                "default_generation_settings.repeat_penalty",
            )?),
            Some("repeat_penalty".to_owned()),
        ),
        None => match settings.get("repetition_penalty") {
            Some(value) => (
                Some(number_as_f64(
                    value,
                    "default_generation_settings.repetition_penalty",
                )?),
                Some("repetition_penalty".to_owned()),
            ),
            None => (None, None),
        },
    };
    Ok(BackendSamplerDefaults {
        temperature: optional_f64(settings, "temperature")?,
        top_p: optional_f64(settings, "top_p")?,
        top_k: optional_i32(settings, "top_k")?,
        min_p: optional_f64(settings, "min_p")?,
        repetition_penalty,
        repeat_last_n: optional_i64(settings, "repeat_last_n")?,
        repetition_penalty_wire_key,
    })
}

fn slots(object: &Map<String, Value>) -> Result<(Option<u32>, Option<u32>), String> {
    let Some(value) = object.get("slots") else {
        return Ok((None, None));
    };
    let slots = value
        .as_array()
        .ok_or_else(|| "slots must be an array".to_owned())?;
    let active = slots
        .iter()
        .filter_map(|slot| slot.get("is_processing"))
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| "slots[].is_processing must be a boolean".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let active_slots =
        (!active.is_empty()).then(|| active.into_iter().filter(|active| *active).count() as u32);
    Ok((active_slots, Some(slots.len() as u32)))
}

fn optional_object<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<Option<&'a Map<String, Value>>, String> {
    match object.get(field) {
        Some(Value::Object(value)) => Ok(Some(value)),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(format!("{field} must be an object")),
    }
}

fn optional_string(object: &Map<String, Value>, field: &str) -> Result<Option<String>, String> {
    match object.get(field) {
        Some(Value::String(value)) => Ok((!value.is_empty()).then(|| value.to_owned())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(format!("{field} must be a string")),
    }
}

fn optional_string_any(
    object: &Map<String, Value>,
    fields: &[&str],
) -> Result<Option<String>, String> {
    for field in fields {
        if object.contains_key(*field) {
            return optional_string(object, field);
        }
    }
    Ok(None)
}

fn optional_bool_any(object: &Map<String, Value>, fields: &[&str]) -> Result<Option<bool>, String> {
    for field in fields {
        match object.get(*field) {
            Some(Value::Bool(value)) => return Ok(Some(*value)),
            Some(Value::Null) | None => continue,
            Some(_) => return Err(format!("{field} must be a boolean")),
        }
    }
    Ok(None)
}

fn optional_u32_any(object: &Map<String, Value>, fields: &[&str]) -> Result<Option<u32>, String> {
    for field in fields {
        if object.contains_key(*field) {
            return optional_u32(object, field);
        }
    }
    Ok(None)
}

fn optional_u32(object: &Map<String, Value>, field: &str) -> Result<Option<u32>, String> {
    match object.get(field) {
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("{field} must be an unsigned 32-bit integer")),
        None => Ok(None),
    }
}

fn optional_i32(object: &Map<String, Value>, field: &str) -> Result<Option<i32>, String> {
    match object.get(field) {
        Some(value) => value
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("default_generation_settings.{field} must be a 32-bit integer")),
        None => Ok(None),
    }
}

fn optional_i64(object: &Map<String, Value>, field: &str) -> Result<Option<i64>, String> {
    match object.get(field) {
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("default_generation_settings.{field} must be an integer")),
        None => Ok(None),
    }
}

fn optional_f64(object: &Map<String, Value>, field: &str) -> Result<Option<f64>, String> {
    match object.get(field) {
        Some(value) => Ok(Some(number_as_f64(
            value,
            &format!("default_generation_settings.{field}"),
        )?)),
        None => Ok(None),
    }
}

fn number_as_f64(value: &Value, field: &str) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("{field} must be a finite number"))
}

fn identity(parts: &[Option<&str>]) -> Option<String> {
    let parts: Vec<_> = parts.iter().flatten().copied().collect();
    (!parts.is_empty()).then(|| parts.join("|"))
}
