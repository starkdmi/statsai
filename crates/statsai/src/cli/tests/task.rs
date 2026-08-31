use super::support::*;
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
fn task_benchmark_reports_current_and_baselines() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-benchmark"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-benchmark/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 14, 10, 0, 0)
        .single()
        .expect("started_at");
    let event_a = test_scan_event(&source, file_path, started_at, "bench-a", 100);
    let event_b = test_scan_event(
        &source,
        file_path,
        started_at + Duration::minutes(2),
        "bench-b",
        120,
    );
    let event_c = test_scan_event(
        &source,
        file_path,
        started_at + Duration::hours(30),
        "bench-c",
        20,
    );
    let span_a = test_task_span(
        &source,
        file_path,
        started_at,
        "bench-span-a",
        "Implement benchmark reporting",
        &event_a,
    );
    let span_b = test_task_span(
        &source,
        file_path,
        started_at + Duration::minutes(2),
        "bench-span-b",
        "Implement benchmark reporting",
        &event_b,
    );
    let span_c = test_task_span(
        &source,
        file_path,
        started_at + Duration::hours(30),
        "bench-span-c",
        "review uncommitted changes",
        &event_c,
    );
    store
        .insert_events(&[event_a, event_b, event_c])
        .expect("insert events");
    store
        .upsert_task_spans(&[span_a.clone(), span_b.clone(), span_c.clone()])
        .expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let work_items = store.work_items().expect("work items");
    let implementation_item = work_items
        .iter()
        .find(|item| item.title == "Implement benchmark reporting")
        .expect("implementation item");
    let review_item = work_items
        .iter()
        .find(|item| item.anchor_span_id == span_c.span_id)
        .expect("review item");

    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Accept {
                    work_item_id: implementation_item.work_item_id.0.clone(),
                },
            },
        },
        &store,
    )
    .expect("accept verify");
    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Reject {
                    work_item_id: review_item.work_item_id.0.clone(),
                    reason: "noise".to_string(),
                },
            },
        },
        &store,
    )
    .expect("reject verify");

    let report = store.task_benchmark_report().expect("benchmark report");
    assert!(report.verified_spans >= 3);
    assert!(report.verified_adjacent_pairs >= 1);
    assert!(report.has_verified_ground_truth);
    assert!(report.has_verified_pairwise_ground_truth);
    assert_eq!(report.baselines.len(), 6);
    assert!(report.manual_constraints_preserved);
    assert_eq!(
        report.failing_baselines.is_empty(),
        report.beats_all_baselines
    );
    assert_eq!(report.shipping_gate_ready, report.gate_blockers.is_empty());
}

#[test]
fn task_benchmark_scores_raw_grouper_not_manual_split_output() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-benchmark-raw-grouper"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-benchmark-raw-grouper/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 14, 11, 0, 0)
        .single()
        .expect("started_at");
    let event_a = test_scan_event(&source, file_path, started_at, "raw-a", 100);
    let event_b = test_scan_event(
        &source,
        file_path,
        started_at + Duration::minutes(2),
        "raw-b",
        120,
    );
    let event_c = test_scan_event(
        &source,
        file_path,
        started_at + Duration::minutes(4),
        "raw-c",
        140,
    );
    let span_a = test_task_span(
        &source,
        file_path,
        started_at,
        "raw-span-a",
        "Implement benchmark reporting",
        &event_a,
    );
    let span_b = test_task_span(
        &source,
        file_path,
        started_at + Duration::minutes(2),
        "raw-span-b",
        "Implement benchmark reporting",
        &event_b,
    );
    let span_c = test_task_span(
        &source,
        file_path,
        started_at + Duration::minutes(4),
        "raw-span-c",
        "Implement benchmark reporting",
        &event_c,
    );
    store
        .insert_events(&[event_a, event_b, event_c])
        .expect("insert events");
    store
        .upsert_task_spans(&[span_a.clone(), span_b.clone(), span_c.clone()])
        .expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let initial = store.work_items().expect("initial work items");
    assert_eq!(initial.len(), 1);
    let work_item = initial.first().expect("work item");

    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Split {
                    work_item_id: work_item.work_item_id.0.clone(),
                    after_span: span_a.span_id.0.clone(),
                    left_title: Some("Investigate benchmark regression".to_string()),
                    right_title: Some("Implement benchmark reporting".to_string()),
                },
            },
        },
        &store,
    )
    .expect("split verify");

    let split_items = store.work_items().expect("split items");
    assert_eq!(split_items.len(), 2);

    let report = store.task_benchmark_report().expect("benchmark report");
    assert!(report.has_verified_ground_truth);
    assert!(report.has_verified_pairwise_ground_truth);
    assert!(report.manual_constraints_preserved);
    assert!(report.current.adjacent_f1 < 1.0);
}

