use super::support::*;
use super::*;
use crate::sync_payload::{
    sanitize_session_rollup_for_sync, sanitize_session_rollup_for_sync_with_projects,
    summary_for_sync,
};

fn project() -> ProjectInfo {
    ProjectInfo {
        project_id: "project-payload".to_string(),
        project_label: Some("payload".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/payload".to_string()),
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/tmp/payload".to_string()),
    }
}

/// A store with one titled session in a project, so it has a daily rollup and
/// a session rollup that both carry the project.
fn store_with_project_usage() -> Store {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sync-payload"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let at = Utc
        .with_ymd_and_hms(2026, 7, 1, 9, 0, 0)
        .single()
        .expect("time");
    let mut event = test_store_event(&source, at, "payload-record");
    event.session.title = Some("Sync payload session".to_string());
    event.project = Some(project());
    store.insert_event(&event).expect("event");
    store
}

#[test]
fn summary_for_sync_drops_the_project_unless_included_and_record_ids_always() {
    let store = store_with_project_usage();
    let summaries = store.all_sync_rollup_summaries().expect("rollups");
    assert_eq!(summaries.len(), 1);
    let stored = summaries[0].clone();
    assert!(stored.project.is_some());

    for include_projects in [false, true] {
        for accepts_cache_health in [false, true] {
            let sent = summary_for_sync(stored.clone(), include_projects, accepts_cache_health);
            assert_eq!(
                sent,
                summary_for_sync_target(
                    sanitize_summary_for_sync_with_projects(stored.clone(), include_projects),
                    accepts_cache_health,
                )
            );
            assert_eq!(sent.project.is_some(), include_projects);
            assert_eq!(sent.source.source_record_id, None);
        }
    }
}

#[test]
fn session_rollups_lose_their_project_unless_projects_are_included() {
    let store = store_with_project_usage();
    let rollups = store.all_session_rollups().expect("session rollups");
    assert_eq!(rollups.len(), 1);
    let stored = rollups[0].clone();
    assert!(stored.project.is_some());

    let included = sanitize_session_rollup_for_sync_with_projects(stored.clone(), true);
    assert_eq!(included, sanitize_session_rollup_for_sync(stored.clone()));
    assert!(included.project.is_some());

    let excluded = sanitize_session_rollup_for_sync_with_projects(stored, false);
    assert_eq!(excluded.project, None);
    assert_eq!(
        excluded,
        statsai_core::SessionRollupV1 {
            project: None,
            ..included
        }
    );
}
