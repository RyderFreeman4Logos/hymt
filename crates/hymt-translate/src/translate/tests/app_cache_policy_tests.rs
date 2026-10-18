use super::*;
use crate::exec_wrapper::translate_cached;
use hymt_cache::ExecCache;

const SOURCE: &str =
    "This report explains why reliable translation history and cache behavior matter.";

#[tokio::test]
async fn disabled_whole_cache_bypasses_existing_tiers_and_preserves_history() {
    for shared in [false, true] {
        let planning = make_stream_config("http://127.0.0.1:1/v1");
        let segmenter = fallback_segmenter();
        let opts = PromptOpts::default();
        let plan = plan_translation(
            SOURCE,
            "zh",
            &planning,
            &segmenter,
            &TemplateType::Default,
            &opts,
        )
        .unwrap();
        assert_eq!(plan.segment_count(), 1);
        let fresh = planned_complete_zh_translations(&plan)
            .into_iter()
            .next()
            .unwrap();
        let server = start_capturing_mock_server(vec![
            MockResponse::Json(fresh.clone()),
            MockResponse::Json(fresh.clone()),
        ])
        .await;
        let config = make_stream_config(&server.endpoint_url);
        let client = TranslationClient::new(config.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(dir.path().join("history.db"));
        let cache =
            ExecCache::with_user_path(dir.path().join("shared.db"), dir.path().join("user.db"));
        let policy = effective_document_translation_policy(&opts, &config);
        let stale = "old whole-output translation";
        if shared {
            cache
                .store_shared("man", "test", SOURCE, "zh", policy, stale)
                .unwrap();
        } else {
            cache
                .store_user("man", "test", SOURCE, "zh", policy, stale)
                .unwrap();
        }
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: false,
        };
        for _ in 0..2 {
            assert_eq!(
                translate_cached("man", "test", SOURCE, "zh", &cache, &opts, &ctx)
                    .await
                    .unwrap(),
                fresh
            );
        }
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        assert_eq!(history.fetch_recent(None).unwrap().len(), 2);
        assert_eq!(
            cache
                .find("man", "test", SOURCE, "zh", policy)
                .unwrap()
                .as_deref(),
            Some(stale),
            "disabled caching must neither overwrite nor delete old entries"
        );
    }
}

