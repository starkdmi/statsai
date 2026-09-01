use super::support::*;
use super::*;

#[test]
fn same_session_investigation_spans_stay_one_work_item() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Investigate rollout failure in task collection",
            Some("Investigate rollout failure in task collection"),
        ),
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(12),
            "Debug rollout failure in local task collection",
            Some("Debug rollout failure in local task collection"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 2);
    assert_eq!(work_items[0].span_count, 2);
}

#[test]
fn two_span_same_session_topic_shift_splits_without_distribution_stats() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Investigate SQLite migration failure in local task store",
            Some("Analyze sqlite migration failure and schema upgrade rollback behavior"),
        ),
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(18),
            "Design benchmark export dashboard for task review",
            Some("Plan benchmark export dashboard metrics and review workflow"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 2);
    assert_eq!(members.len(), 2);
    assert_eq!(work_items[0].span_count, 1);
    assert_eq!(work_items[1].span_count, 1);
}

#[test]
fn same_session_topic_shift_splits_on_cohesion_boundary() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Investigate SQLite migration failure in local task store",
            Some("Analyze sqlite migration failure and schema upgrade rollback behavior"),
        ),
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(18),
            "Design CLI task verification commands",
            Some("Plan accept reject split merge task verification commands"),
        ),
        test_span_with_options(
            "span-c",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(31),
            "Implement CLI task verification commands",
            Some("Implement accept reject split merge task verification output"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 2);
    assert_eq!(members.len(), 3);
    assert_eq!(work_items[0].span_count, 1);
    assert_eq!(work_items[1].span_count, 2);
}

#[test]
fn shared_issue_key_overrides_same_session_topic_boundary() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut span_a = test_span_with_options(
        "span-a",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at,
        "Stabilize menubar wake handling",
        Some("Fix tray wake handling and sleep resume edge cases"),
    );
    span_a.span.issue_keys = vec!["OPS-42".to_string()];
    let mut span_b = test_span_with_options(
        "span-b",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at + chrono::Duration::minutes(18),
        "Design benchmark JSON export gate",
        Some("Plan benchmark json export schema and gate metrics"),
    );
    span_b.span.issue_keys = vec!["OPS-42".to_string()];
    let mut span_c = test_span_with_options(
        "span-c",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at + chrono::Duration::minutes(30),
        "Implement benchmark JSON export gate",
        Some("Implement benchmark json export schema and gate metrics"),
    );
    span_c.span.issue_keys = vec!["OPS-42".to_string()];

    let (work_items, members, _) = build_work_items(vec![span_a, span_b, span_c], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 3);
    assert_eq!(work_items[0].span_count, 3);
}

#[test]
fn recurring_generic_review_shells_split_without_anchor() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Review uncommitted changes",
            Some("Review uncommitted changes"),
        ),
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-b"),
            "bucket-a",
            started_at + chrono::Duration::hours(96),
            "Review uncommitted changes",
            Some("Review uncommitted changes"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 2);
    assert_eq!(members.len(), 2);
}

#[test]
fn same_title_in_different_project_buckets_never_merges() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "repo:a|path:a|branch:a",
            started_at,
            "Implement task verification workflow",
            Some("Implement task verification workflow"),
        ),
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-a"),
            "repo:b|path:b|branch:b",
            started_at + chrono::Duration::minutes(10),
            "Implement task verification workflow",
            Some("Implement task verification workflow"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 2);
    assert_eq!(members.len(), 2);
}

#[test]
fn cross_provider_same_session_can_merge_but_stays_reviewable() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Implement benchmark reporting",
            Some("Implement benchmark reporting"),
        ),
        test_span_with_options(
            "span-b",
            "opencode",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(8),
            "Implement benchmark reporting",
            Some("Implement benchmark reporting"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 2);
    assert!(work_items[0].cross_provider);
    assert_eq!(work_items[0].status, TaskStatus::NeedsReview);
}

#[test]
fn repeated_banner_titles_with_real_usage_do_not_merge() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut contexts = Vec::new();
    for index in 0..5 {
        let timestamp = started_at + chrono::Duration::hours(96 * index as i64);
        let mut context = test_span_with_options(
            &format!("span-banner-{index}"),
            "codex",
            Some(&format!("session-banner-{index}")),
            "bucket-a",
            timestamp,
            "This is NOT the framework you know",
            Some("This is NOT the framework you know"),
        );
        context.span.project = Some(test_git_project("main"));
        context.span.linked_event_ids = vec![EventId(format!("event-banner-{index}"))];
        context.span.event_count = 1;
        context.span.has_usage_evidence = true;
        context.span.total_messages = 8;
        context.span.user_messages = 3;
        context.span.assistant_messages = 3;
        context.span.usage = UsageCounts {
            input_tokens: Some(100),
            output_tokens: Some(20),
            ..UsageCounts::default()
        };
        contexts.push(context);
    }

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 5);
    assert_eq!(members.len(), 5);
    assert!(work_items.iter().all(|item| item.span_count == 1));
    assert!(work_items
        .iter()
        .all(|item| item.title == "This is NOT the framework you know"));
    assert!(work_items.iter().all(|item| item
        .review_reasons
        .contains(&"low_specificity_title".to_string())));
}

