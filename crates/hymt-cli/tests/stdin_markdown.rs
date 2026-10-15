use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

struct TestHome(PathBuf);

impl TestHome {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "hymt-stdin-markdown-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            if !std::thread::panicking() {
                panic!("remove test home: {error}");
            }
        }
    }
}

#[tokio::test]
async fn redirected_stdin_retries_added_code_spans_and_rejects_exhaustion() {
    let source = "Usage: run the translator. Options: read the guide before continuing.\n\nThe property name is ordinary prose and should remain ordinary prose after translation.";
    let first = "用法：运行翻译程序。选项：继续之前阅读说明。";
    let invalid =
        "属性 `[name](https://example.test)` 是普通文字，翻译之后 `property` 仍应保持普通文字。";
    let decorated = "属性 `name` 是普通文字，翻译之后 `property` 仍应保持普通文字。";
    let recovered = "属性 name 是普通文字，翻译之后 property 仍应保持普通文字。";
    let valid = "属性名称是普通文字，翻译之后仍应保持普通文字。";
    for case in [0, 1, 2] {
        let recover = case != 1;
        let direct_recovery = case == 2;
        let home = TestHome::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let reply = if recover { valid } else { invalid }.to_owned();
        let first_reply = if direct_recovery { decorated } else { invalid };
        std::fs::create_dir_all(home.0.join(".config/hymt")).unwrap();
        std::fs::write(
            home.0.join(".config/hymt/config.toml"),
            format!(
                "[endpoint]\nurl = \"http://{address}/v1\"\nbackend = \"openai_compatible\"\nmodel = \"test-model\"\n\n[translation]\ncontext_window = 512\nmax_output_tokens = 40\nconcurrency = 1\nfirst_chunk_priority = false\ntimeout = 5\n\n[completeness]\nmax_retries = 1\n"
            ),
        )
        .unwrap();
        let provider = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0 && request.len() + count <= 8192);
                    request.extend_from_slice(&chunk[..count]);
                }
                let (content_type, body) = if request.starts_with(b"GET /v1/models ") {
                    (
                        "application/json",
                        serde_json::json!({"data": [{"id": "test-model"}]}).to_string(),
                    )
                } else {
                    assert!(request.starts_with(b"POST /v1/chat/completions "));
                    let attempt = observed.fetch_add(1, Ordering::SeqCst);
                    assert!(attempt < 3);
                    if attempt == 0 {
                        (
                            "text/event-stream",
                            format!(
                                "data: {}\n\ndata: [DONE]\n\n",
                                serde_json::json!({"choices": [{"finish_reason": null, "delta": {"content": first}}]})
                            ),
                        )
                    } else {
                        ("application/json", serde_json::json!({"choices": [{"finish_reason": "stop", "message": {"content": if attempt == 1 { first_reply } else { &reply }}}]}).to_string())
                    }
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(), body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let spawn_result = Command::new(env!("CARGO_BIN_EXE_hymt"))
            .args(["--lang", "zh", "--no-cache"])
            .env("HOME", &home.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawn_result {
            Ok(child) => child,
            Err(error) => {
                provider.abort();
                let _ = provider.await;
                panic!("spawn stdin child failed: {error}");
            }
        };
        let write_result = child
            .stdin
            .take()
            .unwrap()
            .write_all(source.as_bytes())
            .await;
        if let Err(error) = write_result {
            let _ = child.start_kill();
            let reap = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            provider.abort();
            let _ = provider.await;
            reap.expect("failed-write child must be reaped").unwrap();
            panic!("write to stdin child failed: {error}");
        }
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let stdout_reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let stderr_reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let status_result = match tokio::time::timeout(Duration::from_secs(20), child.wait()).await
        {
            Ok(result) => result,
            Err(_) => {
                let kill = child.start_kill();
                let reap = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                stdout_reader.abort();
                stderr_reader.abort();
                let _ = stdout_reader.await;
                let _ = stderr_reader.await;
                provider.abort();
                let _ = provider.await;
                kill.expect("timed-out child must be killed");
                reap.expect("timed-out child must be reaped").unwrap();
                panic!("stdin translation timed out");
            }
        };
        if let Err(error) = &status_result {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            stdout_reader.abort();
            stderr_reader.abort();
            let _ = stdout_reader.await;
            let _ = stderr_reader.await;
            provider.abort();
            let _ = provider.await;
            panic!("wait for stdin child failed: {error}");
        }
        let stdout_result = stdout_reader.await;
        let stderr_result = stderr_reader.await;
        provider.abort();
        let provider_result = provider.await;
        let status = status_result.unwrap();
        let stdout = stdout_result.unwrap().unwrap();
        let stderr = stderr_result.unwrap().unwrap();
        assert!(!provider_result.is_err_and(|error| error.is_panic()));
        assert_eq!(
            requests.load(Ordering::SeqCst),
            if direct_recovery { 2 } else { 3 },
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        if recover {
            assert!(status.success(), "{}", String::from_utf8_lossy(&stderr));
            let stdout = String::from_utf8(stdout).unwrap();
            assert!(stdout.contains(first));
            assert!(stdout.contains(if direct_recovery { recovered } else { valid }));
            assert!(!stdout.contains(invalid));
            assert!(!stdout.contains(decorated));
        } else {
            assert!(!status.success());
            assert!(stdout.is_empty());
            assert!(!home.0.join(".local/share/hymt/history.db").exists());
        }
    }
}
