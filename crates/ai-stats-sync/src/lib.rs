//! Sync sink interfaces for `ai-stats`.

use ai_stats_core::{
    SyncAck, SyncBatch, SyncEntityCounts, SYNC_ACK_SCHEMA_VERSION, SYNC_BATCH_SCHEMA_VERSION,
};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::thread;
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
pub struct FirestoreSink {
    project_id: String,
    uid: String,
    bearer_token: String,
}

#[derive(Debug, Clone)]
pub struct FirestoreSendOptions {
    pub commit_chunk_size: usize,
    pub max_retries: u32,
    pub initial_backoff: Duration,
    pub progress: bool,
}

impl Default for FirestoreSendOptions {
    fn default() -> Self {
        Self {
            commit_chunk_size: 450,
            max_retries: 4,
            initial_backoff: Duration::from_millis(800),
            progress: false,
        }
    }
}

impl FirestoreSink {
    #[must_use]
    pub fn new(
        project_id: impl Into<String>,
        uid: impl Into<String>,
        bearer_token: String,
    ) -> Self {
        Self {
            project_id: project_id.into(),
            uid: uid.into(),
            bearer_token,
        }
    }

    /// Sends a batch to Firestore through the REST commit API.
    ///
    /// # Errors
    ///
    /// Returns an error if the batch schema is unsupported, authentication is
    /// missing, Firestore rejects a write, or the response cannot be read.
    pub fn send_with_ack(&self, batch: &SyncBatch) -> Result<SyncAck> {
        self.send_with_ack_and_options(batch, &FirestoreSendOptions::default())
    }

    /// Sends a batch and allows tuning retry behavior and progress logging.
    ///
    /// # Errors
    ///
    /// Returns an error if the batch schema is unsupported, authentication is
    /// missing, Firestore rejects a write, or a network/HTTP response fails.
    pub fn send_with_ack_and_options(
        &self,
        batch: &SyncBatch,
        options: &FirestoreSendOptions,
    ) -> Result<SyncAck> {
        let using_emulator = firestore_emulator_host().is_some();
        if batch.schema_version != SYNC_BATCH_SCHEMA_VERSION {
            bail!("unsupported sync batch schema {}", batch.schema_version);
        }
        if self.bearer_token.trim().is_empty() && !using_emulator {
            bail!("Firebase auth token is required for Firestore sync");
        }

        let chunk_size = options.commit_chunk_size.clamp(1, 450);
        let writes = firestore_writes(batch, &self.project_id, &self.uid)?;
        let total_writes = writes.len();
        let total_chunks = if total_writes == 0 {
            0
        } else {
            total_writes.div_ceil(chunk_size)
        };
        let mut committed_writes = 0usize;

        for (index, chunk) in writes.chunks(chunk_size).enumerate() {
            if options.progress {
                eprintln!(
                    "firestore sync progress: chunk {}/{} ({} writes committed so far)",
                    index + 1,
                    total_chunks,
                    committed_writes
                );
            }
            self.commit_with_retries(chunk, options, index + 1, total_chunks)?;
            committed_writes += chunk.len();
        }

        if options.progress {
            eprintln!(
                "firestore sync progress: done ({} chunks, {} writes)",
                total_chunks, total_writes
            );
        }

        Ok(SyncAck {
            schema_version: SYNC_ACK_SCHEMA_VERSION.to_string(),
            batch_id: batch.batch_id.clone(),
            accepted: SyncEntityCounts {
                sources: batch.sources.len() as u64,
                accounts: batch.accounts.len() as u64,
                subscriptions: batch.subscriptions.len() as u64,
                events: batch.events.len() as u64,
                summaries: batch.summaries.len() as u64,
            },
            duplicates: SyncEntityCounts {
                sources: 0,
                accounts: 0,
                subscriptions: 0,
                events: 0,
                summaries: 0,
            },
            rejected: Vec::new(),
        })
    }

