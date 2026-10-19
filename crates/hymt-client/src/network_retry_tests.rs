use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_stream::StreamExt as _;

use super::contract_test_support::{config_fixture, read_http_request};
use super::{ClientError, TranslationClient, TranslationStreamEvent};

struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn response(status: u16, body: &str, extra_length: usize) -> String {
    format!(
        "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nx-hymt-cache: MISS\r\nconnection: close\r\n\r\n{body}",
        body.len() + extra_length
    )
}

async fn server(replies: Vec<String>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        let mut replies = replies.into_iter();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await.unwrap();
            captured.lock().unwrap().push(request);
            let reply = replies
                .next()
                .unwrap_or_else(|| response(401, "unexpected extra request", 0));
            socket.write_all(reply.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    Server {
        endpoint,
        requests,
        task,
    }
}

fn success() -> String {
    response(
        200,
        r#"{"choices":[{"index":0,"message":{"content":"complete"},"finish_reason":"stop"}]}"#,
        0,
    )
}

#[tokio::test]
async fn complete_http_exchange_retries_body_and_respects_configured_counts() {
    for (retries, first, expected_success, expected_requests) in [
        (1, response(400, "invalid JSON configuration", 0), false, 1),
        (1, response(200, "{\"choices\":", 30), true, 2),
        (0, response(503, "unavailable", 0), false, 1),
        (1, response(503, "unavailable", 0), true, 2),
        (1, response(401, "unauthorized", 0), false, 1),
        (1, response(200, "not json", 0), false, 1),
    ] {
        let server = server(vec![first, success()]).await;
        let fixture = config_fixture(&server.endpoint, "llama_cpp", None, "", false);
        let path = fixture.config.path();
        let text = std::fs::read_to_string(path).unwrap().replace(
            "[translation]\n",
            &format!("[translation]\nmax_retries = {retries}\n"),
        );
        std::fs::write(path, text).unwrap();
        let client =
            TranslationClient::new(hymt_core::config::HotConfig::from_path(path).unwrap()).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.translate_with_completion("same original chunk"),
        )
        .await
        .unwrap();
        assert_eq!(
            result.is_ok(),
            expected_success,
            "retries={retries}: {result:?}"
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), expected_requests);
        if let Ok(completion) = result {
            assert_eq!(completion.text, "complete");
            assert!(!completion.cold_generation);
            assert_eq!(
                requests[0], requests[1],
                "retries must preserve request and cache headers"
            );
        }
    }
}

