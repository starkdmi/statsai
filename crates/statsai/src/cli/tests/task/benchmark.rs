use super::*;

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
