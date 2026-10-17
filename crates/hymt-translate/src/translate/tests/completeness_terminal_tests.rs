use super::*;

#[tokio::test]
async fn exhausted_known_missing_content_preserves_file_and_history() {
    let source =
        r#"{"customer": "Keep the account available throughout the entire migration process."}"#;
    let missing =
        r#"{"wrong": "账户在整个迁移过程中始终保持可用，请持续检查迁移进度以及所有相关服务。"}"#;
    for finish_reason in ["stop", "length"] {
        let server = start_mock_server(
            (0..2)
                .map(|_| MockResponse::JsonWithFinishReason {
                    content: serde_json::to_string(&missing)
                        .unwrap()
                        .trim_matches('"')
                        .to_owned(),
                    finish_reason: finish_reason.to_owned(),
                })
                .collect(),
        )
        .await;
        let config = make_stream_config(&server.endpoint_url);
        let client = TranslationClient::new(config.clone()).unwrap();
        let segmenter = fallback_segmenter();
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("source.md");
        let output = dir.path().join("output.md");
        std::fs::write(&input, source).unwrap();
        std::fs::write(&output, "previous complete translation").unwrap();
        let history = HistoryDB::new(dir.path().join("history.db"));
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: true,
        };
        let result = translate_file(
            &input,
            Some(&output),
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            &ctx,
        )
        .await;
        assert!(
            result.is_err(),
            "known missing JSON key must not become a successful document"
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("json_key_preservation"));
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "previous complete translation"
        );
        assert!(history.fetch_recent(None).unwrap().is_empty());
        let opts = PromptOpts::default();
        let options_hash =
            template_options_hash(&opts, effective_document_translation_policy(&opts, &config));
        let fingerprint = config
            .inference_fingerprint(TemplateType::Default.as_str(), &options_hash)
            .unwrap();
        let scope = SegmentCacheScope {
            target_lang: "zh",
            template_type: TemplateType::Default.as_str(),
            options_hash: &options_hash,
            profile_id: config.model_profile().unwrap().id(),
            inference_fingerprint: fingerprint.hash(),
        };
        let plan = plan_translation(
            source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &opts,
        )
        .unwrap();
        for segment in plan.segments {
            assert!(history
                .find_segment_cached(&segment_cache_hash(&segment), scope)
                .unwrap()
                .is_none());
        }
    }
}

#[tokio::test]
async fn exhausted_url_and_length_failures_are_rejected_before_segment_success() {
    for (source, reason, failure) in [
        (
            "Visit https://example.com/important for the complete instructions.",
            "stop",
            "url_preservation",
        ),
        (
            "Keep the account available throughout the entire migration process.",
            "length",
            "finish_reason_length",
        ),
    ] {
        let server = start_mock_server(
            (0..2)
                .map(|_| MockResponse::JsonWithFinishReason {
                    content: "迁移期间请确保账户始终保持可用，并持续检查相关服务的运行情况。"
                        .to_owned(),
                    finish_reason: reason.to_owned(),
                })
                .collect(),
        )
        .await;
        let config = make_stream_config(&server.endpoint_url);
        let client = TranslationClient::new(config.clone()).unwrap();
        let result = translate_segment_with_completeness(
            0,
            &client,
            source,
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            &config,
        )
        .await;
        assert!(result.is_err(), "{failure} must not become segment success");
        assert!(format!("{:#}", result.err().unwrap()).contains(failure));
    }
}

