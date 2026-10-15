use super::*;

const JSON_SOURCES: [&str; 4] = [
    r#"{"account_id": "Translate this complete instruction for the customer."}"#,
    r#"{"message": "Translate left | right branches for the customer."}"#,
    r#"{"message": "Read https://example.com/important for the complete instructions."}"#,
    r#"{"outer": [{"account_id": "Translate this complete instruction."}]}"#,
];

#[test]
fn atomic_json_and_placeholder_identities_survive_planning() {
    let config = make_stream_config("http://127.0.0.1:9/v1");
    let segmenter = fallback_segmenter();
    for source in JSON_SOURCES {
        let plan = plan_translation(
            source,
            "fr",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert_eq!(
            plan.segments,
            vec![source],
            "valid JSON must retain its whole-input contract"
        );
    }
}

#[test]
fn complete_placeholders_are_source_owned() {
    let config = make_stream_config("http://127.0.0.1:9/v1");
    let segmenter = fallback_segmenter();
    for token in [
        "{{customer_name}}",
        "{account_id}",
        "{{customer}}",
        "{account.id}",
        "{{left | right}}",
    ] {
        let source = format!("Keep {token} available throughout the migration process.");
        let plan = plan_translation(
            &source,
            "fr",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        let translated = plan.reconstruct(&vec![
            "Traduction complete".to_owned();
            plan.segment_count()
        ]);
        assert!(
            translated.contains(token),
            "immutable identity lost: {token}: {translated}"
        );
        assert!(translated.contains("Traduction complete"));
        assert!(plan.segments.iter().all(|segment| !segment.contains(token)));
    }
}

#[tokio::test]
async fn fragmented_json_cannot_publish_buffered_or_validated_stream_success() {
    for source in JSON_SOURCES {
        for streaming in [false, true] {
            let server = start_capturing_mock_server((0..12).map(|_| MockResponse::Json("Une traduction complete conserve les informations et les instructions importantes.".to_owned())).collect()).await;
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
            let opts = PromptOpts::default();
            let result = if streaming {
                let (tx, mut rx) = mpsc::channel(32);
                let result = translate_text_stream_with_mode(
                    source,
                    "fr",
                    &TemplateType::Default,
                    &opts,
                    &ctx,
                    StreamOutputMode::Validated,
                    tx,
                )
                .await;
                while let Some(event) = rx.recv().await {
                    assert!(
                        !matches!(event, StreamEvent::Token(_) | StreamEvent::AllDone(_)),
                        "failed JSON must never publish output"
                    );
                }
                result
            } else {
                let input = dir.path().join("source.json");
                let output = dir.path().join("output.json");
                std::fs::write(&input, source).unwrap();
                std::fs::write(&output, "previous complete translation").unwrap();
                let result = translate_file(
                    &input,
                    Some(&output),
                    "fr",
                    &TemplateType::Default,
                    &opts,
                    &ctx,
                )
                .await;
                assert_eq!(
                    std::fs::read_to_string(output).unwrap(),
                    "previous complete translation",
                    "failed JSON must not overwrite output"
                );
                result
            };
            assert!(
                result.is_err(),
                "fragment validation must not admit broken whole JSON: {source}"
            );
            assert!(history.fetch_recent(None).unwrap().is_empty());
            let options_hash =
                template_options_hash(&opts, effective_document_translation_policy(&opts, &config));
            let fingerprint = config
                .inference_fingerprint("default", &options_hash)
                .unwrap();
            let scope = SegmentCacheScope {
                target_lang: "fr",
                template_type: "default",
                options_hash: &options_hash,
                profile_id: config.model_profile().unwrap().id(),
                inference_fingerprint: fingerprint.hash(),
            };
            let plan = plan_translation(
                source,
                "fr",
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
}

#[tokio::test]
async fn atomic_json_translates_values_and_rejects_changed_nested_keys() {
    for source in JSON_SOURCES {
        let mut complete: serde_json::Value = serde_json::from_str(source).unwrap();
        if let Some(value) = complete.get_mut("account_id") {
            *value = serde_json::json!("Traduisez cette instruction complete pour le client.");
        } else if complete.get("outer").is_some() {
            complete["outer"][0]["account_id"] =
                serde_json::json!("Traduisez cette instruction complete.");
        } else {
            complete["message"] = if source.contains("https:") {
                serde_json::json!(
                    "Lisez https://example.com/important pour les instructions completes."
                )
            } else {
                serde_json::json!("Traduisez les branches gauche | droite pour le client.")
            };
        }
        let complete = complete.to_string();
        let server = start_capturing_mock_server(vec![MockResponse::Json(
            serde_json::to_string(&complete)
                .unwrap()
                .trim_matches('"')
                .to_owned(),
        )])
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
        let outcome = translate_text(
            source,
            "fr",
            &TemplateType::Default,
            &PromptOpts::default(),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(outcome.text, complete);
        assert_ne!(outcome.text, source);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        assert_eq!(history.fetch_recent(None).unwrap().len(), 1);
    }
    assert!(ensure_document_identities_preserved(
        JSON_SOURCES[3],
        r#"{"outer":[{"wrong":"Traduction complete."}]}"#
    )
    .is_err());
    assert!(ensure_document_identities_preserved(
        "Keep {{customer_name}} safe.",
        "Gardez le compte en securite."
    )
    .is_err());
}

#[test]
fn oversized_json_fails_before_fragmenting_and_placeholder_ownership_keeps_code_and_links() {
    let config = make_stream_config("http://127.0.0.1:9/v1");
    let segmenter = fallback_segmenter();
    let source =
        serde_json::json!({"account_id": "Complete instructions. ".repeat(100)}).to_string();
    let error = plan_translation(
        &source,
        "fr",
        &config,
        &segmenter,
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("atomic JSON"));
    let source =
        "Keep `{{code_name}}` and [{{customer_name}}](https://example.com/account) available.";
    let plan = plan_translation(
        source,
        "fr",
        &config,
        &segmenter,
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    let result = plan.reconstruct(&vec![
        "Traduction complete".to_owned();
        plan.segment_count()
    ]);
    assert!(result.contains("`{{code_name}}`"));
    assert!(result.contains("[{{customer_name}}](https://example.com/account)"));
    assert!(ensure_document_identities_preserved(source, &result).is_ok());
    assert!(ensure_markdown_structure_preserved(source, &result).is_ok());
}
