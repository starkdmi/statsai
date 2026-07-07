//! Loopback API + file-watching daemon for `statsai`.

use anyhow::{bail, Context, Result};
use serde_json::json;
use statsai_core::{
    SyncAck, SyncBatch, SyncEntityCounts, SyncRejectedRecord, SYNC_ACK_V1_SCHEMA_VERSION,
    SYNC_ACK_V2_SCHEMA_VERSION, SYNC_BATCH_V1_SCHEMA_VERSION, SYNC_BATCH_V2_SCHEMA_VERSION,
};
use statsai_store::Store;
use std::collections::BTreeSet;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex, MutexGuard};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

fn lock_store(store: &Arc<Mutex<Store>>) -> MutexGuard<'_, Store> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

fn sync_ack_schema_version(batch_schema_version: &str) -> &'static str {
    if batch_schema_version == SYNC_BATCH_V1_SCHEMA_VERSION {
        SYNC_ACK_V1_SCHEMA_VERSION
    } else {
        SYNC_ACK_V2_SCHEMA_VERSION
    }
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

pub fn ingest_sync_batch(store: &Store, batch: &SyncBatch) -> Result<SyncAck> {
    if batch.schema_version != SYNC_BATCH_V1_SCHEMA_VERSION
        && batch.schema_version != SYNC_BATCH_V2_SCHEMA_VERSION
    {
        bail!("unsupported sync batch schema {}", batch.schema_version);
    }

    for source in &batch.sources {
        store.upsert_source(source)?;
    }
    for account in &batch.accounts {
        store.upsert_account(account)?;
    }
    for assignment in &batch.source_account_assignments {
        store.upsert_source_account_assignment(assignment)?;
    }
    for subscription in &batch.subscriptions {
        store.upsert_subscription(subscription)?;
    }
    let inserted_events = store.insert_events(&batch.events)?;
    let written_summaries = store.upsert_summaries(&batch.summaries)?;
    let mut buckets_needing_rebuild = BTreeSet::new();
    for snapshot in &batch.task_buckets {
        let had_newer_local_verifications = store.task_bucket_has_newer_verifications(
            &snapshot.project_bucket,
            snapshot.applied_verification_cursor.as_ref(),
        )?;
        store.replace_task_bucket_snapshot(snapshot)?;
        let has_newer_local_verifications = had_newer_local_verifications
            || store.task_bucket_has_newer_verifications(
                &snapshot.project_bucket,
                snapshot.applied_verification_cursor.as_ref(),
            )?;
        if has_newer_local_verifications {
            buckets_needing_rebuild.insert(snapshot.project_bucket.clone());
        }
    }
    let mut merged_task_verifications = 0u64;
    for verification in &batch.task_verifications {
        if store.merge_task_verification(verification)? {
            merged_task_verifications += 1;
            buckets_needing_rebuild
                .extend(store.project_buckets_for_task_verification(verification)?);
        }
    }
    if !buckets_needing_rebuild.is_empty() {
        store.rebuild_task_work_items_for_project_buckets(&buckets_needing_rebuild)?;
    }

    Ok(SyncAck {
        schema_version: sync_ack_schema_version(&batch.schema_version).to_string(),
        batch_id: batch.batch_id.clone(),
        accepted: SyncEntityCounts {
            sources: batch.sources.len() as u64,
            accounts: batch.accounts.len() as u64,
            source_account_assignments: batch.source_account_assignments.len() as u64,
            subscriptions: batch.subscriptions.len() as u64,
            events: inserted_events,
            summaries: written_summaries,
            task_buckets: batch.task_buckets.len() as u64,
            task_verifications: merged_task_verifications,
        },
        duplicates: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: (batch.events.len() as u64).saturating_sub(inserted_events),
            summaries: 0,
            task_buckets: 0,
            task_verifications: (batch.task_verifications.len() as u64)
                .saturating_sub(merged_task_verifications),
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
    use statsai_adapters::{default_adapters, ProviderAdapter, ScanCandidateFile, ScanOptions};
    use statsai_core::{
        timestamp_in_period, IdentitySource, ProviderAccountId, SourceAccountAssignment,
        SourceKind, SourceLocation, SourceVerificationMode, UsageEvent, UsageSummary,
    };
    use statsai_store::{
        effective_verified_source_state_is_missing, has_active_verified_source_assignment,
        reconcile_verified_source_state, verified_source_state_hash, ScanFileStateEntry, Store,
    };
    use std::collections::HashSet;
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
                if source.source_kind != SourceKind::LocalAdapter {
                    continue;
                }
                if let Some(label) = source.path_label.as_deref().filter(|p| !p.is_empty()) {
                    let path = PathBuf::from(label);
                    if path.is_dir() && !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
        }

        for adapter in default_adapters() {
            for source in adapter.discover() {
                if source.source_kind != SourceKind::LocalAdapter {
                    continue;
                }
                if let Some(label) = source.path_label.as_deref().filter(|p| !p.is_empty()) {
                    let path = PathBuf::from(label);
                    if path.is_dir() && !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
        }

        paths
    }

    fn rescan_changed_sources(store: &Store, device_id: &str, changed: &[PathBuf]) {
        let adapters: Vec<Box<dyn ProviderAdapter>> = default_adapters();
        rescan_changed_sources_with_adapters(store, device_id, changed, &adapters);
    }

    fn rescan_changed_sources_with_adapters(
        store: &Store,
        device_id: &str,
        changed: &[PathBuf],
        adapters: &[Box<dyn ProviderAdapter>],
    ) {
        let configured = match store.list_sources() {
            Ok(sources) => sources,
            Err(e) => {
                eprintln!("daemon: failed to list sources: {e}");
                return;
            }
        };

        for adapter in adapters {
            let sources = scan_sources_for_paths(adapter.as_ref(), &configured, changed);
            for mut source in sources {
                let cache_candidates = match adapter.scan_candidates(&source) {
                    Ok(candidates) => candidates,
                    Err(e) => {
                        eprintln!(
                            "daemon: scan candidate discovery failed for {}: {e}",
                            source.path_label.as_deref().unwrap_or("unknown")
                        );
                        continue;
                    }
                };
                let file_cache_entries = scan_file_state_entries(&cache_candidates);
                let pending_file_entries =
                    match store.pending_scan_file_entries(&source.source_id, &file_cache_entries) {
                        Ok(entries) => entries,
                        Err(e) => {
                            eprintln!(
                                "daemon: scan cache lookup failed for {}: {e}",
                                source.path_label.as_deref().unwrap_or("unknown")
                            );
                            continue;
                        }
                    };
                let verification_mode = source.verification_mode.clone();
                let probed_verified_source_state =
                    if matches!(verification_mode, SourceVerificationMode::Disabled) {
                        None
                    } else {
                        match adapter.probe_verified_source_state(&source) {
                            Ok(state) => state,
                            Err(e) => {
                                eprintln!(
                                    "daemon: verified auth probe failed for {}: {e}",
                                    source.path_label.as_deref().unwrap_or("unknown")
                                );
                                continue;
                            }
                        }
                    };
                let next_verified_state_hash =
                    if matches!(verification_mode, SourceVerificationMode::Auto) {
                        match verified_source_state_hash(probed_verified_source_state.as_ref()) {
                            Ok(hash) => hash,
                            Err(e) => {
                                eprintln!(
                                    "daemon: verified auth hash failed for {}: {e}",
                                    source.path_label.as_deref().unwrap_or("unknown")
                                );
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                let verified_state_changed =
                    matches!(verification_mode, SourceVerificationMode::Auto)
                        && source.verified_state_hash != next_verified_state_hash;
                let legacy_verified_state_needs_reconciliation =
                    matches!(verification_mode, SourceVerificationMode::Auto)
                        && source.verified_state_hash.is_none()
                        && next_verified_state_hash.is_none()
                        && effective_verified_source_state_is_missing(
                            &probed_verified_source_state,
                        )
                        && match has_active_verified_source_assignment(store, &source.source_id) {
                            Ok(active) => active,
                            Err(e) => {
                                eprintln!(
                                    "daemon: verified assignment lookup failed for {}: {e}",
                                    source.path_label.as_deref().unwrap_or("unknown")
                                );
                                continue;
                            }
                        };
                if pending_file_entries.is_empty()
                    && !verified_state_changed
                    && !legacy_verified_state_needs_reconciliation
                {
                    continue;
                }
                let options = ScanOptions {
                    device_id: device_id.to_string(),
                    selected_cache_keys: Some(
                        pending_file_entries
                            .iter()
                            .map(|entry| entry.cache_key.clone())
                            .collect::<HashSet<_>>(),
                    ),
                };
                let scan_result = if pending_file_entries.is_empty() {
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
                                None
                            } else if pending_file_entries.is_empty() {
                                probed_verified_source_state
                            } else {
                                scan.verified_source_state
                                    .take()
                                    .or(probed_verified_source_state)
                            };
                        if let Err(e) = reconcile_verified_source_state(
                            store,
                            &mut source,
                            effective_verified_source_state.as_ref(),
                            next_verified_state_hash,
                        ) {
                            eprintln!("daemon: verified auth reconciliation failed: {e}");
                            continue;
                        }
                        if let Err(e) = store.upsert_source(&source) {
                            eprintln!("daemon: update source verified auth state failed: {e}");
                            continue;
                        }
                        if pending_file_entries.is_empty() {
                            eprintln!(
                                "daemon: reconciled auth state for {} ({})",
                                source.provider,
                                source.path_label.as_deref().unwrap_or("unknown")
                            );
                            continue;
                        }
                        if let Err(e) = apply_source_account_resolution(
                            store,
                            &source,
                            &mut scan.events,
                            &mut scan.summaries,
                        ) {
                            eprintln!("daemon: account resolution failed: {e}");
                            continue;
                        }
                        let inserted_events = match store.insert_events(&scan.events) {
                            Ok(count) => count,
                            Err(e) => {
                                eprintln!("daemon: insert events failed: {e}");
                                continue;
                            }
                        };
                        let written_summaries = match store.upsert_summaries(&scan.summaries) {
                            Ok(count) => count,
                            Err(e) => {
                                eprintln!("daemon: insert summaries failed: {e}");
                                continue;
                            }
                        };
                        if let Err(e) =
                            store.record_scan_file_entries(&source.source_id, &pending_file_entries)
                        {
                            eprintln!("daemon: update scan cache failed: {e}");
                            continue;
                        }
                        eprintln!(
                            "daemon: rescanned {} ({}) — files={}, cached={}, parsed_events={}, inserted_events={}, parsed_summaries={}, summaries_written={}",
                            source.provider,
                            source.path_label.as_deref().unwrap_or("unknown"),
                            scan.diagnostics.files_scanned,
                            scan.diagnostics.files_skipped_unchanged,
                            parsed_events,
                            inserted_events,
                            parsed_summaries,
                            written_summaries
                        );
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
            .filter(|s| {
                s.enabled
                    && s.source_kind == SourceKind::LocalAdapter
                    && s.provider == adapter.provider()
            })
            .cloned()
        {
            if source.path_label.is_some() && source_in_changed_paths(&source, changed) {
                sources.push(source);
            }
        }
        for source in adapter.discover() {
            if source.source_kind != SourceKind::LocalAdapter || source.path_label.is_none() {
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

    fn source_in_changed_paths(source: &SourceLocation, changed: &[PathBuf]) -> bool {
        let Some(label) = source.path_label.as_deref() else {
            return false;
        };
        let source_path = PathBuf::from(label);
        changed.iter().any(|changed_path| {
            changed_path.starts_with(&source_path) || source_path.starts_with(changed_path)
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
        use statsai_core::{
            BillingPeriod, LocationOrigin, SubscriptionStatus, VerifiedSourceState,
            VerifiedSubscriptionState,
        };
        use std::sync::{Arc, Mutex};

        struct TestAdapter {
            provider: &'static str,
            verified_state: Option<VerifiedSourceState>,
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
            ) -> Result<Option<VerifiedSourceState>> {
                Ok(self.verified_state.clone())
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
            let scan_calls = Arc::new(Mutex::new(0u64));
            let adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(TestAdapter {
                provider: "codex",
                verified_state: Some(VerifiedSourceState {
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
                }),
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
            );

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

            let _ = std::fs::remove_dir_all(&root);
        }
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
    use chrono::{TimeZone, Utc};
    use statsai_core::{
        source_id, Confidence, ProjectInfo, SourceKind, TaskBucketSnapshot, TaskSpan, TaskSpanId,
        TaskStatus, TaskVerdict, TaskVerification, TaskVerificationAction, TaskVerificationCursor,
        TaskVerificationId, UsageCounts, WorkItem, WorkItemId, WorkItemMember,
        TASK_SPAN_SCHEMA_VERSION, TASK_VERIFICATION_SCHEMA_VERSION, WORK_ITEM_SCHEMA_VERSION,
    };

    fn empty_batch() -> SyncBatch {
        SyncBatch {
            schema_version: SYNC_BATCH_V2_SCHEMA_VERSION.to_string(),
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
            created_at: Utc::now(),
        }
    }

    #[test]
    fn ingest_empty_sync_batch_returns_ack() {
        let store = Store::in_memory().expect("store");
        let ack = ingest_sync_batch(&store, &empty_batch()).expect("ack");

        assert_eq!(ack.schema_version, SYNC_ACK_V2_SCHEMA_VERSION);
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
    fn ingest_rejects_unsupported_schema() {
        let store = Store::in_memory().expect("store");
        let mut batch = empty_batch();
        batch.schema_version = "sync_batch.v0".to_string();

        let error = ingest_sync_batch(&store, &batch).expect_err("unsupported schema");
        assert!(error.to_string().contains("unsupported sync batch schema"));
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
