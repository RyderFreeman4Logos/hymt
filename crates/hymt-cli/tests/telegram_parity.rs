#![cfg(feature = "telegram")]

#[path = "../src/telegram/mod.rs"]
mod telegram;

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hymt_cache::history::HistoryDB;
use hymt_client::TranslationClient;
use hymt_core::config::HotConfig;
use serde_json::{json, Value};
use telegram::poll::{handle_update, TelegramDocumentApi, TelegramMessageApi, Update};

const SOURCE: &str =
    "FIRSTOWNER café apple.\r\n\r\n```txt\r\nraw 雪 & <tag>\r\n```\r\n\r\nSECONDOWNER omega.";
const LONG_SOURCE: &str = "LONGOWNER needs a document reply.";
const ERROR_SOURCE: &str = "ERROROWNER triggers a transport failure.";

struct Fixture {
    dir: PathBuf,
    server: Child,
}

struct ChildGuard {
    child: Child,
    complete: bool,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "hymt-parity-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let server = Command::new("python3")
            .arg(manifest.join("tests/support/parity_cache.py"))
            .arg(manifest.join("../../scripts/hymt-inference-cache.py"))
            .arg(&dir)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let mut fixture = Self { dir, server };
        let deadline = Instant::now() + Duration::from_secs(15);
        while !fixture.dir.join("port").exists() {
            assert!(
                fixture.server.try_wait().unwrap().is_none(),
                "fixture exited before readiness"
            );
            assert!(Instant::now() < deadline, "fixture did not become ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        fixture
    }

    fn configure(&self) -> PathBuf {
        let home = self.dir.join("home");
        let config_dir = home.join(".config/hymt");
        std::fs::create_dir_all(&config_dir).unwrap();
        let port = std::fs::read_to_string(self.dir.join("port")).unwrap();
        let config = format!(
            "[endpoint]\nurl=\"http://127.0.0.1:{port}/v1\"\nbackend=\"llama_cpp\"\nmodel=\"Hy-MT2-7B\"\nprofile=\"generic\"\n\
             [backend]\ntotal_context=24576\nparallel_slots=3\nper_request_context=8192\n\
             [translation]\nmax_output_tokens=1024\nmax_source_tokens_per_segment=384\nconcurrency=1\ntimeout=10\nmax_retries=0\n\
             [completeness]\nmax_retries=0\n[cache]\nenabled=false\n\
             [telegram]\naccept_documents=true\nowners=[42]\nmode=\"owners\"\n"
        );
        std::fs::write(config_dir.join("config.toml"), config).unwrap();
        home
    }

    fn records(&self, name: &str) -> Vec<Value> {
        std::fs::read_to_string(self.dir.join(name))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !matches!(self.server.try_wait(), Ok(Some(_))) {
            let _ = self.server.kill();
            let _ = self.server.wait();
        }
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            if !std::thread::panicking() {
                panic!("remove parity fixture: {error}");
            }
        }
    }
}

#[derive(Clone)]
struct FakeBot {
    bytes: Arc<Vec<u8>>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

impl FakeBot {
    fn new(source: &str) -> Self {
        Self {
            bytes: Arc::new(source.as_bytes().to_vec()),
            calls: Arc::default(),
        }
    }

    fn replies(&self) -> Vec<(String, String)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _)| kind == "message" || kind == "document")
            .cloned()
            .collect()
    }
}