    fn commit(&self, writes: &[Value]) -> std::result::Result<(), FirestoreCommitError> {
        let url = firestore_commit_url(&self.project_id);
        let request = ureq::post(&url).set("Content-Type", "application/json");
        let request = if self.bearer_token.trim().is_empty() {
            request
        } else {
            request.set("Authorization", &format!("Bearer {}", self.bearer_token))
        };
        let response = request.send_json(json!({ "writes": writes }));

        match response {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err(FirestoreCommitError::Status { code, body })
            }
            Err(error) => Err(FirestoreCommitError::Transport(error.to_string())),
        }
    }

    fn commit_with_retries(
        &self,
        writes: &[Value],
        options: &FirestoreSendOptions,
        chunk_index: usize,
        chunk_total: usize,
    ) -> Result<()> {
        let mut attempt = 0u32;
        loop {
            match self.commit(writes) {
                Ok(()) => return Ok(()),
                Err(FirestoreCommitError::Status { code, body }) => {
                    let retryable = code == 429 || code == 500 || code == 503;
                    if retryable && attempt < options.max_retries {
                        let delay = exponential_backoff(options.initial_backoff, attempt);
                        attempt += 1;
                        if options.progress {
                            eprintln!(
                                "firestore sync retry: chunk {}/{} attempt {}/{} after HTTP {}",
                                chunk_index, chunk_total, attempt, options.max_retries, code
                            );
                        }
                        thread::sleep(delay);
                        continue;
                    }
                    if code == 429 {
                        bail!(
                            "Firestore sync failed (HTTP 429): quota exhausted or throttled. {}\nTip: run smaller chunked syncs and continue with --since-last once quota resets.",
                            body
                        );
                    }
                    bail!("Firestore sync failed (HTTP {}): {}", code, body);
                }
                Err(FirestoreCommitError::Transport(message)) => {
                    if attempt < options.max_retries {
                        let delay = exponential_backoff(options.initial_backoff, attempt);
                        attempt += 1;
                        if options.progress {
                            eprintln!(
                                "firestore sync retry: chunk {}/{} attempt {}/{} after transport error: {}",
                                chunk_index, chunk_total, attempt, options.max_retries, message
                            );
                        }
                        thread::sleep(delay);
                        continue;
                    }
                    bail!("Firestore sync failed: {}", message);
                }
            }
        }
    }
}

impl SyncSink for FirestoreSink {
    fn name(&self) -> &'static str {
        "firestore"
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

fn firestore_writes(batch: &SyncBatch, project_id: &str, uid: &str) -> Result<Vec<Value>> {
    let mut writes = Vec::with_capacity(
        2 + batch.sources.len()
            + batch.accounts.len()
            + batch.subscriptions.len()
            + batch.events.len()
            + batch.summaries.len(),
    );
    let prefix = firestore_document_prefix(project_id, uid);
    let synced_at = Utc::now().to_rfc3339();

    writes.push(firestore_update(
        format!(
            "{prefix}/syncBatches/{}",
            sanitize_document_id(&batch.batch_id)
        ),
        json!({
            "schema_version": batch.schema_version,
            "batch_id": batch.batch_id,
            "device_id": batch.device_id,
            "created_at": batch.created_at,
            "synced_at": synced_at,
            "counts": {
                "sources": batch.sources.len(),
                "accounts": batch.accounts.len(),
                "subscriptions": batch.subscriptions.len(),
                "events": batch.events.len(),
                "summaries": batch.summaries.len()
            }
        }),
    )?);
    writes.push(firestore_update(
        format!(
            "{prefix}/devices/{}",
            sanitize_document_id(&batch.device_id)
        ),
        json!({
            "device_id": batch.device_id,
            "last_batch_id": batch.batch_id,
            "last_synced_at": synced_at
        }),
    )?);

    push_entity_writes(
        &mut writes,
        &prefix,
        "sources",
        "source_id",
        &batch.batch_id,
        &synced_at,
        &batch.sources,
    )?;
    push_entity_writes(
        &mut writes,
        &prefix,
        "accounts",
        "provider_account_id",
        &batch.batch_id,
        &synced_at,
        &batch.accounts,
    )?;
    push_entity_writes(
        &mut writes,
        &prefix,
        "subscriptions",
        "subscription_id",
        &batch.batch_id,
        &synced_at,
        &batch.subscriptions,
    )?;
    push_entity_writes(
        &mut writes,
        &prefix,
        "events",
        "event_id",
        &batch.batch_id,
        &synced_at,
        &batch.events,
    )?;
    push_entity_writes(
        &mut writes,
        &prefix,
        "summaries",
        "summary_id",
        &batch.batch_id,
        &synced_at,
        &batch.summaries,
    )?;

    Ok(writes)
}

fn push_entity_writes<T: serde::Serialize>(
    writes: &mut Vec<Value>,
    prefix: &str,
    collection: &str,
    id_field: &str,
    batch_id: &str,
    synced_at: &str,
    entities: &[T],
) -> Result<()> {
    for entity in entities {
        let mut value = serde_json::to_value(entity)?;
        let doc_id = value
            .get(id_field)
            .and_then(json_scalar_as_string)
            .map(|id| sanitize_document_id(&id))
            .with_context(|| format!("missing {id_field} for Firestore {collection} write"))?;
        if let Value::Object(fields) = &mut value {
            fields.insert(
                "last_batch_id".to_string(),
                Value::String(batch_id.to_string()),
            );
            fields.insert(
                "synced_at".to_string(),
                Value::String(synced_at.to_string()),
            );
        }
        writes.push(firestore_update(
            format!("{prefix}/{collection}/{doc_id}"),
            value,
        )?);
    }
    Ok(())
}

fn firestore_document_prefix(project_id: &str, uid: &str) -> String {
    format!(
        "projects/{}/databases/(default)/documents/users/{}",
        project_id,
        sanitize_document_id(uid)
    )
}

fn firestore_update(name: String, value: Value) -> Result<Value> {
    let Value::Object(fields) = value else {
        bail!("Firestore document must be a JSON object");
    };
    Ok(json!({
        "update": {
            "name": name,
            "fields": firestore_fields(fields)?
        }
    }))
}

fn firestore_fields(fields: Map<String, Value>) -> Result<Map<String, Value>> {
    fields
        .into_iter()
        .map(|(key, value)| Ok((key, firestore_value(value)?)))
        .collect()
}

fn firestore_value(value: Value) -> Result<Value> {
    Ok(match value {
        Value::Null => json!({ "nullValue": null }),
        Value::Bool(value) => json!({ "booleanValue": value }),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                json!({ "integerValue": integer.to_string() })
            } else if let Some(unsigned) = value.as_u64() {
                json!({ "integerValue": unsigned.to_string() })
            } else {
                json!({ "doubleValue": value.as_f64().context("invalid JSON number")? })
            }
        }
        Value::String(value) => json!({ "stringValue": value }),
        Value::Array(values) => {
            let values = values
                .into_iter()
                .map(firestore_value)
                .collect::<Result<Vec<_>>>()?;
            json!({ "arrayValue": { "values": values } })
        }
        Value::Object(fields) => json!({ "mapValue": { "fields": firestore_fields(fields)? } }),
    })
}

