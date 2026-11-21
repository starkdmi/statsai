use super::*;
use statsai_adapters::AntigravityAdapter;
use std::collections::BTreeMap;

/// The Antigravity adapter over one store, discovered there and nowhere else,
/// counting the scans it runs.
struct StoreAdapter {
    source: SourceLocation,
    scans: Arc<Mutex<u64>>,
}

impl ProviderAdapter for StoreAdapter {
    fn id(&self) -> &'static str {
        AntigravityAdapter.id()
    }

    fn version(&self) -> &'static str {
        AntigravityAdapter.version()
    }

    fn provider(&self) -> &'static str {
        AntigravityAdapter.provider()
    }

    fn discover(&self) -> Vec<SourceLocation> {
        vec![self.source.clone()]
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        AntigravityAdapter.scan_candidates(source)
    }

    fn unreadable_history_files(&self, source: &SourceLocation) -> Result<u64> {
        AntigravityAdapter.unreadable_history_files(source)
    }

    fn scan(
        &self,
        source: &SourceLocation,
        options: &ScanOptions,
    ) -> Result<statsai_adapters::AdapterScan> {
        *self.scans.lock().expect("scans") += 1;
        AntigravityAdapter.scan(source, options)
    }
}

fn store_with(files: &[&str]) -> (tempfile::TempDir, SourceLocation) {
    let dir = tempfile::tempdir().expect("tempdir");
    let conversations = dir.path().join("antigravity/conversations");
    std::fs::create_dir_all(&conversations).expect("conversations");
    for name in files {
        let path = conversations.join(name);
        if name.ends_with(".db") {
            // A zero-length file is an empty SQLite database.
            std::fs::write(&path, b"").expect("db");
        } else {
            std::fs::write(&path, [0x9a, 0x01, 0xfe, 0x42]).expect("encrypted");
        }
    }
    let source = SourceLocation::local_adapter(
        AntigravityAdapter.provider(),
        AntigravityAdapter.id(),
        AntigravityAdapter.version(),
        &dir.path().join("antigravity"),
        LocationOrigin::Configured,
    );
    (dir, source)
}

fn scan_once(
    store: &Store,
    source: &SourceLocation,
    scans: &Arc<Mutex<u64>>,
    preview: bool,
) -> ScanReport {
    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        store,
        "device-test",
        vec![Box::new(StoreAdapter {
            source: source.clone(),
            scans: scans.clone(),
        })],
    )
    .expect("scan")
}

fn antigravity(files: u64) -> BTreeMap<String, u64> {
    BTreeMap::from([("antigravity".to_string(), files)])
}

// STA-175: a store that holds only encrypted conversations has no file to
// read, so its adapter never scans, and the warning must still be reported.
#[test]
fn a_store_of_only_encrypted_conversations_reports_its_unreadable_history() {
    let (_dir, source) = store_with(&["one.pb", "two.pb"]);
    let store = Store::in_memory().expect("store");
    let scans = Arc::new(Mutex::new(0));

    for preview in [true, false] {
        let report = scan_once(&store, &source, &scans, preview);
        assert_eq!(
            report.unreadable_history,
            antigravity(2),
            "preview={preview}"
        );
    }

    assert_eq!(*scans.lock().expect("scans"), 0, "nothing was read");
}

// STA-175: a rescan that finds every conversation unchanged reads nothing and
// still reports the encrypted history.
#[test]
fn a_rescan_with_every_conversation_unchanged_still_reports_unreadable_history() {
    let (_dir, source) = store_with(&["readable.db", "legacy.pb"]);
    let store = Store::in_memory().expect("store");
    let scans = Arc::new(Mutex::new(0));

    let first = scan_once(&store, &source, &scans, false);
    let second = scan_once(&store, &source, &scans, false);

    assert_eq!(*scans.lock().expect("scans"), 1, "the rescan read nothing");
    assert_eq!(first.unreadable_history, antigravity(1));
    assert_eq!(second.unreadable_history, antigravity(1));
}

#[test]
fn a_store_without_encrypted_conversations_reports_nothing() {
    let (_dir, source) = store_with(&["readable.db"]);
    let store = Store::in_memory().expect("store");
    let scans = Arc::new(Mutex::new(0));

    let report = scan_once(&store, &source, &scans, false);

    assert!(report.unreadable_history.is_empty());
}
