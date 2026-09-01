use super::super::test_support::*;
use super::*;
use std::collections::BTreeSet;
use std::fs;
use tempfile::TempDir;

fn test_git_stdout(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn local_git_scan_counts_commits_without_remote_access() {
    let temp = TempDir::new().unwrap();
    run_test_git(temp.path(), &["init", "-q"]);
    run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
    run_test_git(temp.path(), &["config", "user.name", "Test"]);
    run_test_git(
        temp.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://127.0.0.1:1/unreachable.git",
        ],
    );
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/lib.rs"), "one\ntwo\n").unwrap();
    run_test_git(temp.path(), &["add", "src/lib.rs"]);
    run_test_git(temp.path(), &["commit", "-qm", "initial"]);

    let scan = scan_local_git_repository(temp.path(), Some("project")).unwrap();
    assert_eq!(scan.commits.len(), 1);
    assert_eq!(scan.commits[0].files[0].counts.source_additions, 2);
    assert_eq!(scan.commits[0].files[0].counts.source_deletions, 0);
}

#[test]
fn git_scan_fails_only_when_no_committer_identity_has_ever_been_known() {
    let temp = TempDir::new().unwrap();
    run_test_git(temp.path(), &["init", "-q"]);
    run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
    run_test_git(temp.path(), &["config", "user.name", "Test"]);
    fs::write(temp.path().join("lib.rs"), "one\n").unwrap();
    run_test_git(temp.path(), &["add", "lib.rs"]);
    run_test_git(temp.path(), &["commit", "-qm", "initial"]);

    // Scanning while the identity is configured is how a repository comes to
    // remember it, so the remembered set comes from the scan itself rather
    // than being assumed here.
    let remembered = scan_local_git_repository(temp.path(), None).expect("initial scan");
    let known = remembered.committer_identities.clone();

    // Unsetting the repository value would fall back to the global one, so
    // the configured identity is emptied outright.
    run_test_git(temp.path(), &["config", "user.email", ""]);
    // A repository seen for the first time without an identity cannot say
    // whose commits it holds, which is an unanswerable question rather than
    // an answer of "none".
    let unknown = scan_local_git_repository(temp.path(), None);
    assert!(matches!(
        unknown,
        Err(GitScanError::UnknownCommitterIdentity(_))
    ));

    // One remembered identity answers it, so the same repository scans
    // cleanly with no address configured at all.
    let scan = scan_local_git_repository_cached(temp.path(), None, &[], &known)
        .expect("scan under a remembered identity");
    assert_eq!(scan.commits.len(), 1);
    assert_eq!(scan.coverage, CoverageStatus::Complete);
    assert_eq!(
        scan.committer_identities,
        BTreeSet::from([committer_identity_hash("test@example.com")])
    );
}

#[test]
fn git_scan_matches_every_identity_the_repository_is_known_by() {
    let temp = TempDir::new().unwrap();
    run_test_git(temp.path(), &["init", "-q"]);
    run_test_git(temp.path(), &["config", "user.name", "Test"]);
    run_test_git(temp.path(), &["config", "user.email", "first@example.com"]);
    fs::write(temp.path().join("lib.rs"), "one\n").unwrap();
    run_test_git(temp.path(), &["add", "lib.rs"]);
    run_test_git(temp.path(), &["commit", "-qm", "first"]);
    let remembered = scan_local_git_repository(temp.path(), None).expect("first scan");

    run_test_git(temp.path(), &["config", "user.email", "second@example.com"]);
    fs::write(temp.path().join("lib.rs"), "one\ntwo\n").unwrap();
    run_test_git(temp.path(), &["add", "lib.rs"]);
    run_test_git(temp.path(), &["commit", "-qm", "second"]);

    // Only the address configured now, so the earlier commit is somebody
    // else's work as far as this scan can tell.
    let configured_only = scan_local_git_repository(temp.path(), None).unwrap();
    assert_eq!(configured_only.commits.len(), 1);

    // Carrying the earlier identity forward recovers it, and the scan
    // reports both identities so the caller can keep remembering them.
    let known = remembered.committer_identities.clone();
    let both = scan_local_git_repository_cached(temp.path(), None, &[], &known).unwrap();
    assert_eq!(both.commits.len(), 2);
    assert_eq!(
        both.committer_identities,
        BTreeSet::from([
            committer_identity_hash("first@example.com"),
            committer_identity_hash("second@example.com"),
        ])
    );
}

