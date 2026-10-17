use super::*;

#[tokio::test]
async fn default_exhaustion_retains_complete_original_and_records_no_cold_sample() {
    let source = "This paragraph must remain fully available after validation fails.";
    let server =
        start_mock_server((0..2).map(|_| MockResponse::Json(String::new())).collect()).await;
    let config = retention_config(&server.endpoint_url);
    config.set_backend_runtime_info(
        hymt_core::runtime::BackendRuntimeInfo::from_llama_cpp_props(
            &serde_json::json!({"model":"test-model", "n_ctx_per_seq":512}),
            123,
        )
        .unwrap(),
    );
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
        "zh",
        &TemplateType::Default,
        &PromptOpts::default(),
        &ctx,
    )
    .await
    .expect("exhausted content validation must retain the original paragraph by default");
    assert!(outcome.is_completeness_degraded());
    assert_eq!(outcome.text.matches(source).count(), 1);
    assert!(outcome.text.contains("Untranslated original"));
    let record = history.fetch_recent(None).unwrap().pop().unwrap();
    assert!(record.cold_source_tokens.is_none());
    assert!(record.estimated_seconds.is_none());
    assert_eq!(record.tokens_per_second, 0.0);
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
    assert!(history
        .find_segment_cached(&segment_cache_hash(source), scope)
        .unwrap()
        .is_none());
    assert!(
        offline_replay::find_replay(source, "zh", &TemplateType::Default, &opts, &ctx, false)
            .unwrap()
            .is_none()
    );
}

fn retention_config(endpoint: &str) -> HotConfig {
    let config = make_stream_config(endpoint);
    let text = std::fs::read_to_string(config.path())
        .unwrap()
        .replace("warn_only = false\n", "");
    std::fs::write(config.path(), text).unwrap();
    HotConfig::from_path(config.path()).unwrap()
}

#[test]
fn owner_restoration_is_occurrence_bound_before_reference_normalization() {
    for source in [
        "First paragraph.\r\n\r\n> - Read [guide][] and *17 credits* with `$x$`.\r\n>   Keep the image ![icon](icon.png).\r\n\r\nLast paragraph.\r\n\r\n[guide]: https://example.test/guide\r\n",
        "Repeated paragraph.\n\nRepeated paragraph.\n\nRepeated paragraph.",
    ] {
        let config = retention_config("http://127.0.0.1:1/v1");
        let plan = plan_translation(source, "zh", &config, &fallback_segmenter(), &TemplateType::Default, &PromptOpts::default()).unwrap();
        let document = plan.document_plan.as_ref().unwrap();
        let failed = plan.segment_section_groups.iter().position(|group| document.sections[group[0]].paragraph_index == Some(2)).unwrap();
        let mut completed = plan.segments.clone();
        for (index, text) in completed.iter_mut().enumerate() {
            if document.sections[plan.segment_section_groups[index][0]].paragraph_index != Some(2) {
                *text = text.replace("First paragraph.", "第一段内容。")
                    .replace("Last paragraph.", "最后一段内容。")
                    .replace("Repeated paragraph.", "重复段落内容。");
            }
        }
        completed[failed] = String::new();
        let mut degraded = vec![failed + 1];
        let (output, regions) = retained_original::finalize(source, &plan, &mut completed, &mut degraded, true).unwrap();
        assert_eq!(regions.len(), 1);
        let region = &regions[0];
        assert_eq!(region.paragraph_index, 2);
        assert_eq!(&output[region.output_bytes.clone()], &source[region.source_bytes.clone()]);
        assert!(output.contains(if source.starts_with("Repeated") { "重复段落内容。" } else { "第一段内容。" }));
        assert!(!output.contains("[guide][guide]"), "failed owner must use pre-normalization bytes");
        for (index, group) in plan.segment_section_groups.iter().enumerate() {
            if document.sections[group[0]].paragraph_index == Some(2) { assert!(degraded.contains(&(index + 1))); }
        }
    }
}

#[test]
fn structural_only_and_oversized_owners_have_safe_root_notice() {
    let source = format!(
        "---\ntitle: synthetic\n---\n\n| Heading | Other |\n| --- | --- |\n| {} | value |\n",
        "large ".repeat(100)
    );
    let config = retention_config("http://127.0.0.1:1/v1");
    let plan = plan_translation(
        &source,
        "zh",
        &config,
        &fallback_segmenter(),
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    let mut completed = plan.segments.clone();
    let (output, regions) =
        retained_original::finalize(&source, &plan, &mut completed, &mut Vec::new(), true).unwrap();
    assert_eq!(regions.len(), 1);
    assert!(output.starts_with("---\ntitle: synthetic\n---\n\nTranslation incomplete."));
    assert_eq!(
        &output[regions[0].output_bytes.clone()],
        &source[regions[0].source_bytes.clone()]
    );
}

#[test]
fn synthetic_repeated_emphasis_omission_uses_existing_structure_guard() {
    let source = "First *17 credits*, second *17 credits*, then *three*, *four*, *five*, *six*.";
    let candidate = "First *17 credits*, second credits, then *three*, *four*, *five*, *six*.";
    assert!(SegmentSource::from(source)
        .structure_preserved(candidate)
        .is_err());
}

#[tokio::test]
async fn default_stream_tokens_equal_marked_final_and_failed_text_never_leaks() {
    let source = "The complete synthetic source must be retained after retries fail.";
    let server = start_mock_server(vec![
        MockResponse::Sse(vec!["bad".into()]),
        MockResponse::Json("bad".into()),
    ])
    .await;
    let config = retention_config(&server.endpoint_url);
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
    let (tx, mut rx) = mpsc::channel(16);
    let outcome = translate_text_stream_with_mode(
        source,
        "zh",
        &TemplateType::Default,
        &PromptOpts::default(),
        &ctx,
        StreamOutputMode::Optimistic,
        tx,
    )
    .await
    .unwrap();
    let mut tokens = String::new();
    let mut terminal = None;
    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Token(text) => tokens.push_str(&text),
            StreamEvent::AllDone(text) => terminal = Some(text),
            _ => {}
        }
    }
    assert_eq!(tokens, outcome.text);
    assert_eq!(terminal.as_deref(), Some(tokens.as_str()));
    assert!(!tokens.contains("bad"));
    assert_eq!(
        &tokens[outcome.retained_regions[0].output_bytes.clone()],
        source
    );
}

#[tokio::test]
async fn retained_original_mode_does_not_convert_cancellation_to_success() {
    let config = retention_config("http://127.0.0.1:1/v1");
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
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    assert!(translate_text_stream(
        "Complete synthetic source.",
        "zh",
        &TemplateType::Default,
        &PromptOpts::default(),
        &ctx,
        tx
    )
    .await
    .is_err());
    assert!(history.fetch_recent(None).unwrap().is_empty());
}
