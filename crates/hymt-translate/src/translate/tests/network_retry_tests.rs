use super::*;

#[tokio::test]
async fn interrupted_validated_stream_replays_only_complete_chunk() {
    let server = start_capturing_mock_server(vec![
        MockResponse::InterruptedSse(sse_token_body("discard interrupted draft")),
        MockResponse::Sse(vec!["complete chunk".into()]),
    ])
    .await;
    let config = make_stream_config(&server.endpoint_url);
    // Exercise the CLI's ordinary-text Optimistic selection with the default
    // retention policy, which must buffer unpublished attempts before replay.
    let contents = std::fs::read_to_string(config.path())
        .unwrap()
        .replace("warn_only = false\n", "");
    std::fs::write(config.path(), contents).unwrap();
    let config = HotConfig::from_path(config.path()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(dir.path().join("history.db"));
    let (outcome, printed) = translate_and_render_stdout_with_mode(
        "This source chunk must be sent completely on every network attempt.",
        &config,
        &fallback_segmenter(),
        &history,
        StreamOutputMode::Optimistic,
    )
    .await
    .unwrap();
    assert_eq!(outcome.trim(), "complete chunk");
    assert_eq!(printed, "complete chunk\n");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    let saved = history.fetch_recent(Some(2)).unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0].output_text.as_deref().unwrap().trim(),
        "complete chunk"
    );
}

#[tokio::test]
async fn exhausted_network_budget_does_not_publish_or_save_a_partial_chunk() {
    let server = start_capturing_mock_server(vec![
        MockResponse::InterruptedSse(sse_token_body("first unpublished draft")),
        MockResponse::InterruptedSse(sse_token_body("second unpublished draft")),
    ])
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
    let (tx, mut rx) = mpsc::channel(64);
    let result = translate_text_stream_with_mode(
        "This source chunk must not disappear after a network interruption.",
        "zh",
        &TemplateType::Default,
        &PromptOpts::default(),
        &ctx,
        StreamOutputMode::Validated,
        tx,
    )
    .await;
    assert!(format!("{:#}", result.unwrap_err()).contains("segment 1"));
    assert!(
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .is_none(),
        "partial output or AllDone escaped"
    );
    assert!(history.fetch_recent(Some(10)).unwrap().is_empty());
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}