impl TelegramMessageApi for FakeBot {
    async fn send_chat_action(&self, _: i64, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn send_message(&self, _: i64, text: &str) -> anyhow::Result<i64> {
        self.calls
            .lock()
            .unwrap()
            .push(("message".into(), text.into()));
        Ok(1)
    }
    async fn edit_message(&self, _: i64, _: i64, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

impl TelegramDocumentApi for FakeBot {
    async fn download_document(&self, file_id: &str, _: u64) -> anyhow::Result<Vec<u8>> {
        assert_eq!(file_id, "fixture-id");
        self.calls
            .lock()
            .unwrap()
            .push(("download".into(), file_id.into()));
        Ok(self.bytes.as_ref().clone())
    }
    async fn send_document(&self, _: i64, name: &str, text: &str) -> anyhow::Result<()> {
        assert!(name == "source.md" || name == "source.txt");
        self.calls
            .lock()
            .unwrap()
            .push(("document".into(), text.into()));
        Ok(())
    }
}

async fn bot(home: &Path, source: &str, filename: &str) -> (anyhow::Result<()>, FakeBot) {
    let config = HotConfig::from_path(home.join(".config/hymt/config.toml")).unwrap();
    let client = TranslationClient::new(config.clone()).unwrap();
    let segmenter = hymt_segment::create_segmenter(config.model_profile().unwrap()).unwrap();
    let history = HistoryDB::new(home.join("bot-history.db"));
    let api = FakeBot::new(source);
    let update: Update = serde_json::from_value(json!({
        "update_id": 1, "message": {"chat": {"id": 42, "type": "private"},
        "document": {"file_id": "fixture-id", "file_name": filename, "file_size": source.len()}}
    }))
    .unwrap();
    let result = handle_update(&config, &api, &client, &segmenter, &history, false, update).await;
    (result, api)
}

fn cli(home: &Path, source: &str, file_output: bool) -> std::process::Output {
    let source_path = home.join("source.md");
    std::fs::write(&source_path, source).unwrap();
    let binary = std::env::var_os("HYMT_PARITY_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_hymt").into());
    let mut command = Command::new(binary);
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .args(["--no-progress", "-y"]);
    if file_output {
        command.arg("-o").arg(home.join("output.md"));
    }
    let stdout = home.join("cli-stdout");
    let stderr = home.join("cli-stderr");
    let child = command
        .arg(source_path)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let mut child = ChildGuard {
        child,
        complete: false,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.child.try_wait().unwrap() {
            child.complete = true;
            break status;
        }
        assert!(Instant::now() < deadline, "CLI exceeded its deadline");
        std::thread::sleep(Duration::from_millis(20));
    };
    std::process::Output {
        status,
        stdout: std::fs::read(stdout).unwrap(),
        stderr: std::fs::read(stderr).unwrap(),
    }
}

fn request_batch(fixture: &Fixture, start: usize, expected_cache: &str) -> Vec<String> {
    let records = fixture.records("requests.jsonl");
    let batch = &records[start..];
    assert_eq!(batch.len(), 2, "unexpected requests: {records:?}");
    let mut bodies: Vec<String> = batch
        .iter()
        .map(|record| {
            assert_eq!(record["status"], 200);
            assert_eq!(record["cache"], expected_cache);
            record["body"].as_str().unwrap().to_owned()
        })
        .collect();
    // Requests may reach the server in either order. Compare complete bodies in
    // source order and verify the reconstructed input separately below.
    bodies.sort_by_key(|body| {
        let payload: Value = serde_json::from_str(body).unwrap();
        let prompt = payload["messages"][0]["content"].as_str().unwrap();
        let part = prompt.split_once("\n\n").unwrap().1;
        SOURCE
            .find(part)
            .expect("request source missing from input")
    });
    bodies
}

#[tokio::test]
async fn cli_and_telegram_document_share_requests_cache_and_output() {
    let _ = telegram::run_telegram_bot;
    for bot_first in [true, false] {
        let fixture = Fixture::new();
        let home = fixture.configure();
        let (bot_result, bot_api, file_output, bodies);
        if bot_first {
            (bot_result, bot_api) = bot(&home, SOURCE, "source.md").await;
            assert!(bot_result.is_ok(), "{bot_result:?}");
            bodies = request_batch(&fixture, 0, "MISS");
            file_output = cli(&home, SOURCE, true);
            assert!(
                file_output.status.success(),
                "{}",
                String::from_utf8_lossy(&file_output.stderr)
            );
            assert_eq!(request_batch(&fixture, 2, "HIT"), bodies);
        } else {
            file_output = cli(&home, SOURCE, true);
            assert!(
                file_output.status.success(),
                "{}",
                String::from_utf8_lossy(&file_output.stderr)
            );
            bodies = request_batch(&fixture, 0, "MISS");
            (bot_result, bot_api) = bot(&home, SOURCE, "source.txt").await;
            assert!(bot_result.is_ok(), "{bot_result:?}");
            assert_eq!(request_batch(&fixture, 2, "HIT"), bodies);
        }
        assert_eq!(fixture.records("upstream.jsonl").len(), 2);
        let source_parts: Vec<String> = bodies
            .iter()
            .map(|body| {
                let payload: Value = serde_json::from_str(body).unwrap();
                assert_eq!(payload["stream"], true);
                payload["messages"][0]["content"]
                    .as_str()
                    .unwrap()
                    .split_once("\n\n")
                    .unwrap()
                    .1
                    .to_owned()
            })
            .collect();
        assert_eq!(
            source_parts,
            ["FIRSTOWNER café apple.", "SECONDOWNER omega."]
        );
        assert_eq!(
            format!(
                "{}\r\n\r\n```txt\r\nraw 雪 & <tag>\r\n```\r\n\r\n{}",
                source_parts[0], source_parts[1]
            ),
            SOURCE
        );
        let file = std::fs::read(home.join("output.md")).unwrap();
        assert_eq!(
            file,
            "FIRSTRESULT\r\n\r\n```txt\r\nraw 雪 & <tag>\r\n```\r\n\r\nSECONDRESULT".as_bytes()
        );
        let replies = bot_api.replies();
        assert!(bot_api
            .calls
            .lock()
            .unwrap()
            .contains(&("download".into(), "fixture-id".into())));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].0, "message");
        assert_eq!(replies[0].1.as_bytes(), file);
        assert!(String::from_utf8_lossy(&file).contains("```txt\r\nraw 雪 & <tag>\r\n```"));
        let stdout = cli(&home, SOURCE, false);
        assert!(
            stdout.status.success(),
            "{}",
            String::from_utf8_lossy(&stdout.stderr)
        );
        assert_eq!(request_batch(&fixture, 4, "HIT"), bodies);
        assert_eq!(stdout.stdout, [file.as_slice(), b"\n"].concat());
        assert_eq!(fixture.records("upstream.jsonl").len(), 2);
    }
}