#[tokio::test]
async fn disabled_whole_cache_does_not_create_cache_files() {
    let planning = make_stream_config("http://127.0.0.1:1/v1");
    let segmenter = fallback_segmenter();
    let opts = PromptOpts::default();
    let plan = plan_translation(
        SOURCE,
        "zh",
        &planning,
        &segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    assert_eq!(plan.segment_count(), 1);
    let fresh = planned_complete_zh_translations(&plan)
        .into_iter()
        .next()
        .unwrap();
    let server = start_capturing_mock_server(vec![MockResponse::Json(fresh.clone())]).await;
    let config = make_stream_config(&server.endpoint_url);
    let client = TranslationClient::new(config.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(dir.path().join("history.db"));
    let shared = dir.path().join("shared.db");
    let user = dir.path().join("user.db");
    let cache = ExecCache::with_user_path(&shared, &user);
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: false,
    };
    assert_eq!(
        translate_cached("man", "test", SOURCE, "zh", &cache, &opts, &ctx)
            .await
            .unwrap(),
        fresh
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(history.fetch_recent(None).unwrap().len(), 1);
    assert!(!shared.exists());
    assert!(!user.exists());
}

#[tokio::test]
async fn explicit_library_cache_opt_in_reuses_whole_output() {
    let planning = make_stream_config("http://127.0.0.1:1/v1");
    let segmenter = fallback_segmenter();
    let opts = PromptOpts::default();
    let plan = plan_translation(
        SOURCE,
        "zh",
        &planning,
        &segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    let fresh = planned_complete_zh_translations(&plan)
        .into_iter()
        .next()
        .unwrap();
    let server = start_capturing_mock_server(vec![MockResponse::Json(fresh.clone())]).await;
    let config = make_stream_config(&server.endpoint_url);
    let client = TranslationClient::new(config.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(dir.path().join("history.db"));
    let cache = ExecCache::with_user_path(dir.path().join("shared.db"), dir.path().join("user.db"));
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: true,
    };
    for _ in 0..2 {
        assert_eq!(
            translate_cached("man", "test", SOURCE, "zh", &cache, &opts, &ctx)
                .await
                .unwrap(),
            fresh
        );
    }
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

// A fresh invocation must validate its retained proof before strict live preflight.
const REPLY: &str = "这份报告说明可靠的翻译历史记录和缓存行为为什么重要。";

#[tokio::test]
async fn populated_stream_proof_binds_captured_requests_for_both_priority_settings() {
    for priority in [true, false] {
        let source = streaming_regression_source();
        let segmenter = fallback_segmenter();
        let planning = make_stream_config_with_fcp("http://127.0.0.1:1/v1", priority);
        let opts = PromptOpts::default();
        let plan = plan_translation(
            &source,
            "zh",
            &planning,
            &segmenter,
            &TemplateType::Default,
            &opts,
        )
        .unwrap();
        assert!(plan.segment_count() > 1);
        let replies = planned_complete_zh_translations(&plan);
        let server = start_capturing_mock_server(
            replies
                .into_iter()
                .enumerate()
                .map(|(i, text)| {
                    if i == 0 {
                        MockResponse::Sse(vec![text])
                    } else {
                        MockResponse::Json(text)
                    }
                })
                .collect(),
        )
        .await;
        let config = make_stream_config_with_fcp(&server.endpoint_url, priority);
        config.set_backend_runtime_info(
            hymt_core::runtime::BackendRuntimeInfo::from_llama_cpp_props(
                &serde_json::json!({"model":"test-model", "n_ctx_per_seq":512}),
                123,
            )
            .unwrap(),
        );
        let client = TranslationClient::new(config.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(dir.path().join("history.db"));
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: true,
        };
        let (tx, mut rx) = mpsc::channel(64);
        translate_text_stream(&source, "zh", &TemplateType::Default, &opts, &ctx, tx)
            .await
            .unwrap();
        assert!(rx.recv().await.is_some());
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &opts,
        )
        .unwrap();
        let (key, _) = offline_replay::population_proof(
            (
                &source,
                history.fetch_recent(Some(1)).unwrap()[0]
                    .output_text
                    .as_deref()
                    .unwrap(),
            ),
            "zh",
            &TemplateType::Default,
            &opts,
            &ctx,
            &plan,
            true,
        )
        .unwrap()
        .expect("initial complete streaming run qualifies");
        let candidates = history
            .find_replay_candidates(&segment_cache_hash(&source), &key)
            .unwrap();
        assert_eq!(candidates.len(), 1);
        let proof: serde_json::Value = serde_json::from_str(&candidates[0].1).unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), plan.segment_count());
        let bodies: Vec<_> = requests
            .iter()
            .map(|request| {
                let body = request.split_once("\r\n\r\n").unwrap().1;
                (
                    body,
                    serde_json::from_str::<serde_json::Value>(body).unwrap(),
                )
            })
            .collect();
        let mut hash = Sha256::new();
        let mut bind = |bytes: &[u8]| {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        };
        bind(
            &serde_json::to_vec(&serde_json::json!({
                "source_tokens":plan.source_tokens,
                "available_source_tokens":plan.available_source_tokens,
                "budget":format!("{:?}",plan.token_budget),
                "document":plan.document_plan,"indexes":plan.segment_section_indexes,
                "groups":plan.segment_section_groups,
                "original_paragraphs":format!("{:?}", plan.original_paragraphs),
                "coherent_paragraphs":plan.coherent_paragraphs,
            }))
            .unwrap(),
        );
        for (index, segment) in plan.segments.iter().enumerate() {
            let prompt = build_prompt(segment, "zh", &TemplateType::Default, &opts).unwrap();
            let (body, payload) = bodies
                .iter()
                .find(|(_, payload)| payload["messages"][0]["content"] == prompt)
                .unwrap();
            assert_eq!(payload["stream"].as_bool(), (index == 0).then_some(true));
            bind(segment.as_bytes());
            bind(prompt.as_bytes());
            bind(body.as_bytes());
        }
        assert_eq!(
            proof["requests"],
            format!("{:x}", hash.finalize()),
            "persisted proof must bind actual wire bytes; first_chunk_priority={priority}"
        );
    }
}

struct OfflineFixture {
    _dir: tempfile::TempDir,
    config: HotConfig,
    history: HistoryDB,
    segmenter: hymt_segment::Segmenter,
    key: String,
    proof: String,
    record: TaskRecord,
}

impl OfflineFixture {
    async fn warm(streaming: bool) -> Self {
        let server = start_capturing_mock_server(vec![if streaming {
            MockResponse::Sse(vec![REPLY.to_owned()])
        } else {
            MockResponse::Json(REPLY.to_owned())
        }])
        .await;
        let config = make_stream_config(&server.endpoint_url);
        let path = config.path().to_owned();
        let raw = std::fs::read_to_string(&path).unwrap().replace(
            "timeout = 5",
            "timeout = 0.1\nstrict_backend_preflight = true",
        );
        std::fs::write(&path, format!("{raw}\n[cache]\nenabled = true\n")).unwrap();
        let config = HotConfig::from_path(&path).unwrap();
        let mut runtime = hymt_core::runtime::BackendRuntimeInfo::from_llama_cpp_props(
            &serde_json::json!({"model":"test-model", "model_path":"test-model", "n_ctx_per_seq":512}), 123).unwrap();
        runtime.false_eos_token = Some("!".into());
        runtime.false_eos_token_id = Some(7);
        config.set_backend_runtime_info(runtime);
        let client = TranslationClient::new(config.clone()).unwrap();
        let segmenter = fallback_segmenter();
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(dir.path().join("history.db"));
        let opts = PromptOpts::default();
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: true,
        };
        if streaming {
            let (tx, mut rx) = mpsc::channel(16);
            translate_text_stream_with_mode(
                SOURCE,
                "zh",
                &TemplateType::Default,
                &opts,
                &ctx,
                StreamOutputMode::Validated,
                tx,
            )
            .await
            .unwrap();
            assert!(matches!(rx.recv().await, Some(StreamEvent::Token(_))));
        } else {
            let warm = translate_text(SOURCE, "zh", &TemplateType::Default, &opts, &ctx)
                .await
                .unwrap();
            assert_eq!(warm.text, REPLY);
        }
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        let plan = plan_translation(
            SOURCE,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &opts,
        )
        .unwrap();
        let (key, proof) = offline_replay::population_proof(
            (SOURCE, REPLY),
            "zh",
            &TemplateType::Default,
            &opts,
            &ctx,
            &plan,
            streaming,
        )
        .unwrap()
        .unwrap();
        let record = history.fetch_recent(None).unwrap().pop().unwrap();
        assert_eq!(
            history
                .find_replay_candidates(&segment_cache_hash(SOURCE), &key)
                .unwrap()
                .len(),
            1
        );
        drop(server);
        Self {
            _dir: dir,
            config,
            history,
            segmenter,
            key,
            proof,
            record,
        }
    }

    fn fresh(&self) -> HotConfig {
        HotConfig::from_path(self.config.path()).unwrap()
    }

    fn replace(&self, proof: &str, record: &TaskRecord) {
        self.history.clear().unwrap();
        self.history
            .insert_task_with_replay_proof(record, Some((&self.key, proof)))
            .unwrap();
    }

    fn candidate(&self, config: &HotConfig, streaming: bool) -> bool {
        offline_replay::find_replay(
            SOURCE,
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            &TranslationCtx {
                config,
                client: &TranslationClient::with_concurrency(config.clone(), 1).unwrap(),
                segmenter: &self.segmenter,
                history: &self.history,
                cache_enabled: true,
            },
            streaming,
        )
        .unwrap()
        .is_some()
    }
}

