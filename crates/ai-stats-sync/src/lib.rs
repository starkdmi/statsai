//! Sync sink interfaces for `ai-stats`.

use ai_stats_core::{SyncAck, SyncBatch, SYNC_ACK_SCHEMA_VERSION};
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

pub trait SyncSink {
    fn name(&self) -> &'static str;
    fn send(&self, batch: &SyncBatch) -> Result<()>;
}

pub struct StdoutSink;

impl SyncSink for StdoutSink {
    fn name(&self) -> &'static str {
        "stdout"
    }

    fn send(&self, batch: &SyncBatch) -> Result<()> {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        serde_json::to_writer_pretty(&mut lock, batch)?;
        writeln!(lock)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct HttpSink {
    endpoint: HttpEndpoint,
    bearer_token: Option<String>,
    timeout: Duration,
}

impl HttpSink {
    /// Creates an HTTP sync sink.
    ///
    /// # Errors
    ///
    /// Returns an error when `endpoint` is not an `http://` URL with a host.
    pub fn new(endpoint: impl AsRef<str>, bearer_token: Option<String>) -> Result<Self> {
        Ok(Self {
            endpoint: HttpEndpoint::parse(endpoint.as_ref())?,
            bearer_token,
            timeout: Duration::from_secs(30),
        })
    }

    /// Sends a batch and returns the server acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint rejects the batch, the connection fails,
    /// or the response is not a supported sync acknowledgement.
    pub fn send_with_ack(&self, batch: &SyncBatch) -> Result<SyncAck> {
        let response = self.send_request(batch)?;
        let status = parse_http_status(&response)?;
        if !(200..300).contains(&status) {
            bail!(
                "sync endpoint returned HTTP {}: {}",
                status,
                response_body_snippet(&response)
            );
        }
        let ack: SyncAck =
            serde_json::from_str(response_body(&response)).context("parse sync ack")?;
        if ack.schema_version != SYNC_ACK_SCHEMA_VERSION {
            bail!("unsupported sync ack schema {}", ack.schema_version);
        }
        Ok(ack)
    }

    fn send_request(&self, batch: &SyncBatch) -> Result<String> {
        let body = serde_json::to_vec(batch)?;
        let mut stream = TcpStream::connect((&*self.endpoint.host, self.endpoint.port))
            .with_context(|| format!("connect {}", self.endpoint.authority()))?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;

        let mut request = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: ai-stats/{}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.endpoint.path,
            self.endpoint.authority(),
            env!("CARGO_PKG_VERSION"),
            body.len()
        );
        if let Some(token) = self
            .bearer_token
            .as_deref()
            .filter(|token| !token.is_empty())
        {
            request.push_str("Authorization: Bearer ");
            request.push_str(token);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");

        stream.write_all(request.as_bytes())?;
        stream.write_all(&body)?;
        stream.flush()?;

        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response)
    }
}

