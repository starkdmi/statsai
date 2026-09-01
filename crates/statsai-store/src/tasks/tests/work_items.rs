use super::support::*;
use super::*;

#[test]
fn derived_work_item_rounds_cost_after_aggregating_exact_micro_usd() {
    let started_at = Utc.with_ymd_and_hms(2026, 7, 25, 12, 0, 0).unwrap();
    let spans = (0..3)
        .map(|index| {
            let mut span = test_span_with_options(
                &format!("span-cost-{index}"),
                "codex",
                Some("session-cost"),
                "bucket-cost",
                started_at + chrono::Duration::minutes(index),
                "Implement exact task pricing",
                Some("Implement exact task pricing"),
            )
            .span;
            span.estimated_cost_usd = Some(0);
            span.estimated_cost_micro_usd = Some(2_250);
            span.event_count = 1;
            span.has_usage_evidence = true;
            span
        })
        .collect::<Vec<_>>();

    let (work_items, _) = derive_task_work_items(spans, &[]);

    assert_eq!(work_items.len(), 1);
    assert_eq!(work_items[0].estimated_cost_micro_usd, Some(6_750));
    assert_eq!(work_items[0].estimated_cost_usd, Some(1));
}

#[test]
fn no_git_path_only_workspace_still_produces_work_item() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![test_span_with_options(
        "span-a",
        "codex",
        Some("session-a"),
        "repo:none|path:abc|branch:none",
        started_at,
        "Implement local task collection",
        Some("Implement local task collection"),
    )];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 1);
    assert_eq!(work_items[0].title, "Implement local task collection");
    assert!(work_items[0].no_git);
    assert_eq!(work_items[0].status, TaskStatus::NeedsReview);
}

#[test]
fn git_anchored_work_item_with_event_evidence_stays_auto_high() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut context = test_span_with_options(
        "span-a",
        "claude_code",
        Some("session-a"),
        "bucket-a",
        started_at,
        "SwiftUI Paywall Sheet Race Condition Fix",
        Some("SwiftUI Paywall Sheet Race Condition Fix"),
    );
    context.span.project = Some(test_git_project("main"));
    context.span.linked_event_ids = vec![EventId("event-a".to_string())];
    context.span.usage = UsageCounts {
        input_tokens: Some(100),
        output_tokens: Some(20),
        ..UsageCounts::default()
    };

    let (work_items, members, _) = build_work_items(vec![context], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 1);
    assert_eq!(work_items[0].status, TaskStatus::Auto);
    assert_eq!(work_items[0].confidence, Confidence::High);
    assert_eq!(work_items[0].event_count, 1);
    assert_eq!(work_items[0].total_tokens, 120);
    assert!(!work_items[0].no_git);
    assert!(work_items[0].review_reasons.is_empty());
}

#[test]
fn git_anchored_work_item_without_event_evidence_needs_review_low() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut context = test_span_with_options(
        "span-a",
        "claude_code",
        Some("session-a"),
        "bucket-a",
        started_at,
        "SwiftUI Paywall Sheet Race Condition Fix",
        Some("SwiftUI Paywall Sheet Race Condition Fix"),
    );
    context.span.project = Some(test_git_project("main"));

    let (work_items, members, _) = build_work_items(vec![context], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 1);
    assert_eq!(work_items[0].status, TaskStatus::NeedsReview);
    assert_eq!(work_items[0].confidence, Confidence::Low);
    assert_eq!(work_items[0].event_count, 0);
    assert_eq!(work_items[0].total_tokens, 0);
    assert!(!work_items[0].no_git);
    assert!(work_items[0]
        .review_reasons
        .contains(&"no_usage_evidence".to_string()));
}

#[test]
fn session_control_item_without_event_evidence_is_rejected_meta() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut context = test_span_with_options(
        "span-a",
        "claude_code",
        Some("session-a"),
        "bucket-a",
        started_at,
        "Clearing Conversation History",
        Some("Clearing Conversation History"),
    );
    context.span.project = Some(test_git_project("main"));

    let (work_items, members, _) = build_work_items(vec![context], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 1);
    assert_eq!(work_items[0].status, TaskStatus::RejectedMeta);
    assert_eq!(work_items[0].confidence, Confidence::Low);
    assert_eq!(work_items[0].title, "Clearing Conversation History");
    assert!(work_items[0]
        .review_reasons
        .contains(&"no_usage_evidence".to_string()));
}

