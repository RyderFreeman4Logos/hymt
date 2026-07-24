use super::*;
use hymt_core::config::HotConfig;
use hymt_core::runtime::BackendVerificationStatus;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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
