//! Opt-in smoke coverage for a real, revision-pinned llama.cpp deployment.
//!
//! This module makes no network connection during ordinary tests. It runs only
//! when the required smoke environment is present; use it against a disposable
//! or scheduled integration deployment:
//!
//! ```text
//! HYMT_LLAMA_CPP_SMOKE_URL=http://127.0.0.1:8080/v1 \
//! HYMT_LLAMA_CPP_SMOKE_MODEL=hy-mt2-7b-q4 \
//! HYMT_LLAMA_CPP_SMOKE_BACKEND_REVISION=b<exact-llama.cpp-revision> \
//! HYMT_LLAMA_CPP_SMOKE_MODEL_REVISION=<exact-model-revision> \
//! HYMT_LLAMA_CPP_SMOKE_PARALLEL_SLOTS=1 \
//! HYMT_LLAMA_CPP_SMOKE_TRUNCATION_PROMPT='...' \
//! just test
//! ```
//!
//! The endpoint, backend revision, model revision, and known truncation prompt
//! are all required once the smoke URL is set; this avoids silently exercising a
//! floating model image.

use std::env;

use hymt_core::config::{GenerationSettings, HotConfig, Setting};
use hymt_core::runtime::BackendVerificationStatus;
use tempfile::TempDir;

use super::{ClientError, TranslationClient};

struct SmokeConfig {
    _dir: TempDir,
    config: HotConfig,
}

struct SmokeEnvironment {
    endpoint: String,
    model: String,
    backend_revision: String,
    model_revision: String,
    truncation_prompt: String,
    parallel_slots: u32,
}

impl SmokeEnvironment {
    fn from_env() -> Option<Self> {
        let endpoint = env::var("HYMT_LLAMA_CPP_SMOKE_URL").ok()?;
        let required = |name: &str| {
            env::var(name).unwrap_or_else(|_| {
                panic!(
                    "{name} is required when HYMT_LLAMA_CPP_SMOKE_URL opts into the llama.cpp smoke test"
                )
            })
        };
        Some(Self {
            endpoint,
            model: required("HYMT_LLAMA_CPP_SMOKE_MODEL"),
            backend_revision: required("HYMT_LLAMA_CPP_SMOKE_BACKEND_REVISION"),
            model_revision: required("HYMT_LLAMA_CPP_SMOKE_MODEL_REVISION"),
            truncation_prompt: required("HYMT_LLAMA_CPP_SMOKE_TRUNCATION_PROMPT"),
            parallel_slots: required("HYMT_LLAMA_CPP_SMOKE_PARALLEL_SLOTS")
                .parse()
                .expect("HYMT_LLAMA_CPP_SMOKE_PARALLEL_SLOTS must be a positive integer"),
        })
    }
}

fn config_for(env: &SmokeEnvironment, overrides: &str) -> SmokeConfig {
    let dir = tempfile::tempdir().expect("create smoke config directory");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[endpoint]\nurl = \"{}\"\nmodel = \"{}\"\nbackend = \"llama_cpp\"\n\n\
             [backend]\ntotal_context = 4096\nparallel_slots = {}\n\n\
             [translation]\nconcurrency = {}\nmax_output_tokens = 1\nstream = false\ntimeout = 120\n\n\
             [completeness]\nmax_retries = 0\n\n{overrides}",
            env.endpoint, env.model, env.parallel_slots, env.parallel_slots
        ),
    )
    .expect("write smoke config");
    SmokeConfig {
        config: HotConfig::from_path(&path).expect("load smoke config"),
        _dir: dir,
    }
}

#[tokio::test]
async fn llama_cpp_smoke_verifies_pinned_service_defaults_overrides_and_truncation() {
    let Some(env) = SmokeEnvironment::from_env() else {
        eprintln!("skipping real llama.cpp smoke; set HYMT_LLAMA_CPP_SMOKE_URL to opt in");
        return;
    };

    let defaults = config_for(&env, "");
    let default_client = TranslationClient::new(defaults.config.clone()).expect("default client");
    let default_report = default_client
        .preflight_backend()
        .await
        .expect("/props preflight");
    assert_eq!(
        default_report.runtime.verification_status,
        BackendVerificationStatus::Verified,
        "the smoke endpoint must expose parseable /props metadata"
    );
    assert_eq!(
        default_report.runtime.served_model.as_deref(),
        Some(env.model.as_str())
    );
    let backend_identity = [
        default_report
            .runtime
            .version
            .as_deref()
            .unwrap_or_default(),
        default_report.runtime.build.as_deref().unwrap_or_default(),
        default_report
            .runtime
            .server_identity
            .as_deref()
            .unwrap_or_default(),
    ]
    .join(" ");
    assert!(
        backend_identity.contains(&env.backend_revision),
        "/props backend revision drifted; expected {}, got {backend_identity}",
        env.backend_revision
    );
    let model_metadata = serde_json::to_string(&default_report.runtime.model_metadata)
        .expect("model metadata must serialize");
    assert!(
        model_metadata.contains(&env.model_revision),
        "/props model revision drifted; expected {}, got {model_metadata}",
        env.model_revision
    );
    assert_eq!(
        default_report.configured.sampler_overrides,
        GenerationSettings::server_defaults(),
        "omitted sampler fields must inherit service defaults"
    );
    assert!(
        default_report.runtime.sampler_defaults.is_complete(),
        "/props must report the resolved service sampler defaults"
    );
    assert_eq!(default_report.configured.parallel_slots, env.parallel_slots);
    assert_eq!(
        default_client.inner.concurrency, env.parallel_slots as usize,
        "the client semaphore must be bounded by the configured slot count"
    );
    assert_eq!(
        default_report.runtime.max_parallel_slots,
        Some(env.parallel_slots),
        "/props must agree that concurrency is bounded as configured"
    );

    let overrides = config_for(
        &env,
        "[inference.override]\ntemperature = 0.2\ntop_k = \"disabled\"\nrepetition_penalty = 1.07\n",
    );
    let override_client =
        TranslationClient::new(overrides.config.clone()).expect("override client");
    let override_report = override_client
        .preflight_backend()
        .await
        .expect("override /props preflight");
    assert_eq!(
        override_report.configured.sampler_overrides,
        GenerationSettings {
            temperature: Setting::Value(0.2),
            top_p: Setting::ServerDefault,
            top_k: Setting::Disabled,
            repetition_penalty: Setting::Value(1.07),
            min_p: Setting::ServerDefault,
            repeat_last_n: Setting::ServerDefault,
        },
        "explicit config must produce distinct resolved sampler settings"
    );
    assert_eq!(
        override_report.configured.parallel_slots,
        env.parallel_slots
    );

    let truncation = default_client
        .translate(&env.truncation_prompt)
        .await
        .expect_err("the pinned truncation fixture must finish at max_tokens");
    assert!(
        matches!(truncation, ClientError::Truncated),
        "finish_reason=length must map to ClientError::Truncated, got {truncation:?}"
    );
}
