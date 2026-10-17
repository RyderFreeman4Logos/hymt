use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

struct TestHome(PathBuf);

impl Drop for TestHome {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove CLI cache-bypass test home");
    }
}

#[tokio::test]
async fn cli_explicit_no_cache_bypasses_inference_cache_without_changing_default() {
    for flag in [None, Some("--no-cache"), Some("--disable-cache")] {
        let home = TestHome(std::env::temp_dir().join(format!(
            "hymt-cli-cache-bypass-{}-{:?}",
            std::process::id(),
            flag
        )));
        let config = home.0.join(".config/hymt/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        std::fs::write(
            config,
            format!("[endpoint]\nurl = \"http://{address}/v1\"\nbackend = \"openai_compatible\"\nmodel = \"test-model\"\n[translation]\ntimeout = 2\n"),
        )
        .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&requests);
        let provider = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 1024];
                let header_end = loop {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0 && bytes.len() + count <= 32768);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|value| value.parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0 && bytes.len() + count <= 32768);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let body = if headers.starts_with("GET /v1/models ") {
                    serde_json::json!({"data": [{"id": "test-model"}]}).to_string()
                } else {
                    assert!(headers.starts_with("POST /v1/chat/completions "));
                    observed.lock().unwrap().push(headers.to_ascii_lowercase());
                    serde_json::json!({"choices": [{"finish_reason": "stop", "message": {"content": "这是用于验证缓存策略的完整合成翻译。"}}]}).to_string()
                };
                let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_hymt"));
        command.args(["-l", "zh", "Synthetic cache policy validation."]);
        if flag != Some("--disable-cache") {
            command.arg("--no-stream");
        }
        if let Some(flag) = flag {
            command.arg(flag);
        }
        let stdout = std::fs::File::create_new(home.0.join("stdout")).unwrap();
        let stderr_path = home.0.join("stderr");
        let stderr = std::fs::File::create_new(&stderr_path).unwrap();
        let spawn = command
            .env("HOME", &home.0)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawn {
            Ok(child) => child,
            Err(error) => {
                provider.abort();
                let _ = provider.await;
                panic!("spawn cache-bypass child failed: {error}");
            }
        };
        let pid = child.id();
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        let reap = if status.is_err() {
            let _ = child.start_kill();
            Some(tokio::time::timeout(Duration::from_secs(5), child.wait()).await)
        } else {
            None
        };
        let request_count = requests.lock().unwrap().len();
        provider.abort();
        let provider_result = provider.await;
        let mut stderr = Vec::new();
        std::fs::File::open(stderr_path)
            .unwrap()
            .take(8192)
            .read_to_end(&mut stderr)
            .unwrap();
        if let Some(reap) = reap {
            reap.expect("timed-out cache-bypass child must be reaped")
                .unwrap();
        }
        assert!(
            !provider_result
                .as_ref()
                .is_err_and(|error| error.is_panic()),
            "flag={flag:?} pid={pid:?} requests={request_count} provider={provider_result:?}"
        );
        let status = status.unwrap_or_else(|error| {
            panic!(
                "bounded CLI cache fixture: {error}; flag={flag:?} pid={pid:?} \
                 requests={request_count} provider={provider_result:?}; stderr={}",
                String::from_utf8_lossy(&stderr)
            )
        });
        assert!(
            status.unwrap().success(),
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        for name in ["x-hymt-cache-bypass", "x-bf-eh-x-hymt-cache-bypass"] {
            assert_eq!(
                requests[0].lines().any(|line| line == format!("{name}: 1")),
                flag.is_some(),
                "{flag:?}: {name}"
            );
        }
    }
}
