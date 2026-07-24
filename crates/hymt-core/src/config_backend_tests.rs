use super::{GenerationBackend, HotConfig};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_CONFIG_ID: AtomicUsize = AtomicUsize::new(0);

fn with_config<T>(name: &str, contents: &str, f: impl FnOnce(&HotConfig) -> T) -> T {
    let id = NEXT_CONFIG_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "hymt-backend-payload-{name}-{}-{id}.toml",
        std::process::id()
    ));
    fs::write(&path, contents).expect("write config fixture");
    let config = HotConfig::from_path(&path);
    let output = f(&config.expect("load config fixture"));
    fs::remove_file(path).expect("remove config fixture");
    output
}

#[test]
fn missing_endpoint_backend_defaults_to_conservative_openai_compatible() {
    with_config(
        "missing-endpoint-backend",
        "[endpoint]\nurl = \"http://localhost:8401/v1\"\n",
        |config| {
            assert_eq!(
                config.generation_backend().expect("backend"),
                GenerationBackend::OpenAiCompatible
            );
        },
    );
}

#[test]
fn endpoint_backend_selects_vllm_and_accepts_its_documented_extensions() {
    with_config(
        "vllm-endpoint-backend",
        "[endpoint]\nbackend = \"vllm\"\n\n[inference.override]\ntop_k = \"disabled\"\nrepetition_penalty = 1.05\nmin_p = 0.1\n",
        |config| {
            assert_eq!(
                config.generation_backend().expect("backend"),
                GenerationBackend::Vllm
            );
        },
    );
}

#[test]
fn fingerprint_uses_backend_normalized_effective_sampling() {
    with_config(
        "fingerprint-normalized-sampling",
        "[endpoint]\nmodel = \"served-model\"\nprofile = \"hy_mt2_7b\"\nbackend = \"openai_compatible\"\n",
        |config| {
            let fingerprint = config
                .inference_fingerprint("default", "")
                .expect("fingerprint");
            let canonical: serde_json::Value =
                serde_json::from_str(fingerprint.canonical_json()).expect("canonical JSON");
            let generation = canonical["generation"]
                .as_object()
                .expect("generation object");

            assert_eq!(canonical["backend"], "openai_compatible");
            assert_eq!(generation["temperature"], 0.7);
            assert_eq!(generation["top_p"], 0.6);
            for field in ["top_k", "repetition_penalty", "min_p", "repeat_last_n"] {
                assert!(
                    !generation.contains_key(field),
                    "strict backend fingerprint must omit unsupported {field}"
                );
            }
        },
    );
}

#[test]
fn legacy_inference_backend_is_rejected_instead_of_silently_selecting_an_adapter() {
    let path = std::env::temp_dir().join(format!(
        "hymt-backend-payload-legacy-{}.toml",
        std::process::id()
    ));
    fs::write(&path, "[inference]\nbackend = \"llama_cpp\"\n").expect("write config fixture");

    let error = HotConfig::from_path(&path).expect_err("legacy backend location must fail");
    fs::remove_file(path).expect("remove config fixture");

    assert!(
        error.to_string().contains("endpoint.backend"),
        "diagnostic must identify the explicit replacement field: {error}"
    );
}