#[test]
fn task_benchmark_reports_missing_ground_truth_explicitly() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-benchmark-empty"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-benchmark-empty/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 14, 12, 0, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "bench-empty", 75);
    let span = test_task_span(
        &source,
        file_path,
        started_at,
        "bench-empty-span",
        "Investigate benchmark readiness",
        &event,
    );
    store.insert_events(&[event]).expect("insert events");
    store.upsert_task_spans(&[span]).expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let report = store.task_benchmark_report().expect("benchmark report");
    assert_eq!(report.verified_spans, 0);
    assert_eq!(report.verified_adjacent_pairs, 0);
    assert!(!report.has_verified_ground_truth);
    assert!(!report.has_verified_pairwise_ground_truth);
    assert!(report.manual_constraints_preserved);
    assert!(!report.beats_all_baselines);
    assert!(!report.shipping_gate_ready);
    assert!(report.failing_baselines.is_empty());
    assert_eq!(
        report.gate_blockers,
        vec!["missing_verified_ground_truth".to_string()]
    );

    let json = benchmark_json_value(&report);
    assert_eq!(json["has_verified_ground_truth"], json!(false));
    assert_eq!(json["has_verified_pairwise_ground_truth"], json!(false));
    assert_eq!(json["shipping_gate_ready"], json!(false));
    assert_eq!(json["verified_spans"], json!(0));
    assert_eq!(json["failing_baselines"], json!([]));
    assert_eq!(
        json["gate_blockers"],
        json!(["missing_verified_ground_truth"])
    );
}

