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
fn fingerprint_omits_profile_sampling_when_server_owns_defaults() {
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
            assert!(
                generation.is_empty(),
                "pure server-default requests must not claim profile sampling values: {generation:?}"
            );
            assert!(
                !fingerprint.is_cache_verified(),
                "server-owned sampling has no client-verifiable cache namespace"
            );
        },
    );
}

#[test]
fn partial_sampler_override_does_not_verify_the_cache_identity() {
    with_config(
        "fingerprint-partial-sampling",
        "[endpoint]\nmodel = \"served-model\"\nbackend = \"llama_cpp\"\n\n[inference.override]\ntemperature = 0.7\n",
        |config| {
            let fingerprint = config
                .inference_fingerprint("default", "")
                .expect("fingerprint");
            let canonical: serde_json::Value =
                serde_json::from_str(fingerprint.canonical_json()).expect("canonical JSON");
            let generation = canonical["generation"]
                .as_object()
                .expect("generation object");

            assert_eq!(generation["temperature"], 0.7);
            assert!(
                !fingerprint.is_cache_verified(),
                "any service-owned sampler makes the cache identity unverifiable"
            );
        },
    );
}

#[test]
fn supplied_llama_cpp_services_pin_the_documented_sampling_profile() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for service in ["hy-mt2-quality.service", "hy-mt2-throughput.service"] {
        let unit = std::fs::read_to_string(repo_root.join("services").join(service))
            .expect("read supplied service unit");
        for flag in [
            "--temp 0.7",
            "--top-p 0.6",
            "--top-k 20",
            "--repeat-penalty 1.05",
            "--min-p 0",
            "--repeat-last-n 64",
        ] {
            assert!(unit.contains(flag), "{service} must explicitly set {flag}");
        }
    }
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
