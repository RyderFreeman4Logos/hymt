use super::*;
use hymt_core::config::HotConfig;
use hymt_core::runtime::BackendVerificationStatus;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static NEXT_PREFLIGHT_CONFIG_ID: AtomicUsize = AtomicUsize::new(0);

fn config_for(endpoint: &str, strict: bool) -> HotConfig {
    let id = NEXT_PREFLIGHT_CONFIG_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "hymt-client-preflight-{}-{id}.toml",
        std::process::id()
    ));
    std::fs::write(
        &path,
        format!(
            "[endpoint]\nurl = \"{endpoint}\"\nmodel = \"served-model\"\nbackend = \"llama_cpp\"\n\n[backend]\ntotal_context = 16384\nparallel_slots = 1\n\n[translation]\nstrict_backend_preflight = {strict}\n"
        ),
    )
    .expect("write config fixture");
    HotConfig::from_path(&path).expect("load config fixture")
}

async fn respond(listener: TcpListener, bodies: Vec<&'static str>) {
    for body in bodies {
        let (mut socket, _) = listener.accept().await.expect("accept props request");
        let mut request = [0_u8; 1024];
        let count = socket.read(&mut request).await.expect("read props request");
        let request = std::str::from_utf8(&request[..count]).expect("utf-8 request");
        assert!(request.starts_with("GET /props HTTP/1.1"));
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("write props response");
    }
}

async fn read_http_request(socket: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let count = socket.read(&mut chunk).await.expect("read request");
        assert!(count > 0, "request ended before headers");
        request.extend_from_slice(&chunk[..count]);
        if let Some(pos) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = std::str::from_utf8(&request[..header_end]).expect("UTF-8 headers");
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .or_else(|| line.strip_prefix("Content-Length:"))
        })
        .map(str::trim)
        .map(|value| value.parse::<usize>().expect("valid content length"))
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        let count = socket.read(&mut chunk).await.expect("read request body");
        assert!(count > 0, "request ended before body");
        request.extend_from_slice(&chunk[..count]);
    }
    String::from_utf8(request).expect("UTF-8 request")
}

async fn respond_json(socket: &mut TcpStream, body: &str) {
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write JSON response");
}

async fn respond_eos_sequence(
    listener: TcpListener,
    eos_token: &'static str,
    expected_false_eos_token_id: Option<u32>,
) {
    let (mut socket, _) = listener.accept().await.expect("accept props request");
    let request = read_http_request(&mut socket).await;
    assert!(request.starts_with("GET /props HTTP/1.1"));
    respond_json(
        &mut socket,
        &format!(
            r#"{{"model_alias":"served-model","n_ctx":16384,"n_ctx_per_seq":16384,"eos_token":"{eos_token}"}}"#
        ),
    )
    .await;

    if let Some(token_id) = expected_false_eos_token_id {
        let (mut socket, _) = listener.accept().await.expect("accept tokenize request");
        let request = read_http_request(&mut socket).await;
        assert!(request.starts_with("POST /tokenize HTTP/1.1"));
        let (_, body) = request.split_once("\r\n\r\n").expect("request body");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).expect("tokenize JSON"),
            serde_json::json!({"content": eos_token}),
        );
        respond_json(
            &mut socket,
            &serde_json::json!({"tokens": [token_id]}).to_string(),
        )
        .await;
    }

    let (mut socket, _) = listener.accept().await.expect("accept completion request");
    let request = read_http_request(&mut socket).await;
    assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
    let (_, body) = request.split_once("\r\n\r\n").expect("request body");
    let payload: serde_json::Value = serde_json::from_str(body).expect("completion JSON");
    match expected_false_eos_token_id {
        Some(token_id) => assert_eq!(
            payload["logit_bias"],
            serde_json::json!({token_id.to_string(): -100}),
        ),
        None => assert!(
            payload.get("logit_bias").is_none(),
            "a normal chat-control EOS must not be biased"
        ),
    }
    respond_json(
        &mut socket,
        r#"{"choices":[{"finish_reason":"stop","message":{"content":"translated"}}]}"#,
    )
    .await;
}

