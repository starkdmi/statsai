use super::scan::TestAdapter;
use super::*;
use notify::{Config, Error as NotifyError, EventHandler, RecursiveMode, Watcher, WatcherKind};
use statsai_adapters::{ScanCandidateFile, ScanOptions, VerifiedSourceObservation};
use statsai_core::{LocationOrigin, SourceLocation};
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
