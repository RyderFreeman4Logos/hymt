use super::*;
use std::path::Path;
use std::sync::atomic::{AtomicBool as TestAtomicBool, AtomicUsize as TestAtomicUsize};

const SOURCE: &str =
    "##### Public Guide\n\nRead [the public reference guide](https://example.test/structure).\n";
const LINK_LABEL: &str = "the public reference guide";
const LINK_URL: &str = "https://example.test/structure";
const VALID_LABEL: &str = "这是完整的链接标签翻译，语义保留。";
const INVALID_LABEL: &str = "这是完整的链接标签翻译，语义保留。]";
const OTHER_TRANSLATION: &str = "这是完整的中文翻译文本，内容准确且语义完整。";
const CACHE_TIME: &str = "2026-10-07T00:00:00Z";

struct LinkMockServer {
    endpoint_url: String,
    posts: Arc<TestAtomicUsize>,
    link_label_broken: Arc<TestAtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for LinkMockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_link_mock_server(broken: bool) -> LinkMockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(TestAtomicUsize::new(0));
    let broken = Arc::new(TestAtomicBool::new(broken));
    let server_posts = Arc::clone(&posts);
    let server_broken = Arc::clone(&broken);
    let handle = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let posts = Arc::clone(&server_posts);
            let broken = Arc::clone(&server_broken);
            tokio::spawn(async move {
                let request = read_http_request(&mut socket)
                    .await
                    .expect("read translation request");
                if request.starts_with(b"POST ") {
                    posts.fetch_add(1, Ordering::SeqCst);
                }
                let body_start = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .expect("HTTP body separator")
                    + 4;
                let payload: serde_json::Value =
                    serde_json::from_slice(&request[body_start..]).expect("request JSON");
                let messages = payload["messages"].as_array().expect("messages array");
                let prompt = messages
                    .last()
                    .and_then(|message| message["content"].as_str())
                    .expect("user prompt");
                let source_segment = prompt
                    .rsplit_once("\n\n")
                    .map_or(prompt, |(_, segment)| segment);
                let reply = translation_for(source_segment, broken.load(Ordering::SeqCst));
                let response = if payload["stream"].as_bool() == Some(true) {
                    MockResponse::Sse(vec![reply])
                } else {
                    MockResponse::Json(reply)
                };
                write_mock_response(socket, response)
                    .await
                    .expect("write translation response");
            });
        }
    });
    LinkMockServer {
        endpoint_url: format!("http://{addr}/v1"),
        posts,
        link_label_broken: broken,
        handle,
    }
}

struct CacheIdentity {
    options_hash: String,
    profile_id: String,
    inference_fingerprint: String,
}

impl CacheIdentity {
    fn new(config: &HotConfig, opts: &PromptOpts) -> Self {
        let options_hash =
            template_options_hash(opts, effective_document_translation_policy(opts, config));
        let fingerprint = config
            .inference_fingerprint(TemplateType::Default.as_str(), &options_hash)
            .expect("cache identity");
        assert!(fingerprint.is_cache_verified());
        Self {
            options_hash,
            profile_id: config
                .model_profile()
                .expect("model profile")
                .id()
                .to_owned(),
            inference_fingerprint: fingerprint.hash().to_owned(),
        }
    }

    fn scope(&self) -> SegmentCacheScope<'_> {
        SegmentCacheScope {
            target_lang: "zh",
            template_type: TemplateType::Default.as_str(),
            options_hash: &self.options_hash,
            profile_id: &self.profile_id,
            inference_fingerprint: &self.inference_fingerprint,
        }
    }
}

fn make_test_config(dir: &Path, endpoint_url: &str) -> HotConfig {
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            r#"[endpoint]
url = "{endpoint_url}"
profile = "hy_mt2_7b"
model = "test-model"
backend = "llama_cpp"

[inference.override]
temperature = 0.7
top_p = 0.6
top_k = 20
repetition_penalty = 1.05
min_p = 0.0
repeat_last_n = 64

[translation]
context_window = 512
max_output_tokens = 40
concurrency = 1
first_chunk_priority = true
timeout = 5

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 1
"#
        ),
    )
    .unwrap();
    HotConfig::from_path(path).unwrap()
}

