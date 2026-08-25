//! Loopback API + file-watching daemon for `statsai`.

use anyhow::{bail, Context, Result};
use serde_json::json;
use statsai_core::{
    SyncAck, SyncBatch, SyncEntityCounts, SyncRejectedRecord, SYNC_ACK_V1_SCHEMA_VERSION,
    SYNC_ACK_V2_SCHEMA_VERSION, SYNC_ACK_V3_SCHEMA_VERSION, SYNC_ACK_V4_SCHEMA_VERSION,
    SYNC_BATCH_V1_SCHEMA_VERSION, SYNC_BATCH_V2_SCHEMA_VERSION, SYNC_BATCH_V3_SCHEMA_VERSION,
    SYNC_BATCH_V4_SCHEMA_VERSION,
};
use statsai_store::Store;
use std::io::Read;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, MutexGuard};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const MAX_SYNC_BATCH_BYTES: usize = 8 * 1024 * 1024;

fn lock_store(store: &Arc<Mutex<Store>>) -> MutexGuard<'_, Store> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

/// Acknowledgement schema owed to a batch schema.
///
/// Every arm is spelled out so that adding a batch version without deciding its
/// acknowledgement fails to compile, rather than silently claiming v3. Callers
/// reject unknown schemas before reaching this, so the error is unreachable
/// today; it exists to keep that true.
fn sync_ack_schema_version(batch_schema_version: &str) -> Result<&'static str> {
    match batch_schema_version {
        SYNC_BATCH_V1_SCHEMA_VERSION => Ok(SYNC_ACK_V1_SCHEMA_VERSION),
        SYNC_BATCH_V2_SCHEMA_VERSION => Ok(SYNC_ACK_V2_SCHEMA_VERSION),
        SYNC_BATCH_V3_SCHEMA_VERSION => Ok(SYNC_ACK_V3_SCHEMA_VERSION),
        SYNC_BATCH_V4_SCHEMA_VERSION => Ok(SYNC_ACK_V4_SCHEMA_VERSION),
        other => bail!("unsupported sync batch schema {other}"),
    }
}

pub fn run(addr: &str, store: Arc<Mutex<Store>>, auth_token: &str) -> Result<()> {
    let bind_addr = resolve_loopback_addr(addr)?;
    let server = Server::http(bind_addr)
        .map_err(|err| anyhow::anyhow!("start local API on {bind_addr}: {err}"))?;

    for request in server.incoming_requests() {
        if let Err(error) = handle_request(request, &store, auth_token) {
            eprintln!("daemon: request failed: {error:#}");
        }
    }

    Ok(())
}

