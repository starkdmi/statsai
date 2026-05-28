//! Loopback API + file-watching daemon for `ai-stats`.

use ai_stats_core::{
    SyncAck, SyncBatch, SyncEntityCounts, SyncRejectedRecord, SYNC_ACK_SCHEMA_VERSION,
    SYNC_BATCH_SCHEMA_VERSION,
};
use ai_stats_store::Store;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex, MutexGuard};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

fn lock_store(store: &Arc<Mutex<Store>>) -> MutexGuard<'_, Store> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn run(addr: &str, store: Arc<Mutex<Store>>) -> Result<()> {
    ensure_loopback(addr)?;
    let server =
        Server::http(addr).map_err(|err| anyhow::anyhow!("start local API on {addr}: {err}"))?;

    for request in server.incoming_requests() {
        handle_request(request, &store)?;
    }

    Ok(())
}

fn handle_request(mut request: Request, store: &Arc<Mutex<Store>>) -> Result<()> {
    let method = request.method().clone();
    let url = request.url().to_string();

    if method == Method::Post && url == "/v1/sync/batches" {
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .context("read sync batch request")?;
        let batch: SyncBatch = match serde_json::from_str(&body) {
            Ok(batch) => batch,
            Err(error) => {
                return respond_text(request, StatusCode(400), &format!("invalid batch: {error}"));
            }
        };
        let ack = {
            let s = lock_store(store);
            match ingest_sync_batch(&s, &batch) {
                Ok(ack) => ack,
                Err(error) => {
                    return respond_text(request, StatusCode(400), &error.to_string());
                }
            }
        };
        return respond_json(request, StatusCode(200), &ack);
    }

    if method != Method::Get {
        return respond_text(request, StatusCode(405), "method not allowed");
    }

    let s = lock_store(store);
    let payload = match url.as_str() {
        "/health" => json!({"status": "ok"}),
        "/status" => json!({
            "events": s.event_count()?,
            "tokens": s.token_total()?
        }),
        "/sources" => serde_json::to_value(s.list_sources()?)?,
        "/accounts" => serde_json::to_value(s.list_accounts()?)?,
        "/subscriptions" => serde_json::to_value(s.list_subscriptions()?)?,
        "/reports/weekly" => json!({
            "events": s.event_count()?,
            "tokens": s.token_total()?
        }),
        _ => {
            drop(s);
            return respond_text(request, StatusCode(404), "not found");
        }
    };
    drop(s);

    respond_json(request, StatusCode(200), &payload)
}

pub fn ingest_sync_batch(store: &Store, batch: &SyncBatch) -> Result<SyncAck> {
    if batch.schema_version != SYNC_BATCH_SCHEMA_VERSION {
        bail!("unsupported sync batch schema {}", batch.schema_version);
    }

    for source in &batch.sources {
        store.upsert_source(source)?;
    }
    for account in &batch.accounts {
        store.upsert_account(account)?;
    }
    for subscription in &batch.subscriptions {
        store.upsert_subscription(subscription)?;
    }
    let inserted_events = store.insert_events(&batch.events)?;
    let written_summaries = store.upsert_summaries(&batch.summaries)?;

    Ok(SyncAck {
        schema_version: SYNC_ACK_SCHEMA_VERSION.to_string(),
        batch_id: batch.batch_id.clone(),
        accepted: SyncEntityCounts {
            sources: batch.sources.len() as u64,
            accounts: batch.accounts.len() as u64,
            subscriptions: batch.subscriptions.len() as u64,
            events: inserted_events,
            summaries: written_summaries,
        },
        duplicates: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            subscriptions: 0,
            events: (batch.events.len() as u64).saturating_sub(inserted_events),
            summaries: 0,
        },
        rejected: Vec::<SyncRejectedRecord>::new(),
    })
}