fn make_markdown_plan(
    config: &HotConfig,
    segmenter: &Segmenter,
    opts: &PromptOpts,
) -> TranslationPlan {
    let plan = plan_translation(
        SOURCE,
        "zh",
        config,
        segmenter,
        &TemplateType::Default,
        opts,
    )
    .expect("translation plan");
    let link_segments: Vec<_> = plan
        .segments
        .iter()
        .enumerate()
        .filter(|(_, segment)| segment.contains(LINK_LABEL))
        .collect();
    assert_eq!(link_segments.len(), 1, "link label must have one owner");
    let (index, link_segment) = link_segments[0];
    assert!(!link_segment.contains('[') && !link_segment.contains(']'));
    assert!(!link_segment.contains(LINK_URL));
    assert_eq!(
        markdown_structure(link_segment),
        MarkdownStructure::default()
    );
    assert_eq!(
        markdown_structure(SOURCE).headings,
        vec![pulldown_cmark::HeadingLevel::H5]
    );
    assert_eq!(markdown_structure(SOURCE).links[0].0, LINK_URL);
    assert!(plan.segments[index].contains(LINK_LABEL));
    plan
}

fn translation_for(segment: &str, broken: bool) -> String {
    if segment.contains(LINK_LABEL) {
        if broken {
            INVALID_LABEL.to_owned()
        } else {
            VALID_LABEL.to_owned()
        }
    } else if segment.trim() == "Public Guide" {
        "公共指南".to_owned()
    } else if segment.trim() == "Read" {
        "阅读 ".to_owned()
    } else if segment
        .trim()
        .chars()
        .all(|character| !character.is_alphanumeric())
    {
        segment.trim().to_owned()
    } else {
        OTHER_TRANSLATION.to_owned()
    }
}

fn seed_segment_cache(
    history: &HistoryDB,
    plan: &TranslationPlan,
    config: &HotConfig,
    opts: &PromptOpts,
    broken: bool,
) -> String {
    let identity = CacheIdentity::new(config, opts);
    let mut link_hash = String::new();
    for (index, segment) in plan.segments.iter().enumerate() {
        let translation = translation_for(segment, broken);
        assert!(
            cached_segment_is_complete(index, segment, &translation, "zh", config),
            "seed translation is incomplete: index={index}, source={segment:?}, translation={translation:?}"
        );
        let hash = segment_cache_hash(segment);
        if segment.contains(LINK_LABEL) {
            link_hash = hash.clone();
        }
        history
            .store_segment_cache(&hash, identity.scope(), &translation, CACHE_TIME)
            .unwrap();
    }
    assert!(!link_hash.is_empty());
    link_hash
}

fn seed_unrelated_cache_entry(history: &HistoryDB, identity: &CacheIdentity) -> String {
    let hash = segment_cache_hash("unrelated cache sentinel");
    history
        .store_segment_cache(
            &hash,
            identity.scope(),
            "preserve this cache row",
            CACHE_TIME,
        )
        .unwrap();
    hash
}

fn assert_valid_translation(translated: &str) {
    assert!(translated.starts_with("##### "), "H5 marker must remain");
    assert!(
        translated.contains(&format!("[{VALID_LABEL}]({LINK_URL})")),
        "link label must be translated with source-owned syntax: {translated:?}"
    );
    assert!(!translated.contains(LINK_LABEL));
    assert_eq!(markdown_structure(translated).headings.len(), 1);
    assert_eq!(markdown_structure(translated).links[0].0, LINK_URL);
}

#[tokio::test]
async fn buffered_structure_rejection_does_not_cache_fresh_segments() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = start_link_mock_server(true).await;
    let config = make_test_config(tmp.path(), &provider.endpoint_url);
    let opts = PromptOpts::default();
    let segmenter = Segmenter::fallback();
    let plan = make_markdown_plan(&config, &segmenter, &opts);
    let history = HistoryDB::new(tmp.path().join("history.db"));
    let identity = CacheIdentity::new(&config, &opts);
    let sentinel = seed_unrelated_cache_entry(&history, &identity);
    let client = TranslationClient::new(config.clone()).unwrap();
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: true,
    };

    let error = translate_text(SOURCE, "zh", &TemplateType::Default, &opts, &ctx)
        .await
        .expect_err("malformed reconstructed Markdown must fail");
    assert!(
        error
            .to_string()
            .contains("translated Markdown structure changed"),
        "unexpected translation failure: {error:#}"
    );
    assert!(provider.posts.load(Ordering::SeqCst) > 0);
    for segment in &plan.segments {
        assert_eq!(
            history
                .find_segment_cached(&segment_cache_hash(segment), identity.scope())
                .unwrap(),
            None,
            "a structurally rejected document must not persist candidate segments"
        );
    }
    assert_eq!(
        history
            .find_segment_cached(&sentinel, identity.scope())
            .unwrap(),
        Some("preserve this cache row".to_owned()),
        "rejection must not clear unrelated cache rows"
    );
}

