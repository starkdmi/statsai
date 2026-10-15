use super::support::*;
use super::*;
use statsai_core::{
    archive_conversation_id, path_hash, ArchiveCompleteness, ArchiveConversation,
    ARCHIVE_CONVERSATION_SCHEMA_VERSION,
};

const REPO_HASH: &str = "repo-hash-example";

fn repo_project(folder: &Path) -> ProjectInfo {
    let path_hash = path_hash(folder);
    ProjectInfo {
        project_id: format!("project_repo_{}", &path_hash[..8]),
        project_label: Some("checkout".to_string()),
        repo_remote_hash: Some(REPO_HASH.to_string()),
        repo_label: Some("example-org/example-repo".to_string()),
        branch_hash: None,
        branch_label: None,
        path_hash: Some(path_hash),
        path_label: Some(folder.to_string_lossy().to_string()),
    }
}

fn path_only_project(folder: &Path) -> ProjectInfo {
    let path_hash = path_hash(folder);
    ProjectInfo {
        project_id: format!("project_path_{}", &path_hash[..8]),
        repo_remote_hash: None,
        repo_label: None,
        ..repo_project(folder)
    }
}

fn source() -> statsai_core::SourceLocation {
    statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-project-identity"),
        LocationOrigin::Configured,
    )
}

fn event_at(
    source: &statsai_core::SourceLocation,
    record_id: &str,
    project: ProjectInfo,
) -> UsageEvent {
    let started_at = Utc
        .with_ymd_and_hms(2026, 7, 1, 12, 0, 0)
        .single()
        .expect("started_at");
    let mut event = test_store_event(source, started_at, record_id);
    event.project = Some(project);
    event
}

fn archive_conversation(
    source: &statsai_core::SourceLocation,
    project: ProjectInfo,
) -> ArchiveConversation {
    ArchiveConversation {
        schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
        conversation_id: archive_conversation_id("codex", "thread-identity"),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        native_conversation_id: "thread-identity".to_string(),
        title: None,
        project: Some(project),
        started_at: None,
        updated_at: None,
        completeness: ArchiveCompleteness::Complete,
        missing_content_count: 0,
        missing_content_scope_id: None,
        discarded_source_record_ids: Vec::new(),
        superseded_conversation_ids: Vec::new(),
        items: Vec::new(),
    }
}

fn stored_project(store: &Store) -> ProjectInfo {
    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
    events[0].project.clone().expect("project")
}

#[test]
fn restore_prefers_the_rows_being_replaced() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");

    let mut replaced = LocationIdentities::default();
    replaced.observe(&event_at(&source, "old", repo_project(&deleted)));
    let mut events = vec![event_at(&source, "new", path_only_project(&deleted))];

    let restored = store
        .restore_project_identities(&mut events, &replaced)
        .expect("restore");

    assert_eq!(restored, 1);
    assert_eq!(events[0].project, Some(repo_project(&deleted)));
}

#[test]
fn restore_reads_other_usage_at_the_same_location() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    store
        .insert_event(&event_at(&source, "earlier", repo_project(&deleted)))
        .expect("insert repo-backed event");

    let mut events = vec![event_at(&source, "later", path_only_project(&deleted))];
    store
        .restore_project_identities(&mut events, &LocationIdentities::default())
        .expect("restore");

    let project = events[0].project.as_ref().expect("project");
    assert_eq!(project.project_id, repo_project(&deleted).project_id);
    assert_eq!(project.repo_remote_hash.as_deref(), Some(REPO_HASH));
}

#[test]
fn restore_leaves_a_readable_folder_path_only() {
    // A folder the scan can read answered for itself: it has no repository now.
    let dir = tempfile::tempdir().expect("tempdir");
    let readable = dir.path().join("plain-folder");
    std::fs::create_dir_all(&readable).expect("folder");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");

    let mut replaced = LocationIdentities::default();
    replaced.observe(&event_at(&source, "old", repo_project(&readable)));
    let mut events = vec![event_at(&source, "new", path_only_project(&readable))];

    let restored = store
        .restore_project_identities(&mut events, &replaced)
        .expect("restore");

    assert_eq!(restored, 0);
    assert_eq!(events[0].project, Some(path_only_project(&readable)));
}