#[tokio::test]
async fn populated_cache_replays_before_strict_preflight() {
    let fixture = OfflineFixture::warm(false).await;
    let offline = fixture.fresh();
    let client = TranslationClient::new(offline.clone()).unwrap();
    let ctx = TranslationCtx {
        config: &offline,
        client: &client,
        segmenter: &fixture.segmenter,
        history: &fixture.history,
        cache_enabled: true,
    };
    assert_eq!(
        translate_text(
            SOURCE,
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            &ctx
        )
        .await
        .unwrap()
        .text,
        REPLY
    );
    assert!(
        offline.backend_runtime_info().is_none(),
        "replay runtime must remain isolated"
    );
    let record = fixture.history.fetch_recent(None).unwrap().remove(0);
    assert_eq!(record.cache_hits, Some(1));
    assert!(record.cold_source_tokens.is_none() && record.estimated_seconds.is_none());
    assert!(record
        .inference_fingerprint
        .starts_with("historical-replay:"));
}

#[tokio::test]
async fn replay_rejects_changed_request_and_policy() {
    let fixture = OfflineFixture::warm(false).await;
    let baseline = std::fs::read_to_string(fixture.config.path()).unwrap();
    for (before, after) in [
        ("test-model", "changed-model"),
        ("context_window = 512", "context_window = 500"),
        ("max_output_tokens = 40", "max_output_tokens = 41"),
        (
            "first_chunk_priority = true",
            "first_chunk_priority = false",
        ),
        ("max_retries = 1", "max_retries = 0"),
        ("concurrency = 1", "concurrency = 2"),
        (
            "concurrency = 1",
            "concurrency = 1\nmax_source_tokens_per_segment = 200",
        ),
        (
            "concurrency = 1",
            "concurrency = 1\nstrict_token_budget = true",
        ),
        (
            "concurrency = 1",
            "concurrency = 1\nforce_translate_all = true",
        ),
    ] {
        assert!(baseline.contains(before), "{before}");
        std::fs::write(fixture.config.path(), baseline.replace(before, after)).unwrap();
        let config = fixture.fresh();
        assert!(
            !fixture.candidate(&config, false),
            "changed {before} must miss"
        );
        assert!(config.backend_runtime_info().is_none());
    }
    std::fs::write(fixture.config.path(), &baseline).unwrap();
    let config = fixture.fresh();
    for (source, target, template, opts) in [
        (
            "Different synthetic source.",
            "zh",
            TemplateType::Default,
            PromptOpts::default(),
        ),
        (SOURCE, "en", TemplateType::Default, PromptOpts::default()),
        (
            SOURCE,
            "zh",
            TemplateType::Delimiters,
            PromptOpts::default(),
        ),
        (
            SOURCE,
            "zh",
            TemplateType::Default,
            PromptOpts {
                instructions: Some(vec!["changed".into()]),
                ..PromptOpts::default()
            },
        ),
        (
            SOURCE,
            "zh",
            TemplateType::Default,
            PromptOpts {
                warn_only_completeness: Some(true),
                ..PromptOpts::default()
            },
        ),
    ] {
        assert!(offline_replay::find_replay(
            source,
            target,
            &template,
            &opts,
            &TranslationCtx {
                config: &config,
                client: &TranslationClient::with_concurrency(config.clone(), 1).unwrap(),
                segmenter: &fixture.segmenter,
                history: &fixture.history,
                cache_enabled: true
            },
            false
        )
        .unwrap()
        .is_none());
    }
    assert!(
        !fixture.candidate(&config, true),
        "stream contract must match"
    );
    assert!(fixture.candidate(&config, false), "unchanged control");
}

