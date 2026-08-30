mod scan;
mod state;

pub(crate) use super::lock_store;
use scan::*;
use state::*;

use anyhow::{Context, Result};
use notify::{Event, EventKind};
use statsai_adapters::{default_adapters, ProviderAdapter};
use statsai_store::Store;
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
                    .map(|mut paths| std::mem::take(&mut *paths).into_iter().collect::<Vec<_>>())
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use notify::{Config, Error as NotifyError, EventHandler, RecursiveMode, Watcher, WatcherKind};
    use statsai_adapters::{ScanCandidateFile, ScanOptions, VerifiedSourceObservation};
    use statsai_core::{
        hash_text, BillingPeriod, IdentitySource, LocationOrigin, SourceLocation,
        SourceVerificationMode, SubscriptionStatus, UsageEvent, VerifiedSourceState,
        VerifiedSubscriptionState,
    };
    use statsai_store::ScanFileStateEntry;
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

    struct AccountEvidenceTrackingAdapter {
        collect_calls: Arc<Mutex<u64>>,
    }

    impl ProviderAdapter for AccountEvidenceTrackingAdapter {
        fn id(&self) -> &'static str {
            "test-account-evidence"
        }

        fn version(&self) -> &'static str {
            "0"
        }

        fn provider(&self) -> &'static str {
            "codex"
        }

        fn discover(&self) -> Vec<SourceLocation> {
            Vec::new()
        }

        fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
            Ok(Vec::new())
        }

        fn collect_account_evidence(
            &self,
            _source: &SourceLocation,
            _checkpoints: &[statsai_core::AccountEvidenceCheckpointV1],
        ) -> Result<statsai_adapters::AccountEvidenceScan> {
            *self.collect_calls.lock().expect("collect calls") += 1;
            Ok(statsai_adapters::AccountEvidenceScan::default())
        }

        fn scan(
            &self,
            _source: &SourceLocation,
            _options: &ScanOptions,
        ) -> Result<statsai_adapters::AdapterScan> {
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
        let root = std::env::temp_dir().join(format!("statsai-watch-auth-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("temp source root");
        let mut source =
            SourceLocation::local_adapter("codex", "test", "0", &root, LocationOrigin::Configured);
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
            &[PathBuf::from(source.path_label.as_deref().expect("path label")).join("auth.json")],
            &adapters,
        )
        .expect("rescan auth state");

        assert_eq!(*scan_calls.lock().expect("scan calls"), 0);
        assert_eq!(store.list_accounts().expect("accounts").len(), 1);
        assert!(store
            .list_subscriptions()
            .expect("subscriptions")
            .is_empty());
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
        // account assignment.
        let unavailable_adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(TestAdapter {
            provider: "codex",
            verified_observation: VerifiedSourceObservation::Unavailable,
            verification_dependencies: Vec::new(),
            scan_calls: Arc::new(Mutex::new(0u64)),
        })];
        rescan_changed_sources_with_adapters(
            &store,
            "device-test",
            &[PathBuf::from(source.path_label.as_deref().expect("path label")).join("auth.json")],
            &unavailable_adapters,
        )
        .expect("rescan unavailable auth state");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments after unavailable auth");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].ended_at, None);
        assert!(store
            .list_subscriptions()
            .expect("subscriptions")
            .is_empty());

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
            &[PathBuf::from(source.path_label.as_deref().expect("path label")).join("auth.json")],
            &blocked_adapters,
        )
        .expect("rescan explicitly blocked auth state");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments after blocked auth");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].ended_at, Some(blocked_since));
        assert!(store
            .list_subscriptions()
            .expect("subscriptions")
            .is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn manual_only_watcher_rescan_does_not_collect_account_evidence() {
        let store = Store::in_memory().expect("store");
        let root = tempfile::tempdir().expect("source root");
        let mut source = SourceLocation::local_adapter(
            "codex",
            "test-account-evidence",
            "0",
            root.path(),
            LocationOrigin::Configured,
        );
        source.verification_mode = SourceVerificationMode::ManualOnly;
        store.upsert_source(&source).expect("source");
        let collect_calls = Arc::new(Mutex::new(0));
        let adapters: Vec<Box<dyn ProviderAdapter>> =
            vec![Box::new(AccountEvidenceTrackingAdapter {
                collect_calls: Arc::clone(&collect_calls),
            })];

        rescan_changed_sources_with_adapters(
            &store,
            "device-test",
            &[root.path().join("auth.json")],
            &adapters,
        )
        .expect("watcher rescan");

        assert_eq!(*collect_calls.lock().expect("collect calls"), 0);
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
        std::fs::write(&archived_file, b"unchanged archived copy").expect("write archived copy");
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
            serde_json::from_value(event_json(archived_file_hash.clone())).expect("archived event");
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
