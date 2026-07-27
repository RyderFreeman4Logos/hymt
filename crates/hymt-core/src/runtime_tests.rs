use crate::config::{GenerationBackend, HotConfig};
use crate::runtime::{BackendRuntimeInfo, BackendVerificationStatus};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_RUNTIME_CONFIG_ID: AtomicUsize = AtomicUsize::new(0);

fn with_config<T>(name: &str, contents: &str, f: impl FnOnce(&HotConfig) -> T) -> T {
    let id = NEXT_RUNTIME_CONFIG_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "hymt-runtime-{name}-{}-{id}.toml",
        std::process::id()
    ));
    fs::write(&path, contents).expect("write config fixture");
    let config = HotConfig::from_path(&path).expect("load config fixture");
    let output = f(&config);
    fs::remove_file(path).expect("remove config fixture");
    output
}

fn complete_llama_props() -> serde_json::Value {
    serde_json::json!({
        "build_info": "b123",
        "version": "llama.cpp-6000",
        "model_alias": "served-hy-mt2",
        "model_path": "/models/served-hy-mt2.gguf",
        "n_ctx": 24576,
        "n_ctx_per_seq": 8192,
        "n_parallel": 3,
        "slots": [
            {"id": 0, "is_processing": true},
            {"id": 1, "is_processing": false},
            {"id": 2, "is_processing": true}
        ],
        "chat_template": "chatml",
        "default_generation_settings": {
            "n_predict": 2048,
            "temperature": 0.7,
            "top_p": 0.6,
            "top_k": 20,
            "min_p": 0.1,
            "repeat_penalty": 1.05,
            "repeat_last_n": 64
        }
    })
}

fn cacheable_llama_config() -> &'static str {
    r#"
[endpoint]
url = "http://127.0.0.1:8401/v1"
model = "served-hy-mt2"
backend = "llama_cpp"

[backend]
total_context = 24576
parallel_slots = 3

[translation]
max_output_tokens = 4096

[inference.override]
temperature = 0.7
top_p = 0.6
top_k = 20
repetition_penalty = 1.05
min_p = 0.1
repeat_last_n = 64
"#
}

#[test]
fn parses_llama_props_into_normalized_runtime_info() {
    let info = BackendRuntimeInfo::from_llama_cpp_props(&complete_llama_props(), 42)
        .expect("complete llama.cpp props parse");

    assert_eq!(info.backend, GenerationBackend::LlamaCpp);
    assert_eq!(info.version.as_deref(), Some("llama.cpp-6000"));
    assert_eq!(info.build.as_deref(), Some("b123"));
    assert_eq!(info.served_model.as_deref(), Some("served-hy-mt2"));
    assert_eq!(info.total_context, Some(24_576));
    assert_eq!(info.per_slot_context, Some(8_192));
    assert_eq!(info.active_slots, Some(2));
    assert_eq!(info.max_parallel_slots, Some(3));
    assert_eq!(info.default_max_generation_tokens, Some(2_048));
    assert_eq!(info.sampler_defaults.temperature, Some(0.7));
    assert_eq!(info.sampler_defaults.repetition_penalty, Some(1.05));
    assert_eq!(
        info.sampler_defaults.repetition_penalty_wire_key.as_deref(),
        Some("repeat_penalty")
    );
    assert_eq!(info.supports_templates, Some(true));
    assert_eq!(info.observed_at_unix_secs, 42);
    assert_eq!(
        info.verification_status,
        BackendVerificationStatus::Verified
    );
}