#[tokio::test]
async fn missing_content_retry_can_publish_only_the_complete_candidate() {
    let source =
        r#"{"customer": "Keep the account available throughout the entire migration process."}"#;
    let complete = r#"{"customer": "整个迁移过程中必须让账户保持可用。"}"#;
    let server = start_capturing_mock_server(vec![
        MockResponse::Json(
            "在整个迁移过程中，账户必须保持可用，所有相关的服务也必须正常运行。".to_owned(),
        ),
        MockResponse::Json(
            serde_json::to_string(&complete)
                .unwrap()
                .trim_matches('"')
                .to_owned(),
        ),
    ])
    .await;
    let config = make_stream_config(&server.endpoint_url);
    let client = TranslationClient::new(config.clone()).unwrap();
    let segmenter = fallback_segmenter();
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(dir.path().join("history.db"));
    let opts = PromptOpts::default();
    let options_hash =
        template_options_hash(&opts, effective_document_translation_policy(&opts, &config));
    let fingerprint = config
        .inference_fingerprint(TemplateType::Default.as_str(), &options_hash)
        .unwrap();
    assert!(fingerprint.is_cache_verified());
    let scope = SegmentCacheScope {
        target_lang: "zh",
        template_type: TemplateType::Default.as_str(),
        options_hash: &options_hash,
        profile_id: config.model_profile().unwrap().id(),
        inference_fingerprint: fingerprint.hash(),
    };
    let plan = plan_translation(
        source,
        "zh",
        &config,
        &segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    assert_eq!(plan.segments.len(), 1);
    let hash = segment_cache_hash(&plan.segments[0]);
    history
        .store_segment_cache(
            &hash,
            scope,
            "迁移过程中必须确保账户始终保持可用，所有相关服务也必须正常运行。",
            "2026-10-12T00:00:00Z",
        )
        .unwrap();
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: true,
    };
    let outcome = translate_text(
        source,
        "zh",
        &TemplateType::Default,
        &PromptOpts::default(),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(outcome.text, complete);
    assert!(!outcome.is_completeness_degraded());
    assert_eq!(history.fetch_recent(None).unwrap().len(), 1);
    assert_eq!(
        server.requests.lock().unwrap().len(),
        2,
        "invalid cached content must be retranslated, including the failed first retry"
    );
    assert_eq!(
        history
            .find_segment_cached(&hash, scope)
            .unwrap()
            .as_deref(),
        Some(complete)
    );
}

#[tokio::test]
async fn incomplete_stream_never_reports_document_success() {
    for mode in [StreamOutputMode::Validated, StreamOutputMode::Optimistic] {
        let source = r#"{"customer": "Keep the account available throughout the entire migration process."}"#;
        let incomplete = "迁移过程中必须确保账户始终可用，所有相关服务也必须正常运行。";
        let server = start_mock_server(vec![
            MockResponse::SseWithFinishReason {
                tokens: vec![incomplete.to_owned()],
                finish_reason: "length".to_owned(),
            },
            MockResponse::JsonWithFinishReason {
                content: incomplete.to_owned(),
                finish_reason: "length".to_owned(),
            },
        ])
        .await;
        let config = make_stream_config_with_fcp(&server.endpoint_url, false);
        let client = TranslationClient::new(config.clone()).unwrap();
        let segmenter = fallback_segmenter();
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(dir.path().join("history.db"));
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: true,
        };
        let (tx, mut rx) = mpsc::channel(32);
        let result = translate_text_stream_with_mode(
            source,
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            &ctx,
            mode,
            tx,
        )
        .await;
        assert!(result.is_err());
        let mut saw_token = false;
        while let Some(event) = rx.recv().await {
            assert!(!matches!(
                event,
                StreamEvent::AllDone(_) | StreamEvent::SegmentDone(_)
            ));
            saw_token |= matches!(event, StreamEvent::Token(_));
        }
        if mode == StreamOutputMode::Validated {
            assert!(!saw_token);
        }
        assert!(history.fetch_recent(None).unwrap().is_empty());
    }
}

#[test]
fn cached_missing_content_is_revalidated() {
    let config = make_stream_config("http://127.0.0.1:9/v1");
    for source in [
        "Keep {{customer}} available throughout the complete migration process.",
        "Visit https://example.com/important for the complete instructions.",
        "First factual claim.\n\nSecond factual claim.\n\nThird factual claim.",
    ] {
        assert!(!cached_segment_is_complete(
            0,
            source,
            "迁移过程中账户必须始终保持可用，所有相关服务也必须继续正常运行。",
            "zh",
            &config
        ));
    }
}

#[tokio::test]
async fn explicit_warn_only_retains_eligible_best_effort_compatibility() {
    let source = "Keep {{customer}} available throughout the complete migration process.";
    let server = start_mock_server(
        (0..2)
            .map(|_| MockResponse::JsonWithFinishReason {
                content:
                    "迁移过程中必须确保 {{customer}} 始终保持可用，所有相关服务也必须正常运行。"
                        .to_owned(),
                finish_reason: "length".to_owned(),
            })
            .collect(),
    )
    .await;
    let config = make_stream_config(&server.endpoint_url);
    let client = TranslationClient::new(config.clone()).unwrap();
    let opts = PromptOpts {
        warn_only_completeness: Some(true),
        ..PromptOpts::default()
    };
    let outcome = translate_segment_with_completeness(
        0,
        &client,
        source,
        "zh",
        &TemplateType::Default,
        &opts,
        &config,
    )
    .await
    .unwrap();
    assert!(outcome.completeness_degraded);
}