#[tokio::test]
async fn necessary_preflight_recovers_from_transient_http_failure() {
    let props = r#"{"model_alias":"served-model","n_ctx":4096,"n_ctx_per_seq":4096}"#;
    let server = server(vec![response(503, "offline", 0), response(200, props, 0)]).await;
    let fixture = config_fixture(
        &server.endpoint,
        "llama_cpp",
        Some("served-model"),
        "",
        false,
    );
    let client = TranslationClient::new(fixture.config.clone()).unwrap();
    let report = client.preflight_backend().await.unwrap();
    assert!(report.runtime.is_verified(), "{report:?}");
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

fn configured_client(
    endpoint: &str,
    retries: Option<u32>,
) -> (
    super::contract_test_support::ConfigFixture,
    TranslationClient,
) {
    let fixture = config_fixture(endpoint, "llama_cpp", None, "", true);
    if let Some(retries) = retries {
        let text = std::fs::read_to_string(fixture.config.path())
            .unwrap()
            .replace(
                "[translation]\n",
                &format!("[translation]\nmax_retries = {retries}\n"),
            );
        std::fs::write(fixture.config.path(), text).unwrap();
    }
    let config = hymt_core::config::HotConfig::from_path(fixture.config.path()).unwrap();
    let client = TranslationClient::new(config).unwrap();
    (fixture, client)
}

fn interrupted_stream() -> String {
    response(
        200,
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"unpublished draft\"}}]}\n\n",
        32,
    )
    .replace("application/json", "text/event-stream")
}

#[tokio::test(start_paused = true)]
async fn default_budget_allows_exactly_one_hundred_additional_requests() {
    let server = server(vec![response(503, "offline", 0); 102]).await;
    let (_fixture, mut client) = configured_client(&server.endpoint, None);
    // Paused time measures the retry count, not I/O deadlines: otherwise Tokio
    // advances the HTTP timeout while the real loopback socket is still readable.
    Arc::get_mut(&mut client.inner).unwrap().http = reqwest::Client::builder()
        .pool_idle_timeout(None)
        .build()
        .unwrap();
    assert_eq!(client.inner.config.network_max_retries(), 100);
    let result = client
        .translate_with_completion("default count fixture")
        .await;
    assert!(
        matches!(result, Err(ClientError::Http { status: 503, .. })),
        "{result:?}"
    );
    assert_eq!(server.requests.lock().unwrap().len(), 101);
}

#[tokio::test]
async fn connection_and_body_failures_share_one_budget() {
    for streaming in [false, true] {
        let partial = if streaming {
            interrupted_stream()
        } else {
            response(200, "{", 40)
        };
        let server = server(vec![response(503, "offline", 0), partial, success()]).await;
        let (_fixture, client) = configured_client(&server.endpoint, Some(1));
        if streaming {
            let events = client
                .translate_stream_with_recovery("shared budget fixture")
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert!(
                matches!(events.last(), Some(Err(ClientError::Request(_)))),
                "{events:?}"
            );
        } else {
            assert!(matches!(
                client
                    .translate_with_completion("shared budget fixture")
                    .await,
                Err(ClientError::Request(_))
            ));
        }
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn stream_json_fallback_retries_without_changing_the_wire_request() {
    let server = server(vec![response(200, "{", 30), success()]).await;
    let (_fixture, client) = configured_client(&server.endpoint, Some(1));
    let events = client
        .translate_stream_with_recovery("fallback fixture")
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(events.contains(&TranslationStreamEvent::Restarted));
    let text: String = events
        .into_iter()
        .filter_map(|event| match event {
            TranslationStreamEvent::Token(token) => Some(token),
            _ => None,
        })
        .collect();
    assert_eq!(text, "complete");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
}

#[tokio::test]
async fn model_discovery_and_false_eos_tokenization_recover() {
    let models = server(vec![
        response(503, "offline", 0),
        response(200, r#"{"data":[{"id":"served-model"}]}"#, 0),
    ])
    .await;
    let fixture = config_fixture(
        &models.endpoint,
        "openai_compatible",
        Some("served-model"),
        "",
        false,
    );
    let report = TranslationClient::new(fixture.config.clone())
        .unwrap()
        .preflight_backend()
        .await
        .unwrap();
    assert!(report.runtime.is_verified());
    assert_eq!(models.requests.lock().unwrap().len(), 2);

    let props = r#"{"model_alias":"served-model","n_ctx":4096,"n_ctx_per_seq":4096,"eos_token":"!","chat_template":"<|eos|>"}"#;
    let tokenizer = server(vec![
        response(200, props, 0),
        response(503, "offline", 0),
        response(200, r#"{"tokens":[17]}"#, 0),
    ])
    .await;
    let (_fixture, client) = configured_client(&tokenizer.endpoint, Some(1));
    let report = client.preflight_backend().await.unwrap();
    assert_eq!(report.runtime.false_eos_token_id, Some(17));
    let requests = tokenizer.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1], requests[2]);
    assert!(requests[1].starts_with("POST /tokenize HTTP/1.1"));
}

#[tokio::test]
async fn optimistic_stream_does_not_replay_published_tokens() {
    let server = server(vec![interrupted_stream(), success()]).await;
    let (_fixture, client) = configured_client(&server.endpoint, Some(1));
    let events = client
        .translate_stream_with_completion("optimistic fixture")
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| matches!(event, Ok(TranslationStreamEvent::Token(token)) if token == "unpublished draft")));
    assert!(matches!(events.last(), Some(Err(ClientError::Request(_)))));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn dropping_recoverable_stream_cancels_backoff_and_releases_slot() {
    let server = server(vec![interrupted_stream(), success()]).await;
    let (_fixture, client) = configured_client(&server.endpoint, None);
    let mut stream = client
        .translate_stream_with_recovery("cancellation fixture")
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        if matches!(event, Ok(TranslationStreamEvent::Token(_))) {
            break;
        }
    }
    drop(stream);
    tokio::time::timeout(Duration::from_secs(2), async {
        while client.inner.semaphore.available_permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn interrupted_headers_recover_and_optional_diagnostics_do_not_retry() {
    let server = server(vec![String::new(), success()]).await;
    let (_fixture, client) = configured_client(&server.endpoint, Some(1));
    assert_eq!(
        client.translate("header fixture").await.unwrap(),
        "complete"
    );
    assert_eq!(server.requests.lock().unwrap().len(), 2);

    let diagnostic = self::server(vec![response(503, "offline", 0)]).await;
    let (_fixture, client) = configured_client(&diagnostic.endpoint, None);
    assert!(client
        .llama_cpp_props_diagnostic()
        .await
        .unwrap()
        .contains("503"));
    assert_eq!(diagnostic.requests.lock().unwrap().len(), 1);

    let inspection = self::server(vec![response(503, "offline", 0)]).await;
    let (_fixture, client) = configured_client(&inspection.endpoint, None);
    assert!(!client
        .inspect_backend()
        .await
        .unwrap()
        .runtime
        .is_verified());
    assert_eq!(inspection.requests.lock().unwrap().len(), 1);
}
