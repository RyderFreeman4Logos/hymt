use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

#[derive(Debug, Default)]
struct ProviderProgress {
    phase: &'static str,
    received_body: usize,
    declared_body: Option<usize>,
}

#[derive(Default)]
struct OutputCapture {
    stdout_bytes: usize,
    stderr_tail: Vec<u8>,
}

async fn read_fixture_request(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    progress: &Mutex<ProviderProgress>,
) -> std::io::Result<Vec<u8>> {
    use std::io::{Error, ErrorKind};
    const LIMIT: usize = 8192;
    let invalid = |message| Error::new(ErrorKind::InvalidData, message);
    *progress.lock().unwrap() = ProviderProgress {
        phase: "header",
        ..ProviderProgress::default()
    };
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end + 4;
        }
        let remaining = LIMIT - request.len();
        if remaining == 0 {
            return Err(invalid("fixture header exceeds limit"));
        }
        let count = reader.read(&mut chunk[..remaining.min(1024)]).await?;
        if count == 0 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "incomplete fixture header",
            ));
        }
        request.extend_from_slice(&chunk[..count]);
    };
    let header = std::str::from_utf8(&request[..header_end - 4])
        .map_err(|_| invalid("invalid fixture header encoding"))?;
    let mut lines = header.split("\r\n");
    let post = match lines.next() {
        Some("GET /v1/models HTTP/1.1") => false,
        Some("POST /v1/chat/completions HTTP/1.1") => true,
        _ => return Err(invalid("unsupported fixture request")),
    };
    let mut length = None;
    let mut json = false;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("malformed fixture header"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(invalid("unsupported fixture framing"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some()
                || value.is_empty()
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(invalid("invalid fixture content length"));
            }
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| invalid("invalid fixture content length"))?,
            );
        }
        if name.eq_ignore_ascii_case("content-type") {
            json = value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"));
        }
    }
    let declared = match (post, length) {
        (true, Some(length)) if json && length > 0 => length,
        (false, None | Some(0)) => 0,
        _ => return Err(invalid("unsupported fixture body")),
    };
    {
        let mut state = progress.lock().unwrap();
        state.phase = "body";
        state.declared_body = Some(declared);
        state.received_body = request.len() - header_end;
    }
    let total = header_end
        .checked_add(declared)
        .filter(|total| *total <= LIMIT)
        .ok_or_else(|| invalid("fixture request exceeds limit"))?;
    if request.len() > total {
        return Err(invalid("unexpected fixture body bytes"));
    }
    while request.len() < total {
        let count = reader
            .read(&mut chunk[..(total - request.len()).min(1024)])
            .await?;
        if count == 0 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "incomplete fixture body",
            ));
        }
        request.extend_from_slice(&chunk[..count]);
        progress.lock().unwrap().received_body = request.len() - header_end;
    }
    Ok(request)
}

#[tokio::test]
async fn fixture_reader_rejects_truncated_declared_body() {
    let mut reader = &b"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 4\r\nContent-Type: application/json\r\n\r\n"[..];
    let progress = Mutex::default();
    let error = read_fixture_request(&mut reader, &progress)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    let progress = progress.lock().unwrap();
    assert_eq!(
        (progress.received_body, progress.declared_body),
        (0, Some(4))
    );
}

#[tokio::test]
async fn fixture_reader_consumes_fragmented_declared_body() {
    let headers = &b"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 4\r\nContent-Type: application/json\r\n\r\n"[..];
    let mut reader = headers.chain(&b"null"[..]);
    let progress = Mutex::default();
    let request = read_fixture_request(&mut reader, &progress).await.unwrap();
    assert_eq!(&request[headers.len()..], b"null");
    let progress = progress.lock().unwrap();
    assert_eq!(
        (progress.received_body, progress.declared_body),
        (4, Some(4))
    );
}

