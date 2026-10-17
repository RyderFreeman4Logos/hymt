use super::*;

async fn marked_server(replies: Vec<(&'static str, MockResponse)>) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        for (marker, response) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_http_request(&mut socket).await.unwrap();
            let (content_type, body) = mock_response_body(response);
            let headers = format!("HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n{marker}connection: close\r\n\r\n", body.len());
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    MockServer {
        endpoint_url: format!("http://{addr}/v1"),
        handle,
    }
}

async fn run_route(source: &str, ctx: &TranslationCtx<'_>, stream: bool) {
    if stream {
        let (tx, mut rx) = mpsc::channel(256);
        let output = translate_text_stream(
            source,
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            ctx,
            tx,
        )
        .await
        .unwrap();
        let mut completed = false;
        while let Some(event) = rx.recv().await {
            if let StreamEvent::AllDone(text) = event {
                assert_eq!(text, output.text);
                completed = true;
            }
        }
        assert!(completed);
    } else {
        translate_text(
            source,
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            ctx,
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn forecast_is_frozen_before_first_request_for_json_sse_and_stream_fallback() {
    for route in 0..3 {
        let tmp = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(tmp.path().join("history.db"));
        let segmenter = fallback_segmenter();
        let source = "This report explains why reliable translation timing matters.";
        let planning = make_stream_config("http://127.0.0.1:1/v1");
        let plan = plan_translation(
            source,
            "zh",
            &planning,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert_eq!(plan.segment_count(), 1);
        let translated = planned_complete_zh_translations(&plan).remove(0);
        let server = marked_server(
            (0..4)
                .map(|_| {
                    (
                        "X-Hymt-Cache: MISS\r\n",
                        if route == 1 {
                            MockResponse::Sse(vec![translated.clone()])
                        } else {
                            MockResponse::Json(translated.clone())
                        },
                    )
                })
                .collect(),
        )
        .await;
        let config = make_stream_config(&server.endpoint_url);
        let client = TranslationClient::new(config.clone()).unwrap();
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: false,
        };
        for run in 0..4 {
            run_route(source, &ctx, route != 0).await;
            let record = history.fetch_recent(Some(1)).unwrap().remove(0);
            assert_eq!(record.cache_hits, Some(0));
            assert_eq!(
                record.cold_source_tokens,
                Some(
                    plan.segments
                        .iter()
                        .map(|s| segmenter.count_tokens(s) as i64)
                        .sum()
                )
            );
            assert_eq!(
                record.estimated_seconds.is_some(),
                run == 3,
                "route={route}, run={run}"
            );
            if let Some(estimate) = record.estimated_seconds {
                assert!(estimate.is_finite() && estimate > 0.0);
            }
        }
    }
}

#[tokio::test]
async fn shared_hits_unknown_and_mixed_priority_parallel_segments_do_not_calibrate() {
    let source = streaming_regression_source();
    let segmenter = fallback_segmenter();
    let planning = make_stream_config("http://127.0.0.1:1/v1");
    let plan = plan_translation(
        &source,
        "zh",
        &planning,
        &segmenter,
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    assert!(plan.segment_count() > 1);
    for stream in [false, true] {
        for bad_index in [0, plan.segment_count() - 1] {
            for marker in [
                "X-Hymt-Cache: HIT\r\n",
                "",
                "X-Hymt-Cache: UNKNOWN\r\n",
                "X-Hymt-Cache: MISS\r\nX-Hymt-Cache: MISS\r\n",
            ] {
                let tmp = tempfile::tempdir().unwrap();
                let history = HistoryDB::new(tmp.path().join("history.db"));
                let replies = planned_complete_zh_translations(&plan)
                    .into_iter()
                    .enumerate()
                    .map(|(i, text)| {
                        (
                            if i == bad_index {
                                marker
                            } else {
                                "X-Hymt-Cache: MISS\r\n"
                            },
                            MockResponse::Json(text),
                        )
                    })
                    .collect();
                let server = marked_server(replies).await;
                let config = make_stream_config_with_concurrency(&server.endpoint_url, 3);
                let client = TranslationClient::new(config.clone()).unwrap();
                let ctx = TranslationCtx {
                    config: &config,
                    client: &client,
                    segmenter: &segmenter,
                    history: &history,
                    cache_enabled: false,
                };
                run_route(&source, &ctx, stream).await;
                let record = history.fetch_recent(Some(1)).unwrap().remove(0);
                assert_eq!(record.cache_hits, Some(0));
                assert!(
                    record.cold_source_tokens.is_none(),
                    "stream={stream}, bad_index={bad_index}, marker={marker:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn completeness_retry_is_not_single_pass_cold_work() {
    for stream in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(tmp.path().join("history.db"));
        let segmenter = fallback_segmenter();
        let source = "This report explains why reliable translation timing matters.";
        let planning = make_stream_config("http://127.0.0.1:1/v1");
        let plan = plan_translation(
            source,
            "zh",
            &planning,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        let translated = planned_complete_zh_translations(&plan).remove(0);
        let server = marked_server(vec![
            (
                "X-Hymt-Cache: MISS\r\n",
                MockResponse::JsonWithFinishReason {
                    content: translated.clone(),
                    finish_reason: "length".to_owned(),
                },
            ),
            ("X-Hymt-Cache: BYPASS\r\n", MockResponse::Json(translated)),
        ])
        .await;
        let config = make_stream_config(&server.endpoint_url);
        let client = TranslationClient::new(config.clone()).unwrap();
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: false,
        };
        run_route(source, &ctx, stream).await;
        assert!(history.fetch_recent(Some(1)).unwrap()[0]
            .cold_source_tokens
            .is_none());
    }
}