#[tokio::test]
async fn identity_contract_rejects_loss_in_strict_warn_only_and_cache_admission() {
    let cases = [
        (
            "Read {{customer}} throughout the complete migration process.",
            "迁移过程中必须确保账户始终保持可用，并检查所有相关服务。",
        ),
        (
            "Read https://example.test/a twice: https://example.test/a for complete instructions.",
            "请完整阅读 https://example.test/a 的所有相关操作说明并检查服务。",
        ),
        (
            "Read {{field}} twice: {{field}} throughout the complete migration process.",
            "请确保 {{field}} 在迁移过程中保持可用，并检查所有相关的服务。",
        ),
        (
            r#"{"message":"Read all migration instructions.","count":2,"enabled":true,"items":["First"]}"#,
            r#"{"message":"请完整阅读所有迁移说明。","count":3,"enabled":false,"items":["第一项","新增项"]}"#,
        ),
    ];
    let mut admitted = Vec::new();
    for (source, candidate) in cases {
        for warn_only in [false, true] {
            let response = serde_json::to_string(candidate).unwrap();
            let server = start_capturing_mock_server(
                (0..2)
                    .map(|_| MockResponse::Json(response[1..response.len() - 1].to_owned()))
                    .collect(),
            )
            .await;
            let config = make_stream_config(&server.endpoint_url);
            let client = TranslationClient::new(config.clone()).unwrap();
            let opts = PromptOpts {
                warn_only_completeness: Some(warn_only),
                ..PromptOpts::default()
            };
            let result = translate_segment_with_completeness(
                0,
                &client,
                source,
                "zh",
                &TemplateType::Default,
                &opts,
                &config,
            )
            .await;
            if result.is_ok() {
                admitted.push((source, warn_only, "segment"));
            }
            if cached_document_is_complete(
                source,
                candidate,
                "zh",
                &config,
                &opts,
                &fallback_segmenter(),
            ) {
                admitted.push((source, warn_only, "cache"));
            }
        }
    }
    assert!(admitted.is_empty(), "identity loss admitted: {admitted:?}");
}

#[tokio::test]
async fn ordinary_prose_failure_preserves_optimistic_prefix_without_success_admission() {
    let source = "Keep the account available throughout the complete migration process and check every related service.";
    let chunks = [
        "迁移期间必须确保账户始终保持可用，",
        "并持续检查所有相关服务的运行情况。",
    ];
    let expected_prefix = chunks.concat();
    for mode in [StreamOutputMode::Optimistic, StreamOutputMode::Validated] {
        let server = start_capturing_mock_server(vec![
            MockResponse::SseWithFinishReason {
                tokens: chunks.iter().map(|chunk| (*chunk).to_owned()).collect(),
                finish_reason: "length".to_owned(),
            },
            MockResponse::JsonWithFinishReason {
                content: expected_prefix.clone(),
                finish_reason: "length".to_owned(),
            },
        ])
        .await;
        let config = make_stream_config(&server.endpoint_url);
        let client = TranslationClient::new(config.clone()).unwrap();
        let segmenter = fallback_segmenter();
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(dir.path().join("history.db"));
        let opts = PromptOpts::default();
        let plan = plan_translation(
            source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &opts,
        )
        .unwrap();
        assert_eq!(plan.segments, vec![source.to_owned()]);
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: true,
        };
        let (tx, mut rx) = mpsc::channel(32);
        let result = translate_text_stream_with_mode(
            source,
            "zh",
            &TemplateType::Default,
            &opts,
            &ctx,
            mode,
            tx,
        )
        .await;
        assert!(format!("{:#}", result.unwrap_err()).contains("finish_reason_length"));
        let mut tokens = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Token(token) => tokens.push(token),
                StreamEvent::SegmentDone(_) | StreamEvent::AllDone(_) => {
                    panic!("failed translation reported success")
                }
            }
        }
        if mode == StreamOutputMode::Optimistic {
            assert_eq!(tokens.concat(), expected_prefix);
            assert!(
                !tokens.is_empty(),
                "ordinary prose must exercise actual emission"
            );
            assert_eq!(
                server.requests.lock().unwrap().len(),
                1,
                "never retry after emitted output"
            );
        } else {
            assert!(tokens.is_empty(), "validated output must remain withheld");
            assert_eq!(
                server.requests.lock().unwrap().len(),
                2,
                "only buffered attempts may retry"
            );
        }
        assert!(history.fetch_recent(None).unwrap().is_empty());
        let options_hash =
            template_options_hash(&opts, effective_document_translation_policy(&opts, &config));
        let fingerprint = config
            .inference_fingerprint(TemplateType::Default.as_str(), &options_hash)
            .unwrap();
        let scope = SegmentCacheScope {
            target_lang: "zh",
            template_type: TemplateType::Default.as_str(),
            options_hash: &options_hash,
            profile_id: config.model_profile().unwrap().id(),
            inference_fingerprint: fingerprint.hash(),
        };
        for segment in &plan.segments {
            assert!(history
                .find_segment_cached(&segment_cache_hash(segment), scope)
                .unwrap()
                .is_none());
        }
    }
}