#[test]
fn nested_llama_props_params_attest_server_default_sampling_for_cache() {
    let props = serde_json::json!({
        "build_info": "b123",
        "model_alias": "served-hy-mt2",
        "default_generation_settings": {
            "repeat_penalty": 1.5,
            "params": {
                "n_predict": 2048,
                "temperature": 0.7,
                "top_p": 0.6,
                "top_k": 20,
                "min_p": 0.1,
                "repeat_penalty": 1.05,
                "repeat_last_n": 64
            }
        }
    });
    let runtime =
        BackendRuntimeInfo::from_llama_cpp_props(&props, 42).expect("nested llama.cpp props parse");

    assert_eq!(runtime.default_max_generation_tokens, Some(2_048));
    assert_eq!(runtime.sampler_defaults.temperature, Some(0.7));
    assert_eq!(runtime.sampler_defaults.top_p, Some(0.6));
    assert_eq!(runtime.sampler_defaults.top_k, Some(20));
    assert_eq!(runtime.sampler_defaults.min_p, Some(0.1));
    assert_eq!(runtime.sampler_defaults.repetition_penalty, Some(1.05));
    assert_eq!(runtime.sampler_defaults.repeat_last_n, Some(64));
    assert_eq!(
        runtime
            .sampler_defaults
            .repetition_penalty_wire_key
            .as_deref(),
        Some("repeat_penalty")
    );
    assert!(runtime.sampler_defaults.is_complete());

    with_config(
        "nested-props-cacheable",
        r#"
[endpoint]
url = "http://127.0.0.1:8401/v1"
model = "served-hy-mt2"
backend = "llama_cpp"
"#,
        |config| {
            config.set_backend_runtime_info(runtime);
            assert!(
                config
                    .inference_fingerprint("default", "")
                    .expect("fingerprint")
                    .is_cache_verified(),
                "complete nested service defaults plus model identity attest cache scope"
            );
        },
    );
}

#[test]
fn older_llama_props_leave_unknown_values_unknown() {
    let info = BackendRuntimeInfo::from_llama_cpp_props(
        &serde_json::json!({"build_info": "old-server", "n_ctx": 4096}),
        42,
    )
    .expect("older props remain usable");

    assert_eq!(info.total_context, Some(4_096));
    assert_eq!(info.per_slot_context, None);
    assert_eq!(info.served_model, None);
    assert_eq!(info.sampler_defaults.temperature, None);
    assert_eq!(info.supports_templates, None);
    assert_eq!(info.default_max_generation_tokens, None);
}

#[test]
fn llama_props_identify_ascii_punctuation_eos_as_false_when_template_uses_control_eos() {
    let info = BackendRuntimeInfo::from_llama_cpp_props(
        &serde_json::json!({"eos_token": "$", "chat_template": "<|eos|>"}),
        42,
    )
    .expect("plain ASCII eos token must be parseable");

    assert_eq!(info.false_eos_token.as_deref(), Some("$"));
    assert_eq!(info.false_eos_token_id, None);
}

#[test]
fn llama_props_preserve_legitimate_single_ascii_eos_used_by_template() {
    let info = BackendRuntimeInfo::from_llama_cpp_props(
        &serde_json::json!({"eos_token": "$", "chat_template": "{{ content }}$"}),
        42,
    )
    .expect("single-character EOS token must be parseable");

    assert_eq!(info.false_eos_token, None);
    assert_eq!(info.false_eos_token_id, None);
}

#[test]
fn llama_props_do_not_flag_chat_control_eos_as_false() {
    let info =
        BackendRuntimeInfo::from_llama_cpp_props(&serde_json::json!({"eos_token": "<|eos|>"}), 42)
            .expect("chat control eos token must be parseable");

    assert_eq!(info.false_eos_token, None);
    assert_eq!(info.false_eos_token_id, None);
}

#[test]
fn hot_reload_clears_stale_backend_runtime_state() {
    with_config("runtime-reload", cacheable_llama_config(), |config| {
        config.set_backend_runtime_info(
            BackendRuntimeInfo::from_llama_cpp_props(&complete_llama_props(), 42)
                .expect("verified runtime"),
        );
        assert!(config.backend_runtime_info().is_some());

        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(
            config.path(),
            cacheable_llama_config().replace("8401", "8402"),
        )
        .expect("rewrite config fixture");

        assert!(config.maybe_reload().expect("reload changed config"));
        assert!(
            config.backend_runtime_info().is_none(),
            "a runtime probe must not survive an endpoint/config snapshot change"
        );
    });
}