#[test]
fn committer_identity_hash_ignores_case_and_surrounding_space() {
    assert_eq!(
        committer_identity_hash("  Test@Example.COM \n"),
        committer_identity_hash("test@example.com")
    );
    assert_ne!(
        committer_identity_hash("test@example.com"),
        committer_identity_hash("other@example.com")
    );
}

#[test]
fn git_scan_only_counts_recent_local_branch_commits_from_configured_identity() {
    let temp = TempDir::new().unwrap();
    run_test_git(temp.path(), &["init", "-q"]);
    run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
    run_test_git(temp.path(), &["config", "user.name", "Test"]);

    commit_test_file(
        temp.path(),
        "old.rs",
        "old\n",
        "old local",
        "test@example.com",
        Utc::now() - chrono::Duration::days(120),
    );
    let main_branch = test_git_stdout(temp.path(), &["branch", "--show-current"]);
    commit_test_file(
        temp.path(),
        "foreign.rs",
        "foreign\n",
        "recent foreign",
        "other@example.com",
        Utc::now() - chrono::Duration::days(2),
    );
    commit_test_file(
        temp.path(),
        "recent.rs",
        "recent\n",
        "recent local",
        "test@example.com",
        Utc::now() - chrono::Duration::days(1),
    );

    run_test_git(temp.path(), &["checkout", "-qb", "imported"]);
    commit_test_file(
        temp.path(),
        "remote.rs",
        "remote\n",
        "remote-tracking only",
        "test@example.com",
        Utc::now() - chrono::Duration::days(1),
    );
    let remote_hash = test_git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    run_test_git(temp.path(), &["checkout", "-q", &main_branch]);
    run_test_git(
        temp.path(),
        &["update-ref", "refs/remotes/origin/imported", &remote_hash],
    );
    run_test_git(temp.path(), &["branch", "-D", "imported"]);

    let scan = scan_local_git_repository(temp.path(), None).unwrap();

    assert_eq!(scan.commits.len(), 1);
    assert!(scan.commits[0]
        .files
        .iter()
        .any(|file| file.relative_path == Path::new("recent.rs")));
}

#[test]
fn cached_git_scan_reuses_reachable_commits_and_inspects_new_commits() {
    let temp = TempDir::new().unwrap();
    run_test_git(temp.path(), &["init", "-q"]);
    run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
    run_test_git(temp.path(), &["config", "user.name", "Test"]);
    fs::write(temp.path().join("first.rs"), "first\n").unwrap();
    run_test_git(temp.path(), &["add", "first.rs"]);
    run_test_git(temp.path(), &["commit", "-qm", "first"]);

    let mut cached = scan_local_git_repository(temp.path(), None).unwrap();
    let cached_hash = cached.commits[0].commit_hash.clone();
    cached.commits[0].files[0].counts.source_additions = 99;
    fs::write(temp.path().join("second.rs"), "second\n").unwrap();
    run_test_git(temp.path(), &["add", "second.rs"]);
    run_test_git(temp.path(), &["commit", "-qm", "second"]);

    let refreshed =
        scan_local_git_repository_cached(temp.path(), None, &cached.commits, &BTreeSet::new())
            .unwrap();
    assert_eq!(refreshed.commits.len(), 2);
    let reused = refreshed
        .commits
        .iter()
        .find(|commit| commit.commit_hash == cached_hash)
        .expect("cached commit remains reachable");
    assert_eq!(reused.files[0].counts.source_additions, 99);
    assert!(refreshed.commits.iter().any(|commit| {
        commit
            .files
            .iter()
            .any(|file| file.relative_path == Path::new("second.rs"))
    }));
}

