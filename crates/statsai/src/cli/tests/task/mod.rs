pub(super) use super::support::*;
pub(crate) use super::*;

mod benchmark;
mod verify;

#[test]
fn task_show_include_evidence_includes_spans_and_rename_verification() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-show"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-show/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 13, 11, 0, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "show-event", 90);
    let span = test_task_span(
        &source,
        file_path,
        started_at,
        "show-span",
        "Investigate work item evidence",
        &event,
    );
    store.insert_events(&[event]).expect("insert events");
    store.upsert_task_spans(&[span]).expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let initial = store.work_items().expect("initial work items");
    let initial_item = initial.first().expect("initial work item");
    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Rename {
                    work_item_id: initial_item.work_item_id.0.clone(),
                    title: "Verified evidence task".to_string(),
                },
            },
        },
        &store,
    )
    .expect("rename verify");

    let renamed = store.work_items().expect("renamed work items");
    assert_eq!(renamed.len(), 1);
    assert_eq!(renamed[0].title, "Verified evidence task");
    assert_eq!(renamed[0].status, TaskStatus::Verified);

    let output =
        load_task_show_output(&store, &renamed[0].work_item_id, true).expect("task show output");
    assert_eq!(output.work_item.title, "Verified evidence task");
    assert_eq!(output.spans.len(), 1);
    assert_eq!(output.verifications.len(), 1);
    assert!(matches!(
        output.verifications[0].action,
        TaskVerificationAction::Rename { .. }
    ));
}

#[test]
fn task_show_without_evidence_omits_spans_and_verifications() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-show-no-evidence"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-show-no-evidence/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 13, 11, 30, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "show-no-evidence", 90);
    let span = test_task_span(
        &source,
        file_path,
        started_at,
        "show-no-evidence-span",
        "Inspect task show output",
        &event,
    );
    store.insert_events(&[event]).expect("insert events");
    store.upsert_task_spans(&[span]).expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let work_item = store
        .work_items()
        .expect("work items")
        .into_iter()
        .next()
        .expect("work item");
    let output =
        load_task_show_output(&store, &work_item.work_item_id, false).expect("task show output");
    assert_eq!(output.work_item.work_item_id, work_item.work_item_id);
    assert!(output.spans.is_empty());
    assert!(output.verifications.is_empty());
}

#[test]
fn task_list_filters_by_provider_and_status() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-list-filters"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-list-filters/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 14, 14, 0, 0)
        .single()
        .expect("started_at");
    let event_auto = test_scan_event(&source, file_path, started_at, "event-auto", 50);
    let event_review = test_scan_event(
        &source,
        file_path,
        started_at + Duration::minutes(10),
        "event-review",
        60,
    );
    let event_reject = test_scan_event(
        &source,
        file_path,
        started_at + Duration::minutes(20),
        "event-reject",
        70,
    );
    let mut span_auto = test_task_span(
        &source,
        file_path,
        started_at,
        "span-auto",
        "Implement task list filters",
        &event_auto,
    );
    let mut span_review = test_task_span(
        &source,
        file_path,
        started_at + Duration::minutes(10),
        "span-review",
        "Review task list filtering behavior",
        &event_review,
    );
    let mut span_reject = test_task_span(
        &source,
        file_path,
        started_at + Duration::minutes(20),
        "span-reject",
        "Noise task entry",
        &event_reject,
    );
    span_auto.project = Some(ProjectInfo {
        project_id: "project-auto".to_string(),
        project_label: Some("auto".to_string()),
        repo_remote_hash: Some("repo-auto".to_string()),
        repo_label: Some("owner/auto".to_string()),
        branch_hash: Some("branch-auto".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-auto".to_string()),
        path_label: Some("/tmp/project-auto".to_string()),
    });
    span_auto.project_bucket = project_bucket_key(span_auto.project.as_ref());
    span_auto.branch_family = branch_family(Some("main"));

    span_review.provider = "opencode".to_string();
    span_review.project = Some(ProjectInfo {
        project_id: "project-review".to_string(),
        project_label: Some("review".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-review".to_string()),
        path_label: Some("/tmp/project-review".to_string()),
    });
    span_review.project_bucket = project_bucket_key(span_review.project.as_ref());

    span_reject.project = Some(ProjectInfo {
        project_id: "project-reject".to_string(),
        project_label: Some("reject".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-reject".to_string()),
        path_label: Some("/tmp/project-reject".to_string()),
    });
    span_reject.project_bucket = project_bucket_key(span_reject.project.as_ref());

    store
        .insert_events(&[event_auto, event_review, event_reject])
        .expect("insert events");
    store
        .upsert_task_spans(&[span_auto.clone(), span_review.clone(), span_reject.clone()])
        .expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let initial = store.work_items().expect("work items");
    let reject_item = initial
        .iter()
        .find(|item| item.anchor_span_id == span_reject.span_id)
        .expect("reject item");
    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Reject {
                    work_item_id: reject_item.work_item_id.0.clone(),
                    reason: "noise".to_string(),
                },
            },
        },
        &store,
    )
    .expect("reject verify");

    let codex_items =
        filtered_task_list_items(&store, Some("codex"), None).expect("codex filtered items");
    assert_eq!(codex_items.len(), 1);
    assert!(codex_items
        .iter()
        .all(|item| item.providers.iter().any(|provider| provider == "codex")));
    assert!(codex_items
        .iter()
        .all(|item| item.status != TaskStatus::RejectedMeta));

    let auto_items = filtered_task_list_items(&store, None, Some(&TaskStatus::Auto))
        .expect("auto filtered items");
    assert_eq!(auto_items.len(), 1);
    assert_eq!(auto_items[0].anchor_span_id, span_auto.span_id);

    let rejected_items = filtered_task_list_items(&store, None, Some(&TaskStatus::RejectedMeta))
        .expect("rejected filtered items");
    assert_eq!(rejected_items.len(), 1);
    assert_eq!(rejected_items[0].anchor_span_id, span_reject.span_id);

    let default_selection = task_list_selection(&store, None, None).expect("default selection");
    assert_eq!(default_selection.items.len(), 2);
    assert_eq!(default_selection.hidden_rejected_meta, 1);
}

