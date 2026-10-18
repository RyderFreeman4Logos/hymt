use super::*;
use crate::exec_wrapper::translate_cached;
use hymt_cache::ExecCache;

#[tokio::test]
async fn refresh_skips_whole_output_hits_and_replaces_the_user_entry() {
    let source = "Read this complete synthetic instruction.";
    let old = "Previous synthetic translation.";
    let updated = "Updated synthetic translation.";
    let server = start_capturing_mock_server(vec![MockResponse::Json(updated.to_owned())]).await;
    let config = make_stream_config(&server.endpoint_url);
    let client = TranslationClient::new(config.clone()).unwrap();
    let refreshed_client = client.clone().with_app_cache_refresh(true);
    let segmenter = fallback_segmenter();
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(dir.path().join("history.db"));
    let cache = ExecCache::with_user_path(dir.path().join("shared.db"), dir.path().join("user.db"));
    let opts = PromptOpts::default();
    let policy = effective_document_translation_policy(&opts, &config);
    cache
        .store_user("man", "fixture", source, "zh", policy, old)
        .unwrap();
    let refreshed = TranslationCtx {
        config: &config,
        client: &refreshed_client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: true,
    };
    assert_eq!(
        translate_cached("man", "fixture", source, "zh", &cache, &opts, &refreshed)
            .await
            .unwrap(),
        updated
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(
        cache.find("man", "fixture", source, "zh", policy).unwrap(),
        Some(updated.to_owned())
    );

    let normal = TranslationCtx {
        client: &client,
        ..refreshed
    };
    assert_eq!(
        translate_cached("man", "fixture", source, "zh", &cache, &opts, &normal)
            .await
            .unwrap(),
        updated
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn whole_output_cache_never_stores_transport_degraded_results() {
    let complete = "Partial response.";
    // Transport failures may retain the original but cannot populate the cache.
    let source =
        "Keep the complete synthetic migration instructions available throughout the process.";
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
    let retained = translate_cached("man", "test", source, "zh", &cache, &warn, &ctx)
        .await
        .unwrap();
    assert!(retained.contains("Untranslated original"));
    assert!(retained.ends_with(source));
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
async fn whole_output_cache_replays_stored_bytes_without_source_comparison() {
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
        let stored = if valid.contains("ORIGINAL") {
            valid.replace("ORIGINAL", "MODIFIED")
        } else {
            valid.replace("原始操作说明", "错误操作说明")
        };
        for shared in [false, true] {
            let server =
                start_capturing_mock_server(vec![MockResponse::Json(translated.to_owned())]).await;
            let config = leaf_fixture_config(&make_stream_config(&server.endpoint_url), &source);
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
                    .store_shared("man", "owned", &source, "zh", policy, &stored)
                    .unwrap();
            } else {
                cache
                    .store_user("man", "owned", &source, "zh", policy, &stored)
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
            assert_eq!(fresh, stored);
            assert_eq!(server.requests.lock().unwrap().len(), 0);
            assert_eq!(
                translate_cached("man", "owned", &source, "zh", &cache, &opts, &ctx)
                    .await
                    .unwrap(),
                fresh
            );
            assert_eq!(
                server.requests.lock().unwrap().len(),
                0,
                "production warm output must not make a request: {source}"
            );
        }
    }
}

#[tokio::test]
async fn raw_assembly_bytes_survive_production_whole_cache_roundtrip() {
    for (source, leaves, expected) in [
        (
            "Alpha*Title*Beta",
            vec!["甲方", "《标题》", "乙方"],
            "甲方*《标题》*乙方",
        ),
        (
            "*Title.*Journal",
            vec!["标题", "《期刊》"],
            "*标题.*《期刊》",
        ),
    ] {
        for shared in [false, true] {
            let server = start_capturing_mock_server(
                leaves
                    .iter()
                    .cycle()
                    .take(leaves.len() * 2)
                    .map(|leaf| MockResponse::Json((*leaf).to_owned()))
                    .collect(),
            )
            .await;
            let config = leaf_fixture_config(&make_stream_config(&server.endpoint_url), source);
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
            let cold = translate_cached("man", "renderer", source, "zh", &cache, &opts, &ctx)
                .await
                .unwrap();
            assert_eq!(cold, expected);
            assert_eq!(server.requests.lock().unwrap().len(), leaves.len());
            // Use an empty segment history so a false whole-cache miss cannot hide
            // behind segment hits. The shared case also has no user-cache entry.
            let warm_history = HistoryDB::new(dir.path().join("warm-history.db"));
            let shared_cache = ExecCache::with_user_path(
                dir.path().join("shared-only.db"),
                dir.path().join("empty-user.db"),
            );
            if shared {
                shared_cache
                    .store_shared("man", "renderer", source, "zh", policy, &cold)
                    .unwrap();
            }
            let warm_ctx = TranslationCtx {
                history: &warm_history,
                ..ctx
            };
            let warm = translate_cached(
                "man",
                "renderer",
                source,
                "zh",
                if shared { &shared_cache } else { &cache },
                &opts,
                &warm_ctx,
            )
            .await
            .unwrap();
            assert_eq!(warm, cold);
            assert_eq!(
                server.requests.lock().unwrap().len(),
                leaves.len(),
                "warm output must make no provider request"
            );
            assert!(warm_history.fetch_recent(None).unwrap().is_empty());
        }
    }
}
