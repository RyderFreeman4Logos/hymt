use super::*;

#[tokio::test]
async fn default_exhaustion_retains_complete_original_and_records_no_cold_sample() {
    let source = "This paragraph must remain fully available after validation fails.";
    let server = start_mock_server(
        (0..2)
            .map(|_| MockResponse::JsonWithFinishReason {
                content: String::new(),
                finish_reason: "length".into(),
            })
            .collect(),
    )
    .await;
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
    .expect("exhausted transport validation must retain the original paragraph by default");
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

fn oversized_reference_table_source() -> (String, String) {
    let table = format!(
        "| Header | Detail |\n| --- | --- |\n| [guide] | ![icon][] {} |\n",
        "padding ".repeat(300)
    );
    let source = format!(
        "Before marker.\n{table}After marker.\n\n[guide]: https://example.test/guide\n[icon]: https://example.test/icon.png\n"
    );
    (source, table)
}

async fn successful_oversized_owner_case(
    source: &str,
    retain: bool,
    streaming: bool,
) -> (TranslationOutcome, String, Vec<String>, Vec<String>) {
    let planning_config = if retain {
        retention_config("http://127.0.0.1:1/v1")
    } else {
        make_protected_block_config("http://127.0.0.1:1/v1")
    };
    let segmenter = fallback_segmenter();
    let opts = PromptOpts::default();
    let plan = plan_translation(
        source,
        "zh",
        &planning_config,
        &segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    assert_eq!(plan.segments, ["Before marker.", "After marker."]);
    let replies = planned_complete_translations(&plan);
    let expected = plan.reconstruct(&replies);
    let responses = replies
        .iter()
        .enumerate()
        .map(|(index, reply)| {
            if streaming && index == 0 {
                MockResponse::Sse(vec![reply.clone()])
            } else {
                MockResponse::Json(reply.clone())
            }
        })
        .collect();
    let server = start_capturing_mock_server(responses).await;
    let config = if retain {
        retention_config(&server.endpoint_url)
    } else {
        make_protected_block_config(&server.endpoint_url)
    };
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
    let (outcome, emitted) = if streaming {
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
            if let StreamEvent::Token(text) = event {
                emitted.push_str(&text);
            }
        }
        (outcome, emitted)
    } else {
        let outcome = translate_text(source, "zh", &TemplateType::Default, &opts, &ctx)
            .await
            .unwrap();
        let emitted = outcome.text.clone();
        (outcome, emitted)
    };
    let requests = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| {
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            body["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once("\n\n")
                .unwrap()
                .1
                .to_owned()
        })
        .collect();
    assert_eq!(outcome.text, expected);
    (outcome, emitted, replies, requests)
}

#[tokio::test]
async fn oversized_copy_only_reference_table_keeps_original_bytes_in_strict_pipeline() {
    let (source, table) = oversized_reference_table_source();
    let (outcome, _, replies, requests) =
        successful_oversized_owner_case(&source, false, false).await;
    assert_eq!(requests, ["Before marker.", "After marker."]);
    assert_eq!(outcome.text.matches(&table).count(), 1);
    assert!(!outcome.text.contains("[guide][guide]"));
    assert!(!outcome.text.contains("![icon][icon]"));
    assert!(outcome.text.find(&replies[0]).unwrap() < outcome.text.find(&table).unwrap());
    assert!(outcome.text.find(&table).unwrap() < outcome.text.find(&replies[1]).unwrap());
}

#[tokio::test]
async fn oversized_owner_keeps_successful_surrounding_replies_and_failed_control_restores_original()
{
    let (source, table) = oversized_reference_table_source();
    for retain in [true, false] {
        for streaming in [false, true] {
            let (outcome, emitted, replies, requests) =
                successful_oversized_owner_case(&source, retain, streaming).await;
            assert_eq!(requests, ["Before marker.", "After marker."]);
            assert_eq!(emitted, outcome.text);
            assert!(!outcome.is_completeness_degraded());
            assert!(outcome.retained_regions.is_empty());
            assert_eq!(outcome.text.matches(&table).count(), 1);
            assert!(!outcome.text.contains("[guide][guide]"));
            assert!(!outcome.text.contains("![icon][icon]"));
            let before = outcome.text.find(&replies[0]).unwrap();
            let protected = outcome.text.find(&table).unwrap();
            let after = outcome.text.find(&replies[1]).unwrap();
            assert!(before < protected && protected < after);
        }
    }

    let server = start_capturing_mock_server(
        (0..4)
            .map(|_| MockResponse::JsonWithFinishReason {
                content: String::new(),
                finish_reason: "length".into(),
            })
            .collect(),
    )
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
        cache_enabled: false,
    };
    let outcome = translate_text(
        &source,
        "zh",
        &TemplateType::Default,
        &PromptOpts::default(),
        &ctx,
    )
    .await
    .unwrap();
    assert!(outcome.is_completeness_degraded());
    assert_eq!(outcome.retained_regions.len(), 1);
    let retained = &outcome.retained_regions[0];
    assert_eq!(
        &outcome.text[retained.output_bytes.clone()],
        &source[retained.source_bytes.clone()]
    );
    assert_eq!(outcome.text.matches(&table).count(), 1);
    assert_eq!(outcome.completeness_degraded_segments, [1, 2]);
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
        let (output, regions) = retained_original::finalize(source, &plan, &mut completed, &mut degraded).unwrap();
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
fn oversized_copy_only_owner_does_not_trigger_fallback_notice() {
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
    let mut degraded = Vec::new();
    let (output, regions) =
        retained_original::finalize(&source, &plan, &mut completed, &mut degraded).unwrap();
    assert_eq!(output, source);
    assert!(regions.is_empty());
    assert!(degraded.is_empty());
}

#[tokio::test]
async fn default_stream_tokens_equal_marked_final_and_failed_text_never_leaks() {
    let source = "The complete synthetic source must be retained after retries fail.";
    let server = start_mock_server(vec![
        MockResponse::SseWithFinishReason {
            tokens: vec!["**bad**".into()],
            finish_reason: "length".into(),
        },
        MockResponse::JsonWithFinishReason {
            content: "**bad**".into(),
            finish_reason: "length".into(),
        },
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

#[test]
fn budget_fitting_inline_paragraph_is_one_coherent_request() {
    let source = "Read *17 credits* beside [the public guide](https://example.test/guide) using `--safe` before choosing **the next action**.\n";
    let config = make_protected_block_config("http://127.0.0.1:1/v1");
    let plan = plan_translation(
        source,
        "zh",
        &config,
        &fallback_segmenter(),
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    assert_eq!(
        plan.segments,
        vec![source],
        "inline syntax must not fragment a budget-fitting original paragraph"
    );
    assert!(plan
        .token_budget
        .segment_input_tokens
        .iter()
        .all(|&tokens| tokens <= plan.token_budget.input_capacity_tokens));
    let translated = "阅读 *17 点数* 并参看 [公共指南](https://example.test/guide) 使用 `--safe` 再选择 **下一步操作**.";
    let mut completed = vec![translated.to_owned()];
    let (output, regions) =
        retained_original::finalize(source, &plan, &mut completed, &mut vec![]).unwrap();
    assert!(regions.is_empty());
    assert!(output.contains(translated));
}

#[test]
fn numeric_citations_keep_complete_owner_and_exact_occurrences() {
    let source = "Compare *the first result* [12] with *the second result* [34] before deciding.\n";
    let config = make_protected_block_config("http://127.0.0.1:1/v1");
    let plan = plan_translation(
        source,
        "zh",
        &config,
        &fallback_segmenter(),
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    assert_eq!(plan.segments, vec![source]);
    assert!(plan.coherent_paragraphs.contains(&1));
    let plain = "Compare this finding [12] with that finding [34].\n";
    let plain_plan = plan_translation(
        plain,
        "zh",
        &config,
        &fallback_segmenter(),
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    assert_eq!(plain_plan.segments, vec![plain]);
    assert!(plain_plan.coherent_paragraphs.contains(&1));
    let reference = format!("{source}\n[12]: https://example.test/reference\n");
    let reference_plan = plan_translation(
        &reference,
        "zh",
        &config,
        &fallback_segmenter(),
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    assert!(!reference_plan.coherent_paragraphs.contains(&1));
    let resolved = reference_plan.reconstruct(&reference_plan.segments);
    assert!(resolved.contains("[12][12]"));
    assert!(resolved.contains("[12]: https://example.test/reference"));
    for unsupported in [
        "Read *the result* [word] before deciding.",
        "Read *the result* [12][] before deciding.",
        "Read *the result* ![12] before deciding.",
        "Read *the result* [12](broken before deciding.",
        "Read *the result* [12] (broken before deciding.",
        "Read *the result* \\[12] before deciding.",
        "Read *the result* $12$ before deciding.",
    ] {
        let plan = plan_translation(
            unsupported,
            "zh",
            &config,
            &fallback_segmenter(),
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert!(plan.coherent_paragraphs.is_empty(), "{unsupported}");
    }
}

const COHERENT_SOURCE: &str = "Read *17 credits* [12] beside [the public guide](https://example.test/guide) using `--safe` [34] before choosing **the next action**.\n";
const COHERENT_TRANSLATION: &str = "阅读 *17 点数* [12] 并参看 [公共指南](https://example.test/guide) 使用 `--safe` [34] 再选择 **下一步操作**.";

fn coherent_config(endpoint: &str, retain: bool) -> HotConfig {
    let config = make_protected_block_config(endpoint);
    if retain {
        let text = std::fs::read_to_string(config.path())
            .unwrap()
            .replace("warn_only = false\n", "");
        std::fs::write(config.path(), text).unwrap();
    }
    let mut text = std::fs::read_to_string(config.path()).unwrap();
    text.push_str("\n[inference.override]\ntemperature=0.7\ntop_p=0.6\ntop_k=20\nrepetition_penalty=1.05\nmin_p=0.0\nrepeat_last_n=64\n");
    std::fs::write(config.path(), text).unwrap();
    let config = HotConfig::from_path(config.path()).unwrap();
    config.set_backend_runtime_info(
        hymt_core::runtime::BackendRuntimeInfo::from_llama_cpp_props(
            &serde_json::json!({"model":"test-model", "model_path":"test-model", "n_ctx_per_seq":8192}),
            123,
        )
        .unwrap(),
    );
    config
}

#[tokio::test]
async fn coherent_http_request_keeps_context_and_roundtrips_offline_proof() {
    for streaming in [false, true] {
        let response = if streaming {
            MockResponse::Sse(vec![COHERENT_TRANSLATION.into()])
        } else {
            MockResponse::Json(COHERENT_TRANSLATION.into())
        };
        let server = start_capturing_mock_server(vec![response]).await;
        let config = coherent_config(&server.endpoint_url, false);
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
        let outcome = if streaming {
            let (tx, mut rx) = mpsc::channel(16);
            let outcome = translate_text_stream_with_mode(
                COHERENT_SOURCE,
                "zh",
                &TemplateType::Default,
                &opts,
                &ctx,
                StreamOutputMode::Optimistic,
                tx,
            )
            .await
            .unwrap();
            let mut tokens = String::new();
            while let Some(event) = rx.recv().await {
                if let StreamEvent::Token(text) = event {
                    tokens.push_str(&text);
                }
            }
            assert_eq!(tokens, outcome.text);
            outcome
        } else {
            translate_text(COHERENT_SOURCE, "zh", &TemplateType::Default, &opts, &ctx)
                .await
                .unwrap()
        };
        assert!(!outcome.is_completeness_degraded());
        assert_eq!(outcome.text, format!("{COHERENT_TRANSLATION}\n"));
        let requests = server.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "one coherent paragraph must make one provider request"
        );
        let body: serde_json::Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(
            body["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains(COHERENT_SOURCE)
        );
        drop(requests);
        assert_eq!(
            offline_replay::find_replay(
                COHERENT_SOURCE,
                "zh",
                &TemplateType::Default,
                &opts,
                &ctx,
                streaming
            )
            .unwrap()
            .and_then(|replay| replay.record.output_text)
            .as_deref(),
            Some(outcome.text.as_str())
        );
        assert_eq!(
            server.requests.lock().unwrap().len(),
            1,
            "offline proof replay must not contact the provider"
        );
    }
}

#[tokio::test]
async fn complex_paragraph_requests_cover_source_and_map_replies_in_order() {
    const FIRST: &str = "Alpha repeats *common* [12] with a unique first marker.\n";
    const LAST: &str = "Beta repeats *common* [12] with a unique last marker.\n";
    const OPAQUE: &str = "```txt\nopaque [99]\n```\n";
    const FIRST_REPLY: &str = "甲段重复 *共有* [12]，首标记。";
    const LAST_REPLY: &str = "乙段重复 *共有* [12]，末标记。";
    let source = format!("{FIRST}\n{OPAQUE}\n{LAST}");
    let expected = format!("{FIRST_REPLY}\n\n{OPAQUE}\n{LAST_REPLY}\n");

    for streaming in [false, true] {
        let replies = [FIRST_REPLY, LAST_REPLY];
        let responses = replies
            .into_iter()
            .enumerate()
            .map(|(index, reply)| {
                if streaming && index == 0 {
                    MockResponse::Sse(vec![reply.to_owned()])
                } else {
                    MockResponse::Json(reply.to_owned())
                }
            })
            .collect();
        let server = start_capturing_mock_server(responses).await;
        let config = coherent_config(&server.endpoint_url, false);
        let segmenter = fallback_segmenter();
        let opts = PromptOpts::default();
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &opts,
        )
        .unwrap();
        let document = plan.document_plan.as_ref().unwrap();
        let mut offset = 0;
        for section in &document.sections {
            let end = offset + section.text.len();
            assert_eq!(source.get(offset..end), Some(section.text.as_str()));
            offset = end;
        }
        assert_eq!(offset, source.len());
        assert_eq!(plan.segments, [FIRST, LAST]);
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
                &source,
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
                if let StreamEvent::Token(text) = event {
                    tokens.push_str(&text);
                }
            }
            assert_eq!(tokens, outcome.text);
            outcome
        } else {
            translate_text(&source, "zh", &TemplateType::Default, &opts, &ctx)
                .await
                .unwrap()
        };
        let requests = server.requests.lock().unwrap();
        let captured: Vec<_> = requests
            .iter()
            .map(|request| {
                let body: serde_json::Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                body["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .split_once("\n\n")
                    .unwrap()
                    .1
                    .to_owned()
            })
            .collect();
        assert_eq!(captured, [FIRST, LAST]);
        assert_eq!(outcome.text, expected);
        assert!(!outcome.is_completeness_degraded());
    }
}

#[test]
fn coherent_selection_keeps_budget_and_source_reconstruction() {
    let segmenter = fallback_segmenter();
    let config = make_stream_config("http://127.0.0.1:1/v1");
    let plan = plan_translation(
        COHERENT_SOURCE,
        "zh",
        &config,
        &segmenter,
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    assert!(
        plan.coherent_paragraphs.is_empty(),
        "rendered prompt budget must win over raw paragraph eligibility"
    );
    assert!(plan.segment_count() > 1);
    assert_eq!(plan.reconstruct(&plan.segments), COHERENT_SOURCE);
    assert!(plan
        .token_budget
        .segment_input_tokens
        .iter()
        .all(|&tokens| tokens <= plan.token_budget.input_capacity_tokens));
    let config = make_protected_block_config("http://127.0.0.1:1/v1");
    for source in [
        "Read [guide](https://example.test/guide)`code`.",
        "Read https://example.test/a[guide](https://example.test/b) carefully.",
        "Read <https://example.test/a> and *the guide*.",
        "Read *the guide* and {{field}}.",
    ] {
        let plan = plan_translation(
            source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert_eq!(plan.reconstruct(&plan.segments), source);
    }
}

#[tokio::test]
async fn transport_retries_stay_inside_the_admitted_request_budget() {
    for context_window in [480, 509, 510] {
        for streaming in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let handle = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let request = read_http_request(&mut socket).await.unwrap();
                    let request = String::from_utf8(request).unwrap();
                    let body: serde_json::Value =
                        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                    let prompt = body["messages"].as_array().unwrap().last().unwrap()["content"]
                        .as_str()
                        .unwrap();
                    let source = prompt.split_once("\n\n").unwrap().1;
                    let reply = source
                        .split_once(COMPLETENESS_RETRY_INSTRUCTION)
                        .map_or_else(String::new, |(source, _)| source.to_owned());
                    let encoded = serde_json::to_string(&reply).unwrap();
                    let reply = encoded[1..encoded.len() - 1].to_owned();
                    let response = if !prompt.contains(COMPLETENESS_RETRY_INSTRUCTION) {
                        if body["stream"] == true {
                            MockResponse::SseWithFinishReason {
                                tokens: vec![reply],
                                finish_reason: "length".into(),
                            }
                        } else {
                            MockResponse::JsonWithFinishReason {
                                content: reply,
                                finish_reason: "length".into(),
                            }
                        }
                    } else {
                        MockResponse::Json(reply)
                    };
                    captured.lock().unwrap().push(request);
                    write_mock_response(socket, response).await.unwrap();
                }
            });
            let server = CapturingMockServer {
                endpoint_url: format!("http://{address}/v1"),
                requests,
                handle,
            };
            let config = make_protected_block_config(&server.endpoint_url);
            let contents = std::fs::read_to_string(config.path())
                .unwrap()
                .replace(
                    "context_window = 8192",
                    &format!("context_window = {context_window}"),
                )
                .replace("max_output_tokens = 4096", "max_output_tokens = 256");
            std::fs::write(config.path(), contents).unwrap();
            let config = HotConfig::from_path(config.path()).unwrap();
            let segmenter = fallback_segmenter();
            let opts = PromptOpts::default();
            let plan = plan_translation(
                COHERENT_SOURCE,
                "en",
                &config,
                &segmenter,
                &TemplateType::Default,
                &opts,
            )
            .unwrap();
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
                let collector = tokio::spawn(async move {
                    let mut tokens = String::new();
                    while let Some(event) = rx.recv().await {
                        if let StreamEvent::Token(text) = event {
                            tokens.push_str(&text);
                        }
                    }
                    tokens
                });
                let outcome = translate_text_stream_with_mode(
                    COHERENT_SOURCE,
                    "en",
                    &TemplateType::Default,
                    &opts,
                    &ctx,
                    StreamOutputMode::Validated,
                    tx,
                )
                .await
                .unwrap();
                assert_eq!(collector.await.unwrap(), outcome.text);
                outcome
            } else {
                translate_text(COHERENT_SOURCE, "en", &TemplateType::Default, &opts, &ctx)
                    .await
                    .unwrap()
            };
            assert_eq!(outcome.text, COHERENT_SOURCE);
            assert!(!outcome.is_completeness_degraded());
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), plan.segment_count() * 2);
            for request in requests.iter() {
                let body: serde_json::Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                let prompt = body["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap();
                let tokens =
                    count_final_request(config.model_profile().unwrap(), &segmenter, prompt);
                assert!(
                    tokens.tokens <= plan.token_budget.input_capacity_tokens,
                    "captured request exceeds admitted input budget: {}/{} (context={context_window}, streaming={streaming})",
                    tokens.tokens,
                    plan.token_budget.input_capacity_tokens
                );
            }
            assert_eq!(!plan.coherent_paragraphs.is_empty(), context_window == 510);
        }
    }
}