fn handle_request(mut request: Request, store: &Arc<Mutex<Store>>, auth_token: &str) -> Result<()> {
    let method = request.method().clone();
    let url = request.url().to_string();

    if let Err(rejection) = validate_http_request(
        &method,
        &url,
        request.headers(),
        request.body_length(),
        auth_token,
    ) {
        return respond_text(request, rejection.status, rejection.message);
    }

    if method == Method::Post && url == "/v1/sync/batches" {
        let mut body = Vec::with_capacity(
            request
                .body_length()
                .unwrap_or_default()
                .min(MAX_SYNC_BATCH_BYTES),
        );
        request
            .as_reader()
            .take((MAX_SYNC_BATCH_BYTES + 1) as u64)
            .read_to_end(&mut body)
            .context("read sync batch request")?;
        if body.len() > MAX_SYNC_BATCH_BYTES {
            return respond_text(request, StatusCode(413), "sync batch is too large");
        }
        let batch: SyncBatch = match serde_json::from_slice(&body) {
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

    if url == "/health" {
        return respond_json(request, StatusCode(200), &health_payload());
    }

    let s = lock_store(store);
    let payload = match url.as_str() {
        "/status" => json!({
            "events": s.event_count()?,
            "tokens": s.token_total()?
        }),
        "/sources" => serde_json::to_value(s.list_sources()?)?,
        "/accounts" => serde_json::to_value(s.list_accounts()?)?,
        "/source-account-assignments" => {
            serde_json::to_value(s.list_source_account_assignments()?)?
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HttpRejection {
    status: StatusCode,
    message: &'static str,
}

fn validate_http_request(
    method: &Method,
    url: &str,
    headers: &[Header],
    body_length: Option<usize>,
    auth_token: &str,
) -> std::result::Result<(), HttpRejection> {
    if headers.iter().any(|header| header.field.equiv("Origin")) {
        return Err(HttpRejection {
            status: StatusCode(403),
            message: "browser-originated requests are not allowed",
        });
    }

    if method == &Method::Get && url == "/health" {
        return Ok(());
    }

    let mut authorization_headers = headers
        .iter()
        .filter(|header| header.field.equiv("Authorization"));
    let supplied_token = authorization_headers
        .next()
        .and_then(|header| header.value.as_str().strip_prefix("Bearer "));
    if authorization_headers.next().is_some()
        || !supplied_token.is_some_and(|token| constant_time_eq(token, auth_token))
    {
        return Err(HttpRejection {
            status: StatusCode(401),
            message: "missing or invalid bearer token",
        });
    }

    if method == &Method::Post && url == "/v1/sync/batches" {
        let mut content_type_headers = headers
            .iter()
            .filter(|header| header.field.equiv("Content-Type"));
        let is_json = content_type_headers.next().is_some_and(|header| {
            header
                .value
                .as_str()
                .split(';')
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        });
        if content_type_headers.next().is_some() || !is_json {
            return Err(HttpRejection {
                status: StatusCode(415),
                message: "content-type must be application/json",
            });
        }
        if body_length.is_some_and(|length| length > MAX_SYNC_BATCH_BYTES) {
            return Err(HttpRejection {
                status: StatusCode(413),
                message: "sync batch is too large",
            });
        }
    }

    Ok(())
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn health_payload() -> serde_json::Value {
    json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

pub fn ingest_sync_batch(store: &Store, batch: &SyncBatch) -> Result<SyncAck> {
    if batch.schema_version != SYNC_BATCH_V1_SCHEMA_VERSION
        && batch.schema_version != SYNC_BATCH_V2_SCHEMA_VERSION
        && batch.schema_version != SYNC_BATCH_V3_SCHEMA_VERSION
        && batch.schema_version != SYNC_BATCH_V4_SCHEMA_VERSION
    {
        bail!("unsupported sync batch schema {}", batch.schema_version);
    }
    if !matches!(
        batch.schema_version.as_str(),
        SYNC_BATCH_V3_SCHEMA_VERSION | SYNC_BATCH_V4_SCHEMA_VERSION
    ) && !batch.code_change_metrics.is_empty()
    {
        bail!("code-change metrics require sync_batch.v3");
    }
    if batch.schema_version != SYNC_BATCH_V4_SCHEMA_VERSION
        && !batch.quota_cycle_contributions.is_empty()
    {
        bail!("quota cycle contributions require sync_batch.v4");
    }
    if batch
        .code_change_metrics
        .iter()
        .any(|metric| metric.device_id != batch.device_id)
    {
        bail!("code-change metric device_id must match batch device_id");
    }
    if batch.authoritative_snapshot.is_some() {
        bail!("authoritative snapshots are not supported by the loopback daemon");
    }
    // A local store holds quota observations and derives its own cycles from
    // them; there is no table for another device's contributions. Acknowledging
    // them as accepted told the sender to record them synced against this
    // target and stop offering them, certifying a write that never happened.
    // Refusing follows the authoritative-snapshot precedent above: this
    // endpoint is for loopback diagnostics, and `/api/sync/batches` is the
    // contract that stores quota cycles.
    if !batch.quota_cycle_contributions.is_empty() {
        bail!("quota cycle contributions are not supported by the loopback daemon");
    }

    let result = store.ingest_sync_batch(batch)?;

    Ok(SyncAck {
        schema_version: sync_ack_schema_version(&batch.schema_version)?.to_string(),
        batch_id: batch.batch_id.clone(),
        accepted: SyncEntityCounts {
            sources: batch.sources.len() as u64,
            accounts: batch.accounts.len() as u64,
            source_account_assignments: batch.source_account_assignments.len() as u64,
            subscriptions: batch.subscriptions.len() as u64,
            events: result.inserted_events,
            summaries: result.written_summaries,
            task_buckets: batch.task_buckets.len() as u64,
            task_verifications: result.merged_task_verifications,
            code_change_metrics: batch.code_change_metrics.len() as u64,
            quota_cycle_contributions: batch.quota_cycle_contributions.len() as u64,
        },
        duplicates: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: (batch.events.len() as u64).saturating_sub(result.inserted_events),
            summaries: 0,
            task_buckets: 0,
            task_verifications: (batch.task_verifications.len() as u64)
                .saturating_sub(result.merged_task_verifications),
            code_change_metrics: 0,
            quota_cycle_contributions: 0,
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
    use anyhow::{Context, Result};
    use chrono::{DateTime, Utc};
    use notify::{Event, EventKind, RecursiveMode, Watcher};
    use statsai_adapters::{
        adapter_for_provider, default_adapters, ProviderAdapter, ScanCandidateFile, ScanOptions,
        VerifiedSourceObservation,
    };
    use statsai_core::{
        hash_text, timestamp_in_period, IdentitySource, ProviderAccountId, SourceAccountAssignment,
        SourceId, SourceKind, SourceLocation, SourceVerificationMode, UsageEvent, UsageSummary,
    };
    use statsai_store::{
        reconcile_verified_source_state, verified_source_observation_hash, ScanFileReplacement,
        ScanFileStateEntry, Store,
    };
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::{Duration, Instant, SystemTime};
    use tiny_http::Server;

    const WATCH_SOURCE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
    const WATCH_SCAN_INITIAL_RETRY_DELAY: Duration = if cfg!(test) {
        Duration::ZERO
    } else {
        Duration::from_millis(250)
    };
    const WATCH_SCAN_MAX_RETRY_DELAY: Duration = if cfg!(test) {
        Duration::ZERO
    } else {
        Duration::from_secs(5)
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum WatchScope {
        Direct,
        Recursive,
    }

    impl WatchScope {
        fn notify_mode(self) -> RecursiveMode {
            match self {
                Self::Direct => RecursiveMode::NonRecursive,
                Self::Recursive => RecursiveMode::Recursive,
            }
        }
    }

    #[derive(Debug, Clone, Default)]
    struct VerificationDependencySnapshot {
        paths_by_source: HashMap<SourceId, Vec<PathBuf>>,
    }

    impl VerificationDependencySnapshot {
        fn paths_for(&self, source: &SourceLocation) -> &[PathBuf] {
            self.paths_by_source
                .get(&source.source_id)
                .map(Vec::as_slice)
                .unwrap_or_default()
        }
    }

    #[derive(Debug, Clone)]
    struct CachedVerificationDependencies {
        source: SourceLocation,
        paths: Vec<PathBuf>,
    }

    #[derive(Debug, Default)]
    struct VerificationDependencyCache {
        entries: HashMap<SourceId, CachedVerificationDependencies>,
    }

    impl VerificationDependencyCache {
        fn paths_for(
            &mut self,
            adapter: &dyn ProviderAdapter,
            source: &SourceLocation,
        ) -> Vec<PathBuf> {
            if let Some(cached) = self
                .entries
                .get(&source.source_id)
                .filter(|cached| verification_dependency_source_matches(&cached.source, source))
            {
                return cached.paths.clone();
            }

            let paths = adapter.verification_dependency_paths(source);
            self.entries.insert(
                source.source_id.clone(),
                CachedVerificationDependencies {
                    source: source.clone(),
                    paths: paths.clone(),
                },
            );
            paths
        }

        fn invalidate_changed(
            &mut self,
            adapters: &[Box<dyn ProviderAdapter>],
            changed: &[PathBuf],
        ) {
            self.entries.retain(|_, cached| {
                let Some(adapter) = adapters
                    .iter()
                    .find(|adapter| source_matches_adapter(&cached.source, adapter.as_ref()))
                else {
                    return false;
                };
                !adapter.verification_dependency_paths_changed(&cached.source, changed)
            });
        }

        fn retain_snapshot(&mut self, snapshot: &VerificationDependencySnapshot) {
            self.entries
                .retain(|source_id, _| snapshot.paths_by_source.contains_key(source_id));
        }
    }

    #[derive(Debug, Default)]
    struct WatchPlan {
        paths: HashMap<PathBuf, WatchScope>,
        verification_dependencies: VerificationDependencySnapshot,
    }

    pub fn watch_and_serve(
        addr: &str,
        store: Arc<Mutex<Store>>,
        device_id: &str,
        auth_token: &str,
    ) -> Result<()> {
        let bind_addr = super::resolve_loopback_addr(addr)?;
        let startup_executable = current_executable_stamp();

        let watch_adapters = default_adapters();
        let mut verification_dependency_cache = VerificationDependencyCache::default();
        let initial_configured_result = {
            let s = super::lock_store(&store);
            s.list_sources()
        };
        let initial_plan = match initial_configured_result {
            Ok(configured) => discover_watch_plan(
                &configured,
                &watch_adapters,
                &mut verification_dependency_cache,
            ),
            Err(error) => {
                eprintln!("daemon: initial watch source discovery failed: {error:#}");
                discover_watch_plan(&[], &watch_adapters, &mut verification_dependency_cache)
            }
        };
        let WatchPlan {
            paths: initial_sources,
            verification_dependencies: initial_verification_dependencies,
        } = initial_plan;
        let verification_dependencies = Arc::new(RwLock::new(initial_verification_dependencies));
        let (watcher_signal_tx, watcher_signal_rx) = mpsc::sync_channel(1);
        let pending_changed_paths = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
        let callback_pending_paths = Arc::clone(&pending_changed_paths);

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                if matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    callback_pending_paths
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .extend(event.paths);
                    let _ = watcher_signal_tx.try_send(());
                }
            }
        })
        .context("create file watcher")?;

        let background_store = {
            let store = super::lock_store(&store);
            store.reopen()
        };
        let (scan_signal_tx, scan_signal_rx) = mpsc::sync_channel(1);
        let worker_scan_signal_tx = scan_signal_tx.clone();
        let pending_scan_paths = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
        let worker_pending_scan_paths = Arc::clone(&pending_scan_paths);
        let worker_shared_store = Arc::clone(&store);
        let worker_verification_dependencies = Arc::clone(&verification_dependencies);
        let worker_device_id = device_id.to_string();
        let _scan_worker = std::thread::Builder::new()
            .name("statsai-watch-scan".to_string())
            .spawn(move || {
                let mut retry_delay = WATCH_SCAN_INITIAL_RETRY_DELAY;
                while scan_signal_rx.recv().is_ok() {
                    let changed = worker_pending_scan_paths
                        .lock()
                        .map(|mut paths| {
                            std::mem::take(&mut *paths).into_iter().collect::<Vec<_>>()
                        })
                        .unwrap_or_else(|error| {
                            std::mem::take(&mut *error.into_inner())
                                .into_iter()
                                .collect::<Vec<_>>()
                        });
                    if changed.is_empty() {
                        continue;
                    }
                    let dependency_snapshot = worker_verification_dependencies
                        .read()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone();
                    let scan_succeeded = process_background_scan(
                        &worker_pending_scan_paths,
                        &worker_scan_signal_tx,
                        changed,
                        retry_delay,
                        |changed| match background_store.as_ref() {
                            Ok(store) => rescan_changed_sources(
                                store,
                                &worker_shared_store,
                                &worker_device_id,
                                changed,
                                &dependency_snapshot,
                            ),
                            Err(error) => {
                                eprintln!(
                                    "daemon: dedicated scan connection unavailable ({error:#}); using shared store"
                                );
                                let store = super::lock_store(&worker_shared_store);
                                let adapters: Vec<Box<dyn ProviderAdapter>> = default_adapters();
                                rescan_changed_sources_with_adapters_and_dependencies(
                                    &store,
                                    &worker_device_id,
                                    changed,
                                    &adapters,
                                    &dependency_snapshot,
                                )
                            }
                        },
                    );
                    retry_delay = if scan_succeeded {
                        WATCH_SCAN_INITIAL_RETRY_DELAY
                    } else {
                        retry_delay
                            .saturating_mul(2)
                            .min(WATCH_SCAN_MAX_RETRY_DELAY)
                    };
                }
            })
            .context("start background scan worker")?;

        let mut watched_sources = HashMap::new();
        let mut uncertain_watch_sources = HashSet::new();
        let initially_watched = reconcile_watch_sources(
            &mut watcher,
            &mut watched_sources,
            &mut uncertain_watch_sources,
            initial_sources,
        );
        enqueue_background_scan(&pending_scan_paths, &scan_signal_tx, initially_watched);
        let mut last_watch_source_refresh = Instant::now();

        eprintln!("daemon: API listening on http://{bind_addr}");
        let server = Server::http(bind_addr)
            .map_err(|err| anyhow::anyhow!("start local API on {bind_addr}: {err}"))?;

        loop {
            if startup_executable
                .as_ref()
                .is_some_and(executable_was_replaced)
            {
                eprintln!("daemon: executable changed on disk; restarting");
                return Ok(());
            }
            match watcher_signal_rx.recv_timeout(Duration::from_millis(250)) {
                Ok(()) => {
                    let changed = pending_changed_paths
                        .lock()
                        .map(|mut paths| {
                            std::mem::take(&mut *paths).into_iter().collect::<Vec<_>>()
                        })
                        .unwrap_or_else(|error| {
                            std::mem::take(&mut *error.into_inner())
                                .into_iter()
                                .collect::<Vec<_>>()
                        });
                    verification_dependency_cache.invalidate_changed(&watch_adapters, &changed);
                    enqueue_background_scan(&pending_scan_paths, &scan_signal_tx, changed);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if last_watch_source_refresh.elapsed() >= WATCH_SOURCE_REFRESH_INTERVAL {
                let configured_result = {
                    let s = super::lock_store(&store);
                    s.list_sources()
                };
                last_watch_source_refresh = Instant::now();
                match configured_result {
                    Ok(configured) => {
                        let desired_plan = discover_watch_plan(
                            &configured,
                            &watch_adapters,
                            &mut verification_dependency_cache,
                        );
                        *verification_dependencies
                            .write()
                            .unwrap_or_else(|error| error.into_inner()) =
                            desired_plan.verification_dependencies;
                        let newly_watched = reconcile_watch_sources(
                            &mut watcher,
                            &mut watched_sources,
                            &mut uncertain_watch_sources,
                            desired_plan.paths,
                        );
                        if !newly_watched.is_empty() {
                            enqueue_background_scan(
                                &pending_scan_paths,
                                &scan_signal_tx,
                                newly_watched,
                            );
                        }
                    }
                    Err(error) => {
                        eprintln!("daemon: watch source discovery failed: {error:#}");
                    }
                }
            }

            if let Ok(Some(request)) = server.try_recv() {
                if let Err(error) = super::handle_request(request, &store, auth_token) {
                    eprintln!("daemon: request failed: {error:#}");
                }
            }
        }

        Ok(())
    }

    fn enqueue_background_scan(
        pending: &Arc<Mutex<HashSet<PathBuf>>>,
        signal: &mpsc::SyncSender<()>,
        changed: Vec<PathBuf>,
    ) {
        pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend(changed);
        let _ = signal.try_send(());
    }

    fn process_background_scan(
        pending: &Arc<Mutex<HashSet<PathBuf>>>,
        signal: &mpsc::SyncSender<()>,
        changed: Vec<PathBuf>,
        retry_delay: Duration,
        scan: impl FnOnce(&[PathBuf]) -> Result<()>,
    ) -> bool {
        if let Err(error) = scan(&changed) {
            eprintln!("daemon: background scan failed and will be retried: {error:#}");
            std::thread::sleep(retry_delay);
            enqueue_background_scan(pending, signal, changed);
            return false;
        }
        true
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ExecutableStamp {
        path: PathBuf,
        len: u64,
        modified: Option<SystemTime>,
    }

    fn executable_stamp(path: &Path) -> Option<ExecutableStamp> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(ExecutableStamp {
            path: path.to_path_buf(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    fn current_executable_stamp() -> Option<ExecutableStamp> {
        let path = std::env::current_exe().ok()?;
        executable_stamp(&path)
    }

    fn executable_was_replaced(startup: &ExecutableStamp) -> bool {
        executable_stamp(&startup.path).as_ref() != Some(startup)
    }

    fn discover_watch_plan(
        configured: &[SourceLocation],
        adapters: &[Box<dyn ProviderAdapter>],
        dependency_cache: &mut VerificationDependencyCache,
    ) -> WatchPlan {
        let mut plan = WatchPlan::default();
        for adapter in adapters {
            for source in watch_sources_for_adapter(adapter.as_ref(), configured) {
                let dependencies = dependency_cache.paths_for(adapter.as_ref(), &source);
                extend_source_watch_paths(&mut plan.paths, &source, &dependencies);
                plan.verification_dependencies
                    .paths_by_source
                    .insert(source.source_id.clone(), dependencies);
            }
        }

        for source in configured.iter().filter(|source| {
            source.enabled
                && source.source_kind == SourceKind::LocalAdapter
                && !adapters
                    .iter()
                    .any(|adapter| source_matches_adapter(source, adapter.as_ref()))
        }) {
            extend_source_root_watch_path(&mut plan.paths, source);
        }

        dependency_cache.retain_snapshot(&plan.verification_dependencies);
        plan
    }

    fn watch_sources_for_adapter(
        adapter: &dyn ProviderAdapter,
        configured: &[SourceLocation],
    ) -> Vec<SourceLocation> {
        let configured = configured
            .iter()
            .filter(|source| {
                source.source_kind == SourceKind::LocalAdapter
                    && source_matches_adapter(source, adapter)
            })
            .collect::<Vec<_>>();
        let mut sources = adapter
            .discover()
            .into_iter()
            .filter(|source| {
                source.enabled
                    && source.source_kind == SourceKind::LocalAdapter
                    && source.path_label.is_some()
                    && !configured
                        .iter()
                        .any(|configured| sources_refer_to_same_location(source, configured))
            })
            .collect::<Vec<_>>();
        for source in configured
            .into_iter()
            .filter(|source| source.enabled)
            .cloned()
        {
            if !sources
                .iter()
                .any(|existing| existing.source_id == source.source_id)
            {
                sources.push(source);
            }
        }
        sources
    }

    fn source_matches_adapter(source: &SourceLocation, adapter: &dyn ProviderAdapter) -> bool {
        adapter_for_provider(&source.provider)
            .is_some_and(|canonical| canonical.provider() == adapter.provider())
    }

    fn sources_refer_to_same_location(left: &SourceLocation, right: &SourceLocation) -> bool {
        if left.source_kind != right.source_kind
            || adapter_for_provider(&left.provider)
                .zip(adapter_for_provider(&right.provider))
                .is_none_or(|(left, right)| left.provider() != right.provider())
        {
            return false;
        }
        if left.source_id == right.source_id
            || left
                .path_hash
                .as_deref()
                .zip(right.path_hash.as_deref())
                .is_some_and(|(left, right)| left == right)
        {
            return true;
        }
        left.path_label
            .as_deref()
            .zip(right.path_label.as_deref())
            .is_some_and(|(left, right)| {
                let left = PathBuf::from(left);
                let right = PathBuf::from(right);
                std::fs::canonicalize(&left).unwrap_or(left)
                    == std::fs::canonicalize(&right).unwrap_or(right)
            })
    }

    fn verification_dependency_source_matches(
        left: &SourceLocation,
        right: &SourceLocation,
    ) -> bool {
        left.provider == right.provider
            && left.source_kind == right.source_kind
            && left.location_origin == right.location_origin
            && left.path_hash == right.path_hash
            && left.path_label == right.path_label
    }

    fn extend_source_root_watch_path(
        paths: &mut HashMap<PathBuf, WatchScope>,
        source: &SourceLocation,
    ) {
        if let Some(label) = source.path_label.as_deref().filter(|path| !path.is_empty()) {
            let path = PathBuf::from(label);
            if path.is_dir() {
                insert_watch_path(paths, path, WatchScope::Recursive);
            }
        }
    }

    fn extend_source_watch_paths(
        paths: &mut HashMap<PathBuf, WatchScope>,
        source: &SourceLocation,
        dependencies: &[PathBuf],
    ) {
        extend_source_root_watch_path(paths, source);
        for dependency in dependencies {
            if dependency.is_dir() {
                insert_watch_path(paths, dependency.clone(), WatchScope::Recursive);
                continue;
            }
            if let Some(parent) = dependency
                .ancestors()
                .skip(1)
                .find(|ancestor| ancestor.is_dir())
            {
                insert_watch_path(paths, parent.to_path_buf(), WatchScope::Direct);
            }
        }
    }

    fn insert_watch_path(
        paths: &mut HashMap<PathBuf, WatchScope>,
        path: PathBuf,
        scope: WatchScope,
    ) {
        paths
            .entry(path)
            .and_modify(|current| {
                if matches!(scope, WatchScope::Recursive) {
                    *current = WatchScope::Recursive;
                }
            })
            .or_insert(scope);
    }

    fn reconcile_watch_sources<W: Watcher>(
        watcher: &mut W,
        watched: &mut HashMap<PathBuf, WatchScope>,
        uncertain: &mut HashSet<PathBuf>,
        desired: HashMap<PathBuf, WatchScope>,
    ) -> Vec<PathBuf> {
        let mut removed = watched
            .iter()
            .filter(|(path, scope)| desired.get(*path) != Some(*scope))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        removed.sort();
        for path in removed {
            if let Err(error) = watcher.unwatch(&path) {
                eprintln!("daemon: cannot stop watching {}: {error}", path.display());
                uncertain.insert(path);
            } else {
                eprintln!("daemon: stopped watching {}", path.display());
                watched.remove(&path);
                uncertain.remove(&path);
            }
        }

        let mut additions = desired
            .iter()
            .filter(|(path, scope)| watched.get(*path) != Some(*scope) || uncertain.contains(*path))
            .map(|(path, scope)| (path.clone(), *scope))
            .collect::<Vec<_>>();
        additions.sort_by(|left, right| left.0.cmp(&right.0));
        let mut newly_watched = Vec::with_capacity(additions.len());
        for (path, scope) in additions {
            if let Err(error) = watcher.watch(&path, scope.notify_mode()) {
                eprintln!("daemon: cannot watch {}: {error}", path.display());
            } else {
                eprintln!("daemon: watching {}", path.display());
                watched.insert(path.clone(), scope);
                uncertain.remove(&path);
                newly_watched.push(path);
            }
        }
        newly_watched
    }

    fn rescan_changed_sources(
        scan_store: &Store,
        commit_store: &Arc<Mutex<Store>>,
        device_id: &str,
        changed: &[PathBuf],
        verification_dependencies: &VerificationDependencySnapshot,
    ) -> Result<()> {
        let adapters: Vec<Box<dyn ProviderAdapter>> = default_adapters();
        rescan_changed_sources_with_adapters_and_commit_store_and_dependencies(
            scan_store,
            Some(commit_store),
            device_id,
            changed,
            &adapters,
            verification_dependencies,
        )
    }

    #[cfg(test)]
    fn rescan_changed_sources_with_adapters(
        store: &Store,
        device_id: &str,
        changed: &[PathBuf],
        adapters: &[Box<dyn ProviderAdapter>],
    ) -> Result<()> {
        rescan_changed_sources_with_adapters_and_dependencies(
            store,
            device_id,
            changed,
            adapters,
            &VerificationDependencySnapshot::default(),
        )
    }

    fn rescan_changed_sources_with_adapters_and_dependencies(
        store: &Store,
        device_id: &str,
        changed: &[PathBuf],
        adapters: &[Box<dyn ProviderAdapter>],
        verification_dependencies: &VerificationDependencySnapshot,
    ) -> Result<()> {
        rescan_changed_sources_with_adapters_and_commit_store_and_dependencies(
            store,
            None,
            device_id,
            changed,
            adapters,
            verification_dependencies,
        )
    }

    #[cfg(test)]
    fn rescan_changed_sources_with_adapters_and_commit_store(
        scan_store: &Store,
        commit_store: Option<&Arc<Mutex<Store>>>,
        device_id: &str,
        changed: &[PathBuf],
        adapters: &[Box<dyn ProviderAdapter>],
    ) -> Result<()> {
        rescan_changed_sources_with_adapters_and_commit_store_and_dependencies(
            scan_store,
            commit_store,
            device_id,
            changed,
            adapters,
            &VerificationDependencySnapshot::default(),
        )
    }

    fn rescan_changed_sources_with_adapters_and_commit_store_and_dependencies(
        scan_store: &Store,
        commit_store: Option<&Arc<Mutex<Store>>>,
        device_id: &str,
        changed: &[PathBuf],
        adapters: &[Box<dyn ProviderAdapter>],
        verification_dependencies: &VerificationDependencySnapshot,
    ) -> Result<()> {
        let configured = scan_store
            .list_sources()
            .context("list sources for changed-source rescan")?;
        let mut failed = false;

        for adapter in adapters {
            let sources = scan_sources_for_paths(
                adapter.as_ref(),
                &configured,
                changed,
                verification_dependencies,
            );
            for mut source in sources {
                let expected_data_version = commit_store
                    .map(|_| scan_store.data_version())
                    .transpose()
                    .context("capture database generation for changed-source rescan")?;
                let expected_source = configured
                    .iter()
                    .find(|configured_source| configured_source.source_id == source.source_id)
                    .cloned();
                let cache_candidates = match adapter.scan_candidates(&source) {
                    Ok(candidates) => candidates,
                    Err(e) => {
                        eprintln!(
                            "daemon: scan candidate discovery failed for {}: {e}",
                            source.path_label.as_deref().unwrap_or("unknown")
                        );
                        failed = true;
                        continue;
                    }
                };
                let compatible_scan_signatures =
                    scan_candidate_compatible_signatures(&cache_candidates);
                let file_cache_entries = scan_file_state_entries(&cache_candidates);
                let selection = match scan_store
                    .select_scan_file_state_entries_with_task_requirement_and_compatibility(
                        &source.source_id,
                        &file_cache_entries,
                        false,
                        &compatible_scan_signatures,
                    ) {
                    Ok(selection) => selection,
                    Err(e) => {
                        eprintln!(
                            "daemon: scan cache lookup failed for {}: {e}",
                            source.path_label.as_deref().unwrap_or("unknown")
                        );
                        failed = true;
                        continue;
                    }
                };
                let pending_file_entries = selection.pending_entries;
                let compatible_entries_to_upgrade = selection.compatible_entries_to_upgrade;
                let tracked_file_entries = match scan_store.scan_file_entries(&source.source_id) {
                    Ok(entries) => entries,
                    Err(e) => {
                        eprintln!("daemon: scan cache listing failed: {e}");
                        failed = true;
                        continue;
                    }
                };
                let current_cache_keys = file_cache_entries
                    .iter()
                    .map(|entry| entry.cache_key.as_str())
                    .collect::<HashSet<_>>();
                let removed_file_entries = tracked_file_entries
                    .into_iter()
                    .filter(|entry| !current_cache_keys.contains(entry.cache_key.as_str()))
                    .collect::<Vec<_>>();
                let has_cache_entry_upgrades = !compatible_entries_to_upgrade.is_empty();
                let verification_mode = source.verification_mode.clone();
                let probed_verified_source_state =
                    if matches!(verification_mode, SourceVerificationMode::Disabled) {
                        VerifiedSourceObservation::Unavailable
                    } else {
                        match adapter.probe_verified_source_state(&source) {
                            Ok(state) => state,
                            Err(e) => {
                                eprintln!(
                                    "daemon: verified auth probe failed for {}: {e}",
                                    source.path_label.as_deref().unwrap_or("unknown")
                                );
                                failed = true;
                                continue;
                            }
                        }
                    };
                let next_verified_state_hash =
                    if matches!(verification_mode, SourceVerificationMode::Auto) {
                        match &probed_verified_source_state {
                            VerifiedSourceObservation::Unavailable => {
                                source.verified_state_hash.clone()
                            }
                            observation => match verified_source_observation_hash(observation) {
                                Ok(hash) => hash,
                                Err(e) => {
                                    eprintln!(
                                        "daemon: verified auth hash failed for {}: {e}",
                                        source.path_label.as_deref().unwrap_or("unknown")
                                    );
                                    failed = true;
                                    continue;
                                }
                            },
                        }
                    } else {
                        None
                    };
                let verified_state_changed =
                    matches!(verification_mode, SourceVerificationMode::Auto)
                        && source.verified_state_hash != next_verified_state_hash;
                let rescan_file_entries = if removed_file_entries.is_empty() {
                    &pending_file_entries
                } else {
                    &file_cache_entries
                };
                if pending_file_entries.is_empty()
                    && removed_file_entries.is_empty()
                    && !has_cache_entry_upgrades
                    && !verified_state_changed
                {
                    continue;
                }
                let options = ScanOptions {
                    device_id: device_id.to_string(),
                    collect_tasks: false,
                    selected_cache_keys: Some(
                        rescan_file_entries
                            .iter()
                            .map(|entry| entry.cache_key.clone())
                            .collect::<HashSet<_>>(),
                    ),
                };
                let scan_result = if rescan_file_entries.is_empty() {
                    Ok(statsai_adapters::AdapterScan::default())
                } else {
                    adapter.scan(&source, &options)
                };
                match scan_result {
                    Ok(mut scan) => {
                        let parsed_events = scan.events.len();
                        let parsed_summaries = scan.summaries.len();
                        let effective_verified_source_state =
                            if matches!(verification_mode, SourceVerificationMode::Disabled) {
                                VerifiedSourceObservation::Unavailable
                            } else if rescan_file_entries.is_empty() {
                                probed_verified_source_state
                            } else {
                                scan.verified_source_state
                                    .take()
                                    .map(Box::new)
                                    .map(VerifiedSourceObservation::Verified)
                                    .unwrap_or(probed_verified_source_state)
                            };
                        let effective_verified_state_hash =
                            if matches!(verification_mode, SourceVerificationMode::Auto) {
                                match &effective_verified_source_state {
                                    VerifiedSourceObservation::Unavailable => {
                                        source.verified_state_hash.clone()
                                    }
                                    observation => {
                                        match verified_source_observation_hash(observation) {
                                            Ok(hash) => hash,
                                            Err(e) => {
                                                eprintln!(
                                                    "daemon: verified auth hash failed for {}: {e}",
                                                    source
                                                        .path_label
                                                        .as_deref()
                                                        .unwrap_or("unknown")
                                                );
                                                failed = true;
                                                continue;
                                            }
                                        }
                                    }
                                }
                            } else {
                                None
                            };
                        let reconciled_file_hashes = rescan_file_entries
                            .iter()
                            .chain(removed_file_entries.iter())
                            .map(|entry| hash_text(&entry.cache_key))
                            .collect::<HashSet<_>>()
                            .into_iter()
                            .collect::<Vec<_>>();
                        let removed_cache_keys = removed_file_entries
                            .iter()
                            .map(|entry| entry.cache_key.clone())
                            .collect::<Vec<_>>();
                        let commit_result = commit_source_scan_if_current(
                            scan_store,
                            commit_store,
                            expected_data_version,
                            expected_source.as_ref(),
                            &mut source,
                            |store, source| {
                                reconcile_verified_source_state(
                                    store,
                                    source,
                                    &effective_verified_source_state,
                                    effective_verified_state_hash,
                                )
                                .context("reconcile verified auth state")?;
                                store
                                    .upsert_source(source)
                                    .context("update source verified auth state")?;
                                if pending_file_entries.is_empty()
                                    && removed_file_entries.is_empty()
                                {
                                    store
                                        .upgrade_scan_file_entries(
                                            &source.source_id,
                                            &compatible_entries_to_upgrade,
                                        )
                                        .context("upgrade scan cache")?;
                                    return Ok(None);
                                }
                                apply_source_account_resolution(
                                    store,
                                    source,
                                    &mut scan.events,
                                    &mut scan.summaries,
                                )
                                .context("resolve source accounts")?;
                                let replacement = store
                                    .replace_scan_file_records(ScanFileReplacement {
                                        source_id: &source.source_id,
                                        reconciled_file_hashes: &reconciled_file_hashes,
                                        events: &scan.events,
                                        summaries: &scan.summaries,
                                        pending_entries: &pending_file_entries,
                                        compatible_entries_to_upgrade:
                                            &compatible_entries_to_upgrade,
                                        removed_cache_keys: &removed_cache_keys,
                                    })
                                    .context("atomically reconcile scan files")?;
                                Ok(Some(replacement))
                            },
                        );
                        match commit_result {
                            Ok(Some(replacement)) => {
                                eprintln!(
                                    "daemon: rescanned {} ({}) — files={}, cached={}, parsed_events={}, inserted_events={}, parsed_summaries={}, summaries_written={}",
                                    source.provider,
                                    source.path_label.as_deref().unwrap_or("unknown"),
                                    scan.diagnostics.files_scanned,
                                    scan.diagnostics.files_skipped_unchanged,
                                    parsed_events,
                                    replacement.inserted_events,
                                    parsed_summaries,
                                    replacement.written_summaries
                                );
                            }
                            Ok(None) => {
                                eprintln!(
                                    "daemon: reconciled auth/cache state for {} ({})",
                                    source.provider,
                                    source.path_label.as_deref().unwrap_or("unknown")
                                );
                            }
                            Err(error) => {
                                return Err(error).with_context(|| {
                                    format!(
                                        "commit changed-source scan for {}",
                                        source.path_label.as_deref().unwrap_or("unknown")
                                    )
                                });
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "daemon: scan failed for {}: {e}",
                            source.path_label.as_deref().unwrap_or("unknown")
                        );
                        failed = true;
                    }
                }
            }
        }

        if failed {
            anyhow::bail!("one or more changed sources could not be rescanned");
        }
        Ok(())
    }

    fn commit_source_scan_if_current<T>(
        scan_store: &Store,
        commit_store: Option<&Arc<Mutex<Store>>>,
        expected_data_version: Option<i64>,
        expected_source: Option<&SourceLocation>,
        source: &mut SourceLocation,
        commit: impl FnOnce(&Store, &mut SourceLocation) -> Result<T>,
    ) -> Result<T> {
        let Some(commit_store) = commit_store else {
            return commit(scan_store, source);
        };

        let store = super::lock_store(commit_store);
        let expected_data_version = expected_data_version
            .context("missing database generation for independent scan connection")?;
        store.apply_scan_update(|store| {
            let current_data_version = scan_store
                .data_version()
                .context("verify database generation before scan commit")?;
            if current_data_version != expected_data_version {
                anyhow::bail!(
                    "database changed while scanning (expected generation {}, found {})",
                    expected_data_version,
                    current_data_version
                );
            }

            let current_source = store
                .source(&source.source_id)
                .context("re-read source before scan commit")?;
            if current_source.as_ref() != expected_source {
                anyhow::bail!("source changed while scanning");
            }
            if let Some(current_source) = current_source {
                *source = current_source;
            }

            commit(store, source)
        })
    }

    fn scan_sources_for_paths(
        adapter: &dyn ProviderAdapter,
        configured: &[SourceLocation],
        changed: &[PathBuf],
        verification_dependencies: &VerificationDependencySnapshot,
    ) -> Vec<SourceLocation> {
        watch_sources_for_adapter(adapter, configured)
            .into_iter()
            .filter(|source| {
                source_in_changed_paths(
                    source,
                    changed,
                    verification_dependencies.paths_for(source),
                )
            })
            .collect()
    }

    fn source_in_changed_paths(
        source: &SourceLocation,
        changed: &[PathBuf],
        verification_dependencies: &[PathBuf],
    ) -> bool {
        let Some(label) = source.path_label.as_deref() else {
            return false;
        };
        std::iter::once(PathBuf::from(label))
            .chain(verification_dependencies.iter().cloned())
            .any(|dependency| {
                changed.iter().any(|changed_path| {
                    changed_path.starts_with(&dependency) || dependency.starts_with(changed_path)
                })
            })
    }

    fn scan_file_state_entries(candidates: &[ScanCandidateFile]) -> Vec<ScanFileStateEntry> {
        candidates
            .iter()
            .map(|candidate| ScanFileStateEntry {
                cache_key: candidate.cache_key.clone(),
                cache_signature: candidate.cache_signature.clone(),
            })
            .collect()
    }

    fn scan_candidate_compatible_signatures(
        candidates: &[ScanCandidateFile],
    ) -> HashMap<String, Vec<String>> {
        candidates
            .iter()
            .filter(|candidate| !candidate.compatible_cache_signatures.is_empty())
            .map(|candidate| {
                (
                    candidate.cache_key.clone(),
                    candidate.compatible_cache_signatures.clone(),
                )
            })
            .collect()
    }

    fn apply_source_account_resolution(
        store: &Store,
        source: &SourceLocation,
        events: &mut [UsageEvent],
        summaries: &mut [UsageSummary],
    ) -> Result<()> {
        let assignments = store.list_source_account_assignments_for_source(&source.source_id)?;
        for event in events {
            apply_account_resolution_to_event(&assignments, event);
        }
        for summary in summaries {
            apply_account_resolution_to_summary(&assignments, summary);
        }
        Ok(())
    }

    fn apply_account_resolution_to_event(
        assignments: &[SourceAccountAssignment],
        event: &mut UsageEvent,
    ) {
        if keep_detected_account_identity(
            event.provider_account_id.as_ref(),
            event
                .parse_evidence
                .as_ref()
                .map(|evidence| &evidence.account_identity_source),
        ) {
            return;
        }
        let assignment = assignment_for_timestamp(assignments, event.session.started_at);
        if let Some(assignment) = assignment {
            event.provider_account_id = Some(assignment.provider_account_id.clone());
            if let Some(evidence) = event.parse_evidence.as_mut() {
                evidence.account_identity_source = IdentitySource::SourceConfig;
            }
        } else if should_clear_resolved_account(
            event.provider_account_id.as_ref(),
            event
                .parse_evidence
                .as_ref()
                .map(|evidence| &evidence.account_identity_source),
        ) {
            event.provider_account_id = None;
            if let Some(evidence) = event.parse_evidence.as_mut() {
                evidence.account_identity_source = IdentitySource::Unresolved;
            }
        }
    }

    fn apply_account_resolution_to_summary(
        assignments: &[SourceAccountAssignment],
        summary: &mut UsageSummary,
    ) {
        if keep_detected_account_identity(
            summary.provider_account_id.as_ref(),
            summary
                .parse_evidence
                .as_ref()
                .map(|evidence| &evidence.account_identity_source),
        ) {
            return;
        }
        let timestamp = summary.period_start.unwrap_or(summary.observed_at);
        let assignment = assignment_for_timestamp(assignments, timestamp);
        if let Some(assignment) = assignment {
            summary.provider_account_id = Some(assignment.provider_account_id.clone());
            if let Some(evidence) = summary.parse_evidence.as_mut() {
                evidence.account_identity_source = IdentitySource::SourceConfig;
            }
        } else if should_clear_resolved_account(
            summary.provider_account_id.as_ref(),
            summary
                .parse_evidence
                .as_ref()
                .map(|evidence| &evidence.account_identity_source),
        ) {
            summary.provider_account_id = None;
            if let Some(evidence) = summary.parse_evidence.as_mut() {
                evidence.account_identity_source = IdentitySource::Unresolved;
            }
        }
    }

    fn keep_detected_account_identity(
        provider_account_id: Option<&ProviderAccountId>,
        identity_source: Option<&IdentitySource>,
    ) -> bool {
        let Some(provider_account_id) = provider_account_id else {
            return false;
        };
        if provider_account_id.0.trim().is_empty() {
            return false;
        }
        let Some(identity_source) = identity_source else {
            return false;
        };
        !matches!(
            identity_source,
            IdentitySource::SourceConfig
                | IdentitySource::UserConfigured
                | IdentitySource::ManualHint
                | IdentitySource::Unknown
                | IdentitySource::Unresolved
        )
    }

    fn should_clear_resolved_account(
        provider_account_id: Option<&ProviderAccountId>,
        identity_source: Option<&IdentitySource>,
    ) -> bool {
        let Some(provider_account_id) = provider_account_id else {
            return false;
        };
        if provider_account_id.0.trim().is_empty() {
            return false;
        }
        matches!(
            identity_source,
            None | Some(
                IdentitySource::SourceConfig
                    | IdentitySource::UserConfigured
                    | IdentitySource::ManualHint
                    | IdentitySource::Unknown
                    | IdentitySource::Unresolved
            )
        )
    }

    fn assignment_for_timestamp(
        assignments: &[SourceAccountAssignment],
        timestamp: DateTime<Utc>,
    ) -> Option<&SourceAccountAssignment> {
        assignments
            .iter()
            .filter(|assignment| {
                timestamp_in_period(timestamp, assignment.started_at, assignment.ended_at)
            })
            .max_by(|left, right| left.started_at.cmp(&right.started_at))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use chrono::TimeZone;
        use notify::{Config, Error as NotifyError, EventHandler, WatcherKind};
        use statsai_core::{
            BillingPeriod, LocationOrigin, SubscriptionStatus, VerifiedSourceState,
            VerifiedSubscriptionState,
        };
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct RecordingWatcher {
            watched: HashSet<PathBuf>,
            recursive: HashMap<PathBuf, bool>,
            rejected: HashSet<PathBuf>,
            rejected_unwatch: HashSet<PathBuf>,
        }

        impl Watcher for RecordingWatcher {
            fn new<F: EventHandler>(_event_handler: F, _config: Config) -> notify::Result<Self> {
                Ok(Self::default())
            }

            fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
                if self.rejected.contains(path) {
                    return Err(NotifyError::generic("rejected test path"));
                }
                self.watched.insert(path.to_path_buf());
                self.recursive.insert(
                    path.to_path_buf(),
                    matches!(recursive_mode, RecursiveMode::Recursive),
                );
                Ok(())
            }

            fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
                if self.rejected_unwatch.contains(path) {
                    return Err(NotifyError::generic("rejected test unwatch path"));
                }
                self.watched.remove(path);
                self.recursive.remove(path);
                Ok(())
            }

            fn kind() -> WatcherKind {
                WatcherKind::NullWatcher
            }
        }

        struct RoutingTestAdapter {
            provider: &'static str,
            discovered: Vec<SourceLocation>,
            verification_dependencies: Vec<PathBuf>,
            dependency_topology_change: Option<PathBuf>,
            dependency_calls: Arc<AtomicU64>,
        }

        impl ProviderAdapter for RoutingTestAdapter {
            fn id(&self) -> &'static str {
                "test-routing-adapter"
            }

            fn version(&self) -> &'static str {
                "0.0.0"
            }

            fn provider(&self) -> &'static str {
                self.provider
            }

            fn discover(&self) -> Vec<SourceLocation> {
                self.discovered.clone()
            }

            fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
                Ok(Vec::new())
            }

            fn verification_dependency_paths(&self, _source: &SourceLocation) -> Vec<PathBuf> {
                self.dependency_calls.fetch_add(1, Ordering::Relaxed);
                self.verification_dependencies.clone()
            }

            fn verification_dependency_paths_changed(
                &self,
                _source: &SourceLocation,
                changed: &[PathBuf],
            ) -> bool {
                self.dependency_topology_change
                    .as_ref()
                    .is_some_and(|topology_change| changed.contains(topology_change))
            }

            fn scan(
                &self,
                _source: &SourceLocation,
                _options: &ScanOptions,
            ) -> Result<statsai_adapters::AdapterScan> {
                Ok(statsai_adapters::AdapterScan::default())
            }
        }

        #[test]
        fn executable_stamp_detects_replaced_binary() {
            let dir = tempfile::tempdir().expect("tempdir");
            let binary = dir.path().join("statsai");
            std::fs::write(&binary, b"old").expect("old binary");
            let startup = executable_stamp(&binary).expect("startup stamp");

            assert!(!executable_was_replaced(&startup));
            std::fs::write(&binary, b"new-binary").expect("new binary");
            assert!(executable_was_replaced(&startup));
        }

        #[test]
        fn watch_source_reconciliation_adds_removes_and_retries_sources() {
            let first = PathBuf::from("/tmp/statsai-watch-first");
            let second = PathBuf::from("/tmp/statsai-watch-second");
            let mut watcher = RecordingWatcher::default();
            let mut watched = HashMap::new();
            let mut uncertain = HashSet::new();

            let added = reconcile_watch_sources(
                &mut watcher,
                &mut watched,
                &mut uncertain,
                HashMap::from([(first.clone(), WatchScope::Recursive)]),
            );
            assert_eq!(added, vec![first.clone()]);
            assert_eq!(
                watched,
                HashMap::from([(first.clone(), WatchScope::Recursive)])
            );

            watcher.rejected_unwatch.insert(first.clone());
            watcher.rejected.insert(second.clone());
            let added = reconcile_watch_sources(
                &mut watcher,
                &mut watched,
                &mut uncertain,
                HashMap::from([(second.clone(), WatchScope::Recursive)]),
            );
            assert!(added.is_empty());
            assert_eq!(
                watched,
                HashMap::from([(first.clone(), WatchScope::Recursive)])
            );
            assert_eq!(uncertain, HashSet::from([first.clone()]));
            assert!(watcher.watched.contains(&first));

            let added = reconcile_watch_sources(
                &mut watcher,
                &mut watched,
                &mut uncertain,
                HashMap::from([(first.clone(), WatchScope::Recursive)]),
            );
            assert_eq!(added, vec![first.clone()]);
            assert!(uncertain.is_empty());

            watcher.rejected_unwatch.remove(&first);
            let added = reconcile_watch_sources(
                &mut watcher,
                &mut watched,
                &mut uncertain,
                HashMap::from([(second.clone(), WatchScope::Recursive)]),
            );
            assert!(added.is_empty());
            assert!(watched.is_empty());
            assert!(!watcher.watched.contains(&first));

            watcher.rejected.remove(&second);
            let added = reconcile_watch_sources(
                &mut watcher,
                &mut watched,
                &mut uncertain,
                HashMap::from([(second.clone(), WatchScope::Recursive)]),
            );
            assert_eq!(added, vec![second.clone()]);
            assert_eq!(watched, HashMap::from([(second, WatchScope::Recursive)]));
        }

        #[test]
        fn disabled_configured_source_suppresses_matching_discovered_watch_scan() {
            let root = tempfile::tempdir().expect("source root");
            let discovered = SourceLocation::local_adapter(
                "claude_code",
                "test",
                "0",
                root.path(),
                LocationOrigin::Default,
            );
            let mut disabled = SourceLocation::local_adapter(
                "claude_code",
                "test",
                "0",
                root.path(),
                LocationOrigin::Configured,
            );
            disabled.enabled = false;
            let adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(RoutingTestAdapter {
                provider: "claude_code",
                discovered: vec![discovered],
                verification_dependencies: Vec::new(),
                dependency_topology_change: None,
                dependency_calls: Arc::new(AtomicU64::new(0)),
            })];
            let mut dependency_cache = VerificationDependencyCache::default();
            let watch_plan = discover_watch_plan(
                std::slice::from_ref(&disabled),
                &adapters,
                &mut dependency_cache,
            );

            let sources = scan_sources_for_paths(
                adapters[0].as_ref(),
                &[disabled],
                std::slice::from_ref(&root.path().to_path_buf()),
                &watch_plan.verification_dependencies,
            );

            assert!(watch_plan.paths.is_empty());
            assert!(sources.is_empty());
        }

        #[test]
        fn watch_path_build_and_event_routing_probe_dependencies_once() {
            let dir = tempfile::tempdir().expect("tempdir");
            let source_root = dir.path().join("source");
            let external_root = dir.path().join("external");
            let external_profile = external_root.join(".claude.json");
            std::fs::create_dir_all(&source_root).expect("source root");
            std::fs::create_dir_all(&external_root).expect("external root");
            std::fs::write(&external_profile, "{}").expect("external profile");
            let source = SourceLocation::local_adapter(
                "claude_code",
                "test",
                "0",
                &source_root,
                LocationOrigin::Configured,
            );
            let dependency_calls = Arc::new(AtomicU64::new(0));
            let session_index = source_root
                .join("projects")
                .join("workspace")
                .join("sessions-index.json");
            let adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(RoutingTestAdapter {
                provider: "claude_code",
                discovered: Vec::new(),
                verification_dependencies: vec![external_profile.clone()],
                dependency_topology_change: Some(session_index.clone()),
                dependency_calls: Arc::clone(&dependency_calls),
            })];

            let mut dependency_cache = VerificationDependencyCache::default();
            let initial_watch_plan = discover_watch_plan(
                std::slice::from_ref(&source),
                &adapters,
                &mut dependency_cache,
            );
            let refreshed_watch_plan = discover_watch_plan(
                std::slice::from_ref(&source),
                &adapters,
                &mut dependency_cache,
            );
            let matching_sources = scan_sources_for_paths(
                adapters[0].as_ref(),
                std::slice::from_ref(&source),
                std::slice::from_ref(&external_profile),
                &refreshed_watch_plan.verification_dependencies,
            );

            assert_eq!(
                initial_watch_plan.paths.get(&source_root),
                Some(&WatchScope::Recursive)
            );
            assert_eq!(
                refreshed_watch_plan.paths.get(&external_root),
                Some(&WatchScope::Direct)
            );
            assert_eq!(matching_sources, vec![source.clone()]);
            assert_eq!(dependency_calls.load(Ordering::Relaxed), 1);

            dependency_cache.invalidate_changed(&adapters, std::slice::from_ref(&session_index));
            let _ = discover_watch_plan(
                std::slice::from_ref(&source),
                &adapters,
                &mut dependency_cache,
            );
            assert_eq!(dependency_calls.load(Ordering::Relaxed), 2);
        }

        #[test]
        fn external_verification_dependency_is_watched_and_routes_to_its_source() {
            let dir = tempfile::tempdir().expect("tempdir");
            let source_root = dir.path().join("source");
            let external_root = dir.path().join("external");
            let external_profile = external_root.join(".claude.json");
            std::fs::create_dir_all(&source_root).expect("source root");
            std::fs::create_dir_all(&external_root).expect("external root");
            std::fs::write(&external_profile, "{}").expect("external profile");
            let source = SourceLocation::local_adapter(
                "claude_code",
                "test",
                "0",
                &source_root,
                LocationOrigin::Configured,
            );
            let adapter = TestAdapter {
                provider: "claude_code",
                verified_observation: VerifiedSourceObservation::Unavailable,
                verification_dependencies: vec![external_profile.clone()],
                scan_calls: Arc::new(Mutex::new(0)),
            };

            let dependencies = adapter.verification_dependency_paths(&source);
            let verification_dependencies = VerificationDependencySnapshot {
                paths_by_source: HashMap::from([(source.source_id.clone(), dependencies.clone())]),
            };
            let mut watch_paths = HashMap::new();
            extend_source_watch_paths(&mut watch_paths, &source, &dependencies);
            let matching_sources = scan_sources_for_paths(
                &adapter,
                std::slice::from_ref(&source),
                std::slice::from_ref(&external_profile),
                &verification_dependencies,
            );

            assert_eq!(watch_paths.get(&source_root), Some(&WatchScope::Recursive));
            assert_eq!(watch_paths.get(&external_root), Some(&WatchScope::Direct));
            assert_eq!(matching_sources, vec![source]);

            let mut watcher = RecordingWatcher::default();
            let mut watched = HashMap::new();
            let mut uncertain = HashSet::new();
            reconcile_watch_sources(&mut watcher, &mut watched, &mut uncertain, watch_paths);
            assert_eq!(watcher.recursive.get(&source_root), Some(&true));
            assert_eq!(watcher.recursive.get(&external_root), Some(&false));
        }

        #[test]
        fn background_scan_queue_coalesces_paths_without_dropping_them() {
            let pending = Arc::new(Mutex::new(HashSet::new()));
            let (signal_tx, signal_rx) = mpsc::sync_channel(1);
            let first = PathBuf::from("/tmp/statsai-scan-first");
            let second = PathBuf::from("/tmp/statsai-scan-second");

            enqueue_background_scan(&pending, &signal_tx, vec![first.clone()]);
            enqueue_background_scan(&pending, &signal_tx, vec![second.clone()]);

            signal_rx.try_recv().expect("one coalesced wakeup");
            assert!(signal_rx.try_recv().is_err());
            assert_eq!(
                *pending.lock().expect("pending scan paths"),
                HashSet::from([first, second])
            );
        }

        #[test]
        fn failed_background_scan_is_requeued_for_retry() {
            let pending = Arc::new(Mutex::new(HashSet::new()));
            let (signal_tx, signal_rx) = mpsc::sync_channel(1);
            let changed = PathBuf::from("/tmp/statsai-scan-retry");

            let scan_succeeded = process_background_scan(
                &pending,
                &signal_tx,
                vec![changed.clone()],
                Duration::ZERO,
                |_| anyhow::bail!("database is locked"),
            );

            assert!(!scan_succeeded);
            signal_rx.try_recv().expect("retry wakeup");
            assert_eq!(
                *pending.lock().expect("pending scan paths"),
                HashSet::from([changed])
            );
        }

        struct TestAdapter {
            provider: &'static str,
            verified_observation: VerifiedSourceObservation,
            verification_dependencies: Vec<PathBuf>,
            scan_calls: Arc<Mutex<u64>>,
        }

        impl ProviderAdapter for TestAdapter {
            fn id(&self) -> &'static str {
                "test-watch-adapter"
            }

            fn version(&self) -> &'static str {
                "0.0.0"
            }

            fn provider(&self) -> &'static str {
                self.provider
            }

            fn discover(&self) -> Vec<SourceLocation> {
                Vec::new()
            }

            fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
                Ok(Vec::new())
            }

            fn probe_verified_source_state(
                &self,
                _source: &SourceLocation,
            ) -> Result<VerifiedSourceObservation> {
                Ok(self.verified_observation.clone())
            }

            fn verification_dependency_paths(&self, _source: &SourceLocation) -> Vec<PathBuf> {
                self.verification_dependencies.clone()
            }

            fn scan(
                &self,
                _source: &SourceLocation,
                _options: &ScanOptions,
            ) -> Result<statsai_adapters::AdapterScan> {
                *self.scan_calls.lock().expect("scan calls") += 1;
                Ok(statsai_adapters::AdapterScan::default())
            }
        }

        struct DuplicateFileAdapter {
            candidate: ScanCandidateFile,
            event: UsageEvent,
            scan_calls: Arc<Mutex<u64>>,
        }

        impl ProviderAdapter for DuplicateFileAdapter {
            fn id(&self) -> &'static str {
                "test-duplicate-file-adapter"
            }

            fn version(&self) -> &'static str {
                "0.0.0"
            }

            fn provider(&self) -> &'static str {
                "codex"
            }

            fn discover(&self) -> Vec<SourceLocation> {
                Vec::new()
            }

            fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
                Ok(vec![self.candidate.clone()])
            }

            fn scan(
                &self,
                _source: &SourceLocation,
                options: &ScanOptions,
            ) -> Result<statsai_adapters::AdapterScan> {
                assert!(options
                    .selected_cache_keys
                    .as_ref()
                    .is_some_and(|keys| keys.contains(&self.candidate.cache_key)));
                *self.scan_calls.lock().expect("scan calls") += 1;
                Ok(statsai_adapters::AdapterScan {
                    events: vec![self.event.clone()],
                    ..statsai_adapters::AdapterScan::default()
                })
            }
        }

        struct FailingScanAdapter {
            candidate: ScanCandidateFile,
        }

        impl ProviderAdapter for FailingScanAdapter {
            fn id(&self) -> &'static str {
                "test-failing-scan-adapter"
            }

            fn version(&self) -> &'static str {
                "0.0.0"
            }

            fn provider(&self) -> &'static str {
                "codex"
            }

            fn discover(&self) -> Vec<SourceLocation> {
                Vec::new()
            }

            fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
                Ok(vec![self.candidate.clone()])
            }

            fn scan(
                &self,
                _source: &SourceLocation,
                _options: &ScanOptions,
            ) -> Result<statsai_adapters::AdapterScan> {
                anyhow::bail!("injected transient scan failure")
            }
        }

        struct ConcurrentSourceUpdateAdapter {
            candidate: ScanCandidateFile,
            store: Arc<Mutex<Store>>,
        }

        impl ProviderAdapter for ConcurrentSourceUpdateAdapter {
            fn id(&self) -> &'static str {
                "test-concurrent-source-update-adapter"
            }

            fn version(&self) -> &'static str {
                "0.0.0"
            }

            fn provider(&self) -> &'static str {
                "codex"
            }

            fn discover(&self) -> Vec<SourceLocation> {
                Vec::new()
            }

            fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
                Ok(vec![self.candidate.clone()])
            }

            fn scan(
                &self,
                source: &SourceLocation,
                _options: &ScanOptions,
            ) -> Result<statsai_adapters::AdapterScan> {
                let store = self.store.lock().expect("primary store");
                let mut current = store
                    .source(&source.source_id)?
                    .context("source exists during concurrent update")?;
                current.enabled = false;
                current.updated_at = Utc::now();
                store.upsert_source(&current)?;
                Ok(statsai_adapters::AdapterScan::default())
            }
        }

        #[test]
        fn rescan_changed_sources_reports_adapter_failure() {
            let store = Store::in_memory().expect("store");
            let root = tempfile::tempdir().expect("source root");
            let changed = root.path().join("session.jsonl");
            std::fs::write(&changed, "{}\n").expect("changed file");
            let source = SourceLocation::local_adapter(
                "codex",
                "test",
                "0",
                root.path(),
                LocationOrigin::Configured,
            );
            store.upsert_source(&source).expect("source");
            let adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(FailingScanAdapter {
                candidate: ScanCandidateFile {
                    path: changed.clone(),
                    cache_key: changed.to_string_lossy().into_owned(),
                    cache_signature: "changed-signature".to_string(),
                    compatible_cache_signatures: Vec::new(),
                },
            })];

            let result = rescan_changed_sources_with_adapters(
                &store,
                "device-test",
                std::slice::from_ref(&changed),
                &adapters,
            );

            assert!(result.is_err());
        }

        #[test]
        fn background_scan_does_not_overwrite_a_concurrent_source_update() {
            let root = tempfile::tempdir().expect("store root");
            let source_root = root.path().join("source");
            std::fs::create_dir_all(&source_root).expect("source root");
            let changed = source_root.join("session.jsonl");
            std::fs::write(&changed, "{}\n").expect("changed file");
            let primary = Store::open(&root.path().join("statsai.db")).expect("primary store");
            let source = SourceLocation::local_adapter(
                "codex",
                "test",
                "0",
                &source_root,
                LocationOrigin::Configured,
            );
            primary.upsert_source(&source).expect("source");
            let scan_store = primary.reopen().expect("scan store");
            let shared_store = Arc::new(Mutex::new(primary));
            let adapters: Vec<Box<dyn ProviderAdapter>> =
                vec![Box::new(ConcurrentSourceUpdateAdapter {
                    candidate: ScanCandidateFile {
                        path: changed.clone(),
                        cache_key: changed.to_string_lossy().into_owned(),
                        cache_signature: "changed-signature".to_string(),
                        compatible_cache_signatures: Vec::new(),
                    },
                    store: Arc::clone(&shared_store),
                })];

            let result = rescan_changed_sources_with_adapters_and_commit_store(
                &scan_store,
                Some(&shared_store),
                "device-test",
                std::slice::from_ref(&changed),
                &adapters,
            );

            let error = result.expect_err("concurrent update invalidates scan");
            assert!(error.to_string().contains("commit changed-source scan"));
            assert!(format!("{error:#}").contains("database changed while scanning"));
            let store = shared_store.lock().expect("primary store");
            let stored_source = store
                .source(&source.source_id)
                .expect("source query")
                .expect("stored source");
            assert!(!stored_source.enabled);
            assert!(store
                .scan_file_entries(&source.source_id)
                .expect("scan cache entries")
                .is_empty());
        }

        #[test]
        fn scan_commit_transaction_blocks_external_writer_after_freshness_check() {
            let root = tempfile::tempdir().expect("store root");
            let primary = Store::open(&root.path().join("statsai.db")).expect("primary store");
            let source = SourceLocation::local_adapter(
                "codex",
                "test",
                "0",
                root.path(),
                LocationOrigin::Configured,
            );
            primary.upsert_source(&source).expect("source");
            let scan_store = primary.reopen().expect("scan store");
            let external_store = primary.reopen().expect("external store");
            let shared_store = Arc::new(Mutex::new(primary));
            let expected_data_version = Some(scan_store.data_version().expect("data version"));
            let mut stale_source = source.clone();
            let (start_writer_tx, start_writer_rx) = mpsc::channel();
            let (writer_committed_tx, writer_committed_rx) = mpsc::channel();
            let writer_source_id = source.source_id.clone();
            let writer = std::thread::spawn(move || -> Result<()> {
                start_writer_rx.recv().context("start external writer")?;
                loop {
                    let mut current = external_store
                        .source(&writer_source_id)?
                        .context("source exists for external writer")?;
                    current.enabled = false;
                    current.updated_at = Utc::now();
                    match external_store.upsert_source(&current) {
                        Ok(()) => break,
                        Err(error) if error.to_string().contains("database is locked") => continue,
                        Err(error) => return Err(error),
                    }
                }
                writer_committed_tx
                    .send(())
                    .context("report external writer commit")?;
                Ok(())
            });

            let writer_committed_before_reconcile = commit_source_scan_if_current(
                &scan_store,
                Some(&shared_store),
                expected_data_version,
                Some(&source),
                &mut stale_source,
                |store, source| {
                    start_writer_tx.send(()).context("start external writer")?;
                    let committed = writer_committed_rx
                        .recv_timeout(Duration::from_millis(100))
                        .is_ok();
                    store.upsert_source(source)?;
                    Ok(committed)
                },
            )
            .expect("commit scan update");
            writer
                .join()
                .expect("external writer thread")
                .expect("external writer");

            assert!(!writer_committed_before_reconcile);
            let store = shared_store.lock().expect("primary store");
            let stored_source = store
                .source(&source.source_id)
                .expect("source query")
                .expect("stored source");
            assert!(!stored_source.enabled);
        }

        #[test]
        fn rescan_changed_sources_reconciles_verified_auth_without_pending_usage_files() {
            let store = Store::in_memory().expect("store");
            let root =
                std::env::temp_dir().join(format!("statsai-watch-auth-{}", std::process::id()));
            std::fs::create_dir_all(&root).expect("temp source root");
            let mut source = SourceLocation::local_adapter(
                "codex",
                "test",
                "0",
                &root,
                LocationOrigin::Configured,
            );
            source.verification_mode = SourceVerificationMode::Auto;
            store.upsert_source(&source).expect("source");

            let authenticated_at = Utc
                .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
                .single()
                .expect("authenticated_at");
            let verified_at = Utc
                .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
                .single()
                .expect("verified_at");
            let current_period_ends_at = Utc
                .with_ymd_and_hms(2026, 6, 29, 10, 12, 43)
                .single()
                .expect("current_period_ends_at");
            let blocked_since = Utc
                .with_ymd_and_hms(2026, 6, 1, 9, 30, 0)
                .single()
                .expect("blocked_since");
            let scan_calls = Arc::new(Mutex::new(0u64));
            let adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(TestAdapter {
                provider: "codex",
                verified_observation: VerifiedSourceObservation::Verified(Box::new(
                    VerifiedSourceState {
                        provider_user_id: Some("acct-watch".to_string()),
                        email: Some("watch@example.com".to_string()),
                        account_label: None,
                        plan_name: Some("Plus".to_string()),
                        authenticated_at: Some(authenticated_at),
                        verified_at: Some(verified_at),
                        subscription: Some(VerifiedSubscriptionState {
                            plan_name: "Plus".to_string(),
                            price: 2000,
                            currency: "USD".to_string(),
                            billing_period: BillingPeriod::Monthly,
                            paid_at: Some(authenticated_at),
                            started_at: authenticated_at,
                            ended_at: Some(current_period_ends_at),
                            current_period_ends_at: Some(current_period_ends_at),
                            status: SubscriptionStatus::Active,
                            verified_at: Some(verified_at),
                        }),
                    },
                )),
                verification_dependencies: Vec::new(),
                scan_calls: scan_calls.clone(),
            })];

            rescan_changed_sources_with_adapters(
                &store,
                "device-test",
                &[
                    PathBuf::from(source.path_label.as_deref().expect("path label"))
                        .join("auth.json"),
                ],
                &adapters,
            )
            .expect("rescan auth state");

            assert_eq!(*scan_calls.lock().expect("scan calls"), 0);
            assert_eq!(store.list_accounts().expect("accounts").len(), 1);
            assert_eq!(store.list_subscriptions().expect("subscriptions").len(), 1);
            let assignments = store
                .list_source_account_assignments_for_source(&source.source_id)
                .expect("assignments");
            assert_eq!(assignments.len(), 1);
            assert_eq!(assignments[0].started_at, authenticated_at);
            assert_eq!(assignments[0].ended_at, None);
            assert_eq!(assignments[0].record_source, IdentitySource::LocalAuth);
            let stored_source = store
                .source(&source.source_id)
                .expect("source")
                .expect("stored source");
            assert!(stored_source.verified_state_hash.is_some());

            // A watcher can observe auth.json while it is being rewritten. That
            // transiently produces no local snapshot, which must not end the
            // account assignment or its verified subscription.
            let unavailable_adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(TestAdapter {
                provider: "codex",
                verified_observation: VerifiedSourceObservation::Unavailable,
                verification_dependencies: Vec::new(),
                scan_calls: Arc::new(Mutex::new(0u64)),
            })];
            rescan_changed_sources_with_adapters(
                &store,
                "device-test",
                &[
                    PathBuf::from(source.path_label.as_deref().expect("path label"))
                        .join("auth.json"),
                ],
                &unavailable_adapters,
            )
            .expect("rescan unavailable auth state");

            let assignments = store
                .list_source_account_assignments_for_source(&source.source_id)
                .expect("assignments after unavailable auth");
            assert_eq!(assignments.len(), 1);
            assert_eq!(assignments[0].ended_at, None);
            let subscriptions = store.list_subscriptions().expect("subscriptions");
            assert_eq!(subscriptions.len(), 1);
            assert_eq!(subscriptions[0].ended_at, None);

            let blocked_adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(TestAdapter {
                provider: "codex",
                verified_observation: VerifiedSourceObservation::AttributionBlocked {
                    blocked_since: Some(blocked_since),
                },
                verification_dependencies: Vec::new(),
                scan_calls: Arc::new(Mutex::new(0u64)),
            })];
            rescan_changed_sources_with_adapters(
                &store,
                "device-test",
                &[
                    PathBuf::from(source.path_label.as_deref().expect("path label"))
                        .join("auth.json"),
                ],
                &blocked_adapters,
            )
            .expect("rescan explicitly blocked auth state");

            let assignments = store
                .list_source_account_assignments_for_source(&source.source_id)
                .expect("assignments after blocked auth");
            assert_eq!(assignments.len(), 1);
            assert_eq!(assignments[0].ended_at, Some(blocked_since));
            let subscriptions = store.list_subscriptions().expect("subscriptions");
            assert_eq!(subscriptions.len(), 1);
            assert_eq!(subscriptions[0].ended_at, None);

            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn rescan_changed_sources_removes_records_for_deleted_files() {
            let store = Store::in_memory().expect("store");
            let root = tempfile::tempdir().expect("source root");
            let deleted_file = root.path().join("deleted.jsonl");
            let source = SourceLocation::local_adapter(
                "codex",
                "test",
                "0",
                root.path(),
                LocationOrigin::Configured,
            );
            store.upsert_source(&source).expect("source");
            let cache_key = deleted_file.to_string_lossy().into_owned();
            store
                .record_scan_file_entries(
                    &source.source_id,
                    &[ScanFileStateEntry {
                        cache_key: cache_key.clone(),
                        cache_signature: "old-signature".to_string(),
                    }],
                )
                .expect("scan cache");
            let now = Utc
                .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
                .single()
                .expect("event time");
            let event: UsageEvent = serde_json::from_value(serde_json::json!({
                "schema_version": "usage_event.v1",
                "event_id": "event-deleted-file",
                "device_id": "device-test",
                "provider": "codex",
                "source_id": source.source_id.clone(),
                "provider_account_id": null,
                "subscription_id": null,
                "source": {
                    "adapter_id": "test-watch-adapter",
                    "adapter_version": "0.0.0",
                    "source_kind": "local_adapter",
                    "location_origin": "configured",
                    "source_type": "jsonl",
                    "source_path_hash": null,
                    "source_record_id": "record-1",
                    "parse_confidence": "high"
                },
                "session": {
                    "session_id": "session-1",
                    "local_session_id_hash": null,
                    "title": null,
                    "started_at": now,
                    "ended_at": null,
                    "duration_seconds": null
                },
                "model": null,
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "cache_creation_tokens": null,
                    "cache_read_tokens": null,
                    "reasoning_tokens": null,
                    "total_tokens": 15,
                    "requests": 1,
                    "local_prompt_eval_tokens": null,
                    "local_eval_tokens": null
                },
                "runtime": null,
                "cost": {
                    "currency": "USD",
                    "estimated_api_equivalent_usd": null,
                    "provider_reported_usd": null,
                    "pricing_source": null,
                    "pricing_version": null,
                    "confidence": "low"
                },
                "parse_evidence": {
                    "event_key_version": "v1",
                    "source_file_path_hash": hash_text(&cache_key),
                    "source_line_number": 1,
                    "source_record_id": "record-1",
                    "model_inferred": false,
                    "timestamp_inferred": false,
                    "account_identity_source": "unresolved"
                },
                "project": null,
                "git": null,
                "privacy": {
                    "mode": "metadata_only",
                    "contains_prompt_text": false,
                    "contains_response_text": false,
                    "contains_file_paths": false
                },
                "created_at": now,
                "imported_at": now
            }))
            .expect("event");
            assert!(store.insert_event(&event).expect("insert event"));

            let scan_calls = Arc::new(Mutex::new(0u64));
            let adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(TestAdapter {
                provider: "codex",
                verified_observation: VerifiedSourceObservation::Unavailable,
                verification_dependencies: Vec::new(),
                scan_calls: Arc::clone(&scan_calls),
            })];
            rescan_changed_sources_with_adapters(
                &store,
                "device-test",
                std::slice::from_ref(&deleted_file),
                &adapters,
            )
            .expect("rescan deleted file");

            assert_eq!(*scan_calls.lock().expect("scan calls"), 0);
            assert_eq!(store.event_count().expect("event count"), 0);
            assert!(store
                .scan_file_entries(&source.source_id)
                .expect("scan entries")
                .is_empty());
        }

        #[test]
        fn rescan_changed_sources_preserves_event_from_unchanged_duplicate_file() {
            let store = Store::in_memory().expect("store");
            let root = tempfile::tempdir().expect("source root");
            let active_file = root.path().join("sessions/duplicate.jsonl");
            let archived_file = root.path().join("archived_sessions/duplicate.jsonl");
            std::fs::create_dir_all(archived_file.parent().expect("archived parent"))
                .expect("create archived directory");
            std::fs::write(&archived_file, b"unchanged archived copy")
                .expect("write archived copy");
            let source = SourceLocation::local_adapter(
                "codex",
                "test",
                "0",
                root.path(),
                LocationOrigin::Configured,
            );
            store.upsert_source(&source).expect("source");
            let active_cache_key = active_file.to_string_lossy().into_owned();
            let archived_cache_key = archived_file.to_string_lossy().into_owned();
            store
                .record_scan_file_entries(
                    &source.source_id,
                    &[
                        ScanFileStateEntry {
                            cache_key: active_cache_key.clone(),
                            cache_signature: "active-signature".to_string(),
                        },
                        ScanFileStateEntry {
                            cache_key: archived_cache_key.clone(),
                            cache_signature: "archived-signature".to_string(),
                        },
                    ],
                )
                .expect("scan cache");
            let now = Utc
                .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
                .single()
                .expect("event time");
            let active_file_hash = hash_text(&active_cache_key);
            let archived_file_hash = hash_text(&archived_cache_key);
            let event_json = |file_hash: String| {
                serde_json::json!({
                    "schema_version": "usage_event.v1",
                    "event_id": "event-duplicate-file",
                    "device_id": "device-test",
                    "provider": "codex",
                    "source_id": source.source_id.clone(),
                    "provider_account_id": null,
                    "subscription_id": null,
                    "source": {
                        "adapter_id": "test-duplicate-file-adapter",
                        "adapter_version": "0.0.0",
                        "source_kind": "local_adapter",
                        "location_origin": "configured",
                        "source_type": "jsonl",
                        "source_path_hash": null,
                        "source_record_id": "record-duplicate",
                        "parse_confidence": "high"
                    },
                    "session": {
                        "session_id": "session-duplicate",
                        "local_session_id_hash": null,
                        "title": null,
                        "started_at": now,
                        "ended_at": null,
                        "duration_seconds": null
                    },
                    "model": null,
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 5,
                        "total_tokens": 15,
                        "requests": 1
                    },
                    "runtime": null,
                    "cost": {
                        "currency": "USD",
                        "estimated_api_equivalent_usd": null,
                        "provider_reported_usd": null,
                        "pricing_source": null,
                        "pricing_version": null,
                        "confidence": "low"
                    },
                    "parse_evidence": {
                        "event_key_version": "v1",
                        "source_file_path_hash": file_hash,
                        "source_line_number": 1,
                        "source_record_id": "record-duplicate",
                        "model_inferred": false,
                        "timestamp_inferred": false,
                        "account_identity_source": "unresolved"
                    },
                    "project": null,
                    "git": null,
                    "privacy": {
                        "mode": "metadata_only",
                        "contains_prompt_text": false,
                        "contains_response_text": false,
                        "contains_file_paths": false
                    },
                    "created_at": now,
                    "imported_at": now
                })
            };
            let active_event: UsageEvent =
                serde_json::from_value(event_json(active_file_hash)).expect("active event");
            let archived_event: UsageEvent =
                serde_json::from_value(event_json(archived_file_hash.clone()))
                    .expect("archived event");
            assert!(store.insert_event(&active_event).expect("insert event"));

            let scan_calls = Arc::new(Mutex::new(0u64));
            let adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(DuplicateFileAdapter {
                candidate: ScanCandidateFile {
                    path: archived_file,
                    cache_key: archived_cache_key.clone(),
                    cache_signature: "archived-signature".to_string(),
                    compatible_cache_signatures: Vec::new(),
                },
                event: archived_event,
                scan_calls: Arc::clone(&scan_calls),
            })];

            rescan_changed_sources_with_adapters(
                &store,
                "device-test",
                std::slice::from_ref(&active_file),
                &adapters,
            )
            .expect("rescan duplicate file");

            assert_eq!(*scan_calls.lock().expect("scan calls"), 1);
            let events = store.events().expect("events");
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event_id.0, "event-duplicate-file");
            assert_eq!(
                events[0]
                    .parse_evidence
                    .as_ref()
                    .and_then(|evidence| evidence.source_file_path_hash.as_deref()),
                Some(archived_file_hash.as_str())
            );
        }
    }
}

