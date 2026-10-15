use super::support::*;
use super::*;
use statsai_core::{
    archive_conversation_id, path_hash, project_bucket_key, ArchiveCompleteness,
    ArchiveConversation, ARCHIVE_CONVERSATION_SCHEMA_VERSION,
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

fn project_with_repo(folder: &Path, repo: &str) -> ProjectInfo {
    let path_hash = path_hash(folder);
    ProjectInfo {
        project_id: format!("project_{repo}_{}", &path_hash[..8]),
        repo_remote_hash: Some(repo.to_string()),
        repo_label: Some(format!("example-org/{repo}")),
        ..repo_project(folder)
    }
}

fn event_started(
    source: &statsai_core::SourceLocation,
    record_id: &str,
    project: ProjectInfo,
    hour: u32,
) -> UsageEvent {
    let started_at = Utc
        .with_ymd_and_hms(2026, 7, 1, hour, 0, 0)
        .single()
        .expect("started_at");
    let mut event = test_store_event(source, started_at, record_id);
    event.project = Some(project);
    event
}

fn task_span(
    source: &statsai_core::SourceLocation,
    span_id: &str,
    project: ProjectInfo,
    linked: &[&UsageEvent],
) -> statsai_core::TaskSpan {
    statsai_core::TaskSpan {
        schema_version: statsai_core::TASK_SPAN_SCHEMA_VERSION.to_string(),
        span_id: statsai_core::TaskSpanId(span_id.to_string()),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        span_kind: "codex_task".to_string(),
        source_record_id: None,
        source_file_path_hash: None,
        summary_id: None,
        session_id: Some("session".to_string()),
        thread_id: None,
        title: "Fix the importer".to_string(),
        normalized_title: "fix the importer".to_string(),
        title_source: None,
        summary_preview: None,
        todo_excerpt: None,
        issue_keys: Vec::new(),
        branch_family: None,
        project_bucket: project_bucket_key(Some(&project)),
        project: Some(project),
        git: None,
        usage: UsageCounts::default(),
        estimated_cost_usd: None,
        estimated_cost_micro_usd: None,
        event_count: 0,
        has_usage_evidence: false,
        total_messages: 0,
        user_messages: 0,
        assistant_messages: 0,
        developer_messages: 0,
        linked_event_ids: linked.iter().map(|event| event.event_id.clone()).collect(),
        confidence: Confidence::Medium,
        is_meta: false,
        started_at: Utc
            .with_ymd_and_hms(2026, 7, 1, 12, 0, 0)
            .single()
            .expect("started_at"),
        ended_at: None,
        duration_seconds: None,
    }
}

#[test]
fn a_replaced_row_that_held_no_repository_stays_path_only() {
    // Sessions before `git init` were path-only when they were written; a
    // later checkout's repository must not be pulled back over them.
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");

    let mut replaced = LocationIdentities::default();
    replaced.observe(&event_started(
        &source,
        "pre-git",
        path_only_project(&deleted),
        9,
    ));
    replaced.observe(&event_started(
        &source,
        "with-git",
        repo_project(&deleted),
        10,
    ));
    let mut events = vec![
        event_started(&source, "pre-git", path_only_project(&deleted), 9),
        event_started(&source, "with-git", path_only_project(&deleted), 10),
        event_started(&source, "unseen", path_only_project(&deleted), 11),
    ];

    let restored = store
        .restore_project_identities(&mut events, &replaced)
        .expect("restore");

    assert_eq!(restored, 2);
    assert_eq!(events[0].project, Some(path_only_project(&deleted)));
    assert_eq!(events[1].project, Some(repo_project(&deleted)));
    assert_eq!(events[2].project, Some(repo_project(&deleted)));
}

#[test]
fn a_replaced_row_keeps_its_repository_over_an_enclosing_one() {
    // A deleted clone nested in another checkout resolves to the outer
    // repository on a re-parse; the record keeps the one it had.
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("platform").join("billing");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");

    let mut replaced = LocationIdentities::default();
    replaced.observe(&event_at(
        &source,
        "record",
        project_with_repo(&deleted, "billing"),
    ));
    let mut events = vec![event_at(
        &source,
        "record",
        project_with_repo(&deleted, "platform"),
    )];

    store
        .restore_project_identities(&mut events, &replaced)
        .expect("restore");

    assert_eq!(
        events[0].project,
        Some(project_with_repo(&deleted, "billing"))
    );
}

#[test]
fn replaced_rows_come_before_the_store_and_the_newest_identity_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    store
        .insert_event(&event_started(
            &source,
            "old",
            project_with_repo(&deleted, "old"),
            8,
        ))
        .expect("older repository");
    store
        .insert_event(&event_started(
            &source,
            "new",
            project_with_repo(&deleted, "renamed"),
            9,
        ))
        .expect("newer repository");

    let restore = |replaced: &LocationIdentities| {
        let mut events = vec![event_started(
            &source,
            "fresh",
            path_only_project(&deleted),
            12,
        )];
        store
            .restore_project_identities(&mut events, replaced)
            .expect("restore");
        events.remove(0).project.expect("project")
    };

    assert_eq!(
        restore(&LocationIdentities::default()),
        project_with_repo(&deleted, "renamed")
    );
    let mut replaced = LocationIdentities::default();
    replaced.observe(&event_started(
        &source,
        "gone",
        project_with_repo(&deleted, "replaced"),
        7,
    ));
    assert_eq!(restore(&replaced), project_with_repo(&deleted, "replaced"));
}

