use super::*;

#[test]
fn task_verify_split_merge_and_reject_survive_rebuilds() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-verify"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-verify/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 13, 9, 0, 0)
        .single()
        .expect("started_at");
    let event_a = test_scan_event(&source, file_path, started_at, "event-a", 100);
    let event_b = test_scan_event(
        &source,
        file_path,
        started_at + Duration::minutes(5),
        "event-b",
        120,
    );
    let span_a = test_task_span(
        &source,
        file_path,
        started_at,
        "span-a",
        "Implement task verification",
        &event_a,
    );
    let span_b = test_task_span(
        &source,
        file_path,
        started_at + Duration::minutes(5),
        "span-b",
        "Implement task verification",
        &event_b,
    );
    store
        .insert_events(&[event_a.clone(), event_b.clone()])
        .expect("insert events");
    store
        .upsert_task_spans(&[span_a.clone(), span_b.clone()])
        .expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let initial = store.work_items().expect("initial work items");
    assert_eq!(initial.len(), 1);

    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Split {
                    work_item_id: initial[0].work_item_id.0.clone(),
                    after_span: span_a.span_id.0.clone(),
                    left_title: Some("Left investigation".to_string()),
                    right_title: Some("Right implementation".to_string()),
                },
            },
        },
        &store,
    )
    .expect("split verify");

    let split_items = store.work_items().expect("split work items");
    assert_eq!(split_items.len(), 2);
    assert!(split_items
        .iter()
        .all(|item| item.status == TaskStatus::Verified));
    assert!(split_items
        .iter()
        .any(|item| item.title == "Left investigation"));
    assert!(split_items
        .iter()
        .any(|item| item.title == "Right implementation"));

    let left = split_items
        .iter()
        .find(|item| item.title == "Left investigation")
        .expect("left work item");
    let right = split_items
        .iter()
        .find(|item| item.title == "Right implementation")
        .expect("right work item");

    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Merge {
                    left_work_item_id: left.work_item_id.0.clone(),
                    right_work_item_id: right.work_item_id.0.clone(),
                    title: Some("Unified verification work".to_string()),
                },
            },
        },
        &store,
    )
    .expect("merge verify");

    let merged = store.work_items().expect("merged work items");
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].title, "Unified verification work");
    assert_eq!(merged[0].status, TaskStatus::Verified);

    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Reject {
                    work_item_id: merged[0].work_item_id.0.clone(),
                    reason: "meta".to_string(),
                },
            },
        },
        &store,
    )
    .expect("reject verify");

    let rejected = store.work_items().expect("rejected work items");
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].status, TaskStatus::RejectedMeta);
    assert_eq!(store.task_verifications().expect("verifications").len(), 3);
}

#[test]
fn rename_and_accept_coexist_for_same_anchor() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-rename-accept"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-rename-accept/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 13, 11, 30, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "rename-accept-event", 90);
    let span = test_task_span(
        &source,
        file_path,
        started_at,
        "rename-accept-span",
        "Investigate rename and accept coexistence",
        &event,
    );
    store.insert_events(&[event]).expect("insert events");
    store.upsert_task_spans(&[span]).expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let initial = store.work_items().expect("initial work items");
    let work_item = initial.first().expect("initial work item");
    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Rename {
                    work_item_id: work_item.work_item_id.0.clone(),
                    title: "Hosted-verified task title".to_string(),
                },
            },
        },
        &store,
    )
    .expect("rename verify");
    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Accept {
                    work_item_id: work_item.work_item_id.0.clone(),
                },
            },
        },
        &store,
    )
    .expect("accept verify");

    let rebuilt = store.work_items().expect("rebuilt work items");
    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0].status, TaskStatus::Verified);
    assert_eq!(rebuilt[0].title, "Hosted-verified task title");

    let verifications = store.task_verifications().expect("verifications");
    assert_eq!(verifications.len(), 2);
    assert!(verifications
        .iter()
        .any(|verification| matches!(verification.action, TaskVerificationAction::Rename { .. })));
    assert!(verifications
        .iter()
        .any(|verification| matches!(verification.action, TaskVerificationAction::Accept { .. })));
}

#[test]
fn accept_after_reject_supersedes_manual_reject_for_same_anchor() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-verify-supersede"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-verify-supersede/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 13, 12, 0, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "supersede-event", 95);
    let span = test_task_span(
        &source,
        file_path,
        started_at,
        "supersede-span",
        "Supersede conflicting verification actions",
        &event,
    );
    store.insert_events(&[event]).expect("insert events");
    store.upsert_task_spans(&[span]).expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let initial = store.work_items().expect("initial work items");
    let work_item = initial.first().expect("initial work item");
    let anchor_action_key = format!("status:{}", work_item.anchor_span_id.0);
    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Reject {
                    work_item_id: work_item.work_item_id.0.clone(),
                    reason: "meta".to_string(),
                },
            },
        },
        &store,
    )
    .expect("reject verify");
    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Accept {
                    work_item_id: work_item.work_item_id.0.clone(),
                },
            },
        },
        &store,
    )
    .expect("accept verify");

    let rebuilt = store.work_items().expect("rebuilt work items");
    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0].status, TaskStatus::Verified);
    assert!(!rebuilt[0]
        .review_reasons
        .iter()
        .any(|reason| reason.starts_with("manual_reject:")));

    let verifications = store.task_verifications().expect("verifications");
    assert_eq!(verifications.len(), 1);
    assert!(matches!(
        verifications[0].action,
        TaskVerificationAction::Accept { .. }
    ));
    assert_eq!(verifications[0].action_key, anchor_action_key);

    let output =
        load_task_show_output(&store, &rebuilt[0].work_item_id, true).expect("task show output");
    assert_eq!(output.verifications.len(), 1);
    assert!(matches!(
        output.verifications[0].action,
        TaskVerificationAction::Accept { .. }
    ));
}
