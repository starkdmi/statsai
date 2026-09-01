use super::*;
pub(super) use statsai_core::{
    CodeCategory, CodeLineCounts, SyncBatch, CODE_CHANGE_METRIC_SCHEMA_VERSION,
    SYNC_BATCH_SCHEMA_VERSION,
};
pub(super) use std::{fs, path::Path, process::Command};
pub(super) use tempfile::TempDir;

/// Repoints the recorded project path at `path`, as a moved worktree would.
pub(super) fn repoint_project_evidence(store: &Store, path: &Path, summary_id: &str) {
    store
        .conn
        .execute("DELETE FROM usage_summaries", [])
        .expect("drop previous evidence");
    insert_project_evidence(store, path, "project", summary_id);
}

/// Creates a repository holding exactly one measurable commit.
pub(super) fn init_test_repository(path: &Path) {
    run_test_git(path, &["init", "-q"]);
    run_test_git(path, &["config", "user.email", "test@example.com"]);
    run_test_git(path, &["config", "user.name", "Test"]);
    fs::write(path.join("main.rs"), "fn main() {}\n").expect("write source");
    run_test_git(path, &["add", "main.rs"]);
    run_test_git(path, &["commit", "-qm", "initial"]);
}

/// Seeds an aged committed metric owned by `repository_hash`.
///
/// Its day is older than the scan window, so no rebuild can reach it and it
/// survives only through the carry-forward path.
pub(super) fn seed_aged_committed_metric(store: &Store, metric_id: &str, repository_hash: &str) {
    let observation_start_day = statsai_core::git_observation_start_day(Utc::now());
    let aged = CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: metric_id.to_string(),
        device_id: "device".to_string(),
        day: observation_start_day.pred_opt().expect("historical day"),
        project_id: Some("project".to_string()),
        repository_hash: Some(repository_hash.to_string()),
        commit_hash: Some("aged-commit".to_string()),
        kind: CodeChangeMetricKind::Committed,
        counts: CodeLineCounts::classified(CodeCategory::Source, 5, 1),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Unavailable,
        git_coverage: CoverageStatus::Complete,
    };
    store
        .replace_matches_and_metrics("device", &[], std::slice::from_ref(&aged))
        .expect("seed aged metric");
}

pub(super) fn stored_repository_hash(store: &Store) -> String {
    store
        .conn
        .query_row("SELECT repository_hash FROM code_git_scans", [], |row| {
            row.get(0)
        })
        .expect("stored scan")
}

pub(super) fn metric_exists(store: &Store, metric_id: &str) -> bool {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM code_change_metrics WHERE metric_id = ?1",
            [metric_id],
            |row| row.get::<_, u64>(0),
        )
        .expect("metric count")
        > 0
}

pub(super) fn insert_project_evidence(
    store: &Store,
    path: &Path,
    project_id: &str,
    summary_id: &str,
) {
    insert_optional_project_evidence(store, path, Some(project_id), summary_id);
}

pub(super) fn insert_optional_project_evidence(
    store: &Store,
    path: &Path,
    project_id: Option<&str>,
    summary_id: &str,
) {
    let payload = serde_json::json!({
        "project": {
            "project_id": project_id,
            "path_label": path.to_string_lossy(),
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
            params![summary_id, payload.to_string()],
        )
        .expect("insert project evidence");
}

pub(super) fn run_test_git(path: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success());
}