#[test]
fn usage_comes_before_archives_and_archives_before_task_spans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    let restore = || {
        let mut events = vec![event_at(&source, "fresh", path_only_project(&deleted))];
        store
            .restore_project_identities(&mut events, &LocationIdentities::default())
            .expect("restore");
        events.remove(0).project.expect("project")
    };

    store
        .upsert_task_spans(&[task_span(
            &source,
            "span-identity",
            project_with_repo(&deleted, "from-span"),
            &[],
        )])
        .expect("task span");
    assert_eq!(restore(), project_with_repo(&deleted, "from-span"));

    store
        .upsert_archive_conversations(&[archive_conversation(
            &source,
            project_with_repo(&deleted, "from-archive"),
        )])
        .expect("archive");
    assert_eq!(restore(), project_with_repo(&deleted, "from-archive"));

    store
        .insert_event(&event_at(
            &source,
            "usage",
            project_with_repo(&deleted, "from-usage"),
        ))
        .expect("usage");
    assert_eq!(restore(), project_with_repo(&deleted, "from-usage"));
}

#[test]
fn the_daemon_rescan_keeps_a_deleted_checkouts_repository() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    let evidence = |record_id: &str| ParseEvidence {
        event_key_version: "test.v1".to_string(),
        source_file_path_hash: Some("file-hash".to_string()),
        source_line_number: Some(1),
        source_record_id: Some(record_id.to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: statsai_core::IdentitySource::Unresolved,
    };
    let mut stored = event_at(&source, "record", repo_project(&deleted));
    stored.parse_evidence = Some(evidence("record"));
    store.insert_event(&stored).expect("insert");
    let mut reparsed = event_at(&source, "record", path_only_project(&deleted));
    reparsed.parse_evidence = Some(evidence("record"));

    store
        .replace_scan_file_records(ScanFileReplacement {
            source_id: &source.source_id,
            reconciled_file_hashes: &["file-hash".to_string()],
            events: std::slice::from_ref(&reparsed),
            summaries: &[],
            session_names: &[],
            activity_invocations: &[],
            activity_coverage: &[],
            activity_persist_mode: ActivityPersistMode::ReplaceFiles,
            activity_scan_cursor: None,
            device_id: "device",
            pending_entries: &[],
            compatible_entries_to_upgrade: &[],
            removed_cache_keys: &[],
        })
        .expect("replace scan files");

    assert_eq!(stored_project(&store), repo_project(&deleted));
    assert!(store
        .metadata_value("project_identity_repair.v1")
        .expect("marker")
        .is_some());
}