#[cfg(not(feature = "watch"))]
pub fn watch_and_serve(
    _addr: &str,
    _store: Arc<Mutex<Store>>,
    _device_id: &str,
    _auth_token: &str,
) -> Result<()> {
    anyhow::bail!(
        "daemon --watch requires the `watch` cargo feature (enable with --features watch)"
    )
}

#[cfg(feature = "watch")]
pub fn watch_and_serve(
    addr: &str,
    store: Arc<Mutex<Store>>,
    device_id: &str,
    auth_token: &str,
) -> Result<()> {
    watch::watch_and_serve(addr, store, device_id, auth_token)
}

fn resolve_loopback_addr(addr: &str) -> Result<SocketAddr> {
    let resolved = addr.to_socket_addrs()?.collect::<Vec<_>>();
    validated_loopback_addr(&resolved)
}

fn validated_loopback_addr(resolved: &[SocketAddr]) -> Result<SocketAddr> {
    let Some(first) = resolved.first().copied() else {
        bail!("local API address did not resolve");
    };
    if resolved.iter().any(|addr| !addr.ip().is_loopback()) {
        bail!("local API address must resolve exclusively to loopback addresses");
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use statsai_core::{
        source_id, CodeChangeMetric, CodeChangeMetricKind, CodeLineCounts, Confidence,
        CoverageStatus, ProjectInfo, SourceKind, SyncAuthoritativeSnapshot, TaskBucketSnapshot,
        TaskSpan, TaskSpanId, TaskStatus, TaskVerdict, TaskVerification, TaskVerificationAction,
        TaskVerificationCursor, TaskVerificationId, UsageCounts, WorkItem, WorkItemId,
        WorkItemMember, CODE_CHANGE_METRIC_SCHEMA_VERSION, TASK_SPAN_SCHEMA_VERSION,
        TASK_VERIFICATION_SCHEMA_VERSION, WORK_ITEM_SCHEMA_VERSION,
    };

    fn empty_batch() -> SyncBatch {
        SyncBatch {
            schema_version: SYNC_BATCH_V3_SCHEMA_VERSION.to_string(),
            batch_id: "batch_test".to_string(),
            device_id: "device_test".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets: Vec::new(),
            task_verifications: Vec::new(),
            code_change_metrics: Vec::new(),
            quota_cycle_contributions: Vec::new(),
            authoritative_snapshot: None,
            created_at: Utc::now(),
        }
    }

    fn test_header(name: &str, value: &str) -> Header {
        Header::from_bytes(name, value).expect("valid test header")
    }

    #[test]
    fn loopback_resolution_rejects_empty_and_mixed_results() {
        let loopback_v4 = "127.0.0.1:8765".parse().expect("IPv4 loopback");
        let loopback_v6 = "[::1]:8765".parse().expect("IPv6 loopback");
        let non_loopback = "192.0.2.1:8765".parse().expect("non-loopback");

        assert!(validated_loopback_addr(&[]).is_err());
        assert!(validated_loopback_addr(&[loopback_v4, non_loopback]).is_err());
        assert_eq!(
            validated_loopback_addr(&[loopback_v6, loopback_v4]).expect("all loopback"),
            loopback_v6
        );
    }

    #[test]
    fn loopback_literal_is_resolved_once_to_a_socket_address() {
        assert_eq!(
            resolve_loopback_addr("127.0.0.1:8765").expect("loopback address"),
            "127.0.0.1:8765".parse().expect("expected address")
        );
        assert!(resolve_loopback_addr("192.0.2.1:8765").is_err());
    }

    #[test]
    fn health_is_public_but_rejects_browser_origins() {
        assert_eq!(
            validate_http_request(&Method::Get, "/health", &[], None, "secret"),
            Ok(())
        );
        assert_eq!(
            validate_http_request(
                &Method::Get,
                "/health",
                &[test_header("Origin", "https://attacker.example")],
                None,
                "secret",
            ),
            Err(HttpRejection {
                status: StatusCode(403),
                message: "browser-originated requests are not allowed",
            })
        );
    }

    #[test]
    fn data_routes_require_the_daemon_bearer_token() {
        assert_eq!(
            validate_http_request(&Method::Get, "/accounts", &[], None, "secret"),
            Err(HttpRejection {
                status: StatusCode(401),
                message: "missing or invalid bearer token",
            })
        );
        assert_eq!(
            validate_http_request(
                &Method::Get,
                "/accounts",
                &[test_header("Authorization", "Bearer secret")],
                None,
                "secret",
            ),
            Ok(())
        );
    }

    #[test]
    fn sync_route_requires_json_and_rejects_oversized_declared_bodies() {
        let authorization = test_header("Authorization", "Bearer secret");
        assert_eq!(
            validate_http_request(
                &Method::Post,
                "/v1/sync/batches",
                &[
                    authorization.clone(),
                    test_header("Content-Type", "text/plain")
                ],
                Some(2),
                "secret",
            ),
            Err(HttpRejection {
                status: StatusCode(415),
                message: "content-type must be application/json",
            })
        );
        assert_eq!(
            validate_http_request(
                &Method::Post,
                "/v1/sync/batches",
                &[
                    authorization,
                    test_header("Content-Type", "application/json; charset=utf-8"),
                ],
                Some(MAX_SYNC_BATCH_BYTES + 1),
                "secret",
            ),
            Err(HttpRejection {
                status: StatusCode(413),
                message: "sync batch is too large",
            })
        );
    }

    #[test]
    fn ingest_empty_sync_batch_returns_ack() {
        let store = Store::in_memory().expect("store");
        let ack = ingest_sync_batch(&store, &empty_batch()).expect("ack");

        assert_eq!(ack.schema_version, SYNC_ACK_V3_SCHEMA_VERSION);
        assert_eq!(ack.batch_id, "batch_test");
        assert_eq!(ack.accepted.events, 0);
        assert_eq!(ack.duplicates.events, 0);
        assert!(ack.rejected.is_empty());
    }

    #[test]
    fn ingest_v1_batch_returns_v1_ack_schema() {
        let store = Store::in_memory().expect("store");
        let mut batch = empty_batch();
        batch.schema_version = SYNC_BATCH_V1_SCHEMA_VERSION.to_string();

        let ack = ingest_sync_batch(&store, &batch).expect("ack");
        assert_eq!(ack.schema_version, SYNC_ACK_V1_SCHEMA_VERSION);
    }

    #[test]
    fn ingest_v2_batch_rejects_code_change_metrics() {
        let store = Store::in_memory().expect("store");
        let mut batch = empty_batch();
        batch.schema_version = SYNC_BATCH_V2_SCHEMA_VERSION.to_string();
        batch.code_change_metrics.push(CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "metric-v2".to_string(),
            device_id: "device_test".to_string(),
            day: Utc::now().date_naive(),
            project_id: None,
            repository_hash: None,
            commit_hash: None,
            kind: CodeChangeMetricKind::AgentEdit,
            counts: CodeLineCounts::default(),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Complete,
            git_coverage: CoverageStatus::Unavailable,
        });

        let error = ingest_sync_batch(&store, &batch).expect_err("v2 metric payload");
        assert!(error
            .to_string()
            .contains("code-change metrics require sync_batch.v3"));
    }

    #[test]
    fn ingest_v3_batch_rejects_quota_cycle_contributions() {
        let store = Store::in_memory().expect("store");
        let mut batch = empty_batch();
        batch.schema_version = SYNC_BATCH_V3_SCHEMA_VERSION.to_string();
        batch
            .quota_cycle_contributions
            .push(statsai_core::QuotaCycleContributionV1 {
                schema_version: statsai_core::QUOTA_CYCLE_CONTRIBUTION_SCHEMA_VERSION.to_string(),
                contribution_id: "quota_cycle_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                provider: "codex".to_string(),
                provider_account_id: statsai_core::ProviderAccountId("acct-1".to_string()),
                limit_id: None,
                window_minutes: statsai_core::QUOTA_WEEKLY_WINDOW_MINUTES,
                representative_reset: Utc::now(),
                representative_reset_epoch_seconds: Utc::now().timestamp(),
                has_schedule_overlap: false,
                daily_envelopes: Vec::new(),
                boundary_slices: Vec::new(),
            });

        let error = ingest_sync_batch(&store, &batch).expect_err("v3 quota payload");
        assert!(error
            .to_string()
            .contains("quota cycle contributions require sync_batch.v4"));
    }

    #[test]
    fn ingest_v4_batch_refuses_quota_cycle_contributions_it_cannot_store() {
        let store = Store::in_memory().expect("store");
        let mut batch = empty_batch();
        batch.schema_version = SYNC_BATCH_V4_SCHEMA_VERSION.to_string();
        batch
            .quota_cycle_contributions
            .push(statsai_core::QuotaCycleContributionV1 {
                schema_version: statsai_core::QUOTA_CYCLE_CONTRIBUTION_SCHEMA_VERSION.to_string(),
                contribution_id: "quota_cycle_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                provider: "codex".to_string(),
                provider_account_id: statsai_core::ProviderAccountId("acct-1".to_string()),
                limit_id: None,
                window_minutes: statsai_core::QUOTA_WEEKLY_WINDOW_MINUTES,
                representative_reset: Utc::now(),
                representative_reset_epoch_seconds: Utc::now().timestamp(),
                has_schedule_overlap: false,
                daily_envelopes: Vec::new(),
                boundary_slices: Vec::new(),
            });

        // Accepting them would tell the sender to stop offering cycles this
        // store has nowhere to put.
        let error = ingest_sync_batch(&store, &batch).expect_err("unsupported contributions");
        assert!(error
            .to_string()
            .contains("quota cycle contributions are not supported"));
    }

    #[test]
    fn ingest_v3_batch_rejects_metric_owned_by_another_device_before_persisting() {
        let store = Store::in_memory().expect("store");
        let mut batch = empty_batch();
        let metric = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "metric-other-device".to_string(),
            device_id: batch.device_id.clone(),
            day: Utc::now().date_naive(),
            project_id: None,
            repository_hash: None,
            commit_hash: None,
            kind: CodeChangeMetricKind::AgentEdit,
            counts: CodeLineCounts::default(),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Complete,
            git_coverage: CoverageStatus::Unavailable,
        };
        batch.code_change_metrics.push(metric.clone());
        ingest_sync_batch(&store, &batch).expect("matching metric owner");
        batch.batch_id = "batch_mismatched_owner".to_string();
        batch.code_change_metrics[0].device_id = "other_device".to_string();

        let error = ingest_sync_batch(&store, &batch).expect_err("mismatched metric owner");

        assert!(error
            .to_string()
            .contains("code-change metric device_id must match batch device_id"));
        assert_eq!(
            store
                .list_code_change_metrics(false)
                .expect("stored metrics"),
            vec![metric]
        );
    }

    #[test]
    fn ingest_rejects_unsupported_schema() {
        let store = Store::in_memory().expect("store");
        let mut batch = empty_batch();
        batch.schema_version = "sync_batch.v0".to_string();

        let error = ingest_sync_batch(&store, &batch).expect_err("unsupported schema");
        assert!(error.to_string().contains("unsupported sync batch schema"));
    }

    #[test]
    fn ingest_rejects_authoritative_snapshot_before_persisting_batch() {
        let store = Store::in_memory().expect("store");
        let mut batch = empty_batch();
        batch.task_verifications = vec![test_task_verification()];
        batch.authoritative_snapshot = Some(SyncAuthoritativeSnapshot {
            snapshot_id: "snapshot_test".to_string(),
            part_index: 0,
            part_count: 1,
            ..SyncAuthoritativeSnapshot::default()
        });

        let error = ingest_sync_batch(&store, &batch).expect_err("unsupported snapshot");

        assert!(error
            .to_string()
            .contains("authoritative snapshots are not supported"));
        assert!(store
            .task_verifications()
            .expect("task verifications")
            .is_empty());
    }

    #[test]
    fn health_payload_reports_daemon_version() {
        assert_eq!(health_payload()["status"], "ok");
        assert_eq!(health_payload()["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn ingest_persists_task_payloads_before_acknowledging_them() {
        let store = Store::in_memory().expect("store");
        let mut batch = empty_batch();
        batch.task_buckets = vec![test_task_bucket_snapshot()];
        batch.task_verifications = vec![test_task_verification()];

        let ack = ingest_sync_batch(&store, &batch).expect("ack");

        assert_eq!(ack.accepted.task_buckets, 1);
        assert_eq!(ack.accepted.task_verifications, 1);
        assert_eq!(ack.duplicates.task_verifications, 0);
        assert_eq!(store.task_spans().expect("task spans").len(), 1);
        assert_eq!(store.work_items().expect("work items").len(), 1);
        assert_eq!(
            store
                .task_verifications()
                .expect("task verifications")
                .len(),
            1
        );
    }

    #[test]
    fn ingest_rebuilds_stale_task_buckets_against_newer_local_verifications() {
        let store = Store::in_memory().expect("store");
        store
            .merge_task_verification(&test_task_verification())
            .expect("seed verification");

        let mut batch = empty_batch();
        batch.task_buckets = vec![test_task_bucket_snapshot()];

        let ack = ingest_sync_batch(&store, &batch).expect("ack");

        assert_eq!(ack.accepted.task_buckets, 1);
        assert_eq!(ack.accepted.task_verifications, 0);
        let work_items = store.work_items().expect("work items");
        assert_eq!(work_items.len(), 1);
        assert_eq!(work_items[0].status, TaskStatus::RejectedMeta);
        assert!(work_items[0]
            .review_reasons
            .iter()
            .any(|reason| reason.starts_with("manual_reject:")));
    }

    #[test]
    fn rejected_batch_rolls_back_earlier_task_verifications() {
        let store = Store::in_memory().expect("store");
        let first = test_task_verification();
        let mut duplicate_id = first.clone();
        duplicate_id.action = TaskVerificationAction::Accept {
            work_item_id: WorkItemId("work_other".to_string()),
            anchor_span_id: TaskSpanId("span_other".to_string()),
        };
        duplicate_id.action_key = duplicate_id.action.action_key();
        let mut batch = empty_batch();
        batch.task_verifications = vec![first, duplicate_id];

        ingest_sync_batch(&store, &batch).expect_err("duplicate verification id");

        assert!(store
            .task_verifications()
            .expect("task verifications")
            .is_empty());
    }

    fn test_task_bucket_snapshot() -> TaskBucketSnapshot {
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 5, 10, 0, 0)
            .single()
            .expect("start");
        let ended_at = Utc
            .with_ymd_and_hms(2026, 7, 5, 10, 5, 0)
            .single()
            .expect("end");
        let span_id = TaskSpanId("span_ingest_test".to_string());
        let work_item_id = WorkItemId("work_ingest_test".to_string());

        TaskBucketSnapshot {
            project_bucket: "bucket-ingest".to_string(),
            generated_at: ended_at,
            applied_verification_cursor: Some(TaskVerificationCursor {
                updated_at: ended_at,
                verification_id: TaskVerificationId("tvf-ingest-cursor".to_string()),
            }),
            work_items: vec![WorkItem {
                schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
                work_item_id: work_item_id.clone(),
                anchor_span_id: span_id.clone(),
                tail_span_id: span_id.clone(),
                project_bucket: "bucket-ingest".to_string(),
                title: "Implement hosted task sync".to_string(),
                normalized_title: "implement hosted task sync".to_string(),
                status: TaskStatus::NeedsReview,
                confidence: Confidence::Medium,
                started_at,
                ended_at,
                duration_seconds: Some(300),
                span_count: 1,
                event_count: 1,
                total_input_tokens: 10,
                total_cache_creation_tokens: 0,
                total_cache_read_tokens: 0,
                total_output_tokens: 5,
                total_reasoning_tokens: 0,
                total_tokens: 15,
                estimated_cost_usd: Some(25),
                estimated_cost_micro_usd: Some(250_000),
                providers: vec!["codex".to_string()],
                issue_keys: Vec::new(),
                repo_label: Some("statsai/repo".to_string()),
                branch_labels: vec!["main".to_string()],
                path_label: Some("/workspace/statsai".to_string()),
                summary_preview: Some("Implement hosted task sync".to_string()),
                todo_excerpt: Some("todo hosted task sync".to_string()),
                no_git: false,
                cross_provider: false,
                continuation_reasons: Vec::new(),
                review_reasons: vec!["needs_review".to_string()],
            }],
            members: vec![WorkItemMember {
                work_item_id,
                span_id: span_id.clone(),
                ordinal: 0,
            }],
            spans: vec![TaskSpan {
                schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                span_id,
                provider: "codex".to_string(),
                source_id: source_id("codex", SourceKind::LocalAdapter, "daemon-ingest"),
                span_kind: "codex_task".to_string(),
                source_record_id: None,
                source_file_path_hash: None,
                summary_id: None,
                session_id: Some("session-ingest".to_string()),
                thread_id: Some("thread-ingest".to_string()),
                title: "Implement hosted task sync".to_string(),
                normalized_title: "implement hosted task sync".to_string(),
                title_source: Some("thread_name".to_string()),
                summary_preview: Some("Implement hosted task sync".to_string()),
                todo_excerpt: Some("todo hosted task sync".to_string()),
                issue_keys: Vec::new(),
                branch_family: Some("main".to_string()),
                project_bucket: "bucket-ingest".to_string(),
                project: Some(ProjectInfo {
                    project_id: "project-ingest".to_string(),
                    project_label: Some("StatsAI".to_string()),
                    repo_remote_hash: Some("repo-hash-ingest".to_string()),
                    repo_label: Some("statsai/repo".to_string()),
                    branch_hash: Some("branch-hash-ingest".to_string()),
                    branch_label: Some("main".to_string()),
                    path_hash: Some("path-hash-ingest".to_string()),
                    path_label: Some("/workspace/statsai".to_string()),
                }),
                git: None,
                usage: UsageCounts {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    total_tokens: Some(15),
                    requests: Some(1),
                    ..UsageCounts::default()
                },
                estimated_cost_usd: Some(25),
                estimated_cost_micro_usd: Some(250_000),
                event_count: 1,
                has_usage_evidence: true,
                total_messages: 2,
                user_messages: 1,
                assistant_messages: 1,
                developer_messages: 0,
                linked_event_ids: Vec::new(),
                confidence: Confidence::High,
                is_meta: false,
                started_at,
                ended_at: Some(ended_at),
                duration_seconds: Some(300),
            }],
        }
    }

    fn test_task_verification() -> TaskVerification {
        let created_at = Utc
            .with_ymd_and_hms(2026, 7, 5, 10, 6, 0)
            .single()
            .expect("created_at");
        TaskVerification {
            schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
            verification_id: TaskVerificationId("tvf-ingest-1".to_string()),
            action_key: "anchor:span_ingest_test".to_string(),
            action: TaskVerificationAction::Reject {
                work_item_id: WorkItemId("work_ingest_test".to_string()),
                anchor_span_id: TaskSpanId("span_ingest_test".to_string()),
                reason: TaskVerdict::Meta,
            },
            created_at,
            updated_at: created_at,
        }
    }
}