#[test]
fn initial_git_scan_batches_uncached_commit_patches() {
    let temp = TempDir::new().unwrap();
    run_test_git(temp.path(), &["init", "-q"]);
    run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
    run_test_git(temp.path(), &["config", "user.name", "Test"]);
    for index in 0..3 {
        fs::write(
            temp.path().join(format!("file-{index}.rs")),
            format!("{index}\n"),
        )
        .unwrap();
        run_test_git(temp.path(), &["add", "."]);
        run_test_git(temp.path(), &["commit", "-qm", &format!("commit {index}")]);
    }

    TEST_GIT_COMMAND_COUNT.set(0);
    let scan = scan_local_git_repository(temp.path(), None).unwrap();

    assert_eq!(scan.commits.len(), 3);
    assert_eq!(
        TEST_GIT_COMMAND_COUNT.get(),
        6,
        "root lookup, identity config/roots, committer lookup, metadata log, and one patch batch"
    );
}

#[test]
fn git_scan_handles_renames_and_excludes_binary_files() {
    let temp = TempDir::new().unwrap();
    run_test_git(temp.path(), &["init", "-q"]);
    run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
    run_test_git(temp.path(), &["config", "user.name", "Test"]);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/lib.rs"), "one\ntwo\n").unwrap();
    run_test_git(temp.path(), &["add", "src/lib.rs"]);
    run_test_git(temp.path(), &["commit", "-qm", "initial"]);
    fs::write(temp.path().join("image.bin"), [0, 159, 146, 150]).unwrap();
    run_test_git(temp.path(), &["add", "image.bin"]);
    run_test_git(temp.path(), &["commit", "-qm", "binary"]);
    fs::rename(
        temp.path().join("src/lib.rs"),
        temp.path().join("src/main.rs"),
    )
    .unwrap();
    fs::write(temp.path().join("src/main.rs"), "one\ntwo\nthree\n").unwrap();
    run_test_git(temp.path(), &["add", "-A"]);
    run_test_git(temp.path(), &["commit", "-qm", "rename"]);

    let scan = scan_local_git_repository(temp.path(), None).unwrap();
    assert_eq!(scan.commits.len(), 3);
    let binary_commit = scan
        .commits
        .iter()
        .find(|commit| commit.files.is_empty())
        .expect("binary-only commit is excluded");
    assert!(binary_commit.files.is_empty());
    let renamed = scan
        .commits
        .iter()
        .flat_map(|commit| &commit.files)
        .find(|file| file.relative_path == Path::new("src/main.rs"))
        .expect("renamed file");
    assert_eq!(renamed.counts.source_additions, 1);
}