#[tokio::test]
async fn replay_denies_corrupt_legacy_historical_and_wrong_eos_proofs() {
    let fixture = OfflineFixture::warm(false).await;
    let offline = fixture.fresh();
    let mut prior_admission: serde_json::Value = serde_json::from_str(&fixture.proof).unwrap();
    prior_admission["version"] = serde_json::json!(1);
    fixture.replace(&prior_admission.to_string(), &fixture.record);
    assert!(
        !fixture.candidate(&offline, false),
        "prior admission version"
    );
    for (field, replacement) in [
        ("version", serde_json::json!(99)),
        ("requests", serde_json::json!("wrong")),
        ("output", serde_json::json!("wrong")),
        ("inference", serde_json::json!("wrong")),
    ] {
        let mut proof: serde_json::Value = serde_json::from_str(&fixture.proof).unwrap();
        proof[field] = replacement;
        fixture.replace(&proof.to_string(), &fixture.record);
        assert!(!fixture.candidate(&offline, false), "{field}");
    }
    for (field, value) in [
        ("verification_status", serde_json::json!("historical")),
        ("verification_status", serde_json::json!("unverified")),
        ("false_eos_token_id", serde_json::Value::Null),
        ("false_eos_token_id", serde_json::json!(8)),
    ] {
        let mut proof: serde_json::Value = serde_json::from_str(&fixture.proof).unwrap();
        proof["runtime"][field] = value;
        fixture.replace(&proof.to_string(), &fixture.record);
        assert!(!fixture.candidate(&offline, false), "runtime {field}");
    }
    for bytes in ["{}", "{invalid", "[]"] {
        fixture.replace(bytes, &fixture.record);
        assert!(!fixture.candidate(&offline, false));
    }
    for output in [Some("".to_owned()), Some("坏".to_owned()), None] {
        let mut record = fixture.record.clone();
        record.output_text = output;
        fixture.replace(&fixture.proof, &record);
        assert!(!fixture.candidate(&offline, false));
    }
    fixture.history.clear().unwrap();
    fixture.history.insert_task(&fixture.record).unwrap();
    assert!(
        !fixture.candidate(&offline, false),
        "legacy tasks carry no proof"
    );
    fixture.replace(&fixture.proof, &fixture.record);
    assert!(fixture.candidate(&offline, false), "unchanged control");
    assert!(offline.backend_runtime_info().is_none());
    assert!(fixture
        .history
        .insert_task_with_replay_proof(&fixture.record, Some((&fixture.key, &"x".repeat(16385))))
        .is_err());
}

