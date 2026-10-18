use super::*;
use statsai_adapters::ClaudeProjectSettingsDeferral;
use statsai_core::AccountEvidenceCheckpointV1;

/// Records, for each step the scan asks of it, whether the project settings
/// deferral was held at the time.
#[derive(Clone)]
struct DeferralRecordingAdapter {
    inner: TestAdapter,
    calls: Arc<Mutex<Vec<(&'static str, bool)>>>,
}

impl DeferralRecordingAdapter {
    fn record(&self, step: &'static str) {
        self.calls
            .lock()
            .expect("calls")
            .push((step, ClaudeProjectSettingsDeferral::is_held()));
    }
}

impl ProviderAdapter for DeferralRecordingAdapter {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn version(&self) -> &'static str {
        self.inner.version()
    }

    fn provider(&self) -> &'static str {
        self.inner.provider()
    }

    fn discover(&self) -> Vec<SourceLocation> {
        self.inner.discover()
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        self.inner.scan_candidates(source)
    }

    fn collect_account_evidence(
        &self,
        _source: &SourceLocation,
        _checkpoints: &[AccountEvidenceCheckpointV1],
    ) -> Result<AccountEvidenceScan> {
        self.record("account_evidence");
        Ok(AccountEvidenceScan::default())
    }

    fn probe_verified_source_state(
        &self,
        source: &SourceLocation,
    ) -> Result<VerifiedSourceObservation> {
        self.record("probe");
        self.inner.probe_verified_source_state(source)
    }

    fn scan(
        &self,
        source: &SourceLocation,
        options: &ScanOptions,
    ) -> Result<statsai_adapters::AdapterScan> {
        self.record("scan");
        self.inner.scan(source, options)
    }
}

fn scan_once(
    store: &Store,
    source: &SourceLocation,
    signature: &str,
    no_cache: bool,
) -> Vec<(&'static str, bool)> {
    let file_path = "/tmp/claude-deferral/projects/p/session.jsonl";
    let calls = Arc::new(Mutex::new(Vec::new()));
    let adapter = DeferralRecordingAdapter {
        inner: TestAdapter {
            provider: "claude_code",
            discovered: vec![source.clone()],
            candidates: vec![ScanCandidateFile {
                path: PathBuf::from(file_path),
                cache_key: file_path.to_string(),
                cache_signature: signature.to_string(),
                compatible_cache_signatures: Vec::new(),
            }],
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        },
        calls: calls.clone(),
    };
    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache,
            replace: false,
            verbose: false,
            explain: false,
        },
        store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");
    let calls = calls.lock().expect("calls").clone();
    calls
}

// STA-40: a scan with nothing new for a source asks for its verified state and
// account evidence only under the deferral, so it opens no project settings.
// A scan that reads files, or rereads them with --no-cache, asks without it.
#[test]
fn scan_defers_project_settings_only_for_a_source_with_nothing_new() {
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-deferral"),
        LocationOrigin::Configured,
    );
    source.verification_mode = SourceVerificationMode::Auto;
    store.upsert_source(&source).expect("source");

    let not_deferred = vec![
        ("account_evidence", false),
        ("probe", false),
        ("scan", false),
    ];
    assert_eq!(
        scan_once(&store, &source, "sig-1", false),
        not_deferred,
        "first scan reads the file"
    );
    assert_eq!(
        scan_once(&store, &source, "sig-1", false),
        vec![("account_evidence", true), ("probe", true)],
        "a scan with nothing new defers both and reads no file"
    );
    assert_eq!(
        scan_once(&store, &source, "sig-1", true),
        not_deferred,
        "--no-cache rereads the file"
    );
    assert_eq!(
        scan_once(&store, &source, "sig-2", false),
        not_deferred,
        "a changed file is read"
    );
    assert!(!ClaudeProjectSettingsDeferral::is_held());
}
