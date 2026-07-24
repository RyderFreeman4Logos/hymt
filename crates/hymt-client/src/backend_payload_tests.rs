use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::ChatPayload;
use hymt_core::config::{GenerationBackend, GenerationSettings, HotConfig, Setting};

static NEXT_CONFIG_ID: AtomicUsize = AtomicUsize::new(0);

fn settings() -> GenerationSettings {
    GenerationSettings {
        temperature: Setting::Value(0.7),
        top_p: Setting::Value(0.6),
        top_k: Setting::Disabled,
        repetition_penalty: Setting::Value(1.05),
        min_p: Setting::Disabled,
        repeat_last_n: Setting::Disabled,
    }
}

fn payload(backend: GenerationBackend, stream: bool) -> serde_json::Value {
    serde_json::to_value(ChatPayload::from_generation_settings(
        "translate this",
        128,
        "hy-mt2".to_owned(),
        stream,
        &settings(),
        backend,
    ))
    .expect("payload must serialize")
}

fn payload_from_config(contents: &str) -> serde_json::Value {
    let id = NEXT_CONFIG_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "hymt-client-payload-{}-{id}.toml",
        std::process::id()
    ));
    fs::write(&path, contents).expect("write config fixture");

    let result = (|| {
        let config = HotConfig::from_path(&path).expect("load config fixture");
        let settings = config.generation_settings().expect("generation settings");
        let backend = config.generation_backend().expect("generation backend");
        serde_json::to_value(ChatPayload::from_generation_settings(
            "translate this",
            128,
            "hy-mt2".to_owned(),
            false,
            &settings,
            backend,
        ))
        .expect("payload must serialize")
    })();

    fs::remove_file(path).expect("remove config fixture");
    result
}

#[test]
fn hy_mt2_7b_profile_defaults_are_serialized_for_llama_cpp() {
    let object =
        payload_from_config("[endpoint]\nprofile = \"hy_mt2_7b\"\nbackend = \"llama_cpp\"\n");

    assert_eq!(object["temperature"], 0.7);
    assert_eq!(object["top_p"], 0.6);
    assert_eq!(object["top_k"], 20);
    assert_eq!(object["repeat_penalty"], 1.05);
    assert!(object.get("repetition_penalty").is_none());
    assert!(object.get("min_p").is_none());
    assert!(object.get("repeat_last_n").is_none());
}

#[test]
fn openai_compatible_omits_nonstandard_sampling_extensions() {
    let object = payload(GenerationBackend::OpenAiCompatible, false);

    assert_eq!(object["temperature"], 0.7);
    assert_eq!(object["top_p"], 0.6);
    for key in [
        "top_k",
        "repeat_penalty",
        "repetition_penalty",
        "min_p",
        "repeat_last_n",
    ] {
        assert!(
            object.get(key).is_none(),
            "openai_compatible must not send nonstandard {key} without an explicit adapter"
        );
    }
}

#[test]
fn llama_cpp_and_vllm_use_distinct_repetition_penalty_wire_keys() {
    let llama = payload(GenerationBackend::LlamaCpp, false);
    assert_eq!(llama["repeat_penalty"], 1.05);
    assert!(llama.get("repetition_penalty").is_none());

    let vllm = payload(GenerationBackend::Vllm, false);
    assert_eq!(vllm["repetition_penalty"], 1.05);
    assert!(vllm.get("repeat_penalty").is_none());
}

#[test]
fn disabled_top_k_uses_each_adapter_documented_wire_value() {
    let llama = payload(GenerationBackend::LlamaCpp, false);
    assert_eq!(llama["top_k"], 0);

    let vllm = payload(GenerationBackend::Vllm, false);
    assert_eq!(vllm["top_k"], -1);
}

#[test]
fn server_default_omits_every_backend_specific_sampling_field() {
    for backend in [GenerationBackend::LlamaCpp, GenerationBackend::Vllm] {
        let object = serde_json::to_value(ChatPayload::from_generation_settings(
            "translate this",
            128,
            String::new(),
            false,
            &GenerationSettings::server_defaults(),
            backend,
        ))
        .expect("payload must serialize");

        for key in [
            "top_k",
            "repeat_penalty",
            "repetition_penalty",
            "min_p",
            "repeat_last_n",
        ] {
            assert!(
                object.get(key).is_none(),
                "{backend:?} server-default payload must omit {key}"
            );
        }
    }
}

#[test]
fn streaming_and_non_streaming_use_the_same_llama_sampling_policy() {
    let non_streaming = payload(GenerationBackend::LlamaCpp, false);
    let mut streaming = payload(GenerationBackend::LlamaCpp, true);

    assert!(non_streaming.get("stream").is_none());
    assert_eq!(streaming["stream"], true);
    streaming
        .as_object_mut()
        .expect("payload object")
        .remove("stream");
    assert_eq!(streaming, non_streaming);
}
