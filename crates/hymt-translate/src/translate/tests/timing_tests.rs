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

async fn retry_then_marked_server(
    marker: &'static str,
    response: MockResponse,
) -> (MockServer, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let handle = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        read_http_request(&mut first).await.unwrap();
        observed.fetch_add(1, Ordering::SeqCst);
        first
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        first.shutdown().await.unwrap();

        let (mut second, _) = listener.accept().await.unwrap();
        read_http_request(&mut second).await.unwrap();
        observed.fetch_add(1, Ordering::SeqCst);
        let (content_type, body) = mock_response_body(response);
        let headers = format!("HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n{marker}connection: close\r\n\r\n", body.len());
        second.write_all(headers.as_bytes()).await.unwrap();
        second.write_all(body.as_bytes()).await.unwrap();
        second.shutdown().await.unwrap();
    });
    (
        MockServer {
            endpoint_url: format!("http://{addr}/v1"),
            handle,
        },
        requests,
    )
}

async fn props_then_completion_server(
    model: &'static str,
    translation: String,
    gate: Option<(Arc<Notify>, Arc<Notify>)>,
) -> (MockServer, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let handle = tokio::spawn(async move {
        let (mut props_socket, _) = listener.accept().await.unwrap();
        read_http_headers(&mut props_socket).await.unwrap();
        observed.fetch_add(1, Ordering::SeqCst);
        if let Some((entered, release)) = gate {
            entered.notify_one();
            release.notified().await;
        }
        let props = format!(r#"{{"model_alias":"{model}","n_ctx":512,"n_ctx_per_seq":512}}"#);
        let headers = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", props.len());
        props_socket.write_all(headers.as_bytes()).await.unwrap();
        props_socket.write_all(props.as_bytes()).await.unwrap();
        props_socket.shutdown().await.unwrap();

        let (mut completion_socket, _) = listener.accept().await.unwrap();
        let completion_request = read_http_request(&mut completion_socket).await.unwrap();
        assert!(completion_request.starts_with(b"POST /v1/chat/completions "));
        observed.fetch_add(1, Ordering::SeqCst);
        let (content_type, body) = mock_response_body(MockResponse::Json(translation));
        let headers = format!("HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
        completion_socket
            .write_all(headers.as_bytes())
            .await
            .unwrap();
        completion_socket.write_all(body.as_bytes()).await.unwrap();
        completion_socket.shutdown().await.unwrap();
    });
    (
        MockServer {
            endpoint_url: format!("http://{addr}/v1"),
            handle,
        },
        requests,
    )
}

#[tokio::test]
async fn transport_retry_cannot_publish_cold_timing_for_json_sse_or_fallback() {
    let source = "This report explains why reliable translation timing matters.";
    let segmenter = fallback_segmenter();
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
    for route in 0..3 {
        for marker in ["X-Hymt-Cache: MISS\r\n", "X-Hymt-Cache: BYPASS\r\n"] {
            let response = if route == 1 {
                MockResponse::Sse(vec![translated.clone()])
            } else {
                MockResponse::Json(translated.clone())
            };
            let (server, requests) = retry_then_marked_server(marker, response).await;
            let config = make_stream_config(&server.endpoint_url);
            let client = TranslationClient::new(config.clone()).unwrap();
            let temp = tempfile::tempdir().unwrap();
            let history = HistoryDB::new(temp.path().join("history.db"));
            let ctx = TranslationCtx {
                config: &config,
                client: &client,
                segmenter: &segmenter,
                history: &history,
                cache_enabled: false,
            };
            run_route(source, &ctx, route != 0).await;
            assert_eq!(requests.load(Ordering::SeqCst), 2);
            let record = history.fetch_recent(Some(1)).unwrap().remove(0);
            assert_eq!(record.cache_hits, Some(0));
            assert_eq!(
                record.cold_source_tokens, None,
                "route={route}, marker={marker}"
            );
        }
    }
}

#[tokio::test]
async fn config_reload_between_plan_and_dispatch_keeps_one_invocation_identity() {
    let source = "This report explains why reliable translation timing matters.";
    let segmenter = fallback_segmenter();
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

    for route in 0..3 {
        let first_response = if route == 1 {
            MockResponse::Sse(vec![translated.clone()])
        } else {
            MockResponse::Json(translated.clone())
        };
        let second_response = if route == 1 {
            MockResponse::Sse(vec![translated.clone()])
        } else {
            MockResponse::Json(translated.clone())
        };
        let first = start_capturing_mock_server(vec![first_response]).await;
        let second = start_capturing_mock_server(vec![second_response]).await;
        let config = make_stream_config(&first.endpoint_url);
        let client = TranslationClient::new(config.clone()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(temp.path().join("history.db"));
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: true,
        };
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let translation = DISPATCH_BARRIER.scope(
            (Arc::clone(&entered), Arc::clone(&release)),
            run_route(source, &ctx, route != 0),
        );
        let update = async {
            entered.notified().await;
            let updated = std::fs::read_to_string(config.path())
                .unwrap()
                .replace(&first.endpoint_url, &second.endpoint_url)
                .replace("model = \"test-model\"", "model = \"next-model\"");
            std::fs::write(config.path(), updated).unwrap();
            std::fs::File::open(config.path())
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new()
                        .set_modified(std::time::SystemTime::now() + Duration::from_secs(2)),
                )
                .unwrap();
            assert!(config.maybe_reload().unwrap());
            release.notify_one();
        };
        tokio::join!(translation, update);

        {
            let first_requests = first.requests.lock().unwrap();
            let second_requests = second.requests.lock().unwrap();
            assert_eq!(first_requests.len(), 1, "route={route}");
            assert_eq!(second_requests.len(), 0, "route={route}");
            assert!(first_requests[0].contains("\"model\":\"test-model\""));
        }
        let first_record = history.fetch_recent(Some(1)).unwrap().remove(0);
        assert_eq!(first_record.model.as_deref(), Some("test-model"));

        run_route(source, &ctx, route != 0).await;
        let second_requests = second.requests.lock().unwrap();
        assert_eq!(second_requests.len(), 1, "next invocation route={route}");
        assert!(second_requests[0].contains("\"model\":\"next-model\""));
        let next_record = history.fetch_recent(Some(1)).unwrap().remove(0);
        assert_eq!(next_record.model.as_deref(), Some("next-model"));
        assert_ne!(
            first_record.inference_fingerprint,
            next_record.inference_fingerprint
        );
    }
}

#[tokio::test]
async fn delayed_preflight_cannot_attach_old_props_to_reloaded_live_config() {
    let source = "This report explains why reliable translation timing matters.";
    let segmenter = fallback_segmenter();
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
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (first, first_requests) = props_then_completion_server(
        "test-model",
        translated.clone(),
        Some((Arc::clone(&entered), Arc::clone(&release))),
    )
    .await;
    let (second, second_requests) =
        props_then_completion_server("next-model", translated, None).await;
    let config = make_stream_config(&first.endpoint_url);
    let strict = std::fs::read_to_string(config.path()).unwrap().replace(
        "timeout = 5",
        "timeout = 5\nstrict_backend_preflight = true",
    );
    std::fs::write(config.path(), strict).unwrap();
    std::fs::File::open(config.path())
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + Duration::from_secs(2)),
        )
        .unwrap();
    assert!(config.maybe_reload().unwrap());
    let client = TranslationClient::new(config.clone()).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(temp.path().join("history.db"));
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: false,
    };
    let update = async {
        entered.notified().await;
        let next = std::fs::read_to_string(config.path())
            .unwrap()
            .replace(&first.endpoint_url, &second.endpoint_url)
            .replace("model = \"test-model\"", "model = \"next-model\"");
        std::fs::write(config.path(), next).unwrap();
        std::fs::File::open(config.path())
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() + Duration::from_secs(4)),
            )
            .unwrap();
        assert!(config.maybe_reload().unwrap());
        release.notify_one();
    };
    let opts = PromptOpts::default();
    let (result, ()) = tokio::join!(
        translate_text(source, "zh", &TemplateType::Default, &opts, &ctx),
        update,
    );
    result.unwrap();
    assert_eq!(first_requests.load(Ordering::SeqCst), 2);
    assert_eq!(second_requests.load(Ordering::SeqCst), 0);
    assert!(config.backend_runtime_info().is_none());
    assert_eq!(
        history.fetch_recent(Some(1)).unwrap()[0].model.as_deref(),
        Some("test-model")
    );

    run_route(source, &ctx, false).await;
    assert_eq!(second_requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        history.fetch_recent(Some(1)).unwrap()[0].model.as_deref(),
        Some("next-model")
    );
}

