use super::*;

#[test]
fn scan_preview_does_not_persist_task_tables() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-preview"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-preview/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 9, 45, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "preview-event", 80);
    let task_span = test_task_span(
        &source,
        file_path,
        started_at,
        "preview-span",
        "Preview task collection",
        &event,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![test_scan_candidate(file_path, "sig-preview")],
        scan_result: statsai_adapters::AdapterScan {
            events: vec![event],
            task_spans: vec![task_span],
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: true,
            preview: true,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("preview scan");

    assert_eq!(store.event_count().expect("event count"), 0);
    assert_eq!(store.summary_count().expect("summary count"), 0);
    assert!(store.task_spans().expect("task spans").is_empty());
    assert!(store.work_items().expect("work items").is_empty());
}

#[test]
fn preview_task_rebuild_counts_only_affected_work_items() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-preview-rebuild"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_a = "/tmp/codex-task-preview-rebuild/a.jsonl";
    let file_b = "/tmp/codex-task-preview-rebuild/b.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 10, 30, 0)
        .single()
        .expect("started_at");
    let event_a = test_scan_event(&source, file_a, started_at, "preview-a", 90);
    let event_b = test_scan_event(
        &source,
        file_b,
        started_at + Duration::minutes(10),
        "preview-b",
        110,
    );
    let mut span_a = test_task_span(
        &source,
        file_a,
        started_at,
        "preview-span-a",
        "Preview rebuild task A",
        &event_a,
    );
    let mut span_b = test_task_span(
        &source,
        file_b,
        started_at + Duration::minutes(10),
        "preview-span-b",
        "Preview rebuild task B",
        &event_b,
    );
    span_a.project = Some(ProjectInfo {
        project_id: "project-a".to_string(),
        project_label: Some("project-a".to_string()),
        repo_remote_hash: Some("repo-a".to_string()),
        repo_label: Some("owner/project-a".to_string()),
        branch_hash: Some("branch-a".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-a".to_string()),
        path_label: Some("/tmp/project-a".to_string()),
    });
    span_a.project_bucket = project_bucket_key(span_a.project.as_ref());
    span_a.branch_family = branch_family(Some("main"));
    span_b.project = Some(ProjectInfo {
        project_id: "project-b".to_string(),
        project_label: Some("project-b".to_string()),
        repo_remote_hash: Some("repo-b".to_string()),
        repo_label: Some("owner/project-b".to_string()),
        branch_hash: Some("branch-b".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-b".to_string()),
        path_label: Some("/tmp/project-b".to_string()),
    });
    span_b.project_bucket = project_bucket_key(span_b.project.as_ref());
    span_b.branch_family = branch_family(Some("main"));

    store
        .insert_events(&[event_a.clone(), event_b.clone()])
        .expect("insert events");
    store
        .upsert_task_spans(&[span_a.clone(), span_b.clone()])
        .expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let mut updated_span_a = span_a.clone();
    updated_span_a.title = "Preview rebuild task A updated".to_string();
    updated_span_a.summary_preview = Some("Preview rebuild task A updated".to_string());
    updated_span_a.normalized_title = normalize_task_title(&updated_span_a.title);

    let pending_entries = scan_file_state_entries(&[test_scan_candidate(file_a, "sig-a-2")]);
    let mut preview = PreviewTaskRebuild::default();
    let rebuilt = preview
        .apply_source_changes(
            &store,
            SourceTaskChangeSet {
                source_id: &source.source_id,
                replace_source_records: false,
                touched_files: true,
                pending_file_entries: &pending_entries,
                removed_file_entries: &[],
                task_spans: &[updated_span_a],
            },
        )
        .expect("preview work items rebuilt");
    assert_eq!(rebuilt, 1);
    assert_eq!(store.task_spans().expect("task spans").len(), 2);
    assert_eq!(store.work_items().expect("work items").len(), 2);
}

#[test]
fn preview_task_rebuild_counts_shared_bucket_rebuilds_per_source_step() {
    let store = Store::in_memory().expect("store");
    let source_a = SourceLocation::local_adapter(
        "claude_code",
        "test-a",
        "0",
        Path::new("/tmp/preview-shared-a"),
        LocationOrigin::Configured,
    );
    let source_b = SourceLocation::local_adapter(
        "claude_code",
        "test-b",
        "0",
        Path::new("/tmp/preview-shared-b"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source_a).expect("source a");
    store.upsert_source(&source_b).expect("source b");

    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 11, 0, 0)
        .single()
        .expect("started_at");
    let file_a = "/tmp/preview-shared-a/session.jsonl";
    let file_b = "/tmp/preview-shared-b/session.jsonl";
    let event_a = test_scan_event(&source_a, file_a, started_at, "shared-a", 120);
    let event_b = test_scan_event(
        &source_b,
        file_b,
        started_at + Duration::minutes(20),
        "shared-b",
        140,
    );
    let mut span_a = test_task_span(
        &source_a,
        file_a,
        started_at,
        "shared-span-a",
        "Shared bucket task",
        &event_a,
    );
    let mut span_b = test_task_span(
        &source_b,
        file_b,
        started_at + Duration::minutes(20),
        "shared-span-b",
        "Shared bucket task",
        &event_b,
    );
    let shared_project = ProjectInfo {
        project_id: "shared-project".to_string(),
        project_label: Some("shared-project".to_string()),
        repo_remote_hash: Some("shared-repo".to_string()),
        repo_label: Some("owner/shared".to_string()),
        branch_hash: Some("shared-branch".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("shared-path".to_string()),
        path_label: Some("/tmp/shared-project".to_string()),
    };
    span_a.project = Some(shared_project.clone());
    span_b.project = Some(shared_project);
    span_a.project_bucket = project_bucket_key(span_a.project.as_ref());
    span_b.project_bucket = project_bucket_key(span_b.project.as_ref());
    span_a.branch_family = branch_family(Some("main"));
    span_b.branch_family = branch_family(Some("main"));

    store
        .insert_events(&[event_a.clone(), event_b.clone()])
        .expect("insert events");
    store
        .upsert_task_spans(&[span_a.clone(), span_b.clone()])
        .expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let mut updated_span_a = span_a.clone();
    updated_span_a.title = "Shared bucket task updated".to_string();
    updated_span_a.summary_preview = Some("Shared bucket task updated".to_string());
    updated_span_a.normalized_title = normalize_task_title(&updated_span_a.title);

    let mut updated_span_b = span_b.clone();
    updated_span_b.summary_preview = Some("Shared bucket task follow-up".to_string());

    let pending_a = scan_file_state_entries(&[test_scan_candidate(file_a, "shared-a-2")]);
    let pending_b = scan_file_state_entries(&[test_scan_candidate(file_b, "shared-b-2")]);
    let mut preview = PreviewTaskRebuild::default();
    let rebuilt_a = preview
        .apply_source_changes(
            &store,
            SourceTaskChangeSet {
                source_id: &source_a.source_id,
                replace_source_records: false,
                touched_files: true,
                pending_file_entries: &pending_a,
                removed_file_entries: &[],
                task_spans: &[updated_span_a],
            },
        )
        .expect("preview rebuild a");
    let rebuilt_b = preview
        .apply_source_changes(
            &store,
            SourceTaskChangeSet {
                source_id: &source_b.source_id,
                replace_source_records: false,
                touched_files: true,
                pending_file_entries: &pending_b,
                removed_file_entries: &[],
                task_spans: &[updated_span_b],
            },
        )
        .expect("preview rebuild b");

    assert_eq!(rebuilt_a, 1);
    assert_eq!(rebuilt_b, 1);
    assert_eq!(rebuilt_a + rebuilt_b, 2);
    assert_eq!(store.work_items().expect("work items").len(), 1);
}
