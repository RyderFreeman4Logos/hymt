use super::*;

#[tokio::test]
async fn cached_prefix_keeps_later_sse_tokens_ordered_before_completion() {
    let source = "---\ntitle: fixture\n---\n\nFirst source paragraph.\n\n```text\nopaque input\n```\n\nSecond source paragraph.\n\nThird source paragraph.\n";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let config = make_stream_config_with_concurrency(&endpoint, 2);
    config.set_backend_runtime_info(
        hymt_core::runtime::BackendRuntimeInfo::from_llama_cpp_props(
            &serde_json::json!({"model":"test-model","n_ctx_per_seq":512}),
            123,
        )
        .unwrap(),
    );
    let segmenter = fallback_segmenter();
    let opts = PromptOpts::default();
    let plan = plan_translation(
        source,
        "zh",
        &config,
        &segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    assert_eq!(plan.segments.len(), 3);
    let replies = vec![
        "CACHED_FIRST".to_owned(),
        "SECOND_END".into(),
        format!("{}_END", "Z".repeat(128)),
    ];
    let expected = plan.reconstruct(&replies);
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(dir.path().join("history.db"));
    let options =
        template_options_hash(&opts, effective_document_translation_policy(&opts, &config));
    let fingerprint = config
        .inference_fingerprint(TemplateType::Default.as_str(), &options)
        .unwrap();
    assert!(
        fingerprint.is_cache_verified(),
        "fixture must exercise an actual app cache hit"
    );
    history
        .store_segment_cache(
            &segment_cache_hash(&plan.segments[0]),
            SegmentCacheScope {
                target_lang: "zh",
                template_type: TemplateType::Default.as_str(),
                options_hash: &options,
                profile_id: "hy_mt2_7b",
                inference_fingerprint: fingerprint.hash(),
            },
            &replies[0],
            &Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        )
        .unwrap();
    let release_second = Arc::new(Notify::new());
    let release_terminal = Arc::new(Notify::new());
    let third_done = Arc::new(Notify::new());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = tokio::spawn({
        let release_second = Arc::clone(&release_second);
        let release_terminal = Arc::clone(&release_terminal);
        let third_done = Arc::clone(&third_done);
        let requests = Arc::clone(&requests);
        async move {
            let mut handlers = JoinSet::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut socket).await.unwrap();
                let raw = std::str::from_utf8(&request).unwrap();
                let payload: serde_json::Value =
                    serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
                let second = payload["messages"][0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("Second source paragraph.");
                requests.lock().unwrap().push(payload);
                let release_second = Arc::clone(&release_second);
                let release_terminal = Arc::clone(&release_terminal);
                let third_done = Arc::clone(&third_done);
                handlers.spawn(async move {
                    let prefix = if second {sse_token_body("SECOND")} else {sse_token_body("Z").repeat(128)};
                    let suffix = sse_token_body("_END") + "data: [DONE]\n\n";
                    if second { release_second.notified().await; }
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{prefix}", prefix.len()+suffix.len()).as_bytes()).await.unwrap();
                    socket.flush().await.unwrap();
                    if second { release_terminal.notified().await; }
                    socket.write_all(suffix.as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
                    if !second { third_done.notify_one(); }
                });
            }
            while let Some(result) = handlers.join_next().await {
                result.unwrap();
            }
        }
    });
    let client = TranslationClient::new(config.clone()).unwrap();
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: true,
    };
    let (tx, mut rx) = mpsc::channel(8);
    let translate = translate_text_stream_with_mode(
        source,
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
        StreamOutputMode::Optimistic,
        tx,
    );
    let render = async {
        let mut printed = String::new();
        while !printed.contains("CACHED_FIRST") {
            if let StreamEvent::Token(text) =
                tokio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            {
                printed.push_str(&text);
            }
        }
        // A future chunk produces more events than the shared channel capacity.
        // It must finish without blocking the earlier frontier or leaking stdout.
        tokio::time::timeout(Duration::from_secs(5), third_done.notified())
            .await
            .unwrap();
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::Token(text) = event {
                printed.push_str(&text);
            }
        }
        assert!(
            !printed.contains('Z'),
            "future chunk crossed the source-order frontier"
        );
        release_second.notify_one();
        while !printed.contains("SECOND") {
            if let StreamEvent::Token(text) =
                tokio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            {
                printed.push_str(&text);
            }
        }
        assert!(
            !printed.contains("SECOND_END"),
            "second chunk was buffered until completion"
        );
        assert!(!printed.contains('Z'));
        release_terminal.notify_one();
        while let Some(event) = rx.recv().await {
            if let StreamEvent::Token(text) = event {
                printed.push_str(&text);
            }
        }
        printed
    };
    let (outcome, printed) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(translate, render)
    })
    .await
    .unwrap();
    provider.await.unwrap();
    assert_eq!(outcome.unwrap().text, expected);
    assert_eq!(printed, expected);
    assert_eq!(
        history.fetch_recent(Some(1)).unwrap()[0].cache_hits,
        Some(1)
    );
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "the cached first segment must not reach inference"
    );
    for payload in requests.iter() {
        assert_eq!(payload["stream"], true);
    }
}

#[tokio::test]
async fn buffered_text_file_and_streaming_share_exact_inference_requests() {
    let source = "First complete source paragraph.\n\n```text\nopaque bytes\n```\n\nSecond complete source paragraph.\n";
    let server = start_capturing_mock_server(
        (0..6)
            .map(|_| MockResponse::Sse(vec!["REPLY".into()]))
            .collect(),
    )
    .await;
    let config = make_stream_config(&server.endpoint_url);
    let segmenter = fallback_segmenter();
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
    let opts = PromptOpts::default();
    // Telegram document dispatch calls this same translate_text entry point.
    let buffered = translate_text(source, "zh", &TemplateType::Default, &opts, &ctx)
        .await
        .unwrap();
    let input = dir.path().join("input.md");
    let output = dir.path().join("output.md");
    std::fs::write(&input, source).unwrap();
    let file = translate_file(
        &input,
        Some(&output),
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(file.text, buffered.text);
    assert_eq!(std::fs::read_to_string(output).unwrap(), buffered.text);
    let (tx, mut rx) = mpsc::channel(32);
    let streamed = translate_text_stream_with_mode(
        source,
        "zh",
        &TemplateType::Default,
        &opts,
        &ctx,
        StreamOutputMode::Optimistic,
        tx,
    )
    .await
    .unwrap();
    let mut printed = String::new();
    while let Some(event) = rx.recv().await {
        if let StreamEvent::Token(text) = event {
            printed.push_str(&text);
        }
    }
    assert_eq!(printed, buffered.text);
    assert_eq!(streamed.text, buffered.text);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    let mut runs = Vec::new();
    for requests in requests.as_chunks::<2>().0 {
        let mut bodies: Vec<_> = requests
            .iter()
            .map(|request| {
                let body = request.split_once("\r\n\r\n").unwrap().1;
                let payload: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(payload["stream"], true);
                body.to_owned()
            })
            .collect();
        bodies.sort();
        runs.push(bodies);
    }
    assert_eq!(runs[0], runs[1]);
    assert_eq!(runs[1], runs[2]);
    let bodies = runs[0].join("\n");
    for segment in [
        "First complete source paragraph.",
        "Second complete source paragraph.",
    ] {
        assert!(bodies.contains(segment));
    }
    assert!(printed.contains("```text\nopaque bytes\n```"));
}
