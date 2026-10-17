use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use hymt_cache::history::{HistoryDB, TaskRecord};
use hymt_core::config::HotConfig;
use hymt_core::runtime::BackendRuntimeInfo;
use hymt_core::templates::{PromptOpts, TemplateType, PROMPT_SCHEMA_ID};
use hymt_segment::Segmenter;
use hymt_translate::translate::{plan_translation, template_options_hash};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

struct TestHome(PathBuf);

impl Drop for TestHome {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove batch test home");
    }
}

#[tokio::test]
async fn batch_preview_uses_effective_cli_concurrency() {
    let home = TestHome(std::env::temp_dir().join(format!(
        "hymt-batch-timing-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )));
    let config_path = home.0.join(".config/hymt/config.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    std::fs::write(
        &config_path,
        format!(
            "[endpoint]\nurl = \"http://{address}/v1\"\nbackend = \"llama_cpp\"\nprofile = \"hy_mt2_7b\"\nmodel = \"test-model\"\n\n[backend]\ntotal_context = 8192\nparallel_slots = 4\n\n[translation]\nmax_output_tokens = 128\nmax_source_tokens_per_segment = 32\nconcurrency = 4\ntimeout = 5\n"
        ),
    )
    .unwrap();
    let props = serde_json::json!({
        "model_alias": "test-model", "n_ctx": 8192, "n_ctx_per_seq": 2048,
        "n_parallel": 4, "chat_template": "<|startoftext|>{{ user }}<|extra_0|>",
        "default_generation_settings": {"params": {
            "temperature": 0.7, "top_p": 0.6, "top_k": 20, "min_p": 0.0,
            "repeat_penalty": 1.05, "repeat_last_n": 64, "n_predict": 128
        }}
    });
    let config = HotConfig::from_path(&config_path).unwrap();
    config.set_backend_runtime_info(BackendRuntimeInfo::from_llama_cpp_props(&props, 0).unwrap());
    let opts = PromptOpts::default();
    let fingerprint = config
        .inference_fingerprint(
            "default",
            &template_options_hash(&opts, config.document_translation_policy()),
        )
        .unwrap();
    assert!(fingerprint.is_cache_verified());
    let source = "This paragraph describes a synthetic translation timing example.\n\n".repeat(12);
    let source_dir = home.0.join("source");
    std::fs::create_dir(&source_dir).unwrap();
    std::fs::write(source_dir.join("input.md"), &source).unwrap();
    let segmenter = Segmenter::fallback();
    let plan = plan_translation(
        &source,
        "zh",
        &config,
        &segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    assert!(plan.segment_count() >= 4);
    let source_tokens = plan
        .segments
        .iter()
        .map(|segment| segmenter.count_tokens(segment) as i64)
        .sum();
    let history = HistoryDB::new(home.0.join(".local/share/hymt/history.db"));
    for _ in 0..3 {
        history
            .insert_task(&TaskRecord {
                id: None,
                started_at: "start".to_owned(),
                finished_at: "finish".to_owned(),
                duration_seconds: 400.0,
                input_tokens: source_tokens,
                output_tokens: 400,
                segments: plan.segment_count() as i64,
                concurrency: 1,
                source_lang: None,
                target_lang: "zh".to_owned(),
                template_type: "default".to_owned(),
                model: Some("test-model".to_owned()),
                profile_id: config.model_profile().unwrap().id().to_owned(),
                inference_fingerprint: fingerprint.hash().to_owned(),
                prompt_schema: PROMPT_SCHEMA_ID.to_owned(),
                cache_hits: Some(0),
                cold_source_tokens: Some(source_tokens),
                counting_source: Some("approximate".to_owned()),
                estimated_seconds: None,
                tokens_per_second: 1.0,
                input_chars: source.chars().count() as i64,
                output_chars: 400,
                output_text: None,
                input_hash: None,
                config_version: config.config_version() as i64,
            })
            .unwrap();
    }
    let provider = tokio::spawn(async move {
        for _ in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0, "incomplete props request");
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() <= 8192, "oversized props request");
            }
            assert!(request.starts_with(b"GET /props "));
            let body = props.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let mut results = Vec::new();
    for (override_value, expected_seconds) in [
        (None, 100),
        (Some("1"), 400),
        (Some("2"), 200),
        (Some("0"), 400),
    ] {
        let stderr_path = home.0.join(format!("preview-{override_value:?}.stderr"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_hymt"));
        command
            .args(["batch", "--dry-run", "--lang", "zh"])
            .arg(&source_dir);
        if let Some(value) = override_value {
            command.args(["--concurrency", value]);
        }
        let mut child = command
            .env("HOME", &home.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
        if status.is_err() {
            child.start_kill().unwrap();
            tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .expect("timed-out batch child must be reaped")
                .unwrap();
        }
        results.push((
            override_value,
            expected_seconds,
            status,
            std::fs::read_to_string(stderr_path).unwrap(),
        ));
    }
    provider.abort();
    assert!(!provider.await.is_err_and(|error| error.is_panic()));
    for (override_value, expected_seconds, status, stderr) in results {
        assert!(status.unwrap().unwrap().success(), "{stderr}");
        assert!(
            stderr.contains(&format!("{} missing)", plan.segment_count())),
            "{stderr}"
        );
        assert!(
            stderr.contains(&format!("Estimated time: ~{expected_seconds}s")),
            "override={override_value:?}: {stderr}"
        );
    }
    assert!(!source_dir.join("input.zh-cn.md").exists());
    assert_eq!(history.fetch_recent(None).unwrap().len(), 3);
}