#[tokio::test]
async fn telegram_upload_and_runtime_failure_follow_real_handler() {
    let fixture = Fixture::new();
    let home = fixture.configure();
    let (result, api) = bot(&home, LONG_SOURCE, "source.md").await;
    assert!(result.is_ok(), "{result:?}");
    let requests = fixture.records("requests.jsonl");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["cache"], "MISS");
    let cli_output = cli(&home, LONG_SOURCE, true);
    assert!(
        cli_output.status.success(),
        "{}",
        String::from_utf8_lossy(&cli_output.stderr)
    );
    let file = std::fs::read(home.join("output.md")).unwrap();
    assert_eq!(
        api.replies(),
        vec![("document".into(), String::from_utf8(file).unwrap())]
    );
    let requests = fixture.records("requests.jsonl");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["cache"], "HIT");
    assert_eq!(requests[0]["body"], requests[1]["body"]);
    assert_eq!(fixture.records("upstream.jsonl").len(), 1);
    let (result, api) = bot(&home, ERROR_SOURCE, "source.txt").await;
    assert!(result.is_err());
    assert!(api.replies().is_empty());
    let cli_error = cli(&home, ERROR_SOURCE, false);
    assert!(!cli_error.status.success());
    assert!(cli_error.stdout.is_empty());
}