#[tokio::test]
async fn fixture_reader_rejects_malformed_and_excessive_framing() {
    for fields in [
        "Content-Length: nope\r\n",
        "Content-Length: 4\r\nContent-Length: 4\r\n",
        "Content-Length: 4\r\nTransfer-Encoding: chunked\r\n",
        "Content-Length: 8192\r\n",
        "Content-Length: 999999999999999999999999999999\r\n",
    ] {
        let input = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nContent-Type: application/json\r\n{fields}\r\n"
        );
        let error = read_fixture_request(&mut input.as_bytes(), &Mutex::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
    let mut get = &b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n"[..];
    assert!(read_fixture_request(&mut get, &Mutex::default())
        .await
        .is_ok());
}

#[tokio::test]
async fn redirected_stdin_publishes_model_content_and_handles_transport_failures() {
    let source = "Usage: run the translator. Options: read the guide before continuing.\n\nThe property name is ordinary prose and should remain ordinary prose after translation.";
    let first = "用法：运行翻译程序。选项：继续之前阅读说明。";
    let changed =
        "属性 `[name](https://example.test)` 是普通文字，翻译之后 `property` 仍应保持普通文字。";
    let decorated = "属性 `name` 是普通文字，翻译之后 `property` 仍应保持普通文字。";
    let valid = "属性名称是普通文字，翻译之后仍应保持普通文字。";
    for case in 0..6 {
        let completed = matches!(case, 2 | 4 | 5);
        let strict = matches!(case, 3..=5);
        let home = TestHome::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let reply = if case == 0 { valid } else { changed }.to_owned();
        let first_reply = match case {
            2 => decorated,
            5 => "",
            _ => changed,
        };
        std::fs::create_dir_all(home.0.join(".config/hymt")).unwrap();
        std::fs::write(
            home.0.join(".config/hymt/config.toml"),
            format!(
                "[endpoint]\nurl = \"http://{address}/v1\"\nbackend = \"openai_compatible\"\nmodel = \"test-model\"\n\n[translation]\ncontext_window = 512\nmax_output_tokens = 40\nconcurrency = 1\nfirst_chunk_priority = false\ntimeout = 5\n\n[completeness]\nmax_retries = 1\n{}", if strict { "warn_only = false\n" } else { "" }
            ),
        )
        .unwrap();
        let progress = Arc::new(Mutex::new(ProviderProgress::default()));
        let provider_progress = Arc::clone(&progress);
        let provider: tokio::task::JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
            loop {
                provider_progress.lock().unwrap().phase = "accept";
                let (mut socket, _) = listener.accept().await?;
                let request = read_fixture_request(&mut socket, &provider_progress).await?;
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
                                serde_json::json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"content": first}}]})
                            ),
                        )
                    } else {
                        let reason = if completed || (case == 0 && attempt == 2) {
                            "stop"
                        } else {
                            "length"
                        };
                        ("application/json", serde_json::json!({"choices": [{"finish_reason": reason, "message": {"content": if attempt == 1 { first_reply } else { &reply }}}]}).to_string())
                    }
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(), body
                );
                provider_progress.lock().unwrap().phase = "write";
                socket.write_all(response.as_bytes()).await?;
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
        let pid = child.id();
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
        let capture = Arc::new(Mutex::new(OutputCapture::default()));
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let stdout_capture = Arc::clone(&capture);
        let stdout_reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let count = stdout.read(&mut chunk).await?;
                if count == 0 {
                    return Ok::<_, std::io::Error>(bytes);
                }
                stdout_capture.lock().unwrap().stdout_bytes += count;
                bytes.extend_from_slice(&chunk[..count]);
            }
        });
        let stderr_capture = Arc::clone(&capture);
        let stderr_reader = tokio::spawn(async move {
            let mut chunk = [0; 1024];
            loop {
                let count = stderr.read(&mut chunk).await?;
                if count == 0 {
                    return Ok::<_, std::io::Error>(());
                }
                let mut capture = stderr_capture.lock().unwrap();
                capture.stderr_tail.extend_from_slice(&chunk[..count]);
                let excess = capture.stderr_tail.len().saturating_sub(1024);
                capture.stderr_tail.drain(..excess);
            }
        });
        let wait_started = Instant::now();
        let diagnostic = |provider: &Result<std::io::Result<()>, tokio::task::JoinError>,
                          wait_elapsed: Duration| {
            let provider = match provider {
                Ok(Ok(())) => "completed".to_owned(),
                Ok(Err(error)) => format!("error kind={:?} message={error}", error.kind()),
                Err(error) if error.is_cancelled() => "intentionally cancelled".to_owned(),
                Err(error) => format!("panic: {error}"),
            };
            let capture = capture.lock().unwrap();
            format!(
                "stdin fixture case={case} pid={pid:?} wait_elapsed={:?} requests={} progress={:?} provider={provider} stdout_bytes={} stderr_tail={:?}",
                wait_elapsed, requests.load(Ordering::SeqCst), progress.lock().unwrap(),
                capture.stdout_bytes, String::from_utf8_lossy(&capture.stderr_tail),
            )
        };
        let status_result = match tokio::time::timeout(Duration::from_secs(20), child.wait()).await
        {
            Ok(result) => result,
            Err(_) => {
                let wait_elapsed = wait_started.elapsed();
                let kill = child.start_kill();
                let reap = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                stdout_reader.abort();
                stderr_reader.abort();
                let _ = stdout_reader.await;
                let _ = stderr_reader.await;
                provider.abort();
                let provider_result = provider.await;
                eprintln!("{}", diagnostic(&provider_result, wait_elapsed));
                kill.expect("timed-out child must be killed");
                reap.expect("timed-out child must be reaped").unwrap();
                panic!("stdin translation timed out");
            }
        };
        let wait_elapsed = wait_started.elapsed();
        if let Err(error) = &status_result {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            stdout_reader.abort();
            stderr_reader.abort();
            let _ = stdout_reader.await;
            let _ = stderr_reader.await;
            provider.abort();
            let provider_result = provider.await;
            eprintln!("{}", diagnostic(&provider_result, wait_elapsed));
            panic!("wait for stdin child failed: {error}");
        }
        let stdout_result = stdout_reader.await;
        let stderr_result = stderr_reader.await;
        provider.abort();
        let provider_result = provider.await;
        let diagnostic = diagnostic(&provider_result, wait_elapsed);
        let status = status_result.unwrap();
        let stdout = stdout_result.expect(&diagnostic).expect(&diagnostic);
        stderr_result.expect(&diagnostic).expect(&diagnostic);
        assert!(
            matches!(provider_result, Ok(Ok(())))
                || provider_result
                    .as_ref()
                    .is_err_and(|error| error.is_cancelled()),
            "{diagnostic}"
        );
        assert_eq!(
            requests.load(Ordering::SeqCst),
            if completed { 2 } else { 3 },
            "{diagnostic}"
        );
        if completed || case == 0 {
            assert!(status.success(), "{diagnostic}");
            let stdout = String::from_utf8(stdout).unwrap();
            let expected = if completed { first_reply } else { valid };
            let mut expected = format!("{first}\n\n{expected}");
            if !expected.ends_with('\n') {
                expected.push('\n');
            }
            assert_eq!(stdout, expected, "{diagnostic}");
        } else if !strict {
            assert!(status.success(), "{diagnostic}");
            let stdout = String::from_utf8(stdout).unwrap();
            assert!(
                stdout.contains("Untranslated original: paragraph 2"),
                "{diagnostic}"
            );
            assert!(stdout.contains(first), "{diagnostic}");
            assert!(
                stdout
                    .strip_suffix('\n')
                    .unwrap_or(&stdout)
                    .ends_with(source.split_once("\n\n").unwrap().1),
                "{diagnostic}"
            );
            assert!(!stdout.contains(changed), "{diagnostic}");
        } else {
            assert!(!status.success(), "{diagnostic}");
            assert!(stdout.is_empty(), "{diagnostic}");
            assert!(
                !home.0.join(".local/share/hymt/history.db").exists(),
                "{diagnostic}"
            );
        }
    }
}