#[test]
fn task_benchmark_reports_label_only_ground_truth_explicitly() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-benchmark-label-only"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_a = "/tmp/codex-task-benchmark-label-only/a.jsonl";
    let file_b = "/tmp/codex-task-benchmark-label-only/b.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 14, 15, 0, 0)
        .single()
        .expect("started_at");
    let event_a = test_scan_event(&source, file_a, started_at, "label-only-a", 80);
    let event_b = test_scan_event(
        &source,
        file_b,
        started_at + Duration::minutes(30),
        "label-only-b",
        90,
    );
    let span_a = test_task_span(
        &source,
        file_a,
        started_at,
        "label-only-span-a",
        "Implement label-only benchmark reporting",
        &event_a,
    );
    let span_b = test_task_span(
        &source,
        file_b,
        started_at + Duration::minutes(30),
        "label-only-span-b",
        "Clearing Conversation History",
        &event_b,
    );
    let mut span_a = span_a;
    let mut span_b = span_b;
    span_a.project = Some(ProjectInfo {
        project_id: "label-only-a".to_string(),
        project_label: Some("label-only-a".to_string()),
        repo_remote_hash: Some("repo-label-a".to_string()),
        repo_label: Some("owner/label-a".to_string()),
        branch_hash: Some("branch-label-a".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-label-a".to_string()),
        path_label: Some("/tmp/label-only-a".to_string()),
    });
    span_a.project_bucket = project_bucket_key(span_a.project.as_ref());
    span_a.branch_family = branch_family(Some("main"));
    span_b.project = Some(ProjectInfo {
        project_id: "label-only-b".to_string(),
        project_label: Some("label-only-b".to_string()),
        repo_remote_hash: Some("repo-label-b".to_string()),
        repo_label: Some("owner/label-b".to_string()),
        branch_hash: Some("branch-label-b".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-label-b".to_string()),
        path_label: Some("/tmp/label-only-b".to_string()),
    });
    span_b.project_bucket = project_bucket_key(span_b.project.as_ref());
    span_b.branch_family = branch_family(Some("main"));
    let span_a_id = span_a.span_id.clone();
    let span_b_id = span_b.span_id.clone();
    store
        .insert_events(&[event_a, event_b])
        .expect("insert events");
    store
        .upsert_task_spans(&[span_a, span_b])
        .expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let work_items = store.work_items().expect("work items");
    let accepted_item = work_items
        .iter()
        .find(|item| item.anchor_span_id == span_a_id)
        .expect("accepted item");
    let rejected_item = work_items
        .iter()
        .find(|item| item.anchor_span_id == span_b_id)
        .expect("rejected item");

    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Accept {
                    work_item_id: accepted_item.work_item_id.0.clone(),
                },
            },
        },
        &store,
    )
    .expect("accept verify");
    task(
        TaskCommand {
            command: TaskSubcommand::Verify {
                command: TaskVerifySubcommand::Reject {
                    work_item_id: rejected_item.work_item_id.0.clone(),
                    reason: "meta".to_string(),
                },
            },
        },
        &store,
    )
    .expect("reject verify");

    let report = store.task_benchmark_report().expect("benchmark report");
    assert_eq!(report.verified_spans, 2);
    assert_eq!(report.verified_adjacent_pairs, 0);
    assert!(report.has_verified_ground_truth);
    assert!(!report.has_verified_pairwise_ground_truth);
    assert!(report.manual_constraints_preserved);
    assert!(!report.beats_all_baselines);
    assert!(!report.shipping_gate_ready);
    assert_eq!(report.failing_baselines, Vec::<String>::new());
    assert_eq!(
        report.gate_blockers,
        vec!["missing_pairwise_ground_truth".to_string()]
    );

    let json = benchmark_json_value(&report);
    assert_eq!(json["has_verified_ground_truth"], json!(true));
    assert_eq!(json["has_verified_pairwise_ground_truth"], json!(false));
    assert_eq!(json["verified_spans"], json!(2));
    assert_eq!(json["verified_adjacent_pairs"], json!(0));
    assert_eq!(
        json["gate_blockers"],
        json!(["missing_pairwise_ground_truth"])
    );
}

#[test]
fn task_benchmark_reports_failing_baselines_when_current_ties_them() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-benchmark-baseline-tie"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-benchmark-baseline-tie/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 14, 16, 0, 0)
        .single()
        .expect("started_at");
    let event_a = test_scan_event(&source, file_path, started_at, "tie-a", 80);
    let event_b = test_scan_event(
        &source,
        file_path,
        started_at + Duration::minutes(2),
        "tie-b",
        90,
    );
    let span_a = test_task_span(
        &source,
        file_path,
        started_at,
        "tie-span-a",
        "Implement benchmark blocking report",
        &event_a,
    );
    let span_b = test_task_span(
        &source,
        file_path,
        started_at + Duration::minutes(2),
        "tie-span-b",
        "Implement benchmark blocking report",
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

    let work_item = store
        .work_items()
        .expect("work items")
        .into_iter()
        .next()
        .expect("work item");
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

    let report = store.task_benchmark_report().expect("benchmark report");
    assert!(report.has_verified_ground_truth);
    assert!(report.has_verified_pairwise_ground_truth);
    assert!(report.manual_constraints_preserved);
    assert!(!report.beats_all_baselines);
    assert!(!report.shipping_gate_ready);
    assert_eq!(
        report.failing_baselines,
        vec![
            "gap_only_2h".to_string(),
            "gap_only_6h".to_string(),
            "gap_only_12h".to_string(),
            "gap_only_24h".to_string(),
            "repo_plus_title".to_string(),
            "repo_plus_branch_plus_title".to_string(),
        ]
    );
    assert_eq!(
        report.gate_blockers,
        vec!["baseline_regressions".to_string()]
    );
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