#[test]
fn manual_split_preservation_uses_explicit_right_boundary() {
    let started_at = Utc.with_ymd_and_hms(2026, 7, 2, 10, 0, 0).unwrap();
    let span_a = test_span_with_options(
        "span-a",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at,
        "Implement task benchmark reporting",
        Some("Implement task benchmark reporting"),
    );
    let span_x = test_span_with_options(
        "span-x",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at + chrono::Duration::minutes(1),
        "Implement task benchmark reporting",
        Some("Implement task benchmark reporting"),
    );
    let span_b = test_span_with_options(
        "span-b",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at + chrono::Duration::minutes(2),
        "Implement task benchmark reporting",
        Some("Implement task benchmark reporting"),
    );
    let predicted_assignments = HashMap::from([
        ("span-a".to_string(), "work-left".to_string()),
        ("span-x".to_string(), "work-right".to_string()),
        ("span-b".to_string(), "work-left".to_string()),
    ]);
    let verification = TaskVerification {
        schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
        verification_id: task_verification_id("split", "split:span-a:span-b"),
        action_key: "split:span-a:span-b".to_string(),
        action: TaskVerificationAction::Split {
            after_span_id: TaskSpanId("span-a".to_string()),
            before_span_id: Some(TaskSpanId("span-b".to_string())),
            left_title: None,
            right_title: None,
        },
        created_at: started_at,
        updated_at: started_at,
    };

    assert!(!manual_constraints_preserved(
        &predicted_assignments,
        &[span_a.span, span_x.span, span_b.span],
        &[verification],
    ));
}

#[test]
fn localized_rebuild_deletes_layouts_reached_by_merged_ranges() {
    let store = Store::in_memory().expect("store");
    let started_at = Utc.with_ymd_and_hms(2026, 7, 2, 11, 0, 0).unwrap();
    let bucket = "bucket-a".to_string();
    let spans = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            &bucket,
            started_at,
            "Alpha payments cleanup",
            Some("Alpha payments cleanup"),
        )
        .span,
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-b"),
            &bucket,
            started_at + chrono::Duration::minutes(10),
            "Vector search benchmark",
            Some("Vector search benchmark"),
        )
        .span,
        test_span_with_options(
            "span-c",
            "codex",
            Some("session-c"),
            &bucket,
            started_at + chrono::Duration::minutes(20),
            "Kernel tuning audit",
            Some("Kernel tuning audit"),
        )
        .span,
        test_span_with_options(
            "span-d",
            "codex",
            Some("session-d"),
            &bucket,
            started_at + chrono::Duration::minutes(30),
            "Latency regression report",
            Some("Latency regression report"),
        )
        .span,
        test_span_with_options(
            "span-e",
            "codex",
            Some("session-e"),
            &bucket,
            started_at + chrono::Duration::minutes(40),
            "Schema export polish",
            Some("Schema export polish"),
        )
        .span,
    ];
    store.upsert_task_spans(&spans).expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild without merge");

    let initial = store.work_items().expect("initial work items");
    assert_eq!(initial.len(), 5);
    let left = initial
        .iter()
        .find(|item| item.anchor_span_id.0 == "span-a")
        .expect("left work item");
    let right = initial
        .iter()
        .find(|item| item.anchor_span_id.0 == "span-e")
        .expect("right work item");
    store
        .upsert_task_verification(TaskVerificationAction::Merge {
            left_work_item_id: left.work_item_id.clone(),
            right_work_item_id: right.work_item_id.clone(),
            left_anchor_span_id: TaskSpanId("span-a".to_string()),
            right_anchor_span_id: TaskSpanId("span-e".to_string()),
            title: Some("Merged endpoint work".to_string()),
        })
        .expect("merge verification");
    store
        .rebuild_all_task_work_items()
        .expect("rebuild merged layouts");

    let merged = store.work_items().expect("merged work items");
    assert_eq!(merged.len(), 4);

    let report = store
        .rebuild_task_work_items_for_changes_report(
            &BTreeSet::from([bucket.clone()]),
            &BTreeSet::from(["span-a".to_string()]),
            &[],
        )
        .expect("localized rebuild after endpoint merge");
    assert_eq!(report.work_items_deleted, 4);
    assert_eq!(report.work_items_rebuilt, 4);
    assert_eq!(report.touched_span_count, 5);

    let after = store
        .work_items()
        .expect("work items after localized rebuild");
    assert_eq!(after.len(), 4);
    let members = store.work_item_members_map().expect("member map");
    assert_eq!(members.len(), 5);
    assert_eq!(members.values().cloned().collect::<HashSet<_>>().len(), 4);
    assert!(members.contains_key("span-d"));
}
