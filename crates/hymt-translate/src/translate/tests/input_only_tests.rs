use super::*;

#[tokio::test]
async fn stream_response_slots_gate_publication_and_application_cache() {
    let source = "Read the complete instruction.";
    for (body, valid) in [
        ("data: [DONE]\n\n", false),
        ("data: {\"choices\":[]}\n\ndata: [DONE]\n\n", false),
        ("data: {\"choices\":[{\"index\":1,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", false),
        ("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", true),
        ("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", true),
    ] {
        let server = start_capturing_mock_server(vec![MockResponse::RawSse(body.into())]).await;
        let config = make_stream_config(&server.endpoint_url);
        config.set_backend_runtime_info(hymt_core::runtime::BackendRuntimeInfo::from_llama_cpp_props(
            &serde_json::json!({"model":"test-model", "model_path":"test-model", "n_ctx_per_seq":512}), 123).unwrap());
        let client = TranslationClient::new(config.clone()).unwrap();
        let segmenter = fallback_segmenter();
        let dir = tempfile::tempdir().unwrap();
        let history_path = dir.path().join("history.db");
        let history = HistoryDB::new(&history_path);
        let opts = PromptOpts::default();
        let options = template_options_hash(&opts, effective_document_translation_policy(&opts, &config));
        let fingerprint = config.inference_fingerprint("default", &options).unwrap();
        assert!(fingerprint.is_cache_verified());
        let scope = SegmentCacheScope {
            target_lang: "zh", template_type: "default", options_hash: &options,
            profile_id: config.model_profile().unwrap().id(), inference_fingerprint: fingerprint.hash(),
        };
        let ctx = TranslationCtx { config: &config, client: &client, segmenter: &segmenter, history: &history, cache_enabled: true };
        let (tx, mut rx) = mpsc::channel(16);
        let result = translate_text_stream_with_mode(source, "zh", &TemplateType::Default, &opts, &ctx, StreamOutputMode::Validated, tx).await;
        let mut published = String::new();
        let mut done = false;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Token(token) => published.push_str(&token),
                StreamEvent::AllDone(_) => done = true,
                _ => {}
            }
        }
        assert_eq!(result.is_ok(), valid, "{body}: {result:?}");
        assert_eq!(done, valid, "{body}");
        assert!(published.is_empty());
        assert_eq!(history.find_segment_cached(&segment_cache_hash(source), scope).unwrap(), valid.then(String::new));
        if valid {
            let (tx, _rx) = mpsc::channel(16);
            assert!(translate_text_stream_with_mode(source, "zh", &TemplateType::Default, &opts, &ctx, StreamOutputMode::Validated, tx).await.unwrap().text.is_empty());
        } else {
            assert!(!history_path.exists(), "failed response created successful history/cache");
        }
        assert_eq!(server.requests.lock().unwrap().len(), 1, "no missing response may be cached or retried as model text");
    }
}

#[tokio::test]
async fn completed_model_content_is_published_without_repair_or_retry() {
    for (source, reply) in [
        ("Read *one* now.", "# Different model formatting"),
        (
            "Read *the guide* at https://example.test/original now.",
            "The model omitted the URL.",
        ),
        (
            r#"{"message":"Read the complete instruction."}"#,
            "not JSON",
        ),
        ("Read the complete instruction.", ""),
        ("Read the complete instruction.", "`model code`"),
    ] {
        for streaming in [false, true] {
            let escaped = serde_json::to_string(reply).unwrap();
            let escaped = escaped[1..escaped.len() - 1].to_owned();
            let responses = (0..2)
                .map(|_| {
                    if streaming {
                        MockResponse::Sse(vec![escaped.clone()])
                    } else {
                        MockResponse::Json(escaped.clone())
                    }
                })
                .collect();
            let server = start_capturing_mock_server(responses).await;
            let config = make_protected_block_config(&server.endpoint_url);
            let segmenter = fallback_segmenter();
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
            assert_eq!(plan.segments, [source]);
            assert_eq!(plan.reconstruct(&plan.segments), source);
            let client = TranslationClient::new(config.clone()).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let history = HistoryDB::new(dir.path().join("history.db"));
            let ctx = TranslationCtx {
                config: &config,
                client: &client,
                segmenter: &segmenter,
                history: &history,
                cache_enabled: false,
            };
            let outcome = if streaming {
                let (tx, mut rx) = mpsc::channel(16);
                let outcome = translate_text_stream_with_mode(
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
                let mut emitted = String::new();
                while let Some(event) = rx.recv().await {
                    if let StreamEvent::Token(token) = event {
                        emitted.push_str(&token);
                    }
                }
                assert_eq!(emitted, reply);
                outcome
            } else {
                translate_text(source, "zh", &TemplateType::Default, &opts, &ctx)
                    .await
                    .unwrap()
            };
            assert_eq!(outcome.text, reply);
            assert!(!outcome.is_completeness_degraded());
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), 1, "content must not trigger a retry");
            let body: serde_json::Value =
                serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
            assert!(
                body["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .ends_with(source)
            );
        }
    }
}

#[tokio::test]
async fn completed_empty_middle_reply_keeps_all_requests_and_order() {
    let source = "First *complete* instruction.\n\nMiddle *complete* instruction.\n\nLast *complete* instruction.";
    for streaming in [false, true] {
        let server = start_capturing_mock_server(vec![
            if streaming {
                MockResponse::Sse(vec!["FIRST".into()])
            } else {
                MockResponse::Json("FIRST".into())
            },
            MockResponse::Json(String::new()),
            MockResponse::Json("LAST".into()),
        ])
        .await;
        let config = make_protected_block_config(&server.endpoint_url);
        let segmenter = fallback_segmenter();
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
        assert_eq!(plan.segment_count(), 3);
        assert_eq!(plan.reconstruct(&plan.segments), source);
        let client = TranslationClient::new(config.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(dir.path().join("history.db"));
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: false,
        };
        let expected = plan.reconstruct(&["FIRST".into(), String::new(), "LAST".into()]);
        let outcome = if streaming {
            let (tx, mut rx) = mpsc::channel(16);
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
            let mut tokens = String::new();
            while let Some(event) = rx.recv().await {
                if let StreamEvent::Token(token) = event {
                    tokens.push_str(&token);
                }
            }
            assert_eq!(tokens, expected);
            result
        } else {
            translate_text(source, "zh", &TemplateType::Default, &opts, &ctx)
                .await
                .unwrap()
        };
        assert_eq!(outcome.text, expected);
        assert!(!outcome.is_completeness_degraded());
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for (request, segment) in requests.iter().zip(&plan.segments) {
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            let prompt = body["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap();
            assert_eq!(
                prompt,
                build_prompt(segment, "zh", &TemplateType::Default, &opts).unwrap()
            );
        }
    }
}