impl SyncSink for HttpSink {
    fn name(&self) -> &'static str {
        "http"
    }

    fn send(&self, batch: &SyncBatch) -> Result<()> {
        self.send_with_ack(batch)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FileSink {
    path: PathBuf,
}

impl FileSink {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl SyncSink for FileSink {
    fn name(&self) -> &'static str {
        "file"
    }

    fn send(&self, batch: &SyncBatch) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let file = std::fs::File::create(&self.path)
            .with_context(|| format!("write {}", self.path.display()))?;
        serde_json::to_writer_pretty(file, batch)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct HttpEndpoint {
    host: String,
    port: u16,
    path: String,
}

impl HttpEndpoint {
    fn parse(value: &str) -> Result<Self> {
        let Some(rest) = value.strip_prefix("http://") else {
            bail!("http sink currently supports http:// endpoints only");
        };
        let (authority, path) = rest
            .split_once('/')
            .map(|(authority, path)| (authority, format!("/{path}")))
            .unwrap_or((rest, "/".to_string()));
        if authority.is_empty() {
            bail!("http endpoint must include a host");
        }
        let (host, port) = parse_authority(authority)?;
        Ok(Self { host, port, path })
    }

    fn authority(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn parse_authority(authority: &str) -> Result<(String, u16)> {
    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        let parsed = port
            .parse::<u16>()
            .with_context(|| format!("invalid http endpoint port {port}"))?;
        (host.to_string(), parsed)
    } else {
        (authority.to_string(), 80)
    };
    if host.is_empty() {
        bail!("http endpoint must include a host");
    }
    Ok((host, port))
}

fn parse_http_status(response: &str) -> Result<u16> {
    let status_line = response
        .lines()
        .next()
        .context("empty response from sync endpoint")?;
    let mut parts = status_line.split_whitespace();
    let _http_version = parts
        .next()
        .context("invalid response from sync endpoint")?;
    let status = parts
        .next()
        .context("invalid response from sync endpoint")?
        .parse::<u16>()
        .context("invalid status from sync endpoint")?;
    Ok(status)
}

fn response_body_snippet(response: &str) -> String {
    response_body(response).trim().chars().take(200).collect()
}

fn response_body(response: &str) -> &str {
    response.split("\r\n\r\n").nth(1).unwrap_or(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_stats_core::SyncBatch;
    use chrono::Utc;
    use std::sync::mpsc;
    use tiny_http::{Header, Method, Response, Server};

    fn empty_batch() -> SyncBatch {
        SyncBatch {
            schema_version: "sync_batch.v1".to_string(),
            batch_id: "batch_1".to_string(),
            device_id: "device".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            subscriptions: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn file_sink_writes_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("batch.json");
        let sink = FileSink::new(path.clone());
        sink.send(&empty_batch()).expect("write");

        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("batch_1"));
        assert!(content.contains("device"));
    }

    #[test]
    fn http_sink_posts_sync_batch_with_bearer_token() {
        let server = Server::http("127.0.0.1:0").expect("server");
        let endpoint = format!("http://{}/v1/sync/batches", server.server_addr());
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut request = server.recv().expect("request");
            assert_eq!(request.method(), &Method::Post);
            assert_eq!(request.url(), "/v1/sync/batches");
            let auth = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Authorization"))
                .map(|header| header.value.as_str().to_string());
            let content_type = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Content-Type"))
                .map(|header| header.value.as_str().to_string());
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).expect("body");
            tx.send((auth, content_type, body)).expect("send body");
            let response = Response::from_string(test_ack_json("batch_1"))
                .with_header(Header::from_bytes("content-type", "application/json").unwrap());
            request.respond(response).expect("respond");
        });

        let sink = HttpSink::new(endpoint, Some("token_123".to_string())).expect("sink");
        sink.send(&empty_batch()).expect("send");
        handle.join().expect("server thread");
        let (auth, content_type, body) = rx.recv().expect("request body");
        assert_eq!(auth.as_deref(), Some("Bearer token_123"));
        assert_eq!(content_type.as_deref(), Some("application/json"));
        assert!(body.contains("\"schema_version\":\"sync_batch.v1\""));
        assert!(body.contains("\"batch_id\":\"batch_1\""));
    }

    #[test]
    fn http_sink_rejects_non_success_status() {
        let server = Server::http("127.0.0.1:0").expect("server");
        let endpoint = format!("http://{}/v1/sync/batches", server.server_addr());
        let handle = std::thread::spawn(move || {
            let request = server.recv().expect("request");
            request
                .respond(Response::from_string("nope").with_status_code(500))
                .expect("respond");
        });

        let sink = HttpSink::new(endpoint, None).expect("sink");
        let error = sink.send(&empty_batch()).expect_err("500 should fail");
        handle.join().expect("server thread");
        assert!(error.to_string().contains("HTTP 500"));
    }

    #[test]
    fn http_sink_requires_http_url() {
        let error = HttpSink::new("https://example.com/v1/sync/batches", None)
            .expect_err("https is not enabled yet");
        assert!(error.to_string().contains("http://"));
    }

    fn test_ack_json(batch_id: &str) -> String {
        format!(
            r#"{{
              "schema_version":"sync_ack.v1",
              "batch_id":"{batch_id}",
              "accepted":{{"sources":0,"accounts":0,"subscriptions":0,"events":0,"summaries":0}},
              "duplicates":{{"sources":0,"accounts":0,"subscriptions":0,"events":0,"summaries":0}},
              "rejected":[]
            }}"#
        )
    }
}