#[tokio::test]
async fn disabled_empty_and_rejected_stream_replay_keep_live_gate_and_emit_no_prefix() {
    let fixture = OfflineFixture::warm(true).await;
    for (enabled, empty, corrupt) in [
        (false, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        fixture.replace(&fixture.proof, &fixture.record);
        if empty {
            fixture.history.clear().unwrap();
        }
        if corrupt {
            fixture.replace("{}", &fixture.record);
        }
        let config = fixture.fresh();
        let client = TranslationClient::new(config.clone())
            .unwrap()
            .with_deferred_preflight(true);
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &fixture.segmenter,
            history: &fixture.history,
            cache_enabled: enabled,
        };
        let (tx, mut rx) = mpsc::channel(16);
        let error = translate_text_stream_with_mode(
            SOURCE,
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            &ctx,
            StreamOutputMode::Optimistic,
            tx,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("strict backend preflight"));
        assert!(
            rx.recv().await.is_none(),
            "rejected proof must emit no prefix"
        );
        assert!(
            config.backend_runtime_info().is_none(),
            "miss must not publish historical runtime"
        );
    }
    fixture.replace(&fixture.proof, &fixture.record);
    let config = fixture.fresh();
    let client = TranslationClient::new(config.clone()).unwrap();
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &fixture.segmenter,
        history: &fixture.history,
        cache_enabled: true,
    };
    let (tx, mut rx) = mpsc::channel(16);
    assert_eq!(
        translate_text_stream_with_mode(
            SOURCE,
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            &ctx,
            StreamOutputMode::Validated,
            tx
        )
        .await
        .unwrap()
        .text,
        REPLY
    );
    assert_eq!(rx.recv().await, Some(StreamEvent::Token(REPLY.to_owned())));
    assert_eq!(
        rx.recv().await,
        Some(StreamEvent::AllDone(REPLY.to_owned()))
    );
}