fn json_scalar_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn sanitize_document_id(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'));
    if safe && value != "." && value != ".." && !value.starts_with("__") && !value.ends_with("__") {
        return value.to_string();
    }

    let mut output = String::with_capacity(4 + value.len() * 2);
    output.push_str("hex_");
    for byte in value.as_bytes() {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn firestore_commit_url(project_id: &str) -> String {
    if let Some(host) = firestore_emulator_host() {
        return format!(
            "http://{host}/v1/projects/{project_id}/databases/(default)/documents:commit"
        );
    }
    format!("https://firestore.googleapis.com/v1/projects/{project_id}/databases/(default)/documents:commit")
}

fn firestore_emulator_host() -> Option<String> {
    std::env::var("FIRESTORE_EMULATOR_HOST")
        .ok()
        .map(|value| {
            value
                .trim()
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .to_string()
        })
        .filter(|value| !value.is_empty())
}

#[derive(Debug)]
enum FirestoreCommitError {
    Status { code: u16, body: String },
    Transport(String),
}

fn exponential_backoff(initial: Duration, attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt.min(10)).unwrap_or(1024);
    initial.saturating_mul(factor)
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

    #[test]
    fn firestore_writes_create_user_scoped_documents() {
        let writes =
            firestore_writes(&empty_batch(), "project-1", "user/with/slash").expect("writes");
        assert_eq!(writes.len(), 2);
        let first_name = writes[0]["update"]["name"].as_str().expect("name");
        assert!(
            first_name.starts_with("projects/project-1/databases/(default)/documents/users/hex_")
        );
        assert!(first_name.ends_with("/syncBatches/batch_1"));
        assert_eq!(
            writes[0]["update"]["fields"]["schema_version"]["stringValue"],
            "sync_batch.v1"
        );
        assert_eq!(
            writes[0]["update"]["fields"]["counts"]["mapValue"]["fields"]["events"]["integerValue"],
            "0"
        );
    }

    #[test]
    fn firestore_commit_url_uses_emulator_when_host_is_set() {
        std::env::set_var("FIRESTORE_EMULATOR_HOST", "http://127.0.0.1:8080");
        let url = firestore_commit_url("proj");
        std::env::remove_var("FIRESTORE_EMULATOR_HOST");
        assert_eq!(
            url,
            "http://127.0.0.1:8080/v1/projects/proj/databases/(default)/documents:commit"
        );
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
