use std::io;
use std::time::Duration;

use hymt_core::config::HotConfig;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub(crate) struct ConfigFixture {
    _dir: TempDir,
    pub(crate) config: HotConfig,
}

pub(crate) fn config_fixture(
    endpoint: &str,
    backend: &str,
    model: Option<&str>,
    overrides: &str,
    stream: bool,
) -> ConfigFixture {
    let dir = tempfile::tempdir().expect("create config tempdir");
    let model = model
        .map(|model| format!("model = \"{model}\"\n"))
        .unwrap_or_default();
    let contents = format!(
        "[endpoint]\nurl = \"{endpoint}\"\nbackend = \"{backend}\"\n{model}\n\
         [backend]\ntotal_context = 4096\nparallel_slots = 1\n\n\
         [translation]\nmax_output_tokens = 37\nconcurrency = 2\nstream = {stream}\ntimeout = 5\n\n\
         [completeness]\nmax_retries = 1\n\n{overrides}"
    );
    let path = dir.path().join("config.toml");
    std::fs::write(&path, contents).expect("write config fixture");
    let config = HotConfig::from_path(&path).expect("load config fixture");
    ConfigFixture { _dir: dir, config }
}

pub(crate) struct MockReply {
    pub(crate) content_type: &'static str,
    pub(crate) chunks: Vec<Vec<u8>>,
    pub(crate) advertised_length: Option<usize>,
    pub(crate) chunk_delay: Duration,
}

impl MockReply {
    pub(crate) fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            content_type: "application/json",
            chunks: vec![body.into()],
            advertised_length: None,
            chunk_delay: Duration::ZERO,
        }
    }

    pub(crate) fn sse(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            content_type: "text/event-stream",
            chunks,
            advertised_length: None,
            chunk_delay: Duration::ZERO,
        }
    }

    fn body_len(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }
}

pub(crate) struct MockServer {
    pub(crate) endpoint: String,
    request: tokio::task::JoinHandle<io::Result<String>>,
}

impl MockServer {
    pub(crate) async fn received_request(self) -> String {
        self.request
            .await
            .expect("mock server task must complete")
            .expect("mock server I/O must succeed")
    }
}

pub(crate) async fn start_one_shot_server(reply: MockReply) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let address = listener.local_addr().expect("mock server address");
    let request = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let request = read_http_request(&mut socket).await?;
        let content_length = reply.advertised_length.unwrap_or_else(|| reply.body_len());
        let headers = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {}\r\ncontent-length: {content_length}\r\nconnection: close\r\n\r\n",
            reply.content_type
        );
        socket.write_all(headers.as_bytes()).await?;
        for chunk in reply.chunks {
            socket.write_all(&chunk).await?;
            socket.flush().await?;
            if !reply.chunk_delay.is_zero() {
                tokio::time::sleep(reply.chunk_delay).await;
            }
        }
        socket.shutdown().await?;
        Ok(request)
    });

    MockServer {
        endpoint: format!("http://{address}/v1"),
        request,
    }
}

async fn read_http_request(socket: &mut TcpStream) -> io::Result<String> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let count = socket.read(&mut chunk).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "HTTP request ended before headers",
            ));
        }
        request.extend_from_slice(&chunk[..count]);
        if let Some(pos) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = std::str::from_utf8(&request[..header_end]).map_err(io::Error::other)?;
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .or_else(|| line.strip_prefix("Content-Length:"))
        })
        .map(str::trim)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request has no content length"))?
        .parse::<usize>()
        .map_err(io::Error::other)?;
    while request.len() < header_end + content_length {
        let count = socket.read(&mut chunk).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "HTTP request ended before body",
            ));
        }
        request.extend_from_slice(&chunk[..count]);
    }
    String::from_utf8(request).map_err(io::Error::other)
}

pub(crate) fn request_json(request: &str) -> serde_json::Value {
    assert!(
        request.starts_with("POST /v1/chat/completions HTTP/1.1"),
        "unexpected inference request: {request}"
    );
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("request must have headers and body");
    serde_json::from_str(body).expect("request body must be JSON")
}