#[test]
fn the_repair_runs_only_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    store
        .upsert_archive_conversations(&[archive_conversation(&source, repo_project(&deleted))])
        .expect("archive");
    assert_eq!(store.repair_project_identities().expect("first repair"), 0);

    store
        .insert_event(&event_at(&source, "later", path_only_project(&deleted)))
        .expect("insert path-only event");
    assert_eq!(store.repair_project_identities().expect("second repair"), 0);
    assert_eq!(stored_project(&store), path_only_project(&deleted));

    store
        .conn
        .execute(
            "DELETE FROM local_metadata WHERE key = 'project_identity_repair.v1'",
            [],
        )
        .expect("clear marker");
    assert_eq!(store.repair_project_identities().expect("third repair"), 1);
    assert_eq!(
        stored_project(&store).project_id,
        repo_project(&deleted).project_id
    );
}

#[test]
fn the_repair_moves_task_spans_and_summaries_with_their_usage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    store
        .upsert_archive_conversations(&[archive_conversation(&source, repo_project(&deleted))])
        .expect("archive");
    let mut summary = test_store_summary(&source, Utc::now(), 10);
    summary.project = Some(path_only_project(&deleted));
    store.upsert_summaries(&[summary]).expect("summary");
    store
        .upsert_task_spans(&[task_span(&source, "span", path_only_project(&deleted), &[])])
        .expect("task span");

    assert_eq!(store.repair_project_identities().expect("repair"), 2);

    let spans = store.task_spans().expect("task spans");
    assert_eq!(spans[0].project, Some(repo_project(&deleted)));
    assert_eq!(
        spans[0].project_bucket,
        project_bucket_key(Some(&repo_project(&deleted)))
    );
    let work_items = store.work_items().expect("work items");
    assert_eq!(work_items.len(), 1);
    assert_eq!(
        work_items[0].project_bucket,
        project_bucket_key(Some(&repo_project(&deleted)))
    );
    let summaries = store.summaries().expect("summaries");
    assert_eq!(summaries[0].project, Some(repo_project(&deleted)));
}

#[test]
fn task_spans_and_summaries_follow_their_restored_usage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deleted = dir.path().join("deleted-checkout");
    let store = Store::in_memory().expect("store");
    let source = source();
    store.upsert_source(&source).expect("source");
    let mut replaced = LocationIdentities::default();
    replaced.observe(&event_at(&source, "record", repo_project(&deleted)));
    let mut events = vec![event_at(&source, "record", path_only_project(&deleted))];
    store
        .restore_project_identities(&mut events, &replaced)
        .expect("restore events");

    let mut spans = vec![
        task_span(
            &source,
            "linked",
            path_only_project(&deleted),
            &[&events[0]],
        ),
        task_span(&source, "unlinked", path_only_project(&deleted), &[]),
    ];
    let restored = store
        .restore_task_span_project_identities(&mut spans, &events, &LocationIdentities::default())
        .expect("restore spans");
    assert_eq!(restored, 2);
    for span in &spans {
        assert_eq!(span.project, Some(repo_project(&deleted)));
        assert_eq!(
            span.project_bucket,
            project_bucket_key(Some(&repo_project(&deleted)))
        );
    }

    let mut summary = test_store_summary(&source, Utc::now(), 10);
    summary.project = Some(path_only_project(&deleted));
    let mut summaries = vec![summary];
    store
        .restore_summary_project_identities(&mut summaries, &events, &LocationIdentities::default())
        .expect("restore summaries");
    assert_eq!(summaries[0].project, Some(repo_project(&deleted)));
}