#[test]
fn selected_rebuild_project_buckets_filter_by_provider_and_source() {
    let store = Store::in_memory().expect("store");
    let source_codex = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-rebuild-filter-a"),
        LocationOrigin::Configured,
    );
    let source_open = SourceLocation::local_adapter(
        "opencode",
        "test",
        "0",
        Path::new("/tmp/codex-task-rebuild-filter-b"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source_codex).expect("codex source");
    store.upsert_source(&source_open).expect("opencode source");

    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 15, 9, 0, 0)
        .single()
        .expect("started_at");
    let event_codex = test_scan_event(
        &source_codex,
        "/tmp/codex-task-rebuild-filter-a/session.jsonl",
        started_at,
        "event-codex",
        50,
    );
    let event_open = test_scan_event(
        &source_open,
        "/tmp/codex-task-rebuild-filter-b/session.jsonl",
        started_at + Duration::minutes(5),
        "event-open",
        60,
    );
    let span_codex = test_task_span(
        &source_codex,
        "/tmp/codex-task-rebuild-filter-a/session.jsonl",
        started_at,
        "span-codex",
        "Codex rebuild target",
        &event_codex,
    );
    let mut span_open = test_task_span(
        &source_open,
        "/tmp/codex-task-rebuild-filter-b/session.jsonl",
        started_at + Duration::minutes(5),
        "span-open",
        "OpenCode rebuild target",
        &event_open,
    );
    span_open.provider = "opencode".to_string();

    store
        .insert_events(&[event_codex, event_open])
        .expect("insert events");
    store
        .upsert_task_spans(&[span_codex.clone(), span_open.clone()])
        .expect("insert spans");

    let codex_buckets =
        selected_rebuild_project_buckets(&store, Some("codex"), None).expect("codex buckets");
    assert_eq!(
        codex_buckets,
        BTreeSet::from([span_codex.project_bucket.clone()])
    );

    let open_buckets =
        selected_rebuild_project_buckets(&store, Some("opencode"), None).expect("opencode buckets");
    assert_eq!(
        open_buckets,
        BTreeSet::from([span_open.project_bucket.clone()])
    );

    let source_buckets =
        selected_rebuild_project_buckets(&store, None, Some(&source_codex.source_id.0))
            .expect("source buckets");
    assert_eq!(
        source_buckets,
        BTreeSet::from([span_codex.project_bucket.clone()])
    );
}

#[test]
fn task_rebuild_is_idempotent() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-rebuild"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-rebuild/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 14, 14, 0, 0)
        .single()
        .expect("started_at");
    let event_a = test_scan_event(&source, file_path, started_at, "rebuild-a", 75);
    let event_b = test_scan_event(
        &source,
        file_path,
        started_at + Duration::minutes(3),
        "rebuild-b",
        60,
    );
    let span_a = test_task_span(
        &source,
        file_path,
        started_at,
        "rebuild-span-a",
        "Rebuild task work items",
        &event_a,
    );
    let span_b = test_task_span(
        &source,
        file_path,
        started_at + Duration::minutes(3),
        "rebuild-span-b",
        "Rebuild task work items",
        &event_b,
    );
    store
        .insert_events(&[event_a, event_b])
        .expect("insert events");
    store
        .upsert_task_spans(&[span_a, span_b])
        .expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let first = store.work_items().expect("first rebuild work items");
    task(
        TaskCommand {
            command: TaskSubcommand::Rebuild {
                provider: None,
                source_id: None,
                all: true,
            },
        },
        &store,
    )
    .expect("first task rebuild");
    let second = store.work_items().expect("second rebuild work items");
    task(
        TaskCommand {
            command: TaskSubcommand::Rebuild {
                provider: None,
                source_id: None,
                all: true,
            },
        },
        &store,
    )
    .expect("second task rebuild");
    let third = store.work_items().expect("third rebuild work items");

    assert_eq!(first, second);
    assert_eq!(second, third);
}
