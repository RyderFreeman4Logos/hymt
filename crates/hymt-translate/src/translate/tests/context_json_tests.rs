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
        assert_contextual_leaf(
            prefix,
            "请仔细审查所有相关代码。",
            "请仔细审查所有相关代码。",
        )
        .await;
    }
}

#[tokio::test]
async fn model_code_is_returned_unchanged_in_original_leaf_context() {
    for prefix in [">5 ", "# ", "- ", "1. ", "~~~ ", "--- ", "5 "] {
        assert_contextual_leaf(
            prefix,
            "请仔细审查所有相关 `代码`。",
            "请仔细审查所有相关 `代码`。",
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

#[tokio::test]
async fn completed_body_changes_and_empty_strings_reuse_all_cache_paths() {
    let source = r#"{"message":"Read {{field}} at https://example.test/a.","count":2}"#;
    for content in [
        "",
        " ",
        "not JSON",
        r#"{"changed":"","count":false}"#,
        "# New markup `code`",
    ] {
        assert_json_translation_and_cache_paths(source, content).await;
    }
}
