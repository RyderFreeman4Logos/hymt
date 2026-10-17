use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

const SOURCE: &str = "This report explains reliable translation history and cache behavior.";
const REPLY: &str = "这份报告说明可靠的翻译历史记录和缓存行为为什么重要。";

struct Fixture {
    root: PathBuf,
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    provider: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(props: Value) -> Self {
        let root = std::env::temp_dir().join(format!(
            "hymt-offline-preflight-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".config/hymt")).unwrap();
        std::fs::create_dir(root.join("source")).unwrap();
        std::fs::write(root.join("source/input.txt"), SOURCE).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&requests);
        let provider = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                let header_end = loop {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0 && request.len() + count <= 32768);
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|value| value.parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while request.len() < header_end + length {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0 && request.len() + count <= 32768);
                    request.extend_from_slice(&buffer[..count]);
                }
                let body = if headers.starts_with("GET /props ") {
                    props.clone()
                } else {
                    assert!(headers.starts_with("POST /v1/chat/completions "));
                    json!({"choices":[{"finish_reason":"stop","message":{"content":REPLY}}]})
                }
                .to_string();
                observed
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(request).unwrap());
                let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self {
            root,
            endpoint,
            requests,
            provider,
        }
    }

    fn configure(&self, strict: bool, max_output: u32) {
        std::fs::write(self.root.join(".config/hymt/config.toml"), format!(
            "[endpoint]\nurl = \"{}\"\nbackend = \"llama_cpp\"\nmodel = \"test-model\"\n\
             [backend]\ntotal_context = 4096\nparallel_slots = 1\n\
             [translation]\nmax_output_tokens = {max_output}\nstrict_backend_preflight = {strict}\ntimeout = 2\n\
             [cache]\nenabled = true\n", self.endpoint)).unwrap();
    }

    async fn run(&self, label: &str, args: &[&str]) -> (bool, String, String) {
        let stdout = self.root.join(format!("{label}.stdout"));
        let stderr = self.root.join(format!("{label}.stderr"));
        let mut child = Command::new(env!("CARGO_BIN_EXE_hymt"))
            .args(args)
            .env("HOME", &self.root)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create_new(&stdout).unwrap())
            .stderr(std::fs::File::create_new(&stderr).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(15), child.wait()).await;
        if status.is_err() {
            child.start_kill().unwrap();
            tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .expect("timed out child must be reaped")
                .unwrap();
        }
        let stderr = std::fs::read_to_string(stderr).unwrap();
        let success = status
            .unwrap_or_else(|error| panic!("{error}: {stderr}"))
            .unwrap()
            .success();
        (success, std::fs::read_to_string(stdout).unwrap(), stderr)
    }

    async fn stop(&mut self) {
        self.provider.abort();
        assert!(!(&mut self.provider)
            .await
            .is_err_and(|error| error.is_panic()));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.provider.abort();
        std::fs::remove_dir_all(&self.root).expect("remove synthetic preflight fixture");
    }
}

fn props(model: &str, cap: u32) -> Value {
    json!({"model":model,"n_ctx":4096,"n_ctx_per_seq":4096,
        "default_generation_settings":{"params":{"n_predict":cap,
            "temperature":0.7,"top_p":0.6,"top_k":20,"min_p":0.0,
            "repeat_penalty":1.05,"repeat_last_n":64}}})
}

#[tokio::test]
async fn batch_miss_preflights_before_runtime_dependent_plan() {
    let mut fixture = Fixture::new(props("test-model", 128)).await;
    fixture.configure(true, 4096);
    let (success, _, stderr) = fixture
        .run("batch", &["batch", "--lang", "zh", "source"])
        .await;
    fixture.stop().await;
    assert!(success, "{stderr}");
    assert!(
        stderr.contains("Batch translation complete: 1 files"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("source/input.zh-cn.txt")).unwrap(),
        REPLY
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "one preflight and one translation");
    assert!(requests[0].starts_with("GET /props "));
    let payload: Value =
        serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(payload["max_tokens"], 128);
}

#[tokio::test]
async fn batch_miss_cannot_skip_strict_offline_or_failed_live_plan() {
    for offline in [true, false] {
        let mut fixture = Fixture::new(props("test-model", 4096)).await;
        fixture.configure(true, 4096);
        if offline {
            fixture.stop().await;
        }
        let (success, stdout, stderr) = fixture
            .run("batch", &["batch", "--lang", "zh", "source"])
            .await;
        if !offline {
            fixture.stop().await;
        }
        assert!(
            !success,
            "failed batch must be nonzero; offline={offline}: {stderr}"
        );
        assert!(stdout.is_empty());
        let expected = if offline {
            "strict backend preflight"
        } else {
            "too small for complete final request"
        };
        assert!(stderr.contains(expected), "{stderr}");
        assert!(!fixture.root.join("source/input.zh-cn.txt").exists());
        assert_eq!(
            fixture.requests.lock().unwrap().len(),
            usize::from(!offline)
        );
    }
}

#[tokio::test]
async fn deferred_non_strict_preflight_delivers_warnings_once() {
    for (metadata, warning) in [
        (json!([]), "runtime preflight is unverified"),
        (props("different-model", 128), "profile/model mismatch"),
    ] {
        for batch in [false, true] {
            let mut fixture = Fixture::new(metadata.clone()).await;
            fixture.configure(false, 128);
            let args = if batch {
                vec!["batch", "--lang", "zh", "source"]
            } else {
                vec!["--no-stream", "--lang", "zh", SOURCE]
            };
            let (success, _, stderr) = fixture.run("warning", &args).await;
            fixture.stop().await;
            assert!(success, "{stderr}");
            assert_eq!(
                stderr.matches(warning).count(),
                1,
                "batch={batch}: {stderr}"
            );
            assert_eq!(fixture.requests.lock().unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn complete_replay_does_not_probe_or_emit_live_warnings() {
    let mut fixture = Fixture::new(props("test-model", 128)).await;
    fixture.configure(true, 128);
    let args = ["--no-stream", "--lang", "zh", SOURCE];
    let (success, stdout, stderr) = fixture.run("warm", &args).await;
    assert!(success, "{stderr}");
    assert_eq!(stdout.trim(), REPLY);
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
    let (success, stdout, stderr) = fixture.run("replay", &args).await;
    fixture.stop().await;
    assert!(success, "{stderr}");
    assert_eq!(stdout.trim(), REPLY);
    assert!(stderr.contains("Application cache replay:"), "{stderr}");
    assert!(!stderr.contains("runtime preflight"), "{stderr}");
    assert!(!stderr.contains("mismatch"), "{stderr}");
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        2,
        "replay must not touch the live backend"
    );
}
