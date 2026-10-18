use futures_core::Stream;
use tokio_stream::StreamExt as _;

use super::contract_test_support::{config_fixture, start_one_shot_server, MockReply};
use super::{ClientError, TranslationClient, TranslationStreamEvent};
use hymt_core::completeness::CompletionTermination;

async fn stream_results(
    mut stream: impl Stream<Item = Result<String, ClientError>> + Unpin,
) -> Vec<Result<String, ClientError>> {
    let mut results = Vec::new();
    while let Some(item) = stream.next().await {
        results.push(item);
    }
    results
}

fn client_for(
    server_endpoint: &str,
) -> (
    super::contract_test_support::ConfigFixture,
    TranslationClient,
) {
    let fixture = config_fixture(server_endpoint, "llama_cpp", None, "", true);
    let client = TranslationClient::new(fixture.config.clone()).expect("client");
    (fixture, client)
}

#[tokio::test]
async fn completion_stream_requires_a_response_slot_and_accepts_zero_text_choices() {
    for (body, valid) in [
        ("data: [DONE]\n\n", false),
        ("data: {\"error\":{\"message\":\"fixture error\"}}\n\ndata: [DONE]\n\n", false),
        ("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", false),
        ("data: {\"choices\":[{\"index\":0,\"delta\":null,\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", false),
        ("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":{}},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", false),
        ("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":[]},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", false),
        ("data: {\"choices\":[]}\n\ndata: [DONE]\n\n", false),
        ("data: {\"choices\":[{\"index\":1,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", false),
        ("data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", false),
        ("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":123},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", false),
        ("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", true),
        ("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[]}\n\ndata: [DONE]\n\n", true),
        ("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", true),
        ("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n", true),
        ("data: {\"choices\":[{\"index\":0,\"text\":\"\",\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", true),
    ] {
        let server = start_one_shot_server(MockReply::sse(vec![body.as_bytes().to_vec()])).await;
        let (_fixture, client) = client_for(&server.endpoint);
        let events = client.translate_stream_with_completion("slot fixture").await.unwrap()
            .collect::<Vec<_>>().await;
        assert_eq!(events.iter().all(Result::is_ok), valid, "{body}: {events:?}");
        if valid {
            assert!(events.iter().any(|event| matches!(event, Ok(TranslationStreamEvent::Finished(CompletionTermination::Stop)))));
            assert!(!events.iter().any(|event| matches!(event, Ok(TranslationStreamEvent::Token(_)))));
        } else {
            assert!(!events.iter().any(|event| matches!(event, Ok(TranslationStreamEvent::Finished(CompletionTermination::Stop)))));
        }
        server.received_request().await;
    }
}

