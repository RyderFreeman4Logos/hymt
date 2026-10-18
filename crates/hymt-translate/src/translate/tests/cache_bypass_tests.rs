use super::*;

async fn reply(mut socket: TcpStream, response: MockResponse) {
    let (content_type, body) = mock_response_body(response);
    let headers = format!("HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
    socket.write_all(headers.as_bytes()).await.unwrap();
    socket.write_all(body.as_bytes()).await.unwrap();
    socket.shutdown().await.unwrap();
}

async fn run(source: &str, ctx: &TranslationCtx<'_>, stream: bool) -> TranslationOutcome {
    if !stream {
        return translate_text(
            source,
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            ctx,
        )
        .await
        .unwrap();
    }
    let (tx, mut rx) = mpsc::channel(32);
    let outcome = translate_text_stream(
        source,
        "zh",
        &TemplateType::Default,
        &PromptOpts::default(),
        ctx,
        tx,
    )
    .await
    .unwrap();
    let mut terminal = None;
    while let Some(event) = rx.recv().await {
        if let StreamEvent::AllDone(text) = event {
            terminal = Some(text);
        }
    }
    assert_eq!(terminal.as_deref(), Some(outcome.text.as_str()));
    outcome
}

#[tokio::test]
async fn identical_rejected_retries_bypass_cached_bad_output() {
    for route in 0..3 {
        for cache_enabled in [false, true] {
            tokio::time::timeout(Duration::from_secs(10), async {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
                let config = make_stream_config(&endpoint);
                let contents = std::fs::read_to_string(config.path())
                    .unwrap()
                    .replace("max_retries = 1", "max_retries = 2");
                std::fs::write(config.path(), contents).unwrap();
                let config = HotConfig::from_path(config.path()).unwrap();
                let segmenter = fallback_segmenter();
                let source = "This report explains why complete translation matters.";
                let plan = plan_translation(
                    source,
                    "zh",
                    &config,
                    &segmenter,
                    &TemplateType::Default,
                    &PromptOpts::default(),
                )
                .unwrap();
                assert_eq!(plan.segment_count(), 1);
                let good = planned_complete_zh_translations(&plan).remove(0);
                let expected = plan.reconstruct(std::slice::from_ref(&good));
                let requests = Arc::new(Mutex::new(Vec::new()));
                let observed = Arc::clone(&requests);
                let handle = tokio::spawn(async move {
                    let mut cache = HashMap::<String, String>::new();
                    let mut generations = 0;
                    for attempt in 0..3 {
                        let (mut socket, _) = listener.accept().await.unwrap();
                        let request =
                            String::from_utf8(read_http_request(&mut socket).await.unwrap())
                                .unwrap();
                        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
                        let headers = headers.to_ascii_lowercase();
                        let bypass = headers.lines().any(|line| line == "x-hymt-cache-bypass: 1")
                            && headers
                                .lines()
                                .any(|line| line == "x-bf-eh-x-hymt-cache-bypass: 1");
                        let text = if !bypass && cache.contains_key(body) {
                            cache[body].clone()
                        } else {
                            generations += 1;
                            let text = if generations == 3 {
                                good.clone()
                            } else {
                                "**invalid**".into()
                            };
                            if !bypass {
                                cache.insert(body.to_owned(), text.clone());
                            }
                            text
                        };
                        observed
                            .lock()
                            .unwrap()
                            .push((body.to_owned(), bypass, generations));
                        let response = if route == 1 && attempt == 0 {
                            MockResponse::Sse(vec![text])
                        } else {
                            MockResponse::Json(text)
                        };
                        reply(socket, response).await;
                    }
                });
                let _server = MockServer {
                    endpoint_url: endpoint,
                    handle,
                };
                let client = TranslationClient::new(config.clone()).unwrap();
                let temp = tempfile::tempdir().unwrap();
                let history = HistoryDB::new(temp.path().join("history.db"));
                let ctx = TranslationCtx {
                    config: &config,
                    client: &client,
                    segmenter: &segmenter,
                    history: &history,
                    cache_enabled,
                };
                assert_eq!(run(source, &ctx, route != 0).await.text, expected);
                let requests = requests.lock().unwrap();
                assert_eq!(requests.len(), 3);
                assert!(
                    !requests[0].1,
                    "app-cache OFF must not bypass initial inference-cache lookup"
                );
                assert!(requests[1].1 && requests[2].1);
                assert_eq!(
                    requests[1].0, requests[2].0,
                    "identical retry prompts need fresh inference"
                );
                assert_eq!(requests[2].2, 3);
                assert!(history.fetch_recent(Some(1)).unwrap()[0]
                    .cold_source_tokens
                    .is_none());
            })
            .await
            .expect("bounded fake cache retries");
        }
    }
}

#[tokio::test]
async fn clone_local_bypass_survives_snapshot_without_leaking_to_concurrent_clients() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let server = start_capturing_mock_server(vec![
            MockResponse::Json("reply".into()),
            MockResponse::Json("reply".into()),
        ])
        .await;
        let config = make_stream_config_with_concurrency(&server.endpoint_url, 2);
        let original = TranslationClient::new(config.clone()).unwrap();
        let bypass = original
            .with_cache_bypass(true)
            .clone()
            .with_config(config.detached_snapshot())
            .unwrap()
            .with_cache_bypass(false);
        let ordinary = original.with_config(config.detached_snapshot()).unwrap();
        let (bypassed, ordinary) = tokio::join!(
            bypass.translate_with_completion("bypass request"),
            ordinary.translate_with_completion("ordinary request")
        );
        bypassed.unwrap();
        ordinary.unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            let (headers, body) = request.split_once("\r\n\r\n").unwrap();
            let expected = body.contains("bypass request");
            let headers = headers.to_ascii_lowercase();
            for name in ["x-hymt-cache-bypass", "x-bf-eh-x-hymt-cache-bypass"] {
                assert_eq!(
                    headers.lines().any(|line| line == format!("{name}: 1")),
                    expected
                );
            }
        }
    })
    .await
    .expect("bounded concurrent clone policy");
}