#[test]
fn low_volume_generic_exchange_is_rejected_meta() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut context = test_span_with_options(
        "span-a",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at,
        "Morning Greetings",
        Some("Say hi, nothing else"),
    );
    context.span.linked_event_ids = vec![EventId("event-a".to_string())];
    context.span.event_count = 1;
    context.span.has_usage_evidence = true;
    context.span.total_messages = 2;
    context.span.user_messages = 1;
    context.span.assistant_messages = 1;

    let (work_items, members, _) = build_work_items(vec![context], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 1);
    assert_eq!(work_items[0].status, TaskStatus::RejectedMeta);
    assert_eq!(work_items[0].confidence, Confidence::Low);
    assert!(work_items[0]
        .review_reasons
        .contains(&"low_signal_exchange".to_string()));
}

#[test]
fn repeated_low_volume_generic_shells_are_rejected_meta() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut morning = test_span_with_options(
        "span-a",
        "codex",
        Some("quota-session"),
        "bucket-a",
        started_at,
        "Morning Greetings",
        Some("Say hi, nothing else"),
    );
    morning.span.linked_event_ids = vec![EventId("event-a".to_string())];
    morning.span.event_count = 1;
    morning.span.has_usage_evidence = true;
    morning.span.total_messages = 2;
    morning.span.user_messages = 1;
    morning.span.assistant_messages = 1;

    let mut lunch = test_span_with_options(
        "span-b",
        "codex",
        Some("quota-session"),
        "bucket-a",
        started_at + chrono::Duration::hours(4),
        "Lunch Greetings",
        Some("Say hi, nothing else"),
    );
    lunch.span.linked_event_ids = vec![EventId("event-b".to_string())];
    lunch.span.event_count = 1;
    lunch.span.has_usage_evidence = true;
    lunch.span.total_messages = 2;
    lunch.span.user_messages = 1;
    lunch.span.assistant_messages = 1;

    let mut evening = test_span_with_options(
        "span-c",
        "codex",
        Some("quota-session"),
        "bucket-a",
        started_at + chrono::Duration::hours(8),
        "Evening Greetings",
        Some("Say hi, nothing else"),
    );
    evening.span.linked_event_ids = vec![EventId("event-c".to_string())];
    evening.span.event_count = 1;
    evening.span.has_usage_evidence = true;
    evening.span.total_messages = 2;
    evening.span.user_messages = 1;
    evening.span.assistant_messages = 1;

    let (work_items, members, _) = build_work_items(vec![morning, lunch, evening], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 3);
    assert_eq!(work_items[0].status, TaskStatus::RejectedMeta);
    assert_eq!(work_items[0].confidence, Confidence::Low);
    assert!(work_items[0]
        .review_reasons
        .contains(&"low_signal_exchange".to_string()));
}

#[test]
fn work_items_are_ordered_for_review_queue() {
    let store = Store::in_memory().expect("store");
    let ended_base = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let work_items = vec![
        test_work_item(
            "verified-high",
            "span-verified",
            TaskStatus::Verified,
            Confidence::High,
            999,
            ended_base + chrono::Duration::hours(5),
        ),
        test_work_item(
            "auto-low",
            "span-auto",
            TaskStatus::Auto,
            Confidence::Low,
            9999,
            ended_base + chrono::Duration::hours(4),
        ),
        test_work_item(
            "needs-medium",
            "span-medium",
            TaskStatus::NeedsReview,
            Confidence::Medium,
            100,
            ended_base + chrono::Duration::hours(3),
        ),
        test_work_item(
            "needs-low-earlier",
            "span-low-earlier",
            TaskStatus::NeedsReview,
            Confidence::Low,
            500,
            ended_base + chrono::Duration::hours(1),
        ),
        test_work_item(
            "needs-low-later",
            "span-low-later",
            TaskStatus::NeedsReview,
            Confidence::Low,
            500,
            ended_base + chrono::Duration::hours(2),
        ),
    ];
    let members = work_items
        .iter()
        .map(|item| WorkItemMember {
            work_item_id: item.work_item_id.clone(),
            span_id: item.anchor_span_id.clone(),
            ordinal: 0,
        })
        .collect::<Vec<_>>();

    store
        .insert_work_items_in_tx(&work_items, &members)
        .expect("insert work items");

    let ordered = store.work_items().expect("ordered work items");
    let ids = ordered
        .iter()
        .map(|item| item.work_item_id.0.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            "needs-low-later",
            "needs-low-earlier",
            "needs-medium",
            "auto-low",
            "verified-high",
        ]
    );
}