#[tokio::test]
async fn exec_legacy_cache_cannot_bypass_deferred_live_gate() {
    let fixture = OfflineFixture::warm(false).await;
    fixture.history.clear().unwrap();
    let config = fixture.fresh();
    let opts = PromptOpts::default();
    let client = TranslationClient::new(config.clone())
        .unwrap()
        .with_deferred_preflight(true);
    let cache = ExecCache::with_user_path(
        fixture._dir.path().join("shared.db"),
        fixture._dir.path().join("exec.db"),
    );
    cache
        .store_user(
            "man",
            "test",
            SOURCE,
            "zh",
            effective_document_translation_policy(&opts, &config),
            REPLY,
        )
        .unwrap();
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &fixture.segmenter,
        history: &fixture.history,
        cache_enabled: true,
    };
    assert!(format!(
        "{:#}",
        translate_cached("man", "test", SOURCE, "zh", &cache, &opts, &ctx)
            .await
            .unwrap_err()
    )
    .contains("strict backend preflight"));
}

#[tokio::test]
async fn batch_preview_reaches_validated_replay() {
    let fixture = OfflineFixture::warm(false).await;
    let inputs = fixture._dir.path().join("inputs");
    std::fs::create_dir(&inputs).unwrap();
    std::fs::write(inputs.join("sample.txt"), SOURCE).unwrap();
    let config = fixture.fresh();
    let prompt = PromptOpts::default();
    let opts = crate::batch::BatchPlanOpts {
        output_dir: None,
        target_lang: "zh",
        template: &TemplateType::Default,
        prompt_opts: &prompt,
        recursive: false,
        explicit_target: true,
        concurrency_override: None,
        no_cache: false,
    };
    let client = TranslationClient::new(config.clone())
        .unwrap()
        .with_deferred_preflight(true);
    let plan = crate::batch::build_batch_plan(
        &inputs,
        &config,
        &client,
        &fixture.segmenter,
        &fixture.history,
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.total_missing_segments(), 0);
    crate::batch::run_batch_translation(
        &plan,
        &config,
        &client,
        &fixture.segmenter,
        &fixture.history,
        &TemplateType::Default,
        &prompt,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&plan.files[0].output_path).unwrap(),
        REPLY
    );
}

