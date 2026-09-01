use super::support::*;
use super::*;

#[test]
fn refresh_discovers_git_history_from_usage_projects_without_trace_edits() {
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
    let payload = serde_json::json!({
        "project": {
            "project_id": "project-from-summary",
            "path_label": repository.path().to_string_lossy(),
        }
    });
    store
        .conn
        .execute(
            r#"
                INSERT INTO usage_summaries
                  (summary_id, provider, source_id, observed_at, total_tokens, payload)
                VALUES (?1, 'codex', 'source', '2026-08-01T00:00:00Z', 0, ?2)
                "#,
            params!["summary", payload.to_string()],
        )
        .expect("insert project evidence");

    let report = store
        .refresh_code_changes("device")
        .expect("refresh changes");
    assert_eq!(report.repositories, 1);
    assert_eq!(report.commits, 1);
    let metrics = store.list_code_change_metrics(false).expect("metrics");
    assert!(metrics
        .iter()
        .any(|metric| metric.kind == CodeChangeMetricKind::Committed));

    run_test_git(
        repository.path(),
        &["remote", "add", "origin", "https://example.com/renamed.git"],
    );
    let refreshed = store
        .refresh_code_changes("device")
        .expect("refresh after identity change");
    assert_eq!(refreshed.repositories, 1);
    assert_eq!(refreshed.commits, 1);
    let scan_count: u64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM code_git_scans", [], |row| row.get(0))
        .expect("scan count");
    assert_eq!(scan_count, 1);
    let committed_metrics = store
        .list_code_change_metrics(false)
        .expect("refreshed metrics")
        .into_iter()
        .filter(|metric| metric.kind == CodeChangeMetricKind::Committed)
        .count();
    assert_eq!(committed_metrics, 1);
}

#[test]
fn losing_the_committer_identity_retains_already_measured_commits() {
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
    let payload = serde_json::json!({
        "project": {
            "project_id": "project",
            "path_label": repository.path().to_string_lossy(),
        }
    });
    store
        .conn
        .execute(
            r#"
                INSERT INTO usage_summaries
                  (summary_id, provider, source_id, observed_at, total_tokens, payload)
                VALUES (?1, 'codex', 'source', '2026-08-01T00:00:00Z', 0, ?2)
                "#,
            params!["summary", payload.to_string()],
        )
        .expect("insert project evidence");

    let report = store
        .refresh_code_changes("device")
        .expect("refresh changes");
    assert_eq!(report.commits, 1);

    // The repository is already known by the identity that measured these
    // commits, so losing the configured one costs nothing: the scan still
    // recognises them and reports the period as fully measured. Matching
    // only the currently configured address would instead succeed with zero
    // commits and delete work it had already measured.
    // Unsetting the repository value would fall back to the global one, so
    // the configured identity is emptied outright.
    run_test_git(repository.path(), &["config", "user.email", ""]);
    let refreshed = store
        .refresh_code_changes("device")
        .expect("refresh without identity");
    assert_eq!(refreshed.commits, 1);
    assert_eq!(refreshed.git_coverage, CoverageStatus::Complete);
    let committed = store
        .list_code_change_metrics(false)
        .expect("metrics")
        .into_iter()
        .filter(|metric| metric.kind == CodeChangeMetricKind::Committed)
        .count();
    assert_eq!(committed, 1);
}

#[test]
fn changing_the_committer_identity_retains_already_measured_commits() {
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
    let payload = serde_json::json!({
        "project": {
            "project_id": "project",
            "path_label": repository.path().to_string_lossy(),
        }
    });
    store
        .conn
        .execute(
            r#"
                INSERT INTO usage_summaries
                  (summary_id, provider, source_id, observed_at, total_tokens, payload)
                VALUES (?1, 'codex', 'source', '2026-08-01T00:00:00Z', 0, ?2)
                "#,
            params!["summary", payload.to_string()],
        )
        .expect("insert project evidence");

    let report = store
        .refresh_code_changes("device")
        .expect("refresh changes");
    assert_eq!(report.commits, 1);

    // Reconfiguring the identity does not rewrite the commits already in
    // the object database, and the work they hold was still this user's.
    // Filtering on whichever address is configured right now would report
    // an authoritative scan of zero commits, deleting measured history from
    // the store and retiring it remotely.
    run_test_git(
        repository.path(),
        &["config", "user.email", "renamed@example.com"],
    );
    let refreshed = store
        .refresh_code_changes("device")
        .expect("refresh after rename");
    assert_eq!(refreshed.commits, 1);
    assert_eq!(refreshed.git_coverage, CoverageStatus::Complete);
    let committed = store
        .list_code_change_metrics(false)
        .expect("metrics")
        .into_iter()
        .filter(|metric| metric.kind == CodeChangeMetricKind::Committed)
        .count();
    assert_eq!(committed, 1);
}

