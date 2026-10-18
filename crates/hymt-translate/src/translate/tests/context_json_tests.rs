use super::*;
use crate::exec_wrapper::translate_cached;
use hymt_cache::ExecCache;

#[tokio::test]
async fn container_content_retries_structure_before_publication() {
    let instruction = "Read the complete synthetic instruction.";
    let complete = "请完整阅读这一条合成操作说明。";
    for (prefix, inserted) in [
        ("", "- "),
        ("> ", "- "),
        ("> > ", "# "),
        ("- ", "> "),
        ("1. ", "- "),
        ("> - ", "# "),
    ] {
        for streaming in [false, true] {
            let source = format!("{prefix}{instruction}");
            let expected = format!("{prefix}{complete}");
            let invalid = format!("{inserted}{complete}");
            let server = start_capturing_mock_server(vec![
                if streaming {
                    MockResponse::Sse(vec![invalid.clone()])
                } else {
                    MockResponse::Json(invalid.clone())
                },
                MockResponse::Json(complete.to_owned()),
            ])
            .await;
            let config = verified_json_config(&server.endpoint_url);
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
                    &source,
                    "zh",
                    &TemplateType::Default,
                    &opts,
                    &ctx,
                    StreamOutputMode::Validated,
                    tx,
                )
                .await;
                let mut text = String::new();
                let mut done = 0;
                while let Some(event) = rx.recv().await {
                    match event {
                        StreamEvent::Token(token) => text.push_str(&token),
                        StreamEvent::AllDone(_) => done += 1,
                        _ => {}
                    }
                }
                assert_eq!(text, expected, "{prefix:?}: {result:?}");
                assert_eq!(done, 1);
                result
            } else {
                translate_text(&source, "zh", &TemplateType::Default, &opts, &ctx).await
            };
            assert_eq!(result.unwrap().text, expected, "{prefix:?}");
            assert_eq!(
                server.requests.lock().unwrap().len(),
                2,
                "reject before losing the retry: {prefix:?}"
            );
            assert_eq!(history.fetch_recent(None).unwrap().len(), 1);
            assert_eq!(
                translate_text(&source, "zh", &TemplateType::Default, &opts, &ctx)
                    .await
                    .unwrap()
                    .text,
                expected
            );
            assert_eq!(server.requests.lock().unwrap().len(), 2);
            let plan = plan_translation(
                &source,
                "zh",
                &config,
                &segmenter,
                &TemplateType::Default,
                &opts,
            )
            .unwrap();
            let contexts = segment_contexts(&plan).unwrap();
            assert_eq!(plan.segment_count(), 1);
            assert!(!cached_segment_is_complete(
                0,
                SegmentSource::new(&plan.segments[0], contexts[0]),
                &invalid
            ));
        }
    }
}

fn verified_json_config(endpoint: &str) -> HotConfig {
    let config = make_stream_config_with_fcp(endpoint, true);
    let text = std::fs::read_to_string(config.path())
        .unwrap()
        .replace("context_window = 512", "context_window = 8192")
        .replace("max_output_tokens = 40", "max_output_tokens = 4096");
    std::fs::write(config.path(), text).unwrap();
    HotConfig::from_path(config.path()).unwrap()
}

