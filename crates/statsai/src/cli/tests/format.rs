use super::support::*;
use super::*;

#[test]
fn preview_path_label_abbreviates_home_paths() {
    let Some(home) = home_dir() else {
        return;
    };
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        &home.join(".codex"),
        LocationOrigin::Default,
    );

    assert!(preview_path_label(&source).starts_with("~/.codex"));
}

#[test]
fn format_task_list_item_appends_review_reasons_when_present() {
    let ended_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut work_item = WorkItem {
        schema_version: "work_item.v1".to_string(),
        work_item_id: WorkItemId("work-review".to_string()),
        anchor_span_id: statsai_core::TaskSpanId("span-review".to_string()),
        tail_span_id: statsai_core::TaskSpanId("span-review".to_string()),
        project_bucket: "bucket".to_string(),
        title: "Reviewable item".to_string(),
        normalized_title: "reviewable item".to_string(),
        status: TaskStatus::NeedsReview,
        confidence: Confidence::Low,
        started_at: ended_at - Duration::minutes(5),
        ended_at,
        duration_seconds: Some(300),
        span_count: 1,
        event_count: 0,
        total_input_tokens: 0,
        total_cache_creation_tokens: 0,
        total_cache_read_tokens: 0,
        total_output_tokens: 0,
        total_reasoning_tokens: 0,
        total_tokens: 0,
        estimated_cost_usd: None,
        estimated_cost_micro_usd: None,
        providers: vec!["claude_code".to_string()],
        issue_keys: Vec::new(),
        repo_label: None,
        branch_labels: Vec::new(),
        path_label: None,
        summary_preview: None,
        todo_excerpt: None,
        no_git: false,
        cross_provider: false,
        continuation_reasons: Vec::new(),
        review_reasons: vec!["no_usage_evidence".to_string(), "generic_title".to_string()],
    };

    let line = format_task_list_item(&work_item);
    assert!(line.contains("review=no_usage_evidence,generic_title"));

    work_item.review_reasons.clear();
    let clean_line = format_task_list_item(&work_item);
    assert!(!clean_line.contains("review="));
}

#[test]
fn stats_json_value_exposes_expected_fields() {
    let stats = statsai_store::TaskStats {
        total_spans: 10,
        total_work_items: 3,
        verified_percentage: 25.0,
        no_git_percentage: 50.0,
        cross_provider_percentage: 10.0,
        rejected_meta_percentage: 5.0,
        average_spans_per_work_item: 3.33,
    };

    let json = stats_json_value(&stats);
    assert_eq!(json["total_spans"], json!(10));
    assert_eq!(json["total_work_items"], json!(3));
    assert_eq!(json["verified_percentage"], json!(25.0));
    assert_eq!(json["no_git_percentage"], json!(50.0));
    assert_eq!(json["cross_provider_percentage"], json!(10.0));
    assert_eq!(json["rejected_meta_percentage"], json!(5.0));
    assert_eq!(json["average_spans_per_work_item"], json!(3.33));
}

#[test]
fn usd_amount_json_uses_major_units() {
    assert_eq!(usd_amount_json(Some(125)), json!(1.25));
    assert_eq!(usd_amount_json(None), Value::Null);
}

#[test]
fn subscription_json_value_preserves_major_unit_price() {
    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id(
            "codex",
            &provider_account_id("codex", "acct-test"),
            "Plus",
            started_at,
        ),
        provider: "codex".to_string(),
        provider_account_id: provider_account_id("codex", "acct-test"),
        plan_name: "Plus".to_string(),
        price: 2000,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: Some(started_at),
        renewal_day: Some(29),
        started_at,
        ended_at: None,
        current_period_ends_at: None,
        status: SubscriptionStatus::Active,
        record_source: IdentitySource::UserConfigured,
        verified_at: None,
        notes: None,
    };

    let value = subscription_json_value(&subscription);

    assert_eq!(value["price"], json!(20.0));
    assert_eq!(value["price_cents"], json!(2000));
}