fn respond_json<T: serde::Serialize>(
    request: Request,
    status: StatusCode,
    payload: &T,
) -> Result<()> {
    let body = serde_json::to_string_pretty(payload)?;
    let response = Response::from_string(body)
        .with_status_code(status)
        .with_header(content_type_json());
    request.respond(response)?;
    Ok(())
}

fn respond_text(request: Request, status: StatusCode, body: &str) -> Result<()> {
    let response = Response::from_string(body).with_status_code(status);
    request.respond(response)?;
    Ok(())
}

fn content_type_json() -> Header {
    Header::from_bytes("content-type", "application/json").expect("static header is valid")
}

#[cfg(feature = "watch")]
mod watch {
    use ai_stats_adapters::{default_adapters, ProviderAdapter, ScanOptions};
    use ai_stats_core::{
        provider_account_id, IdentitySource, SourceLocation, UsageEvent, UsageSummary,
    };
    use ai_stats_store::Store;
    use anyhow::{Context, Result};
    use notify::{Event, EventKind, RecursiveMode, Watcher};
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tiny_http::Server;

    pub fn watch_and_serve(addr: &str, store: Arc<Mutex<Store>>, device_id: &str) -> Result<()> {
        super::ensure_loopback(addr)?;

        let sources = {
            let s = super::lock_store(&store);
            discover_watch_sources(&s)
        };
        let (tx, rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                    let changed: Vec<PathBuf> = event.paths;
                    let _ = tx.send(changed);
                }
            }
        })
        .context("create file watcher")?;

        for path in &sources {
            if let Err(e) = watcher.watch(path, RecursiveMode::Recursive) {
                eprintln!("daemon: cannot watch {}: {e}", path.display());
            } else {
                eprintln!("daemon: watching {}", path.display());
            }
        }

        eprintln!("daemon: API listening on http://{addr}");
        let server = Server::http(addr)
            .map_err(|err| anyhow::anyhow!("start local API on {addr}: {err}"))?;

        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(changed) => {
                    let s = super::lock_store(&store);
                    rescan_changed_sources(&s, device_id, &changed);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if let Ok(Some(request)) = server.try_recv() {
                super::handle_request(request, &store)?;
            }
        }

        Ok(())
    }

    fn discover_watch_sources(store: &Store) -> Vec<PathBuf> {
        let mut paths = Vec::new();

        if let Ok(configured) = store.list_sources() {
            for source in configured {
                if let Some(label) = source.path_label.as_deref().filter(|p| !p.is_empty()) {
                    paths.push(PathBuf::from(label));
                }
            }
        }

        for adapter in default_adapters() {
            for source in adapter.discover() {
                if let Some(label) = source.path_label.as_deref().filter(|p| !p.is_empty()) {
                    let path = PathBuf::from(label);
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
        }

        paths
    }

    fn rescan_changed_sources(store: &Store, device_id: &str, changed: &[PathBuf]) {
        let adapters: Vec<Box<dyn ProviderAdapter>> = default_adapters();
        let configured = match store.list_sources() {
            Ok(sources) => sources,
            Err(e) => {
                eprintln!("daemon: failed to list sources: {e}");
                return;
            }
        };

        for adapter in &adapters {
            let sources = scan_sources_for_paths(adapter.as_ref(), &configured, changed);
            for source in sources {
                let options = ScanOptions {
                    device_id: device_id.to_string(),
                };
                match adapter.scan(&source, &options) {
                    Ok(mut scan) => {
                        apply_source_account_hint(&source, &mut scan.events, &mut scan.summaries);
                        if !scan.events.is_empty() || !scan.summaries.is_empty() {
                            let events = scan.events.len();
                            let summaries = scan.summaries.len();
                            if let Err(e) = store.insert_events(&scan.events) {
                                eprintln!("daemon: insert events failed: {e}");
                            }
                            if let Err(e) = store.upsert_summaries(&scan.summaries) {
                                eprintln!("daemon: insert summaries failed: {e}");
                            }
                            eprintln!(
                                "daemon: rescanned {} ({}) — {} new events, {} summaries",
                                source.provider,
                                source.path_label.as_deref().unwrap_or("unknown"),
                                events,
                                summaries
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "daemon: scan failed for {}: {e}",
                            source.path_label.as_deref().unwrap_or("unknown")
                        );
                    }
                }
            }
        }
    }

    fn scan_sources_for_paths(
        adapter: &dyn ProviderAdapter,
        configured: &[SourceLocation],
        changed: &[PathBuf],
    ) -> Vec<SourceLocation> {
        let mut sources = Vec::new();
        for source in configured
            .iter()
            .filter(|s| s.enabled && s.provider == adapter.provider())
            .cloned()
        {
            if source.path_label.is_some() && source_in_changed_paths(&source, changed) {
                sources.push(source);
            }
        }
        for source in adapter.discover() {
            if source.path_label.is_none() {
                continue;
            }
            if source_in_changed_paths(&source, changed)
                && !sources.iter().any(|s| s.source_id == source.source_id)
            {
                sources.push(source);
            }
        }
        sources
    }

    fn apply_source_account_hint(
        source: &SourceLocation,
        events: &mut [UsageEvent],
        summaries: &mut [UsageSummary],
    ) {
        let Some(account_hint) = source.account_hint.as_deref() else {
            return;
        };
        let account_id = provider_account_id(&source.provider, account_hint);
        for event in events {
            event.provider_account_id = Some(account_id.clone());
            if let Some(evidence) = event.parse_evidence.as_mut() {
                evidence.account_identity_source = IdentitySource::ManualHint;
            }
        }
        for summary in summaries {
            summary.provider_account_id = Some(account_id.clone());
            if let Some(evidence) = summary.parse_evidence.as_mut() {
                evidence.account_identity_source = IdentitySource::ManualHint;
            }
        }
    }

    fn source_in_changed_paths(source: &SourceLocation, changed: &[PathBuf]) -> bool {
        let Some(label) = source.path_label.as_deref() else {
            return false;
        };
        let source_path = PathBuf::from(label);
        changed.iter().any(|changed_path| {
            changed_path.starts_with(&source_path) || source_path.starts_with(changed_path)
        })
    }
}

#[cfg(not(feature = "watch"))]
pub fn watch_and_serve(_addr: &str, _store: Arc<Mutex<Store>>, _device_id: &str) -> Result<()> {
    anyhow::bail!(
        "daemon --watch requires the `watch` cargo feature (enable with --features watch)"
    )
}

#[cfg(feature = "watch")]
pub fn watch_and_serve(addr: &str, store: Arc<Mutex<Store>>, device_id: &str) -> Result<()> {
    watch::watch_and_serve(addr, store, device_id)
}

fn ensure_loopback(addr: &str) -> Result<()> {
    let mut addrs = addr.to_socket_addrs()?;
    let Some(addr) = addrs.next() else {
        anyhow::bail!("local API address did not resolve");
    };
    if !addr.ip().is_loopback() {
        anyhow::bail!("local API must bind to a loopback address");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn empty_batch() -> SyncBatch {
        SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_test".to_string(),
            device_id: "device_test".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            subscriptions: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn ingest_empty_sync_batch_returns_ack() {
        let store = Store::in_memory().expect("store");
        let ack = ingest_sync_batch(&store, &empty_batch()).expect("ack");

        assert_eq!(ack.schema_version, SYNC_ACK_SCHEMA_VERSION);
        assert_eq!(ack.batch_id, "batch_test");
        assert_eq!(ack.accepted.events, 0);
        assert_eq!(ack.duplicates.events, 0);
        assert!(ack.rejected.is_empty());
    }

    #[test]
    fn ingest_rejects_unsupported_schema() {
        let store = Store::in_memory().expect("store");
        let mut batch = empty_batch();
        batch.schema_version = "sync_batch.v0".to_string();

        let error = ingest_sync_batch(&store, &batch).expect_err("unsupported schema");
        assert!(error.to_string().contains("unsupported sync batch schema"));
    }
}