#[tokio::test]
async fn completion_stream_distinguishes_eof_only_done_and_explicit_finish() {
    let token =
        "data: {\"choices\":[{\"index\":0,\"finish_reason\":null,\"delta\":{\"content\":\"prefix\"}}]}\n\n";
    for (suffix, expected) in [
        ("", CompletionTermination::MissingTerminal),
        ("data: [DONE]\n\n", CompletionTermination::Stop),
        (
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{\"content\":\"\"}}]}\n\n",
            CompletionTermination::Stop,
        ),
        (
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"length\",\"delta\":{\"content\":\"\"}}]}\n\n",
            CompletionTermination::Length,
        ),
        (
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"custom\",\"delta\":{\"content\":\"\"}}]}\n\n",
            CompletionTermination::Unknown,
        ),
    ] {
        let server =
            start_one_shot_server(MockReply::sse(
                vec![format!("{token}{suffix}").into_bytes()],
            ))
            .await;
        let (_fixture, client) = client_for(&server.endpoint);
        let events = client
            .translate_stream_with_completion("terminal fixture")
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(events.contains(&TranslationStreamEvent::Token("prefix".into())));
        assert!(
            events.contains(&TranslationStreamEvent::Finished(expected)),
            "{events:?}"
        );
        server.received_request().await;
    }
}

#[tokio::test]
async fn sse_accepts_crlf_lf_comments_multiline_data_and_utf8_chunk_boundaries() {
    let record = concat!(
        ": heartbeat\r\n",
        "event: message\r\n",
        "id: 17\r\n",
        "data: {\"choices\":[\r\n",
        "data: {\"finish_reason\":null,\"delta\":{\"content\":\"你\"}}]}\r\n",
        "\r\n",
        "data: {\"choices\":[{\"index\":0,\"finish_reason\":null,\"delta\":{\"content\":\"好\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let bytes = record.as_bytes();
    let chinese = bytes
        .windows("你".len())
        .position(|window| window == "你".as_bytes())
        .expect("fixture contains first UTF-8 token");
    let chunks = vec![
        bytes[..chinese + 1].to_vec(),
        bytes[chinese + 1..chinese + 2].to_vec(),
        bytes[chinese + 2..].to_vec(),
    ];
    let mut reply = MockReply::sse(chunks);
    reply.chunk_delay = std::time::Duration::from_millis(2);
    let server = start_one_shot_server(reply).await;
    let (_fixture, client) = client_for(&server.endpoint);

    let results = stream_results(
        client
            .translate_stream("sse fixture")
            .await
            .expect("stream request"),
    )
    .await;
    assert_eq!(
        results
            .into_iter()
            .collect::<Result<String, _>>()
            .expect("valid SSE"),
        "你好"
    );
    server.received_request().await;
}

#[tokio::test]
async fn sse_flushes_final_buffered_event_without_a_blank_terminator() {
    let body = b"data: {\"choices\":[{\"index\":0,\"finish_reason\":null,\"delta\":{\"content\":\"tail\"}}]}";
    let server = start_one_shot_server(MockReply::sse(vec![body.to_vec()])).await;
    let (_fixture, client) = client_for(&server.endpoint);

    let results = stream_results(
        client
            .translate_stream("tail fixture")
            .await
            .expect("stream request"),
    )
    .await;
    assert_eq!(
        results
            .into_iter()
            .collect::<Result<String, _>>()
            .expect("final buffer token"),
        "tail"
    );
    server.received_request().await;
}

#[tokio::test]
async fn sse_length_finish_reason_is_a_truncation_error() {
    let body = concat!(
        "data: {\"choices\":[{\"index\":0,\"finish_reason\":null,\"delta\":{\"content\":\"prefix\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"length\",\"delta\":{\"content\":\"\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let server = start_one_shot_server(MockReply::sse(vec![body.as_bytes().to_vec()])).await;
    let (_fixture, client) = client_for(&server.endpoint);

    let results = stream_results(
        client
            .translate_stream("length fixture")
            .await
            .expect("stream request"),
    )
    .await;
    assert_eq!(
        results
            .first()
            .expect("prefix token")
            .as_ref()
            .expect("token"),
        "prefix"
    );
    assert!(
        matches!(results.get(1), Some(Err(ClientError::Truncated))),
        "finish_reason=length must remain observable to callers: {results:?}"
    );
    server.received_request().await;
}

#[tokio::test]
async fn sse_malformed_event_and_dropped_connection_surface_errors() {
    let malformed =
        start_one_shot_server(MockReply::sse(vec![b"data: not-json\n\n".to_vec()])).await;
    let (_malformed_fixture, malformed_client) = client_for(&malformed.endpoint);
    let malformed_results = stream_results(
        malformed_client
            .translate_stream("bad fixture")
            .await
            .expect("stream request"),
    )
    .await;
    assert!(matches!(
        malformed_results.as_slice(),
        [Err(ClientError::Json(_))]
    ));
    malformed.received_request().await;

    let partial =
        b"data: {\"choices\":[{\"index\":0,\"finish_reason\":null,\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let mut dropped_reply = MockReply::sse(vec![partial.to_vec()]);
    dropped_reply.advertised_length = Some(partial.len() + 32);
    let dropped = start_one_shot_server(dropped_reply).await;
    let (_dropped_fixture, dropped_client) = client_for(&dropped.endpoint);
    let dropped_results = stream_results(
        dropped_client
            .translate_stream("dropped fixture")
            .await
            .expect("stream request"),
    )
    .await;
    assert!(
        dropped_results
            .iter()
            .any(|result| matches!(result, Err(ClientError::Request(_)))),
        "an incomplete HTTP body must not look like a clean stream: {dropped_results:?}"
    );
    dropped.received_request().await;
}
