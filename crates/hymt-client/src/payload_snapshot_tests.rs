use futures_core::Stream;
use tokio_stream::StreamExt as _;

use super::contract_test_support::{
    config_fixture, request_json, start_one_shot_server, MockReply,
};
use super::TranslationClient;

fn completed_json(text: &str) -> Vec<u8> {
    format!(r#"{{"choices":[{{"finish_reason":"stop","message":{{"content":"{text}"}}}}]}}"#)
        .into_bytes()
}

async fn collect_stream(
    mut stream: impl Stream<Item = Result<String, super::ClientError>> + Unpin,
) -> Result<String, super::ClientError> {
    let mut text = String::new();
    while let Some(token) = stream.next().await {
        text.push_str(&token?);
    }
    Ok(text)
}

#[tokio::test]
async fn payload_snapshot_server_defaults_omit_sampler_fields_and_empty_model() {
    let server = start_one_shot_server(MockReply::json(completed_json("ok"))).await;
    let fixture = config_fixture(&server.endpoint, "llama_cpp", None, "", false);
    let client = TranslationClient::new(fixture.config.clone()).expect("client");

    assert_eq!(
        client
            .translate("translate this")
            .await
            .expect("translation"),
        "ok"
    );

    let actual = request_json(&server.received_request().await);
    assert_eq!(
        actual,
        serde_json::json!({
            "messages": [{"role": "user", "content": "translate this"}],
            "max_tokens": 37,
        }),
        "server-default mode is a complete request snapshot, not a partial-field assertion"
    );
    for forbidden in [
        "temperature",
        "top_p",
        "top_k",
        "repeat_penalty",
        "repetition_penalty",
        "min_p",
        "repeat_last_n",
        "model",
        "stream",
    ] {
        assert!(
            actual.get(forbidden).is_none(),
            "{forbidden} must be omitted"
        );
    }
}

#[tokio::test]
async fn payload_snapshot_llama_cpp_uses_only_llama_wire_keys() {
    let server = start_one_shot_server(MockReply::json(completed_json("ok"))).await;
    let fixture = config_fixture(
        &server.endpoint,
        "llama_cpp",
        Some("pinned-llama-model"),
        "[inference.override]\ntemperature = 0.8\ntop_p = 0.6\ntop_k = \"disabled\"\nrepetition_penalty = 1.1\nmin_p = 0.05\nrepeat_last_n = 64\n",
        false,
    );
    let client = TranslationClient::new(fixture.config.clone()).expect("client");

    client
        .translate("llama request")
        .await
        .expect("translation");

    let actual = request_json(&server.received_request().await);
    assert_eq!(
        actual,
        serde_json::json!({
            "messages": [{"role": "user", "content": "llama request"}],
            "max_tokens": 37,
            "model": "pinned-llama-model",
            "temperature": 0.8,
            "top_p": 0.6,
            "top_k": 0,
            "repeat_penalty": 1.1,
            "min_p": 0.05,
            "repeat_last_n": 64,
        })
    );
    assert!(
        actual.get("repetition_penalty").is_none(),
        "the vLLM key must fail this exact snapshot for llama.cpp"
    );
}

#[tokio::test]
async fn payload_snapshot_vllm_streaming_uses_only_vllm_wire_keys() {
    let body = b"data: {\"choices\":[{\"finish_reason\":null,\"delta\":{\"content\":\"streamed\"}}]}\n\ndata: [DONE]\n\n";
    let server = start_one_shot_server(MockReply::sse(vec![body.to_vec()])).await;
    let fixture = config_fixture(
        &server.endpoint,
        "vllm",
        Some("pinned-vllm-model"),
        "[inference.override]\ntemperature = 0.2\ntop_p = 0.9\ntop_k = \"disabled\"\nrepetition_penalty = 1.07\nmin_p = 0.03\n",
        true,
    );
    let client = TranslationClient::new(fixture.config.clone()).expect("client");

    assert_eq!(
        collect_stream(
            client
                .translate_stream("vllm request")
                .await
                .expect("stream request")
        )
        .await
        .expect("stream response"),
        "streamed"
    );

    let actual = request_json(&server.received_request().await);
    assert_eq!(
        actual,
        serde_json::json!({
            "messages": [{"role": "user", "content": "vllm request"}],
            "max_tokens": 37,
            "model": "pinned-vllm-model",
            "stream": true,
            "temperature": 0.2,
            "top_p": 0.9,
            "top_k": -1,
            "repetition_penalty": 1.07,
            "min_p": 0.03,
        })
    );
    assert!(
        actual.get("repeat_penalty").is_none(),
        "the llama.cpp key must fail this exact snapshot for vLLM"
    );
}