#[tokio::test]
async fn json_empty_value_retries_before_publication_and_cache() {
    for streaming in [false, true] {
        let source = serde_json::json!({"message": "First complete instruction.\r\n \t\nSecond complete instruction.\r\n\r\nThird complete instruction."}).to_string();
        let complete = serde_json::json!({"message": "请完整保留第一条操作说明。\n\n请完整保留第二条操作说明。\n\n请完整保留第三条操作说明。"}).to_string();
        let missing = serde_json::json!({"message": ""}).to_string();
        let response = |text: &str, stream: bool| {
            let encoded = serde_json::to_string(text).unwrap();
            let content = encoded[1..encoded.len() - 1].to_owned();
            if stream {
                MockResponse::Sse(vec![content])
            } else {
                MockResponse::Json(content)
            }
        };
        let server = start_capturing_mock_server(vec![
            response(&missing, streaming),
            response(&complete, false),
        ])
        .await;
        let config = verified_json_config(&server.endpoint_url);
        let client = TranslationClient::new(config.clone()).unwrap();
        let segmenter = fallback_segmenter();
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(dir.path().join("history.db"));
        let opts = PromptOpts::default();
        let policy = effective_document_translation_policy(&opts, &config);
        let options_hash = template_options_hash(&opts, policy);
        let fingerprint = config
            .inference_fingerprint(TemplateType::Default.as_str(), &options_hash)
            .unwrap();
        assert!(fingerprint.is_cache_verified());
        let profile = config.model_profile().unwrap().id().to_owned();
        let scope = SegmentCacheScope {
            target_lang: "zh",
            template_type: TemplateType::Default.as_str(),
            options_hash: &options_hash,
            profile_id: &profile,
            inference_fingerprint: fingerprint.hash(),
        };
        history
            .store_segment_cache(
                &segment_cache_hash(&source),
                scope,
                &missing,
                "2026-10-07T00:00:00Z",
            )
            .unwrap();
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: true,
        };
        let outcome = if streaming {
            let (tx, mut rx) = mpsc::channel(32);
            let outcome = translate_text_stream_with_mode(
                &source,
                "zh",
                &TemplateType::Default,
                &opts,
                &ctx,
                StreamOutputMode::Validated,
                tx,
            )
            .await;
            let mut published = String::new();
            let mut done = 0;
            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::Token(text) => published.push_str(&text),
                    StreamEvent::AllDone(_) => done += 1,
                    _ => {}
                }
            }
            assert_eq!(published, complete);
            assert_eq!(done, 1);
            outcome
        } else {
            translate_text(&source, "zh", &TemplateType::Default, &opts, &ctx).await
        };
        assert_eq!(outcome.unwrap().text, complete);
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        assert_eq!(history.fetch_recent(None).unwrap().len(), 1);
        let cache =
            ExecCache::with_user_path(dir.path().join("shared.db"), dir.path().join("user.db"));
        if streaming {
            cache
                .store_shared("man", "newlines", &source, "zh", policy, &missing)
                .unwrap();
        } else {
            cache
                .store_user("man", "newlines", &source, "zh", policy, &missing)
                .unwrap();
        }
        assert_eq!(
            translate_cached("man", "newlines", &source, "zh", &cache, &opts, &ctx)
                .await
                .unwrap(),
            complete
        );
        assert_eq!(
            server.requests.lock().unwrap().len(),
            2,
            "valid segment cache must serve regeneration"
        );
        let tasks = history.fetch_recent(None).unwrap().len();
        assert_eq!(
            translate_cached("man", "newlines", &source, "zh", &cache, &opts, &ctx)
                .await
                .unwrap(),
            complete
        );
        assert_eq!(history.fetch_recent(None).unwrap().len(), tasks);
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }
}

#[test]
fn source_context_preserves_inline_and_heading_prefix_roles() {
    let config = make_protected_block_config("http://127.0.0.1:9/v1");
    let segmenter = fallback_segmenter();
    for prefix in ["# ", "> # ", "- # ", "Count|", "*", "<b>"] {
        let suffix = match prefix {
            "*" => "*",
            "<b>" => "</b>",
            _ => "",
        };
        let source = format!("{prefix}Read the complete synthetic instruction.{suffix}");
        let config = leaf_fixture_config(&config, &source);
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        let contexts = segment_contexts(&plan).unwrap();
        let index = plan
            .segments
            .iter()
            .position(|segment| segment.starts_with("Read"))
            .unwrap();
        let leaf = SegmentSource::new(&plan.segments[index], contexts[index]);
        assert!(
            leaf.structure_preserved("- 请完整阅读这一条合成操作说明。")
                .is_ok(),
            "{prefix:?}"
        );
        assert_eq!(
            recover_generated_inline_code(leaf, "请完整阅读 `合成` 操作说明。").as_deref(),
            Some("请完整阅读 合成 操作说明。")
        );
        assert!(recover_generated_inline_code(leaf, "请完整阅读 `*合成*` 操作说明。").is_none());
    }
}

#[tokio::test]
async fn contextual_markdown_leaves_publish_and_reuse_cache() {
    for prefix in [">5 ", "# ", "- ", "1. ", "~~~ ", "--- ", "5 "] {
        assert_contextual_leaf(
            prefix,
            "请仔细审查所有相关代码。",
            "请仔细审查所有相关代码。",
        )
        .await;
    }
}

#[tokio::test]
async fn contextual_code_recovery_uses_original_leaf_context() {
    for prefix in [">5 ", "# ", "- ", "1. ", "~~~ ", "--- ", "5 "] {
        assert_contextual_leaf(
            prefix,
            "请仔细审查所有相关 `代码`。",
            "请仔细审查所有相关 代码。",
        )
        .await;
    }
}

