use super::*;
use crate::exec_wrapper::translate_cached;
use hymt_cache::ExecCache;

const SOURCE: &str =
    "This report explains why reliable translation history and cache behavior matter.";

#[tokio::test]
async fn disabled_whole_cache_bypasses_existing_tiers_and_preserves_history() {
    for shared in [false, true] {
        let planning = make_stream_config("http://127.0.0.1:1/v1");
        let segmenter = fallback_segmenter();
        let opts = PromptOpts::default();
        let plan = plan_translation(
            SOURCE,
            "zh",
            &planning,
            &segmenter,
            &TemplateType::Default,
            &opts,
        )
        .unwrap();
        assert_eq!(plan.segment_count(), 1);
        let fresh = planned_complete_zh_translations(&plan)
            .into_iter()
            .next()
            .unwrap();
        let server = start_capturing_mock_server(vec![
            MockResponse::Json(fresh.clone()),
            MockResponse::Json(fresh.clone()),
        ])
        .await;
        let config = make_stream_config(&server.endpoint_url);
        let client = TranslationClient::new(config.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(dir.path().join("history.db"));
        let cache =
            ExecCache::with_user_path(dir.path().join("shared.db"), dir.path().join("user.db"));
        let policy = effective_document_translation_policy(&opts, &config);
        let stale = "old whole-output translation";
        if shared {
            cache
                .store_shared("man", "test", SOURCE, "zh", policy, stale)
                .unwrap();
        } else {
            cache
                .store_user("man", "test", SOURCE, "zh", policy, stale)
                .unwrap();
        }
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: false,
        };
        for _ in 0..2 {
            assert_eq!(
                translate_cached("man", "test", SOURCE, "zh", &cache, &opts, &ctx)
                    .await
                    .unwrap(),
                fresh
            );
        }
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        assert_eq!(history.fetch_recent(None).unwrap().len(), 2);
        assert_eq!(
            cache
                .find("man", "test", SOURCE, "zh", policy)
                .unwrap()
                .as_deref(),
            Some(stale),
            "disabled caching must neither overwrite nor delete old entries"
        );
    }
}

#[tokio::test]
async fn disabled_whole_cache_does_not_create_cache_files() {
    let planning = make_stream_config("http://127.0.0.1:1/v1");
    let segmenter = fallback_segmenter();
    let opts = PromptOpts::default();
    let plan = plan_translation(
        SOURCE,
        "zh",
        &planning,
        &segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    assert_eq!(plan.segment_count(), 1);
    let fresh = planned_complete_zh_translations(&plan)
        .into_iter()
        .next()
        .unwrap();
    let server = start_capturing_mock_server(vec![MockResponse::Json(fresh.clone())]).await;
    let config = make_stream_config(&server.endpoint_url);
    let client = TranslationClient::new(config.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(dir.path().join("history.db"));
    let shared = dir.path().join("shared.db");
    let user = dir.path().join("user.db");
    let cache = ExecCache::with_user_path(&shared, &user);
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: false,
    };
    assert_eq!(
        translate_cached("man", "test", SOURCE, "zh", &cache, &opts, &ctx)
            .await
            .unwrap(),
        fresh
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(history.fetch_recent(None).unwrap().len(), 1);
    assert!(!shared.exists());
    assert!(!user.exists());
}

#[tokio::test]
async fn explicit_library_cache_opt_in_reuses_whole_output() {
    let planning = make_stream_config("http://127.0.0.1:1/v1");
    let segmenter = fallback_segmenter();
    let opts = PromptOpts::default();
    let plan = plan_translation(
        SOURCE,
        "zh",
        &planning,
        &segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    let fresh = planned_complete_zh_translations(&plan)
        .into_iter()
        .next()
        .unwrap();
    let server = start_capturing_mock_server(vec![MockResponse::Json(fresh.clone())]).await;
    let config = make_stream_config(&server.endpoint_url);
    let client = TranslationClient::new(config.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(dir.path().join("history.db"));
    let cache = ExecCache::with_user_path(dir.path().join("shared.db"), dir.path().join("user.db"));
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: true,
    };
    for _ in 0..2 {
        assert_eq!(
            translate_cached("man", "test", SOURCE, "zh", &cache, &opts, &ctx)
                .await
                .unwrap(),
            fresh
        );
    }
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}