#[test]
fn git_scan_handles_linked_worktrees_branches_and_merges() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repository");
    let worktree = temp.path().join("feature-worktree");
    fs::create_dir_all(&repository).unwrap();
    run_test_git(&repository, &["init", "-q"]);
    run_test_git(&repository, &["config", "user.email", "test@example.com"]);
    run_test_git(&repository, &["config", "user.name", "Test"]);
    fs::write(repository.join("base.rs"), "base\n").unwrap();
    run_test_git(&repository, &["add", "base.rs"]);
    run_test_git(&repository, &["commit", "-qm", "base"]);

    let worktree_text = worktree.to_string_lossy().into_owned();
    run_test_git(
        &repository,
        &["worktree", "add", "-q", "-b", "feature", &worktree_text],
    );
    fs::write(worktree.join("feature.rs"), "feature\n").unwrap();
    run_test_git(&worktree, &["add", "feature.rs"]);
    run_test_git(&worktree, &["commit", "-qm", "feature"]);

    fs::write(repository.join("main.rs"), "main\n").unwrap();
    run_test_git(&repository, &["add", "main.rs"]);
    run_test_git(&repository, &["commit", "-qm", "main"]);
    run_test_git(
        &repository,
        &["merge", "--no-ff", "-qm", "merge", "feature"],
    );

    let main_scan = scan_local_git_repository(&repository, None).unwrap();
    let worktree_scan = scan_local_git_repository(&worktree, None).unwrap();
    assert_eq!(main_scan.repository_hash, worktree_scan.repository_hash);
    assert_eq!(main_scan.commits.len(), worktree_scan.commits.len());
    assert!(
        main_scan
            .commits
            .iter()
            .all(|commit| commit.parent_count <= 1),
        "merge commits replay branch churn and must not be counted"
    );
    assert_eq!(
        main_scan
            .commits
            .iter()
            .flat_map(|commit| &commit.files)
            .filter(|file| file.relative_path == Path::new("feature.rs"))
            .map(|file| file.counts.source_additions)
            .sum::<u64>(),
        1,
        "a merged branch's additions are counted once"
    );
}

#[test]
fn repository_identity_normalizes_ssh_and_https_origin_urls() {
    let repository = TempDir::new().unwrap();
    run_test_git(repository.path(), &["init", "-q"]);
    run_test_git(
        repository.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_test_git(repository.path(), &["config", "user.name", "Test"]);
    fs::write(repository.path().join("main.rs"), "fn main() {}\n").unwrap();
    run_test_git(repository.path(), &["add", "main.rs"]);
    run_test_git(repository.path(), &["commit", "-qm", "initial"]);
    run_test_git(
        repository.path(),
        &["remote", "add", "origin", "git@github.com:Owner/Repo.git"],
    );

    let ssh_scan = scan_local_git_repository(repository.path(), None).unwrap();
    run_test_git(
        repository.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/Owner/Repo.git",
        ],
    );
    let https_scan = scan_local_git_repository(repository.path(), None).unwrap();

    assert_eq!(ssh_scan.repository_hash, https_scan.repository_hash);
    assert_eq!(
        ssh_scan.commits[0].deduplication_id,
        https_scan.commits[0].deduplication_id
    );
}

#[test]
fn remote_repository_identity_ignores_different_local_root_sets() {
    let repository = TempDir::new().unwrap();
    run_test_git(repository.path(), &["init", "-q"]);
    run_test_git(
        repository.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_test_git(repository.path(), &["config", "user.name", "Test"]);
    run_test_git(
        repository.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repository.git",
        ],
    );
    fs::write(repository.path().join("main.rs"), "fn main() {}\n").unwrap();
    run_test_git(repository.path(), &["add", "main.rs"]);
    run_test_git(repository.path(), &["commit", "-qm", "initial"]);
    let shared_commit = test_git_stdout(repository.path(), &["rev-parse", "HEAD"]);
    let initial_scan = scan_local_git_repository(repository.path(), None).unwrap();

    run_test_git(
        repository.path(),
        &["checkout", "-q", "--orphan", "unrelated"],
    );
    fs::write(
        repository.path().join("unrelated.rs"),
        "pub fn unrelated() {}\n",
    )
    .unwrap();
    run_test_git(repository.path(), &["add", "unrelated.rs"]);
    run_test_git(repository.path(), &["commit", "-qm", "unrelated root"]);
    let expanded_scan = scan_local_git_repository(repository.path(), None).unwrap();

    assert_eq!(initial_scan.repository_hash, expanded_scan.repository_hash);
    let initial_metric_key = initial_scan
        .commits
        .iter()
        .find(|commit| commit.commit_hash == shared_commit)
        .unwrap()
        .deduplication_id
        .clone();
    let expanded_metric_key = expanded_scan
        .commits
        .iter()
        .find(|commit| commit.commit_hash == shared_commit)
        .unwrap()
        .deduplication_id
        .clone();
    assert_eq!(initial_metric_key, expanded_metric_key);
}