#[test]
fn deleting_a_files_events_reports_their_repository_identities() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    let mut event = event_at(&source, "old", repo_project(&deleted));
    event.parse_evidence = Some(ParseEvidence {
        event_key_version: "test.v1".to_string(),
        source_file_path_hash: Some("file-hash".to_string()),
        source_line_number: Some(1),
        source_record_id: Some("old".to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: statsai_core::IdentitySource::Unresolved,
    });
    store.insert_event(&event).expect("insert");

    let impact = store
        .delete_events_for_source_file_hashes(&source.source_id, &["file-hash".to_string()])
        .expect("delete");

    assert_eq!(impact.deleted, 1);
    let mut events = vec![event_at(&source, "new", path_only_project(&deleted))];
    store
        .restore_project_identities(&mut events, &impact.replaced_identities)
        .expect("restore");
    assert_eq!(events[0].project, Some(repo_project(&deleted)));
}

#[test]
fn repair_restores_a_path_only_row_from_an_archive_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    store
        .upsert_archive_conversations(&[archive_conversation(&source, repo_project(&deleted))])
        .expect("archive");
    store
        .insert_event(&event_at(&source, "rebuilt", path_only_project(&deleted)))
        .expect("insert path-only event");
    let synced = store
        .dirty_sync_rollup_summaries()
        .expect("dirty rollups")
        .into_iter()
        .map(|summary| summary.summary_id)
        .collect::<Vec<_>>();
    store
        .mark_sync_rollups_synced(&synced)
        .expect("mark rollups synced");
    assert!(store
        .dirty_sync_rollup_summaries()
        .expect("dirty rollups")
        .is_empty());

    let repaired = store.repair_project_identities().expect("repair");

    assert_eq!(repaired, 1);
    let project = stored_project(&store);
    assert_eq!(project.project_id, repo_project(&deleted).project_id);
    assert_eq!(project.repo_remote_hash.as_deref(), Some(REPO_HASH));
    assert_eq!(project.path_hash, repo_project(&deleted).path_hash);
    let dirty = store.dirty_sync_rollup_summaries().expect("dirty rollups");
    assert_eq!(dirty.len(), 1, "the corrected rollup must sync again");
    assert_eq!(
        dirty[0]
            .project
            .as_ref()
            .and_then(|project| project.repo_remote_hash.as_deref()),
        Some(REPO_HASH)
    );
}

#[test]
fn repair_runs_once_and_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    store
        .upsert_archive_conversations(&[archive_conversation(&source, repo_project(&deleted))])
        .expect("archive");
    store
        .insert_event(&event_at(&source, "rebuilt", path_only_project(&deleted)))
        .expect("insert path-only event");

    assert_eq!(store.repair_project_identities().expect("first repair"), 1);
    let repaired = stored_project(&store);
    assert_eq!(store.repair_project_identities().expect("second repair"), 0);

    // Even with the marker gone, a repaired store has nothing left to change.
    store
        .conn
        .execute(
            "DELETE FROM local_metadata WHERE key = 'project_identity_repair.v1'",
            [],
        )
        .expect("clear marker");
    assert_eq!(store.repair_project_identities().expect("third repair"), 0);
    assert_eq!(stored_project(&store), repaired);
}

#[test]
fn repair_leaves_rows_without_a_known_repository() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("never-a-repository");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    store
        .insert_event(&event_at(&source, "plain", path_only_project(&deleted)))
        .expect("insert path-only event");

    assert_eq!(store.repair_project_identities().expect("repair"), 0);
    assert_eq!(stored_project(&store), path_only_project(&deleted));
}

#[test]
fn restore_reads_the_legacy_tasks_table_when_present() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    store
        .conn
        .execute_batch("CREATE TABLE tasks (task_id TEXT PRIMARY KEY, payload TEXT NOT NULL);")
        .expect("legacy tasks table");
    let payload = serde_json::json!({ "project": repo_project(&deleted) }).to_string();
    store
        .conn
        .execute(
            "INSERT INTO tasks (task_id, payload) VALUES ('task-1', ?1)",
            params![payload],
        )
        .expect("legacy task");

    let mut events = vec![event_at(&source, "rebuilt", path_only_project(&deleted))];
    store
        .restore_project_identities(&mut events, &LocationIdentities::default())
        .expect("restore");

    assert_eq!(events[0].project, Some(repo_project(&deleted)));
}