#[tokio::test]
async fn buffered_warm_cache_retranslates_and_replaces_poisoned_segments() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = start_link_mock_server(true).await;
    let config = make_test_config(tmp.path(), &provider.endpoint_url);
    let opts = PromptOpts::default();
    let segmenter = Segmenter::fallback();
    let plan = make_markdown_plan(&config, &segmenter, &opts);
    let history = HistoryDB::new(tmp.path().join("history.db"));
    let identity = CacheIdentity::new(&config, &opts);
    let sentinel = seed_unrelated_cache_entry(&history, &identity);
    let link_hash = seed_segment_cache(&history, &plan, &config, &opts, true);
    let client = TranslationClient::new(config.clone()).unwrap();
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: true,
    };

    let failed = translate_text(SOURCE, "zh", &TemplateType::Default, &opts, &ctx)
        .await
        .expect_err("broken provider must not commit a structurally invalid repair");
    assert!(failed
        .to_string()
        .contains("translated Markdown structure changed"));
    assert!(provider.posts.load(Ordering::SeqCst) > 0);
    assert_eq!(
        history
            .find_segment_cached(&link_hash, identity.scope())
            .unwrap(),
        None,
        "the invalid owned cache row must be evicted for a later warm retry"
    );
    for segment in &plan.segments {
        if !segment.contains(LINK_LABEL) {
            assert_eq!(
                history
                    .find_segment_cached(&segment_cache_hash(segment), identity.scope())
                    .unwrap(),
                Some(translation_for(segment, false)),
                "valid owned cache rows must survive targeted recovery"
            );
        }
    }
    assert_eq!(
        history
            .find_segment_cached(&sentinel, identity.scope())
            .unwrap(),
        Some("preserve this cache row".to_owned())
    );

    provider.link_label_broken.store(false, Ordering::SeqCst);
    let posts_before_recovery = provider.posts.load(Ordering::SeqCst);
    let recovered = translate_text(SOURCE, "zh", &TemplateType::Default, &opts, &ctx)
        .await
        .expect("fixed provider must recover the poisoned warm cache");
    assert_valid_translation(&recovered.text);
    assert!(provider.posts.load(Ordering::SeqCst) > posts_before_recovery);
    assert_eq!(
        history
            .find_segment_cached(&link_hash, identity.scope())
            .unwrap(),
        Some(VALID_LABEL.to_owned())
    );

    let posts_after_recovery = provider.posts.load(Ordering::SeqCst);
    let warm = translate_text(SOURCE, "zh", &TemplateType::Default, &opts, &ctx)
        .await
        .expect("corrected cache must be reusable");
    assert_eq!(warm.text, recovered.text);
    assert_eq!(provider.posts.load(Ordering::SeqCst), posts_after_recovery);
}

#[tokio::test]
async fn streaming_structure_rejection_neither_leaks_tokens_nor_caches_candidates() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = start_link_mock_server(true).await;
    let config = make_test_config(tmp.path(), &provider.endpoint_url);
    let opts = PromptOpts::default();
    let segmenter = Segmenter::fallback();
    let plan = make_markdown_plan(&config, &segmenter, &opts);
    let history = HistoryDB::new(tmp.path().join("history.db"));
    let identity = CacheIdentity::new(&config, &opts);
    let sentinel = seed_unrelated_cache_entry(&history, &identity);
    let client = TranslationClient::new(config.clone()).unwrap();
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: true,
    };
    let (event_tx, mut event_rx) = mpsc::channel(64);

    let error = translate_text_stream_with_mode(
        SOURCE,
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
        StreamOutputMode::Validated,
        event_tx,
    )
    .await
    .expect_err("malformed reconstructed Markdown must fail");
    let mut events = Vec::new();
    while let Some(event) = event_rx.recv().await {
        events.push(event);
    }
    assert!(
        error
            .to_string()
            .contains("translated Markdown structure changed"),
        "unexpected translation failure: {error:#}"
    );
    assert!(
        events.is_empty(),
        "unvalidated Markdown tokens must not leak: {events:?}"
    );
    assert!(provider.posts.load(Ordering::SeqCst) > 0);
    for segment in &plan.segments {
        assert_eq!(
            history
                .find_segment_cached(&segment_cache_hash(segment), identity.scope())
                .unwrap(),
            None,
            "a structurally rejected stream must not persist candidate segments"
        );
    }
    assert_eq!(
        history
            .find_segment_cached(&sentinel, identity.scope())
            .unwrap(),
        Some("preserve this cache row".to_owned())
    );
}

