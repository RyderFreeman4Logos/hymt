use super::*;
use crate::exec_wrapper::translate_cached;
use hymt_cache::ExecCache;

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
async fn contextual_markdown_leaves_publish_and_reuse_cache() {
    for prefix in [">5 ", "# ", "- ", "1. ", "~~~ ", "--- ", "5 "] {
        for streaming in [false, true] {
            let source = format!("Count|{prefix}lines require a careful review.");
            let expected = "数量|请仔细审查所有相关代码。";
            let server = start_capturing_mock_server(vec![
                if streaming {
                    MockResponse::Sse(vec!["数量".to_owned()])
                } else {
                    MockResponse::Json("数量".to_owned())
                },
                MockResponse::Json("请仔细审查所有相关代码。".to_owned()),
                MockResponse::Json("请仔细审查所有相关代码。".to_owned()),
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
            assert!(ensure_markdown_structure_preserved(&source, expected).is_ok());
        }
    }
}

#[tokio::test]
async fn json_content_admission_ignores_external_formatting_on_all_cache_paths() {
    let value = serde_json::json!({"message":"Read the complete synthetic instruction.","count":2,"enabled":true,"items":["First"]});
    let source = serde_json::to_string_pretty(&value)
        .unwrap()
        .replace('\n', &"\n".repeat(80));
    let complete = r#"{"message":"请完整阅读这一条合成操作说明。","count":2,"enabled":true,"items":["第一项"]}"#;
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
            translate_text(&source, "zh", &TemplateType::Default, &opts, &ctx).await
        };
        assert_eq!(result.unwrap().text, complete);
        assert_eq!(
            translate_text(&source, "zh", &TemplateType::Default, &opts, &ctx)
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
                    .store_shared("man", "fixture", &source, "zh", policy, complete)
                    .unwrap();
            } else {
                cache
                    .store_user("man", "fixture", &source, "zh", policy, complete)
                    .unwrap();
            }
            assert_eq!(
                translate_cached("man", "fixture", &source, "zh", &cache, &opts, &ctx)
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
fn json_density_measures_each_string_instead_of_keys_and_layout() {
    let mut source =
        serde_json::json!({"other":"Keep this entire additional instruction available."});
    source.as_object_mut().unwrap().insert(
        "long_unchanged_key_padding".repeat(100),
        serde_json::json!(
            "Read the complete synthetic instruction. Another complete sentence must also survive."
        ),
    );
    let mut missing = source.clone();
    *missing
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap() = serde_json::json!("译");
    let missing = validate_completeness_with_context(
        &source.to_string(),
        &missing.to_string(),
        "zh",
        None,
        &CompletenessContext::default(),
    );
    assert!(
        !missing.is_complete,
        "unchanged keys or another long value must not hide string truncation"
    );
    assert!(missing.checks_failed.iter().any(|s| s == "token_ratio"));
}

#[test]
fn json_string_paragraphs_remain_content_and_other_values_cannot_mask_loss() {
    let source = serde_json::json!({"message":"First complete instruction.\n\nSecond complete instruction.\n\nThird complete instruction.","other":"Keep this additional instruction available."});
    let complete = serde_json::json!({"message":"请完整保留第一条操作说明。\n\n请完整保留第二条操作说明。\n\n请完整保留第三条操作说明。","other":"请完整保留这一条额外操作说明。"});
    assert!(
        validate_completeness_with_context(
            &source.to_string(),
            &complete.to_string(),
            "zh",
            None,
            &CompletenessContext::default()
        )
        .is_complete
    );
    let mut flattened = complete.clone();
    flattened["message"] = serde_json::json!(
        "请完整保留第一条操作说明。请完整保留第二条操作说明。请完整保留第三条操作说明。"
    );
    let result = validate_completeness_with_context(
        &source.to_string(),
        &flattened.to_string(),
        "zh",
        None,
        &CompletenessContext::default(),
    );
    assert!(result
        .checks_failed
        .iter()
        .any(|code| code == "paragraph_count"));
    let mut truncated = complete;
    truncated["message"] = serde_json::json!("译");
    truncated["other"] = serde_json::json!("请始终保留另一条很长的额外操作说明。".repeat(8));
    let result = validate_completeness_with_context(
        &source.to_string(),
        &truncated.to_string(),
        "zh",
        None,
        &CompletenessContext::default(),
    );
    assert!(result
        .checks_failed
        .iter()
        .any(|code| code == "token_ratio"));
}

#[tokio::test]
async fn json_string_truncation_revalidates_whole_cache_and_retries_before_success() {
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
    missing["message"] = serde_json::json!("译");
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
            "truncated JSON value must be regenerated"
        );
        assert_eq!(
            server.requests.lock().unwrap().len(),
            2,
            "reject cached truncation, retry invalid response, then admit complete response"
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
async fn warn_only_json_cannot_hide_one_truncated_string() {
    let source = serde_json::json!({"message":"Read the complete instruction and retain every important detail throughout the entire migration.","other":"Keep this additional complete instruction available."});
    let missing = serde_json::json!({"message":"译","other":"请始终保留另一条额外操作说明，并在整个迁移过程中检查所有相关服务。"});
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
    assert!(format!("{error:#}").contains("unrecoverably incomplete"));
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    assert!(history.fetch_recent(None).unwrap().is_empty());
}
