use super::*;

#[tokio::test]
async fn terminal_less_sse_retries_before_validated_publication() {
    let source = "Read this complete source instruction and preserve its detailed meaning.";
    let complete = "请完整阅读并保留这条说明。";
    let server = start_capturing_mock_server(vec![
        MockResponse::SseWithoutTerminal(vec!["译".to_owned()]),
        MockResponse::Json(complete.to_owned()),
    ])
    .await;
    let config = make_stream_config(&server.endpoint_url);
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
        StreamOutputMode::Validated,
        tx,
    )
    .await
    .unwrap();
    let mut published = String::new();
    let mut done = 0;
    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Token(text) => published.push_str(&text),
            StreamEvent::AllDone(_) => done += 1,
            _ => {}
        }
    }
    assert_eq!(result.text, complete);
    assert_eq!(published, complete);
    assert_eq!(done, 1);
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn exhausted_length_preserves_file_and_history() {
    let source =
        r#"{"customer": "Keep the account available throughout the entire migration process."}"#;
    let missing =
        r#"{"wrong": "账户在整个迁移过程中始终保持可用，请持续检查迁移进度以及所有相关服务。"}"#;
    for finish_reason in ["length"] {
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
            "truncated response must not become a successful document"
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("finish_reason_length"));
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
async fn exhausted_length_is_rejected_before_segment_success() {
    for (source, reason, failure) in [(
        "Keep the account available throughout the entire migration process.",
        "length",
        "finish_reason_length",
    )] {
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

#[tokio::test]
async fn explicit_warn_only_retains_original_instead_of_best_effort() {
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

#[tokio::test]
async fn model_wording_passes_once_in_buffered_and_streaming_modes() {
    let source = "Read this complete source instruction and preserve its detailed meaning throughout the migration.";
    for translated in [
        "译",
        "cannot translate",
        "无法翻译",
        "First model paragraph.\n\nSecond model paragraph.",
    ] {
        for retain in [false, true] {
            for streaming in [false, true] {
                let encoded = serde_json::to_string(translated).unwrap();
                let wire = encoded[1..encoded.len() - 1].to_owned();
                let replies = (0..2)
                    .map(|_| {
                        if streaming {
                            MockResponse::Sse(vec![wire.clone()])
                        } else {
                            MockResponse::Json(wire.clone())
                        }
                    })
                    .collect();
                let server = start_capturing_mock_server(replies).await;
                let config = make_stream_config(&server.endpoint_url);
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
                let opts = PromptOpts {
                    warn_only_completeness: Some(retain),
                    ..PromptOpts::default()
                };
                let result = if streaming {
                    let (tx, mut rx) = mpsc::channel(32);
                    let result = translate_text_stream_with_mode(
                        source,
                        "zh",
                        &TemplateType::Default,
                        &opts,
                        &ctx,
                        StreamOutputMode::Validated,
                        tx,
                    )
                    .await
                    .unwrap();
                    let mut published = String::new();
                    let mut completed = 0;
                    while let Some(event) = rx.recv().await {
                        match event {
                            StreamEvent::Token(text) => published.push_str(&text),
                            StreamEvent::AllDone(_) => completed += 1,
                            _ => {}
                        }
                    }
                    assert_eq!(published, translated);
                    assert_eq!(completed, 1);
                    result
                } else {
                    translate_text(source, "zh", &TemplateType::Default, &opts, &ctx)
                        .await
                        .unwrap()
                };
                assert_eq!(result.text, translated);
                assert!(!result.is_completeness_degraded());
                assert_eq!(server.requests.lock().unwrap().len(), 1);
                let cached = translate_text(source, "zh", &TemplateType::Default, &opts, &ctx)
                    .await
                    .unwrap();
                assert_eq!(cached.text, translated);
                assert_eq!(
                    server.requests.lock().unwrap().len(),
                    1,
                    "model wording must remain cacheable"
                );
            }
        }
    }
}