#[tokio::test]
async fn streaming_warm_cache_retranslates_poisoned_segments_before_emission() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = start_link_mock_server(true).await;
    let config = make_test_config(tmp.path(), &provider.endpoint_url);
    let opts = PromptOpts::default();
    let segmenter = Segmenter::fallback();
    let plan = make_markdown_plan(&config, &segmenter, &opts);
    let history = HistoryDB::new(tmp.path().join("history.db"));
    let identity = CacheIdentity::new(&config, &opts);
    let sentinel = seed_unrelated_cache_entry(&history, &identity);
    let link_hash = seed_segment_cache(&history, &plan, &config, &opts, true);
    let client = TranslationClient::new(config.clone()).unwrap();
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: true,
    };
    let (event_tx, mut event_rx) = mpsc::channel(64);

    let failed = translate_text_stream_with_mode(
        SOURCE,
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
        StreamOutputMode::Validated,
        event_tx,
    )
    .await
    .expect_err("broken provider must not commit a structurally invalid repair");
    let failed_events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    assert!(failed
        .to_string()
        .contains("translated Markdown structure changed"));
    assert!(
        failed_events.is_empty(),
        "unvalidated stream events leaked: {failed_events:?}"
    );
    assert!(provider.posts.load(Ordering::SeqCst) > 0);
    assert_eq!(
        history
            .find_segment_cached(&link_hash, identity.scope())
            .unwrap(),
        None,
        "the invalid owned cache row must be evicted for a later warm retry"
    );
    for segment in &plan.segments {
        if !segment.contains(LINK_LABEL) {
            assert_eq!(
                history
                    .find_segment_cached(&segment_cache_hash(segment), identity.scope())
                    .unwrap(),
                Some(translation_for(segment, false)),
                "valid owned cache rows must survive targeted recovery"
            );
        }
    }
    assert_eq!(
        history
            .find_segment_cached(&sentinel, identity.scope())
            .unwrap(),
        Some("preserve this cache row".to_owned())
    );

    provider.link_label_broken.store(false, Ordering::SeqCst);
    let posts_before_recovery = provider.posts.load(Ordering::SeqCst);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let recovered = translate_text_stream_with_mode(
        SOURCE,
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
        StreamOutputMode::Validated,
        event_tx,
    )
    .await
    .expect("fixed provider must recover poisoned warm-cache entries");
    assert_valid_translation(&recovered.text);
    assert!(provider.posts.load(Ordering::SeqCst) > posts_before_recovery);
    let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    let streamed: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Token(token) => Some(token.as_str()),
            StreamEvent::SegmentDone(_) | StreamEvent::AllDone(_) => None,
        })
        .collect();
    assert_eq!(streamed, recovered.text);
    assert_eq!(
        events.last(),
        Some(&StreamEvent::AllDone(recovered.text.clone()))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::SegmentDone(_)))
            .count(),
        plan.segment_count()
    );
    assert_eq!(
        history
            .find_segment_cached(&link_hash, identity.scope())
            .unwrap(),
        Some(VALID_LABEL.to_owned())
    );

    let posts_after_recovery = provider.posts.load(Ordering::SeqCst);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let warm = translate_text_stream_with_mode(
        SOURCE,
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
        StreamOutputMode::Validated,
        event_tx,
    )
    .await
    .expect("validated warm stream must succeed");
    assert_eq!(warm.text, recovered.text);
    assert_eq!(provider.posts.load(Ordering::SeqCst), posts_after_recovery);
    let warm_events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    assert_eq!(
        warm_events.last(),
        Some(&StreamEvent::AllDone(warm.text.clone()))
    );
    assert!(event_rx.recv().await.is_none());
}