#[test]
fn verified_runtime_without_served_model_bypasses_cache() {
    with_config("identity-incomplete", cacheable_llama_config(), |config| {
        let runtime = BackendRuntimeInfo::from_llama_cpp_props(
            &serde_json::json!({
                "build_info": "old-server",
                "n_ctx": 24576,
                "n_ctx_per_seq": 8192,
                "default_generation_settings": {
                    "temperature": 0.7,
                    "top_p": 0.6,
                    "top_k": 20,
                    "min_p": 0.1,
                    "repeat_penalty": 1.05,
                    "repeat_last_n": 64
                }
            }),
            42,
        )
        .expect("older props remain parseable");
        assert!(runtime.is_verified());
        assert_eq!(runtime.served_model, None);
        config.set_backend_runtime_info(runtime);

        assert!(
            !config
                .inference_fingerprint("default", "")
                .expect("fingerprint")
                .is_cache_verified(),
            "a verified response without model identity cannot attest a cache namespace"
        );
    });
}

#[test]
fn runtime_fingerprint_excludes_active_slots() {
    with_config("active-slots", cacheable_llama_config(), |config| {
        let busy_runtime = BackendRuntimeInfo::from_llama_cpp_props(&complete_llama_props(), 42)
            .expect("verified runtime");
        let mut idle_runtime = busy_runtime.clone();
        idle_runtime.active_slots = Some(0);

        config.set_backend_runtime_info(busy_runtime);
        let busy = config
            .inference_fingerprint("default", "")
            .expect("busy fingerprint");
        config.set_backend_runtime_info(idle_runtime);
        let idle = config
            .inference_fingerprint("default", "")
            .expect("idle fingerprint");

        assert_eq!(busy.hash(), idle.hash());
        let canonical: serde_json::Value =
            serde_json::from_str(busy.canonical_json()).expect("canonical JSON");
        assert!(canonical["runtime"].get("active_slots").is_none());
    });
}

#[test]
fn malformed_props_are_rejected_without_inventing_runtime_metadata() {
    let error =
        BackendRuntimeInfo::from_llama_cpp_props(&serde_json::json!({"n_ctx": "not-a-number"}), 42)
            .expect_err("wrongly typed context must not be accepted");

    assert!(
        error.contains("n_ctx"),
        "error should name malformed field: {error}"
    );
}

#[test]
fn unverified_runtime_uses_conservative_planning_and_verified_runtime_fingerprints() {
    with_config(
        "resolved-config",
        r#"
[endpoint]
url = "http://127.0.0.1:8401/v1"
model = "served-hy-mt2"
backend = "llama_cpp"

[backend]
total_context = 24576
parallel_slots = 3

[translation]
max_output_tokens = 4096

[inference.override]
temperature = 0.7
top_p = 0.6
top_k = 20
repetition_penalty = 1.05
min_p = 0.1
repeat_last_n = 64
"#,
        |config| {
            config.set_backend_runtime_info(BackendRuntimeInfo::unverified(
                GenerationBackend::LlamaCpp,
                42,
                "endpoint unavailable",
            ));
            assert_eq!(config.resolved_per_request_context(), 4_096);

            let verified = BackendRuntimeInfo::from_llama_cpp_props(&complete_llama_props(), 43)
                .expect("verified props");
            config.set_backend_runtime_info(verified);
            assert_eq!(config.resolved_per_request_context(), 8_192);
            assert_eq!(config.resolved_max_output_tokens(), 2_048);

            let fingerprint = config
                .inference_fingerprint("default", "")
                .expect("fingerprint includes resolved runtime state");
            let canonical: serde_json::Value =
                serde_json::from_str(fingerprint.canonical_json()).expect("canonical JSON");
            assert_eq!(canonical["runtime"]["served_model"], "served-hy-mt2");
            assert_eq!(canonical["runtime"]["per_slot_context"], 8192);
            assert!(fingerprint.is_cache_verified());
        },
    );
}