#[test]
fn project_paths_outside_any_repository_do_not_degrade_git_coverage() {
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
    // A scratch directory an agent ran in once, and a directory that has
    // since been removed. Neither was ever a repository.
    let scratch = TempDir::new().expect("scratch directory");
    let removed = TempDir::new().expect("removed directory");
    let removed_path = removed.path().to_path_buf();
    drop(removed);

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, repository.path(), "project", "summary");
    insert_project_evidence(&store, scratch.path(), "scratch", "scratch-summary");
    insert_project_evidence(&store, &removed_path, "removed", "removed-summary");

    let report = store
        .refresh_code_changes("device")
        .expect("refresh with non-repository paths");

    assert_eq!(report.repositories, 1);
    assert_eq!(report.commits, 1);
    assert_eq!(report.git_coverage, CoverageStatus::Complete);
    assert!(store
        .list_code_change_metrics(false)
        .expect("metrics")
        .iter()
        .all(|metric| metric.git_coverage == CoverageStatus::Complete));
}

#[test]
fn a_deleted_subdirectory_of_a_healthy_repository_keeps_git_coverage_complete() {
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
    let workspace = repository.path().join("fixtures");
    fs::create_dir_all(&workspace).expect("create subdirectory");

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, repository.path(), "project", "root-summary");
    insert_project_evidence(&store, &workspace, "project", "subdirectory-summary");
    assert_eq!(
        store
            .refresh_code_changes("device")
            .expect("initial refresh")
            .git_coverage,
        CoverageStatus::Complete
    );

    // The agent's working subdirectory is removed, but the repository it
    // lives in still scans cleanly through the recorded root path.
    fs::remove_dir_all(&workspace).expect("remove subdirectory");
    let refreshed = store
        .refresh_code_changes("device")
        .expect("refresh after the subdirectory was removed");

    assert_eq!(refreshed.repositories, 1);
    assert_eq!(refreshed.commits, 1);
    assert_eq!(
        refreshed.git_coverage,
        CoverageStatus::Complete,
        "a vanished subdirectory of a healthy repository is not a lost measurement"
    );
}

#[test]
fn shared_git_roots_do_not_assign_all_commits_to_one_nested_project() {
    let repository = TempDir::new().expect("temporary repository");
    run_test_git(repository.path(), &["init", "-q"]);
    run_test_git(
        repository.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_test_git(repository.path(), &["config", "user.name", "Test"]);
    let project_a = repository.path().join("packages/a");
    let project_b = repository.path().join("packages/b");
    fs::create_dir_all(&project_a).expect("create project a");
    fs::create_dir_all(&project_b).expect("create project b");
    fs::write(project_a.join("a.rs"), "pub fn a() {}\n").expect("write project a");
    fs::write(project_b.join("b.rs"), "pub fn b() {}\n").expect("write project b");
    run_test_git(repository.path(), &["add", "."]);
    run_test_git(repository.path(), &["commit", "-qm", "initial"]);

    let store = Store::in_memory().expect("store");
    insert_project_evidence(&store, &project_a, "project-a", "summary-a");
    insert_project_evidence(&store, &project_b, "project-b", "summary-b");

    let report = store
        .refresh_code_changes("device")
        .expect("refresh shared root");
    let committed = store
        .list_code_change_metrics(false)
        .expect("metrics")
        .into_iter()
        .filter(|metric| metric.kind == CodeChangeMetricKind::Committed)
        .collect::<Vec<_>>();

    assert_eq!(report.repositories, 1);
    assert_eq!(committed.len(), 1);
    assert!(committed[0].project_id.is_none());
}

#[test]
fn repository_projects_preserve_only_unambiguous_project_ids_per_path() {
    let store = Store::in_memory().expect("store");
    let ambiguous_path = Path::new("/workspace/ambiguous");
    insert_project_evidence(&store, ambiguous_path, "project-a", "ambiguous-a");
    insert_project_evidence(&store, ambiguous_path, "project-b", "ambiguous-b");
    let consistent_path = Path::new("/workspace/consistent");
    insert_project_evidence(&store, consistent_path, "project-c", "consistent-a");
    insert_project_evidence(&store, consistent_path, "project-c", "consistent-b");
    insert_optional_project_evidence(&store, consistent_path, None, "consistent-null");
    let unidentified_path = Path::new("/workspace/unidentified");
    insert_optional_project_evidence(&store, unidentified_path, None, "unidentified");

    let projects = store.repository_projects().expect("repository projects");

    assert_eq!(projects.get(ambiguous_path), Some(&None));
    assert_eq!(
        projects.get(consistent_path),
        Some(&Some("project-c".to_string()))
    );
    assert_eq!(projects.get(unidentified_path), Some(&None));
}