#[test]
fn rebased_commits_replace_unreachable_ids_and_deduplicate_across_devices() {
    let temp = TempDir::new().unwrap();
    run_test_git(temp.path(), &["init", "-q"]);
    run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
    run_test_git(temp.path(), &["config", "user.name", "Test"]);
    fs::write(temp.path().join("base.rs"), "base\n").unwrap();
    run_test_git(temp.path(), &["add", "base.rs"]);
    run_test_git(temp.path(), &["commit", "-qm", "base"]);
    let main_branch = test_git_stdout(temp.path(), &["branch", "--show-current"]);
    run_test_git(temp.path(), &["checkout", "-qb", "feature"]);
    fs::write(temp.path().join("feature.rs"), "before rebase\n").unwrap();
    run_test_git(temp.path(), &["add", "feature.rs"]);
    run_test_git(temp.path(), &["commit", "-qm", "feature"]);
    let old_hash = test_git_stdout(temp.path(), &["rev-parse", "HEAD"]);

    run_test_git(temp.path(), &["checkout", "-q", &main_branch]);
    fs::write(temp.path().join("main.rs"), "main\n").unwrap();
    run_test_git(temp.path(), &["add", "main.rs"]);
    run_test_git(temp.path(), &["commit", "-qm", "main"]);
    run_test_git(temp.path(), &["checkout", "-q", "feature"]);
    run_test_git(temp.path(), &["rebase", &main_branch]);
    let new_hash = test_git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    assert_ne!(old_hash, new_hash);

    let scan = scan_local_git_repository(temp.path(), None).unwrap();
    assert!(scan
        .commits
        .iter()
        .any(|commit| commit.commit_hash == new_hash));
    assert!(!scan
        .commits
        .iter()
        .any(|commit| commit.commit_hash == old_hash));
    let committed_metric_ids = scan
        .commits
        .iter()
        .enumerate()
        .map(|(index, commit)| {
            (
                commit.deduplication_id.clone(),
                format!("opaque-commit-{index}"),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let first = build_code_change_metrics(
        Utc::now(),
        "device-a",
        &[],
        std::slice::from_ref(&scan),
        &[],
        &committed_metric_ids,
        CoverageStatus::Unavailable,
    )
    .unwrap();
    let second = build_code_change_metrics(
        Utc::now(),
        "device-b",
        &[],
        &[scan],
        &[],
        &committed_metric_ids,
        CoverageStatus::Unavailable,
    )
    .unwrap();
    let first_ids = first
        .metrics
        .iter()
        .filter(|metric| metric.kind == CodeChangeMetricKind::Committed)
        .map(|metric| &metric.metric_id)
        .collect::<BTreeSet<_>>();
    let second_ids = second
        .metrics
        .iter()
        .filter(|metric| metric.kind == CodeChangeMetricKind::Committed)
        .map(|metric| &metric.metric_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(first_ids, second_ids);
}

#[test]
fn commitless_repositories_without_a_remote_do_not_share_one_identity() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    for repository in [&first, &second] {
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
    }

    let first_scan = scan_local_git_repository(first.path(), None).unwrap();
    let second_scan = scan_local_git_repository(second.path(), None).unwrap();
    assert!(first_scan.commits.is_empty());
    assert_ne!(
        first_scan.repository_hash, second_scan.repository_hash,
        "unrelated repositories must not merge before their first commit"
    );

    // The first commit supplies a stable root, and the identity re-keys to
    // one that other devices can derive too.
    fs::write(first.path().join("main.rs"), "fn main() {}\n").unwrap();
    run_test_git(first.path(), &["add", "main.rs"]);
    run_test_git(first.path(), &["commit", "-qm", "initial"]);
    let committed = scan_local_git_repository(first.path(), None).unwrap();
    assert_ne!(committed.repository_hash, first_scan.repository_hash);
}