async fn assert_contextual_leaf(prefix: &str, reply: &str, expected_leaf: &str) {
    for streaming in [false, true] {
        let source = format!("Count|{prefix}lines require a careful review.");
        let expected = format!("数量|{expected_leaf}");
        let server = start_capturing_mock_server(vec![
            if streaming {
                MockResponse::Sse(vec!["数量".to_owned()])
            } else {
                MockResponse::Json("数量".to_owned())
            },
            MockResponse::Json(reply.to_owned()),
            MockResponse::Json(reply.to_owned()),
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
        let opts = PromptOpts::default();
        let result = if streaming {
            let (tx, mut rx) = mpsc::channel(32);
            let result = translate_text_stream_with_mode(
                &source,
                "zh",
                &TemplateType::Default,
                &opts,
                &ctx,
                StreamOutputMode::Validated,
                tx,
            )
            .await;
            let mut tokens = String::new();
            let mut done = 0;
            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::Token(token) => tokens.push_str(&token),
                    StreamEvent::AllDone(_) => done += 1,
                    _ => {}
                }
            }
            assert!(result.is_ok(), "{prefix:?}: {result:?}");
            assert_eq!(tokens, expected);
            assert_eq!(done, 1);
            result
        } else {
            translate_text(&source, "zh", &TemplateType::Default, &opts, &ctx).await
        };
        assert_eq!(result.unwrap().text, expected, "{prefix:?}");
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        assert_eq!(
            translate_text(&source, "zh", &TemplateType::Default, &opts, &ctx)
                .await
                .unwrap()
                .text,
            expected
        );
        assert_eq!(
            server.requests.lock().unwrap().len(),
            2,
            "warm context must remain valid"
        );
        assert!(ensure_markdown_structure_preserved(&source, &expected).is_ok());
    }
}
#[tokio::test]
async fn json_content_admission_ignores_external_formatting_on_all_cache_paths() {
    let value = serde_json::json!({"message":"Read the complete synthetic instruction.","count":2,"enabled":true,"items":["First"]});
    let source = serde_json::to_string_pretty(&value)
        .unwrap()
        .replace('\n', &"\n".repeat(80));
    let complete = r#"{"message":"请完整阅读这一条合成操作说明。","count":2,"enabled":true,"items":["第一项"]}"#;
    assert_json_translation_and_cache_paths(&source, complete).await;
}

#[tokio::test]
async fn json_escaped_identities_publish_and_reuse_all_cache_paths() {
    let source = r#"{"message":"Read {{field}} and {{field}} at https://example.test/a and https://example.test/a again.","count":2}"#;
    let complete = r#"{"message":"请完整阅读 \u007b\u007bfield\u007d\u007d 和 \u007b\u007bfield\u007d\u007d，参见 https:\/\/example.test/a 和 https:\/\/example.test/a 的相关说明。","count":2}"#;
    assert_json_translation_and_cache_paths(source, complete).await;
}

async fn assert_json_translation_and_cache_paths(source: &str, complete: &str) {
    let encoded = serde_json::to_string(complete).unwrap();
    for streaming in [false, true] {
        let server = start_capturing_mock_server(vec![
            if streaming {
                MockResponse::Sse(vec![encoded[1..encoded.len() - 1].to_owned()])
            } else {
                MockResponse::Json(encoded[1..encoded.len() - 1].to_owned())
            },
            MockResponse::Json(encoded[1..encoded.len() - 1].to_owned()),
        ])
        .await;
        let config = verified_json_config(&server.endpoint_url);
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
                "zh",
                &TemplateType::Default,
                &opts,
                &ctx,
                StreamOutputMode::Validated,
                tx,
            )
            .await;
            let mut text = String::new();
            let mut done = 0;
            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::Token(s) => text.push_str(&s),
                    StreamEvent::AllDone(_) => done += 1,
                    _ => {}
                }
            }
            assert!(result.is_ok(), "{result:?}");
            assert_eq!(text, complete);
            assert_eq!(done, 1);
            result
        } else {
            translate_text(source, "zh", &TemplateType::Default, &opts, &ctx).await
        };
        assert_eq!(result.unwrap().text, complete);
        assert_eq!(
            translate_text(source, "zh", &TemplateType::Default, &opts, &ctx)
                .await
                .unwrap()
                .text,
            complete
        );
        assert_eq!(
            server.requests.lock().unwrap().len(),
            1,
            "valid segment cache must be reusable"
        );
        let policy = effective_document_translation_policy(&opts, &config);
        for shared in [false, true] {
            let cache = ExecCache::with_user_path(
                dir.path().join(format!("shared-{shared}.db")),
                dir.path().join(format!("user-{shared}.db")),
            );
            if shared {
                cache
                    .store_shared("man", "fixture", source, "zh", policy, complete)
                    .unwrap();
            } else {
                cache
                    .store_user("man", "fixture", source, "zh", policy, complete)
                    .unwrap();
            }
            assert_eq!(
                translate_cached("man", "fixture", source, "zh", &cache, &opts, &ctx)
                    .await
                    .unwrap(),
                complete
            );
            assert_eq!(
                server.requests.lock().unwrap().len(),
                1,
                "whole JSON cache must ignore formatting"
            );
        }
    }
}