#[tokio::test]
async fn unavailable_preflight_is_unverified_and_uses_conservative_context() {
    let config = config_for("http://127.0.0.1:1/v1", false);
    let client = TranslationClient::new(config.clone()).expect("client");

    let report = client.preflight_backend().await.expect("normal preflight");

    assert_eq!(
        report.runtime.verification_status,
        BackendVerificationStatus::Unverified
    );
    assert!(!report.warnings.is_empty());
    assert_eq!(config.resolved_per_request_context(), 4_096);
    assert!(!config
        .inference_fingerprint("default", "")
        .expect("fingerprint")
        .is_cache_verified());
}

#[tokio::test]
async fn malformed_props_fail_open_without_claiming_service_metadata() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(respond(listener, vec![r#"{"n_ctx":"wrong"}"#]));
    let config = config_for(&format!("http://{address}/v1"), false);
    let client = TranslationClient::new(config).expect("client");

    let report = client.preflight_backend().await.expect("normal preflight");

    server.await.expect("server task");
    assert_eq!(
        report.runtime.verification_status,
        BackendVerificationStatus::Unverified
    );
    assert_eq!(report.runtime.total_context, None);
    assert!(report.warnings.join(" ").contains("malformed"));
}

#[tokio::test]
async fn strict_preflight_refuses_material_context_mismatch_before_translation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(respond(
        listener,
        vec![r#"{"model_alias":"served-model","n_ctx":8192,"n_ctx_per_seq":4096}"#],
    ));
    let config = config_for(&format!("http://{address}/v1"), true);
    let client = TranslationClient::new(config).expect("client");

    let error = client
        .preflight_backend()
        .await
        .expect_err("strict preflight must reject context mismatch");

    server.await.expect("server task");
    assert!(error.to_string().contains("strict backend preflight"));
}

#[tokio::test]
async fn preflight_reports_profile_model_mismatch() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(respond(
        listener,
        vec![
            r#"{"model_alias":"different-model","n_ctx":16384,"n_ctx_per_seq":16384,"default_generation_settings":{"repetition_penalty":1.0}}"#,
        ],
    ));
    let config = config_for(&format!("http://{address}/v1"), false);
    let client = TranslationClient::new(config).expect("client");

    let report = client.preflight_backend().await.expect("normal preflight");

    server.await.expect("server task");
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("profile/model mismatch")));
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("sampler wire-key mismatch")));
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("unexpected sampler state")));
}

#[tokio::test]
async fn false_ascii_eos_is_tokenized_and_suppressed_in_llama_payload() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(respond_eos_sequence(listener, "$", Some(3)));
    let config = config_for(&format!("http://{address}/v1"), false);
    let client = TranslationClient::new(config.clone()).expect("client");

    let report = client.preflight_backend().await.expect("preflight");
    assert_eq!(report.runtime.false_eos_token.as_deref(), Some("$"));
    assert_eq!(report.runtime.false_eos_token_id, Some(3));
    assert_eq!(
        client
            .translate("translate this")
            .await
            .expect("translation"),
        "translated"
    );

    server.await.expect("server task");
}

#[tokio::test]
async fn chat_control_eos_does_not_add_llama_logit_bias() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(respond_eos_sequence(listener, "<|eos|>", None));
    let config = config_for(&format!("http://{address}/v1"), false);
    let client = TranslationClient::new(config).expect("client");

    client.preflight_backend().await.expect("preflight");
    assert_eq!(
        client
            .translate("translate this")
            .await
            .expect("translation"),
        "translated"
    );

    server.await.expect("server task");
}

#[tokio::test]
async fn forced_refresh_replaces_runtime_info_after_server_restart() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(respond(
        listener,
        vec![
            r#"{"build_info":"before","model_alias":"served-model","n_ctx":8192,"n_ctx_per_seq":8192}"#,
            r#"{"build_info":"after","model_alias":"replacement-model","n_ctx":4096,"n_ctx_per_seq":4096}"#,
        ],
    ));
    let config = config_for(&format!("http://{address}/v1"), false);
    let client = TranslationClient::new(config.clone()).expect("client");

    let before = client.preflight_backend().await.expect("first preflight");
    let after = client
        .refresh_backend_preflight()
        .await
        .expect("forced refresh");

    server.await.expect("server task");
    assert_ne!(
        before.runtime.server_identity,
        after.runtime.server_identity
    );
    assert_eq!(
        after.runtime.served_model.as_deref(),
        Some("replacement-model")
    );
    assert_eq!(config.resolved_per_request_context(), 4_096);
}