#[tokio::test]
async fn direct_preflight_does_not_publish_old_props_after_live_reload() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (first, requests) = props_then_completion_server(
        "test-model",
        "unused".to_owned(),
        Some((Arc::clone(&entered), Arc::clone(&release))),
    )
    .await;
    let config = make_stream_config(&first.endpoint_url);
    let client = TranslationClient::new(config.clone()).unwrap();
    let update = async {
        entered.notified().await;
        let next = std::fs::read_to_string(config.path())
            .unwrap()
            .replace(&first.endpoint_url, "http://127.0.0.1:1/v1")
            .replace("model = \"test-model\"", "model = \"next-model\"");
        std::fs::write(config.path(), next).unwrap();
        std::fs::File::open(config.path())
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() + Duration::from_secs(2)),
            )
            .unwrap();
        assert!(config.maybe_reload().unwrap());
        release.notify_one();
    };
    let (result, ()) = tokio::join!(client.preflight_backend(), update);
    assert!(matches!(
        result,
        Err(hymt_client::ClientError::ConfigChangedDuringPreflight)
    ));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(config.backend_runtime_info().is_none());
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

#[tokio::test]
async fn reload_then_replay_or_direct_miss_cannot_reuse_old_client_runtime() {
    let source = "This report explains why reliable translation timing matters.";
    let segmenter = fallback_segmenter();
    for replay_first in [false, true] {
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
        let translation = planned_complete_zh_translations(&plan).remove(0);
        let (server, requests) =
            props_then_completion_server("test-model", translation.clone(), None).await;
        let original = make_stream_config(&server.endpoint_url);
        let raw = std::fs::read_to_string(original.path()).unwrap().replace(
            "timeout = 5",
            "timeout = 0.1\nstrict_backend_preflight = true",
        );
        std::fs::write(original.path(), &raw).unwrap();
        let config = HotConfig::from_path(original.path()).unwrap();
        let client = TranslationClient::new(config.clone()).unwrap();
        // Populate the original client's runtime cache before the generation changes.
        client.preflight_backend().await.unwrap();
        assert!(config.backend_runtime_info().unwrap().is_verified());
        let temp = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(temp.path().join("history.db"));
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: true,
        };
        assert_eq!(
            translate_text(
                source,
                "zh",
                &TemplateType::Default,
                &PromptOpts::default(),
                &ctx
            )
            .await
            .unwrap()
            .text,
            translation
        );
        while !server.handle.is_finished() {
            tokio::task::yield_now().await;
        }
        drop(server);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        let generation = config.generation();
        std::fs::write(
            config.path(),
            format!("{raw}\n# trigger reload without changing request identity\n"),
        )
        .unwrap();
        std::fs::File::open(config.path())
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() + Duration::from_secs(2)),
            )
            .unwrap();
        if replay_first {
            assert_eq!(
                translate_text(
                    source,
                    "zh",
                    &TemplateType::Default,
                    &PromptOpts::default(),
                    &ctx
                )
                .await
                .unwrap()
                .text,
                translation
            );
            assert!(config.generation() > generation);
            assert!(config.backend_runtime_info().is_none());
        }
        let error = translate_text(
            "A different report requires a new request.",
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("strict backend preflight"),
            "{error:#}"
        );
        assert!(config.generation() > generation);
        assert!(config.backend_runtime_info().is_none());
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }
}