#[test]
fn json_nonempty_values_may_change_length_and_paragraph_layout() {
    let source = serde_json::json!({"message":"First instruction.\n\nSecond instruction.\n\nThird instruction.","other":"Read all instructions.","count":7});
    for output in ["译", "One merged paragraph.", "无法翻译"] {
        let translated = serde_json::json!({"message":output,"other":"内容","count":7});
        assert!(validate_completeness(&source.to_string(), &translated.to_string()).is_complete);
    }
    let empty = serde_json::json!({"message":" ","other":"内容","count":7});
    assert!(
        validate_completeness(&source.to_string(), &empty.to_string())
            .checks_failed
            .iter()
            .any(|code| code == "empty_output")
    );
}

#[tokio::test]
async fn json_empty_string_revalidates_whole_cache_and_retries_before_success() {
    let mut source = serde_json::json!({"message":"Read this complete instruction. Keep all important details available throughout the entire migration.","other":"Keep this additional instruction available throughout the migration."});
    source
        .as_object_mut()
        .unwrap()
        .insert("unchanged_key_padding_".repeat(20), serde_json::json!(7));
    let mut complete = source.clone();
    complete["message"] =
        serde_json::json!("请完整阅读操作说明，并在整个迁移过程中始终保留所有重要细节和相关内容。");
    complete["other"] =
        serde_json::json!("迁移过程中请始终保留这一条额外操作说明，并持续检查所有相关服务。");
    let mut missing = complete.clone();
    missing["message"] = serde_json::json!("");
    let (source, complete, missing) = (
        source.to_string(),
        complete.to_string(),
        missing.to_string(),
    );
    for shared in [false, true] {
        let replies = [&missing, &complete].map(|s| {
            let encoded = serde_json::to_string(s).unwrap();
            MockResponse::Json(encoded[1..encoded.len() - 1].to_owned())
        });
        let server = start_capturing_mock_server(Vec::from(replies)).await;
        let config = verified_json_config(&server.endpoint_url);
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
        let policy = effective_document_translation_policy(&opts, &config);
        let cache =
            ExecCache::with_user_path(dir.path().join("shared.db"), dir.path().join("user.db"));
        if shared {
            cache
                .store_shared("man", "fixture", &source, "zh", policy, &missing)
                .unwrap();
        } else {
            cache
                .store_user("man", "fixture", &source, "zh", policy, &missing)
                .unwrap();
        }
        assert_eq!(
            translate_cached("man", "fixture", &source, "zh", &cache, &opts, &ctx)
                .await
                .unwrap(),
            complete,
            "empty JSON value must be regenerated"
        );
        assert_eq!(
            server.requests.lock().unwrap().len(),
            2,
            "reject cached empty value, retry invalid response, then admit complete response"
        );
        assert_eq!(history.fetch_recent(None).unwrap().len(), 1);
        assert_eq!(
            translate_cached("man", "fixture", &source, "zh", &cache, &opts, &ctx)
                .await
                .unwrap(),
            complete
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }
}

#[test]
fn fragment_context_scans_source_once() {
    let config = make_protected_block_config("http://127.0.0.1:9/v1");
    let segmenter = fallback_segmenter();
    let mut samples = Vec::new();
    for count in [64, 128] {
        let source = "Count|>5 lines require a careful review. ".repeat(count);
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        markdown_context::take_context_work();
        assert_eq!(segment_contexts(&plan).unwrap().len(), plan.segment_count());
        let work = markdown_context::take_context_work();
        assert!(work > 0);
        assert!(work <= source.len());
        samples.push(work);
    }
    assert!(samples[1] <= samples[0] * 3);
}

#[tokio::test]
async fn warn_only_json_cannot_hide_one_empty_string() {
    let source = serde_json::json!({"message":"Read the complete instruction and retain every important detail throughout the entire migration.","other":"Keep this additional complete instruction available."});
    let missing = serde_json::json!({"message":"","other":"请始终保留另一条额外操作说明，并在整个迁移过程中检查所有相关服务。"});
    let encoded = serde_json::to_string(&missing.to_string()).unwrap();
    let server = start_capturing_mock_server(
        (0..2)
            .map(|_| MockResponse::Json(encoded[1..encoded.len() - 1].to_owned()))
            .collect(),
    )
    .await;
    let config = verified_json_config(&server.endpoint_url);
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
        warn_only_completeness: Some(true),
        ..PromptOpts::default()
    };
    let error = translate_text(
        &source.to_string(),
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("empty_output"));
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    assert!(history.fetch_recent(None).unwrap().is_empty());
}