#[tokio::test]
async fn historical_or_unresolved_runtime_and_legacy_segment_hits_cannot_populate_proof() {
    let fixture = OfflineFixture::warm(false).await;
    let config = &fixture.config;
    let opts = PromptOpts::default();
    let client = TranslationClient::new(config.clone()).unwrap();
    let plan = plan_translation(
        SOURCE,
        "zh",
        config,
        &fixture.segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    let runtime = config.backend_runtime_info().unwrap();
    for status in [
        hymt_core::runtime::BackendVerificationStatus::Historical,
        hymt_core::runtime::BackendVerificationStatus::Unverified,
    ] {
        let mut altered = runtime.clone();
        altered.verification_status = status;
        config.set_backend_runtime_info(altered);
        let ctx = TranslationCtx {
            config,
            client: &client,
            segmenter: &fixture.segmenter,
            history: &fixture.history,
            cache_enabled: true,
        };
        assert!(offline_replay::population_proof(
            (SOURCE, REPLY),
            "zh",
            &TemplateType::Default,
            &opts,
            &ctx,
            &plan,
            false
        )
        .unwrap()
        .is_none());
    }
    let mut unresolved = runtime.clone();
    unresolved.false_eos_token_id = None;
    config.set_backend_runtime_info(unresolved);
    let ctx = TranslationCtx {
        config,
        client: &client,
        segmenter: &fixture.segmenter,
        history: &fixture.history,
        cache_enabled: true,
    };
    assert!(offline_replay::population_proof(
        (SOURCE, REPLY),
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
        &plan,
        false
    )
    .unwrap()
    .is_none());
    config.set_backend_runtime_info(runtime);
    fixture.history.clear().unwrap();
    let options_hash =
        template_options_hash(&opts, effective_document_translation_policy(&opts, config));
    let fingerprint = config
        .inference_fingerprint("default", &options_hash)
        .unwrap();
    fixture
        .history
        .store_segment_cache(
            &segment_cache_hash(SOURCE),
            SegmentCacheScope {
                target_lang: "zh",
                template_type: "default",
                options_hash: &options_hash,
                profile_id: config.model_profile().unwrap().id(),
                inference_fingerprint: fingerprint.hash(),
            },
            REPLY,
            "2026-10-17T00:00:00Z",
        )
        .unwrap();
    assert_eq!(
        translate_text(SOURCE, "zh", &TemplateType::Default, &opts, &ctx)
            .await
            .unwrap()
            .text,
        REPLY
    );
    assert!(fixture
        .history
        .find_replay_candidates(&segment_cache_hash(SOURCE), &fixture.key)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn request_binding_uses_actual_rendered_content_adapter_bytes_and_order() {
    let fixture = OfflineFixture::warm(false).await;
    let config = fixture.config.detached_snapshot();
    let opts = PromptOpts::default();
    let client = TranslationClient::new(config.clone()).unwrap();
    let source = format!(
        "{} {}",
        SOURCE.repeat(4),
        "A distinct final synthetic paragraph.".repeat(4)
    );
    let mut plan = plan_translation(
        &source,
        "zh",
        &config,
        &fixture.segmenter,
        &TemplateType::Personalization,
        &opts,
    )
    .unwrap();
    assert!(plan.segment_count() > 1);
    let original = offline_replay::request_plan_digest(
        &plan,
        "zh",
        &TemplateType::Personalization,
        &opts,
        &client,
        false,
    )
    .unwrap();
    let changed = PromptOpts {
        instructions: Some(vec!["Use a different rendered instruction.".into()]),
        ..PromptOpts::default()
    };
    // Test the request binding itself, without a configured-options hash change.
    assert_ne!(
        original,
        offline_replay::request_plan_digest(
            &plan,
            "zh",
            &TemplateType::Personalization,
            &changed,
            &client,
            false
        )
        .unwrap()
    );
    let mut runtime = config.backend_runtime_info().unwrap();
    runtime.false_eos_token_id = Some(8);
    config.set_backend_runtime_info(runtime);
    // Same plan and prompt: only the adapter-produced logit_bias bytes change.
    assert_ne!(
        original,
        offline_replay::request_plan_digest(
            &plan,
            "zh",
            &TemplateType::Personalization,
            &opts,
            &client,
            false
        )
        .unwrap()
    );
    config.set_backend_runtime_info(fixture.config.backend_runtime_info().unwrap());
    assert_eq!(
        original,
        offline_replay::request_plan_digest(
            &plan,
            "zh",
            &TemplateType::Personalization,
            &opts,
            &client,
            false
        )
        .unwrap()
    );
    plan.segments.reverse();
    assert_ne!(
        original,
        offline_replay::request_plan_digest(
            &plan,
            "zh",
            &TemplateType::Personalization,
            &opts,
            &client,
            false
        )
        .unwrap()
    );
}

#[tokio::test]
async fn completeness_retry_cannot_populate_offline_proof() {
    let fixture = OfflineFixture::warm(false).await;
    let server = start_capturing_mock_server(vec![
        MockResponse::Json(String::new()),
        MockResponse::Json(REPLY.into()),
    ])
    .await;
    let config = make_stream_config(&server.endpoint_url);
    config.set_backend_runtime_info(fixture.config.backend_runtime_info().unwrap());
    let client = TranslationClient::new(config.clone()).unwrap();
    let history = HistoryDB::new(fixture._dir.path().join("retried.db"));
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &fixture.segmenter,
        history: &history,
        cache_enabled: true,
    };
    let opts = PromptOpts::default();
    assert_eq!(
        translate_text(SOURCE, "zh", &TemplateType::Default, &opts, &ctx)
            .await
            .unwrap()
            .text,
        REPLY
    );
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    let plan = plan_translation(
        SOURCE,
        "zh",
        &config,
        &fixture.segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    let (key, _) = offline_replay::population_proof(
        (SOURCE, REPLY),
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
        &plan,
        false,
    )
    .unwrap()
    .unwrap();
    assert!(history
        .find_replay_candidates(&segment_cache_hash(SOURCE), &key)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn terminal_less_stream_retry_cannot_populate_offline_proof() {
    let fixture = OfflineFixture::warm(false).await;
    let server = start_capturing_mock_server(vec![
        MockResponse::SseWithoutTerminal(vec!["译".into()]),
        MockResponse::Json(REPLY.into()),
    ])
    .await;
    let config = make_stream_config(&server.endpoint_url);
    config.set_backend_runtime_info(fixture.config.backend_runtime_info().unwrap());
    let client = TranslationClient::new(config.clone()).unwrap();
    let history = HistoryDB::new(fixture._dir.path().join("terminal-retried.db"));
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &fixture.segmenter,
        history: &history,
        cache_enabled: true,
    };
    let opts = PromptOpts::default();
    let (tx, mut rx) = mpsc::channel(16);
    let result = translate_text_stream_with_mode(
        SOURCE,
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
        StreamOutputMode::Validated,
        tx,
    )
    .await
    .unwrap();
    assert_eq!(result.text, REPLY);
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    let mut published = String::new();
    while let Some(event) = rx.recv().await {
        if let StreamEvent::Token(text) = event {
            published.push_str(&text);
        }
    }
    assert_eq!(published, REPLY);
    let plan = plan_translation(
        SOURCE,
        "zh",
        &config,
        &fixture.segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    let (key, _) = offline_replay::population_proof(
        (SOURCE, REPLY),
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
        &plan,
        true,
    )
    .unwrap()
    .unwrap();
    assert!(history
        .find_replay_candidates(&segment_cache_hash(SOURCE), &key)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn changed_loaded_tokenizer_bytes_deny_replay() {
    if !hymt_segment::has_tokenizer_support() {
        return;
    }
    let fixture = OfflineFixture::warm(false).await;
    let path = fixture._dir.path().join("tokenizer.json");
    let bytes = r#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,"pre_tokenizer":null,"post_processor":null,"decoder":null,"model":{"type":"WordLevel","vocab":{"[UNK]":0},"unk_token":"[UNK]"}}"#;
    std::fs::write(&path, bytes).unwrap();
    let first = hymt_segment::Segmenter::new(Some(path.clone())).unwrap();
    // JSON whitespace changes the retained artifact without changing token counts.
    std::fs::write(&path, format!("{bytes}\n")).unwrap();
    let second = hymt_segment::Segmenter::new(Some(path)).unwrap();
    assert_eq!(first.count_tokens(SOURCE), second.count_tokens(SOURCE));
    assert_ne!(first.tokenizer_identity(), second.tokenizer_identity());
    let config = fixture.fresh();
    let client = TranslationClient::new(config.clone()).unwrap();
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &second,
        history: &fixture.history,
        cache_enabled: true,
    };
    assert!(offline_replay::find_replay(
        SOURCE,
        "zh",
        &TemplateType::Default,
        &PromptOpts::default(),
        &ctx,
        false
    )
    .unwrap()
    .is_none());
    assert!(fixture.candidate(&config, false));
}
