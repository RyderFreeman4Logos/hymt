use super::*;
use crate::exec_wrapper::translate_cached;
use hymt_cache::ExecCache;

#[tokio::test]
async fn whole_output_cache_revalidates_hits_and_never_stores_degraded_results() {
    let source =
        r#"{"customer": "Keep the account available throughout the entire migration process."}"#;
    let complete = r#"{"customer": "整个迁移过程中必须保持账户可用，并持续检查所有相关服务。"}"#;
    let missing = r#"{"wrong": "整个迁移过程中必须保持账户可用，并持续检查所有相关服务。"}"#;
    for shared in [false, true] {
        let server = start_capturing_mock_server(vec![
            MockResponse::Json(
                serde_json::to_string(&missing)
                    .unwrap()
                    .trim_matches('"')
                    .to_owned(),
            ),
            MockResponse::Json(
                serde_json::to_string(&missing)
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
        let cache =
            ExecCache::with_user_path(dir.path().join("shared.db"), dir.path().join("user.db"));
        let opts = PromptOpts::default();
        let policy = effective_document_translation_policy(&opts, &config);
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: true,
        };
        if shared {
            cache
                .store_shared("man", "test", source, "zh", policy, missing)
                .unwrap();
        } else {
            cache
                .store_user("man", "test", source, "zh", policy, missing)
                .unwrap();
        }
        let result = translate_cached("man", "test", source, "zh", &cache, &opts, &ctx).await;
        assert!(
            result.is_err(),
            "a poisoned whole-output cache must not bypass strict validation"
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        assert!(history.fetch_recent(None).unwrap().is_empty());
        cache
            .store_user("man", "test", source, "zh", policy, complete)
            .unwrap();
        assert_eq!(
            translate_cached("man", "test", source, "zh", &cache, &opts, &ctx)
                .await
                .unwrap(),
            complete
        );
        assert_eq!(
            server.requests.lock().unwrap().len(),
            2,
            "valid warm cache must make no request"
        );
    }

    let server = start_capturing_mock_server(
        (0..4)
            .map(|_| MockResponse::JsonWithFinishReason {
                content: serde_json::to_string(&complete)
                    .unwrap()
                    .trim_matches('"')
                    .to_owned(),
                finish_reason: "length".to_owned(),
            })
            .collect(),
    )
    .await;
    let config = make_stream_config(&server.endpoint_url);
    let client = TranslationClient::new(config.clone()).unwrap();
    let segmenter = fallback_segmenter();
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(dir.path().join("history.db"));
    let cache = ExecCache::with_user_path(dir.path().join("shared.db"), dir.path().join("user.db"));
    let strict = PromptOpts::default();
    let policy = effective_document_translation_policy(&strict, &config);
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: true,
    };
    let warn = PromptOpts {
        warn_only_completeness: Some(true),
        ..PromptOpts::default()
    };
    assert_eq!(
        translate_cached("man", "test", source, "zh", &cache, &warn, &ctx)
            .await
            .unwrap(),
        complete
    );
    assert!(
        cache
            .find("man", "test", source, "zh", policy)
            .unwrap()
            .is_none(),
        "degraded output must not become a future strict cache hit"
    );
    assert!(
        translate_cached("man", "test", source, "zh", &cache, &strict, &ctx)
            .await
            .is_err()
    );
    assert_eq!(server.requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn whole_output_cache_rejects_missing_urls_and_changed_markdown() {
    for (source, poisoned, expected) in [
        (
            "Read https://example.com/important",
            "Consultez les instructions.",
            "Consultez https://example.com/important",
        ),
        ("# Read", "Consultez", "# Consultez"),
    ] {
        let server =
            start_capturing_mock_server(vec![MockResponse::Json("Consultez".to_owned())]).await;
        let config = make_stream_config(&server.endpoint_url);
        let client = TranslationClient::new(config.clone()).unwrap();
        let segmenter = fallback_segmenter();
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(dir.path().join("history.db"));
        let cache =
            ExecCache::with_user_path(dir.path().join("shared.db"), dir.path().join("user.db"));
        let opts = PromptOpts::default();
        let policy = effective_document_translation_policy(&opts, &config);
        cache
            .store_user("man", "test", source, "fr", policy, poisoned)
            .unwrap();
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: true,
        };
        assert_eq!(
            translate_cached("man", "test", source, "fr", &cache, &opts, &ctx)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn whole_output_cache_preserves_owned_payloads_and_production_warm_output() {
    let instruction = "Read this complete instruction.";
    let translated = "请完整阅读这一条操作说明。";
    for source in [
        format!("{instruction}\n\n```sh\necho ORIGINAL\n```\n"),
        format!("{instruction} `ORIGINAL`\n"),
        format!("{instruction}\n\n    echo ORIGINAL\n"),
        format!("{instruction}\n\n<div>ORIGINAL</div>\n"),
        format!("---\nname: ORIGINAL\n---\n{instruction}\n"),
        format!("{instruction}\n\n这段中文已经完整表达了原始操作说明，必须保持不变。\n"),
    ] {
        let valid = source.replace(instruction, translated);
        let poisoned = if valid.contains("ORIGINAL") {
            valid.replace("ORIGINAL", "MODIFIED")
        } else {
            valid.replace("原始操作说明", "错误操作说明")
        };
        for shared in [false, true] {
            let server =
                start_capturing_mock_server(vec![MockResponse::Json(translated.to_owned())]).await;
            let config = make_stream_config(&server.endpoint_url);
            let client = TranslationClient::new(config.clone()).unwrap();
            let segmenter = fallback_segmenter();
            let dir = tempfile::tempdir().unwrap();
            let history = HistoryDB::new(dir.path().join("history.db"));
            let cache =
                ExecCache::with_user_path(dir.path().join("shared.db"), dir.path().join("user.db"));
            let opts = PromptOpts::default();
            let policy = effective_document_translation_policy(&opts, &config);
            if shared {
                cache
                    .store_shared("man", "owned", &source, "zh", policy, &poisoned)
                    .unwrap();
            } else {
                cache
                    .store_user("man", "owned", &source, "zh", policy, &poisoned)
                    .unwrap();
            }
            let ctx = TranslationCtx {
                config: &config,
                client: &client,
                segmenter: &segmenter,
                history: &history,
                cache_enabled: true,
            };
            let fresh = translate_cached("man", "owned", &source, "zh", &cache, &opts, &ctx)
                .await
                .unwrap();
            assert_eq!(fresh, valid, "changed protected payload accepted: {source}");
            assert_eq!(server.requests.lock().unwrap().len(), 1);
            assert_eq!(
                translate_cached("man", "owned", &source, "zh", &cache, &opts, &ctx)
                    .await
                    .unwrap(),
                fresh
            );
            assert_eq!(
                server.requests.lock().unwrap().len(),
                1,
                "production warm output must not make a request: {source}"
            );
        }
    }
}

#[test]
fn whole_output_cache_uses_source_policy_and_anchored_payloads() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let opts = PromptOpts::default();
    let accepts = |source: &str, cached: &str, opts: &PromptOpts| {
        cached_document_is_complete(source, cached, "zh", &config, opts, &segmenter)
    };
    let source =
        "Read this complete instruction.\n\n这段中文已经完整表达了原始操作说明，必须保持不变。\n";
    let valid = source.replace(
        "Read this complete instruction.",
        "请完整阅读这一条操作说明。",
    );
    assert!(accepts(source, &valid, &opts));
    let changed = valid.replace("原始操作说明", "错误操作说明");
    assert!(!accepts(source, &changed, &opts));
    let all = PromptOpts {
        document_translation_policy: Some(DocumentTranslationPolicy::TranslateAll),
        ..PromptOpts::default()
    };
    assert!(accepts(source, &changed, &all));
    for (source, valid, changed) in [
        (
            "Read <i>ORIGINAL</i> carefully.",
            "请阅读 <i>ORIGINAL</i> 并仔细检查。",
            "请阅读 <b>ORIGINAL</b> 并仔细检查。",
        ),
        (
            "Read this complete instruction.\n\n```sh\necho ORIGINAL\n```\n",
            "请完整阅读这一条操作说明。\n\n```sh\necho ORIGINAL\n```\n",
            "echo ORIGINAL 请完整阅读这一条操作说明。\n\n```sh\necho MODIFIED\n```\n",
        ),
        (
            "Read [guide].\n\n[guide]: https://example.com\n",
            "请阅读 [完整指南][guide].\n\n[guide]: https://example.com\n",
            "请阅读 [完整指南][guide].\n\n[guide]: https://example.net\n",
        ),
    ] {
        assert!(
            accepts(source, valid, &opts),
            "valid owned bytes rejected: {source}"
        );
        assert!(
            !accepts(source, changed, &opts),
            "changed owned bytes accepted: {source}"
        );
    }
}
