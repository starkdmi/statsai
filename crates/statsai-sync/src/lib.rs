//! Sync sink interfaces for `statsai`.

use anyhow::{bail, Context, Result};
use statsai_core::{SyncAck, SyncBatch, SYNC_ACK_SCHEMA_VERSION};
use std::io::Write;
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
    endpoint: String,
    bearer_token: Option<String>,
    timeout: Duration,
}

impl HttpSink {
    /// Creates an HTTP sync sink.
    ///
    /// # Errors
    ///
    /// Returns an error when `endpoint` is not an `http://` or `https://` URL.
    pub fn new(endpoint: impl AsRef<str>, bearer_token: Option<String>) -> Result<Self> {
        let endpoint = endpoint.as_ref().trim();
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
            bail!("http sink supports http:// and https:// endpoints only");
        }
        Ok(Self {
            endpoint: endpoint.to_string(),
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
        let request = ureq::post(&self.endpoint)
            .timeout(self.timeout)
            .set(
                "User-Agent",
                &format!("statsai/{}", env!("CARGO_PKG_VERSION")),
            )
            .set("Content-Type", "application/json")
            .set("Accept", "application/json");
        let request = if let Some(token) = self
            .bearer_token
            .as_deref()
            .filter(|token| !token.is_empty())
        {
            request.set("Authorization", &format!("Bearer {token}"))
        } else {
            request
        };
        let response = request.send_json(serde_json::to_value(batch)?);
        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(code, response)) => {
                let body = response.into_string().unwrap_or_default();
                bail!(
                    "sync endpoint returned HTTP {}: {}",
                    code,
                    body.trim().chars().take(200).collect::<String>()
                );
            }
            Err(error) => bail!("sync endpoint request failed: {}", error),
        };
        let ack: SyncAck = response.into_json().context("parse sync ack")?;
        validate_sync_ack(batch, &ack)?;
        Ok(ack)
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

fn validate_sync_ack(batch: &SyncBatch, ack: &SyncAck) -> Result<()> {
    if ack.schema_version != SYNC_ACK_SCHEMA_VERSION {
        bail!("unsupported sync ack schema {}", ack.schema_version);
    }
    if ack.batch_id != batch.batch_id {
        bail!(
            "sync ack batch_id mismatch: expected {}, got {}",
            batch.batch_id,
            ack.batch_id
        );
    }
    if !ack.rejected.is_empty() {
        let rejected = &ack.rejected[0];
        bail!(
            "sync ack rejected {} record(s); first rejection kind={} id={} reason={}",
            ack.rejected.len(),
            rejected.kind,
            rejected.id.as_deref().unwrap_or("unknown"),
            rejected.reason
        );
    }

    validate_sync_ack_counts(
        "sources",
        batch.sources.len() as u64,
        ack.accepted.sources,
        ack.duplicates.sources,
    )?;
    validate_sync_ack_counts(
        "accounts",
        batch.accounts.len() as u64,
        ack.accepted.accounts,
        ack.duplicates.accounts,
    )?;
    validate_sync_ack_counts(
        "source_account_assignments",
        batch.source_account_assignments.len() as u64,
        ack.accepted.source_account_assignments,
        ack.duplicates.source_account_assignments,
    )?;
    validate_sync_ack_counts(
        "subscriptions",
        batch.subscriptions.len() as u64,
        ack.accepted.subscriptions,
        ack.duplicates.subscriptions,
    )?;
    validate_sync_ack_counts(
        "events",
        batch.events.len() as u64,
        ack.accepted.events,
        ack.duplicates.events,
    )?;
    validate_sync_ack_counts(
        "summaries",
        batch.summaries.len() as u64,
        ack.accepted.summaries,
        ack.duplicates.summaries,
    )?;
    Ok(())
}

fn validate_sync_ack_counts(
    label: &str,
    submitted: u64,
    accepted: u64,
    duplicates: u64,
) -> Result<()> {
    let total = accepted
        .checked_add(duplicates)
        .context("sync ack count overflow")?;
    if total != submitted {
        bail!(
            "sync ack {} count mismatch: submitted {}, accepted {}, duplicates {}",
            label,
            submitted,
            accepted,
            duplicates
        );
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use statsai_core::SyncBatch;
    use std::sync::mpsc;
    use tiny_http::{Header, Method, Response, Server};

    fn empty_batch() -> SyncBatch {
        SyncBatch {
            schema_version: "sync_batch.v1".to_string(),
            batch_id: "batch_1".to_string(),
            device_id: "device".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
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
            let response = Response::from_string(test_ack_json("batch_1", 0, 0, Vec::new()))
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
    fn http_sink_rejects_ack_with_wrong_batch_id() {
        let server = Server::http("127.0.0.1:0").expect("server");
        let endpoint = format!("http://{}/v1/sync/batches", server.server_addr());
        let handle = std::thread::spawn(move || {
            let request = server.recv().expect("request");
            let response = Response::from_string(test_ack_json("batch_other", 0, 0, Vec::new()))
                .with_header(Header::from_bytes("content-type", "application/json").unwrap());
            request.respond(response).expect("respond");
        });

        let sink = HttpSink::new(endpoint, None).expect("sink");
        let error = sink
            .send(&empty_batch())
            .expect_err("ack mismatch should fail");
        handle.join().expect("server thread");
        assert!(error.to_string().contains("batch_id mismatch"));
    }

    #[test]
    fn http_sink_rejects_ack_with_rejected_records() {
        let server = Server::http("127.0.0.1:0").expect("server");
        let endpoint = format!("http://{}/v1/sync/batches", server.server_addr());
        let handle = std::thread::spawn(move || {
            let request = server.recv().expect("request");
            let response = Response::from_string(test_ack_json(
                "batch_1",
                0,
                0,
                vec![r#"{"kind":"event","id":"event-1","reason":"invalid"}"#.to_string()],
            ))
            .with_header(Header::from_bytes("content-type", "application/json").unwrap());
            request.respond(response).expect("respond");
        });

        let sink = HttpSink::new(endpoint, None).expect("sink");
        let error = sink
            .send(&empty_batch())
            .expect_err("rejected ack should fail");
        handle.join().expect("server thread");
        assert!(error.to_string().contains("rejected 1 record"));
    }

    #[test]
    fn http_sink_rejects_ack_with_incomplete_counts() {
        let server = Server::http("127.0.0.1:0").expect("server");
        let endpoint = format!("http://{}/v1/sync/batches", server.server_addr());
        let handle = std::thread::spawn(move || {
            let request = server.recv().expect("request");
            let response = Response::from_string(test_ack_json("batch_1", 0, 0, Vec::new()))
                .with_header(Header::from_bytes("content-type", "application/json").unwrap());
            request.respond(response).expect("respond");
        });

        let sink = HttpSink::new(endpoint, None).expect("sink");
        let batch = SyncBatch {
            events: vec![empty_batch_event()],
            ..empty_batch()
        };
        let error = sink
            .send(&batch)
            .expect_err("incomplete counts should fail");
        handle.join().expect("server thread");
        assert!(error.to_string().contains("events count mismatch"));
    }

    #[test]
    fn http_sink_rejects_non_http_url() {
        let error =
            HttpSink::new("ftp://example.com/v1/sync/batches", None).expect_err("bad scheme");
        assert!(error.to_string().contains("http://"));
    }

    fn empty_batch_event() -> statsai_core::UsageEvent {
        statsai_core::UsageEvent {
            schema_version: statsai_core::USAGE_EVENT_SCHEMA_VERSION.to_string(),
            event_id: statsai_core::EventId("event_1".to_string()),
            device_id: "device".to_string(),
            provider: "codex".to_string(),
            source_id: statsai_core::SourceId("source_1".to_string()),
            provider_account_id: None,
            subscription_id: None,
            source: statsai_core::EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: statsai_core::SourceKind::LocalAdapter,
                location_origin: Some(statsai_core::LocationOrigin::Configured),
                source_type: "jsonl".to_string(),
                source_path_hash: None,
                source_record_id: Some("record_1".to_string()),
                parse_confidence: statsai_core::Confidence::High,
            },
            session: statsai_core::SessionInfo {
                session_id: "session_1".to_string(),
                local_session_id_hash: None,
                title: None,
                started_at: Utc::now(),
                ended_at: None,
                duration_seconds: None,
            },
            model: None,
            usage: statsai_core::UsageCounts::default(),
            runtime: None,
            cost: statsai_core::CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: None,
                provider_reported_usd: None,
                pricing_source: None,
                pricing_version: None,
                confidence: statsai_core::Confidence::Low,
            },
            parse_evidence: None,
            project: None,
            git: None,
            privacy: statsai_core::PrivacyInfo {
                mode: statsai_core::PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            created_at: Utc::now(),
            imported_at: Utc::now(),
        }
    }

    fn test_ack_json(
        batch_id: &str,
        accepted_events: u64,
        duplicate_events: u64,
        rejected: Vec<String>,
    ) -> String {
        let rejected = if rejected.is_empty() {
            "[]".to_string()
        } else {
            format!("[{}]", rejected.join(","))
        };
        format!(
            r#"{{
              "schema_version":"sync_ack.v1",
              "batch_id":"{batch_id}",
              "accepted":{{"sources":0,"accounts":0,"source_account_assignments":0,"subscriptions":0,"events":{accepted_events},"summaries":0}},
              "duplicates":{{"sources":0,"accounts":0,"source_account_assignments":0,"subscriptions":0,"events":{duplicate_events},"summaries":0}},
              "rejected":{rejected}
            }}"#
        )
    }
}
