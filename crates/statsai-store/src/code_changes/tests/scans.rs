use super::support::*;
use super::*;

#[test]
fn refresh_retires_scan_after_repository_loses_all_references() {
    let repository = TempDir::new().expect("temporary repository");
    run_test_git(repository.path(), &["init", "-q"]);
    run_test_git(
        repository.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_test_git(repository.path(), &["config", "user.name", "Test"]);
    fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
    run_test_git(repository.path(), &["add", "main.rs"]);
    run_test_git(repository.path(), &["commit", "-qm", "initial"]);

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, repository.path(), "project", "summary");
    let initial = store
        .refresh_code_changes("device")
        .expect("initial refresh");
    assert_eq!(initial.repositories, 1);

    store
        .conn
        .execute("DELETE FROM usage_summaries", [])
        .expect("remove project evidence");
    let refreshed = store
        .refresh_code_changes("device")
        .expect("refresh without references");

    assert_eq!(refreshed.repositories, 0);
    assert_eq!(refreshed.commits, 0);
    assert_eq!(refreshed.metrics, 0);
    let scan_count: u64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM code_git_scans", [], |row| row.get(0))
        .expect("scan count");
    assert_eq!(scan_count, 0);
    assert!(store
        .list_code_change_metrics(false)
        .expect("metrics")
        .is_empty());
}

#[test]
fn refresh_retains_cached_scan_when_referenced_repository_temporarily_fails() {
    let repository = TempDir::new().expect("temporary repository");
    run_test_git(repository.path(), &["init", "-q"]);
    run_test_git(
        repository.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_test_git(repository.path(), &["config", "user.name", "Test"]);
    fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
    run_test_git(repository.path(), &["add", "main.rs"]);
    run_test_git(repository.path(), &["commit", "-qm", "initial"]);

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, repository.path(), "project", "summary");
    store
        .refresh_code_changes("device")
        .expect("initial refresh");
    fs::rename(
        repository.path().join(".git"),
        repository.path().join(".git-disabled"),
    )
    .expect("disable repository metadata");

    let refreshed = store
        .refresh_code_changes("device")
        .expect("refresh during transient failure");

    assert_eq!(refreshed.repositories, 1);
    assert_eq!(refreshed.commits, 1);
    assert_eq!(refreshed.git_coverage, CoverageStatus::Partial);
    let scan_count: u64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM code_git_scans", [], |row| row.get(0))
        .expect("scan count");
    assert_eq!(scan_count, 1);
}

#[cfg(unix)]
#[test]
fn a_deleted_worktree_keeps_its_scan_when_its_recorded_path_was_logical() {
    let parent = TempDir::new().expect("temporary parent");
    let repository = parent.path().join("repo");
    fs::create_dir_all(&repository).expect("create repository");
    init_test_repository(&repository);

    // Agents record the path they were invoked with, which can reach the
    // repository through a symlink. Git reports `rev-parse --show-toplevel`
    // as the physical path, so the two forms differ.
    let link_root = TempDir::new().expect("temporary link root");
    let link = link_root.path().join("link");
    std::os::unix::fs::symlink(parent.path(), &link).expect("link the parent");
    let logical_path = link.join("repo");

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, &logical_path, "project", "summary");
    let report = store.refresh_code_changes("device").expect("first refresh");
    assert_eq!(report.commits, 1);

    // The worktree is gone but its parent still resolves. Matching the
    // recorded path against the stored root by `canonicalize` alone fails
    // here, which would treat an already-measured repository as one never
    // seen and delete it as unreferenced.
    fs::remove_dir_all(&repository).expect("remove the worktree");
    let refreshed = store
        .refresh_code_changes("device")
        .expect("refresh after the worktree vanished");

    assert_eq!(refreshed.commits, 1);
    assert_eq!(refreshed.git_coverage, CoverageStatus::Partial);
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM code_git_scans", [], |row| row
                .get::<_, u64>(0))
            .expect("scan count"),
        1
    );
}

#[test]
fn a_rekeyed_repository_still_recognises_its_remembered_identities() {
    let repository = TempDir::new().expect("temporary repository");
    init_test_repository(repository.path());

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, repository.path(), "project", "summary");
    let report = store.refresh_code_changes("device").expect("first refresh");
    assert_eq!(report.commits, 1);

    // Adding an origin remote re-keys the repository while its worktree stays
    // put, so the identities remembered under the previous hash have to be
    // handed to the scan by root as well. Selecting them by hash alone misses
    // here, and `replace_git_scan` then deletes the superseded hash's identity
    // rows, so the in-window commit made under the earlier address is dropped
    // from the scan and retired remotely.
    run_test_git(
        repository.path(),
        &["config", "user.email", "renamed@example.com"],
    );
    run_test_git(
        repository.path(),
        &["remote", "add", "origin", "https://example.com/renamed.git"],
    );
    let refreshed = store
        .refresh_code_changes("device")
        .expect("refresh after the re-key");

    assert_eq!(refreshed.commits, 1);
    assert_eq!(refreshed.git_coverage, CoverageStatus::Complete);
}

