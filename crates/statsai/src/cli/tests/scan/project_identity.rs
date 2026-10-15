use super::*;
use std::io::Write as _;

/// The real Codex parser, without discovery: the test scans only the source it
/// configures, never the Codex folders of the machine running it.
struct ConfiguredCodexAdapter(Box<dyn ProviderAdapter>);

impl ConfiguredCodexAdapter {
    fn new() -> Self {
        Self(statsai_adapters::adapter_for_provider("codex").expect("codex adapter"))
    }
}

impl ProviderAdapter for ConfiguredCodexAdapter {
    fn id(&self) -> &'static str {
        self.0.id()
    }

    fn version(&self) -> &'static str {
        self.0.version()
    }

    fn provider(&self) -> &'static str {
        self.0.provider()
    }

    fn discover(&self) -> Vec<SourceLocation> {
        Vec::new()
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        self.0.scan_candidates(source)
    }

    fn scan(
        &self,
        source: &SourceLocation,
        options: &ScanOptions,
    ) -> Result<statsai_adapters::AdapterScan> {
        self.0.scan(source, options)
    }
}

/// One Codex session that ran in `workspace` and names no repository itself,
/// so its project identity comes from the checkout's `.git/config`.
fn write_session(path: &Path, workspace: &Path, turns: usize) {
    let mut file = std::fs::File::create(path).expect("session file");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{{"id":"identity-session","cwd":"{}"}}}}"#,
        workspace.display()
    )
    .expect("session meta");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:01Z","type":"turn_context","payload":{{"cwd":"{}","model":"gpt-5","approval_policy":"never"}}}}"#,
        workspace.display()
    )
    .expect("turn context");
    for turn in 0..turns {
        let total = 120 * (turn + 1);
        writeln!(
            file,
            r#"{{"timestamp":"2026-06-01T08:0{}:05Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":{},"cached_input_tokens":{},"output_tokens":{},"reasoning_output_tokens":{},"total_tokens":{total}}}}}}}}}"#,
            turn + 1,
            80 * (turn + 1),
            20 * (turn + 1),
            40 * (turn + 1),
            10 * (turn + 1),
        )
        .expect("token count");
    }
}

fn write_git_config(workspace: &Path) {
    let git_dir = workspace.join(".git");
    std::fs::create_dir_all(&git_dir).expect("git dir");
    std::fs::write(
        git_dir.join("config"),
        "[remote \"origin\"]\n\turl = git@github.com:example-org/example-repo.git\n",
    )
    .expect("git config");
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("git head");
}

fn scan_codex(store: &Store, replace: bool) {
    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace,
            verbose: false,
            explain: false,
        },
        store,
        "device-test",
        vec![Box::new(ConfiguredCodexAdapter::new())],
    )
    .expect("scan");
}

/// `(project_id, repo_remote_hash)` of every stored event.
fn stored_identities(store: &Store) -> Vec<(String, Option<String>)> {
    let mut identities = store
        .events()
        .expect("events")
        .into_iter()
        .map(|event| {
            let project = event.project.expect("project");
            (project.project_id, project.repo_remote_hash)
        })
        .collect::<Vec<_>>();
    identities.sort();
    identities
}

struct DeletedCheckoutFixture {
    _dir: tempfile::TempDir,
    session: std::path::PathBuf,
    workspace: std::path::PathBuf,
    source: SourceLocation,
}

fn deleted_checkout_fixture() -> DeletedCheckoutFixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    let workspace = dir.path().join("workspace").join("example-repo");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_config(&workspace);
    let session = sessions.join("rollout.jsonl");
    write_session(&session, &workspace, 1);
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );
    DeletedCheckoutFixture {
        _dir: dir,
        session,
        workspace,
        source,
    }
}

#[test]
fn reimporting_usage_from_a_deleted_checkout_keeps_its_repository_project() {
    let fixture = deleted_checkout_fixture();
    let store = Store::in_memory().expect("store");
    store.upsert_source(&fixture.source).expect("source");

    scan_codex(&store, false);
    let before = stored_identities(&store);
    assert_eq!(before.len(), 1);
    assert!(before[0].1.is_some(), "the first scan reads the remote");

    std::fs::remove_dir_all(&fixture.workspace).expect("delete checkout");

    // The file changed, so its records are pruned and parsed again.
    write_session(&fixture.session, &fixture.workspace, 2);
    scan_codex(&store, false);
    let after_prune = stored_identities(&store);
    assert_eq!(after_prune.len(), 2);
    assert!(after_prune.iter().all(|identity| identity == &before[0]));

    // A full re-import deletes the source's rows before writing them again.
    scan_codex(&store, true);
    assert_eq!(stored_identities(&store), after_prune);
}

#[test]
fn a_deleted_checkout_the_store_never_saw_as_a_repository_stays_path_only() {
    let fixture = deleted_checkout_fixture();
    std::fs::remove_dir_all(&fixture.workspace).expect("delete checkout");
    let store = Store::in_memory().expect("store");
    store.upsert_source(&fixture.source).expect("source");

    scan_codex(&store, false);

    let identities = stored_identities(&store);
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].1, None);
}