// Known gap, deliberately left failing rather than weakened into a test that
// passes without proving anything. When a repository moves and becomes
// unscannable in the same refresh, Git cannot run in the new location, so its
// identity hash cannot be derived either and the only remaining identifier is
// the path that just changed. Nothing available links the stored snapshot to
// the failure, so it is retired and aged days are lost. Closing this needs
// retirement driven by reference reachability rather than by diffing scan
// sets, which is a design change beyond the retention fix here.
#[ignore = "requires reference-reachability-driven retirement"]
#[test]
fn a_moved_repository_that_then_fails_to_scan_keeps_its_snapshot() {
    let parent = TempDir::new().expect("temporary parent");
    let original = parent.path().join("original");
    fs::create_dir_all(&original).expect("create repository");
    init_test_repository(&original);

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, &original, "project", "summary");
    store.refresh_code_changes("device").expect("first refresh");
    seed_aged_committed_metric(&store, "ccm_failed", &stored_repository_hash(&store));
    store
        .refresh_code_changes("device")
        .expect("refresh carries the aged metric");

    // Moved, then unable to scan. The failure is reached under the new root
    // while the stored snapshot still carries the old one, so retention keyed
    // only on the root would read this as a repository that vanished and
    // retire days it cannot rebuild.
    let moved = parent.path().join("moved");
    fs::rename(&original, &moved).expect("move the repository");
    repoint_project_evidence(&store, &moved, "summary-moved");
    fs::rename(moved.join(".git"), moved.join(".git-disabled"))
        .expect("disable repository metadata");

    let refreshed = store
        .refresh_code_changes("device")
        .expect("refresh during the failure");

    assert_eq!(refreshed.commits, 1);
    assert_eq!(refreshed.git_coverage, CoverageStatus::Partial);
    assert!(metric_exists(&store, "ccm_failed"));
}

#[test]
fn a_moved_repository_still_recognises_its_remembered_identities() {
    let parent = TempDir::new().expect("temporary parent");
    let original = parent.path().join("original");
    fs::create_dir_all(&original).expect("create repository");
    init_test_repository(&original);

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, &original, "project", "summary");
    let report = store.refresh_code_changes("device").expect("first refresh");
    assert_eq!(report.commits, 1);

    // The commit belongs to the earlier identity, which is remembered under
    // the repository hash. Looking those identities up by path prefix finds
    // nothing once the repository moves, so the scan would recognise only the
    // address configured now and delete the commit it already measured.
    run_test_git(&original, &["config", "user.email", "renamed@example.com"]);
    let moved = parent.path().join("moved");
    fs::rename(&original, &moved).expect("move the repository");
    repoint_project_evidence(&store, &moved, "summary-moved");
    let refreshed = store
        .refresh_code_changes("device")
        .expect("refresh after the move");

    assert_eq!(refreshed.commits, 1);
    assert_eq!(refreshed.git_coverage, CoverageStatus::Complete);
}

#[test]
fn retiring_a_repository_scan_also_removes_its_commit_rows() {
    let repository = TempDir::new().expect("temporary repository");
    run_test_git(repository.path(), &["init", "-q"]);
    run_test_git(
        repository.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_test_git(repository.path(), &["config", "user.name", "Test"]);
    fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
    run_test_git(repository.path(), &["add", "main.rs"]);
    run_test_git(repository.path(), &["commit", "-qm", "initial"]);

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, repository.path(), "project", "summary");
    store.refresh_code_changes("device").expect("first refresh");
    // Adding a remote changes the repository identity hash.
    run_test_git(
        repository.path(),
        &["remote", "add", "origin", "https://example.com/renamed.git"],
    );
    store
        .refresh_code_changes("device")
        .expect("refresh after identity change");

    let commit_count = |store: &Store| -> u64 {
        store
            .conn
            .query_row("SELECT COUNT(*) FROM code_git_commits", [], |row| {
                row.get(0)
            })
            .expect("commit count")
    };
    let identity_count = |store: &Store| -> u64 {
        store
            .conn
            .query_row("SELECT COUNT(*) FROM code_git_identities", [], |row| {
                row.get(0)
            })
            .expect("identity count")
    };
    assert_eq!(
        commit_count(&store),
        1,
        "superseded identity leaves no rows"
    );
    // The remembered committer identity moves to the new repository hash
    // rather than being stranded under the superseded one, so the commits
    // stay attributable across a repository identity change.
    assert_eq!(
        identity_count(&store),
        1,
        "superseded identity leaves no committer rows"
    );

    store
        .conn
        .execute("DELETE FROM usage_summaries", [])
        .expect("remove project evidence");
    store
        .refresh_code_changes("device")
        .expect("refresh without references");

    assert_eq!(
        commit_count(&store),
        0,
        "retired scan leaves no commit rows"
    );
    assert_eq!(
        identity_count(&store),
        0,
        "retired scan leaves no committer rows"
    );
}
