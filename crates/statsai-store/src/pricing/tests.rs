use super::*;
use crate::CURRENT_SCHEMA_VERSION;
use chrono::Utc;
use statsai_core::{
    event_id, summary_id, task_span_id, Confidence, CostInfo, EventSource, LocationOrigin,
    ModelInfo, PrivacyInfo, PrivacyMode, SessionInfo, SourceKind, SummaryMetadata, TaskSpan,
    UsageCounts, UsageEvent, UsageSummary, TASK_SPAN_SCHEMA_VERSION, USAGE_EVENT_SCHEMA_VERSION,
    USAGE_SUMMARY_SCHEMA_VERSION,
};
use std::path::Path;
use std::sync::{Arc, Barrier};

fn test_model(name: &str) -> ModelInfo {
    ModelInfo {
        name: Some(name.to_string()),
        normalized_name: Some(name.to_string()),
        provider_model_id: Some(name.to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    }
}

fn parse_utc(value: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(value)
        .expect("valid timestamp")
        .with_timezone(&chrono::Utc)
}

fn test_source(path: &str) -> statsai_core::SourceLocation {
    statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new(path),
        LocationOrigin::Configured,
    )
}

fn test_event(
    source: &statsai_core::SourceLocation,
    started_at: chrono::DateTime<Utc>,
    record_id: &str,
    model: &str,
    usage: UsageCounts,
    cost: CostInfo,
) -> UsageEvent {
    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: event_id("codex", &source.source_id, record_id, None, started_at),
        device_id: "device".to_string(),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        subscription_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "jsonl".to_string(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some(record_id.to_string()),
            parse_confidence: Confidence::High,
        },
        session: SessionInfo {
            session_id: "session".to_string(),
            local_session_id_hash: Some("same-session".to_string()),
            title: None,
            started_at,
            ended_at: None,
            duration_seconds: None,
        },
        model: Some(test_model(model)),
        usage,
        runtime: None,
        cost,
        parse_evidence: None,
        project: None,
        git: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        created_at: started_at,
        imported_at: started_at,
    }
}

fn missing_cost() -> CostInfo {
    CostInfo {
        currency: "USD".to_string(),
        estimated_api_equivalent_usd: None,
        provider_reported_usd: None,
        estimated_api_equivalent_micro_usd: None,
        provider_reported_micro_usd: None,
        pricing_source: Some("unknown".to_string()),
        pricing_version: None,
        confidence: Confidence::Low,
    }
}

fn million_token_usage() -> UsageCounts {
    UsageCounts {
        input_tokens: Some(1_000_000),
        cache_creation_tokens: Some(1_000_000),
        cache_read_tokens: Some(1_000_000),
        output_tokens: Some(1_000_000),
        total_tokens: Some(4_000_000),
        ..UsageCounts::default()
    }
}

fn expected_review_cost(started_at: chrono::DateTime<Utc>) -> CostInfo {
    estimate_cost_at(
        "codex",
        Some(&test_model("codex-auto-review")),
        &million_token_usage(),
        &started_at,
    )
}

fn store_with_source(path: &str) -> (Store, statsai_core::SourceLocation) {
    let store = Store::in_memory().expect("store");
    let source = test_source(path);
    store.upsert_source(&source).expect("source");
    (store, source)
}

fn stored_event(store: &Store, event_id: &str) -> UsageEvent {
    store
        .event_by_id(event_id)
        .expect("load event")
        .expect("event present")
}

fn test_span(
    source: &statsai_core::SourceLocation,
    started_at: chrono::DateTime<Utc>,
    record_id: &str,
    linked_event_ids: Vec<statsai_core::EventId>,
    estimated_cost_usd: Option<i64>,
    estimated_cost_micro_usd: Option<i64>,
) -> TaskSpan {
    TaskSpan {
        schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
        span_id: task_span_id("codex", &source.source_id, record_id),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        span_kind: "codex_session".to_string(),
        source_record_id: Some(record_id.to_string()),
        source_file_path_hash: None,
        summary_id: None,
        session_id: Some("session".to_string()),
        thread_id: None,
        title: "Review".to_string(),
        normalized_title: "review".to_string(),
        title_source: Some("summary".to_string()),
        summary_preview: None,
        todo_excerpt: None,
        issue_keys: Vec::new(),
        branch_family: None,
        project_bucket: "none".to_string(),
        project: None,
        git: None,
        usage: million_token_usage(),
        estimated_cost_usd,
        estimated_cost_micro_usd,
        event_count: linked_event_ids.len() as u64,
        has_usage_evidence: !linked_event_ids.is_empty(),
        total_messages: 0,
        user_messages: 0,
        assistant_messages: 0,
        developer_messages: 0,
        linked_event_ids,
        confidence: Confidence::Medium,
        is_meta: false,
        started_at,
        ended_at: Some(started_at),
        duration_seconds: Some(0),
    }
}

fn test_summary(
    source: &statsai_core::SourceLocation,
    model: &str,
    start: chrono::DateTime<Utc>,
    end: chrono::DateTime<Utc>,
    cost: CostInfo,
) -> UsageSummary {
    UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: summary_id("codex", &source.source_id, "period"),
        device_id: "device".to_string(),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "stats-cache.json".to_string(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some("period".to_string()),
            parse_confidence: Confidence::Medium,
        },
        model: Some(test_model(model)),
        models: Vec::new(),
        usage: million_token_usage(),
        cost,
        parse_evidence: None,
        project: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        metrics: None,
        period_start: Some(start),
        period_end: Some(end),
        observed_at: end,
        metadata: SummaryMetadata {
            summary_format: "grok_build_session_summary".to_string(),
            summary_version: Some("1".to_string()),
            total_sessions: Some(1),
            total_messages: Some(2),
            last_computed_at: Some(end),
        },
        imported_at: end,
    }
}

#[test]
fn store_open_does_not_reprice() {
    let (store, source) = store_with_source("/tmp/codex-open-no-reprice");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let event = test_event(
        &source,
        started_at,
        "legacy-review",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    store.insert_event(&event).expect("insert");

    assert_eq!(store.applied_pricing_ruleset_version().expect("meta"), None);
    assert!(stored_event(&store, &event.event_id.0)
        .cost
        .estimated_api_equivalent_usd
        .is_none());
}

#[test]
fn legacy_codex_auto_review_event_is_repriced_without_source_files() {
    let (store, source) = store_with_source("/tmp/codex-legacy-review");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let event = test_event(
        &source,
        started_at,
        "legacy-review",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    store.insert_event(&event).expect("insert");

    let report = store.ensure_current_pricing().expect("reprice");

    assert_eq!(report.examined_events, 1);
    assert_eq!(report.changed_events, 1);
    assert!(!report.already_current);
    let stored = stored_event(&store, &event.event_id.0);
    assert_eq!(stored.cost, expected_review_cost(started_at));
    assert_eq!(stored.event_id, event.event_id);
    assert_eq!(stored.usage, event.usage);
    assert_eq!(stored.session.started_at, event.session.started_at);
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        Some(PRICING_RULESET_VERSION)
    );
    assert_eq!(
        store
            .metadata_value(APPLIED_PRICING_CATALOG_VERSION_KEY)
            .expect("catalog"),
        Some(PRICING_CATALOG_VERSION.to_string())
    );
}

#[test]
fn date_aware_codex_auto_review_boundary_is_preserved() {
    let (store, source) = store_with_source("/tmp/codex-review-boundary");
    let before = parse_utc("2026-07-29T23:59:59Z");
    let after = parse_utc("2026-07-30T00:00:00Z");
    let before_event = test_event(
        &source,
        before,
        "before-boundary",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    let after_event = test_event(
        &source,
        after,
        "after-boundary",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    store
        .insert_events(&[before_event.clone(), after_event.clone()])
        .expect("insert");

    store.ensure_current_pricing().expect("reprice");

    let before_cost = stored_event(&store, &before_event.event_id.0).cost;
    let after_cost = stored_event(&store, &after_event.event_id.0).cost;
    assert_eq!(before_cost, expected_review_cost(before));
    assert_eq!(after_cost, expected_review_cost(after));
    assert_ne!(
        before_cost.estimated_api_equivalent_usd,
        after_cost.estimated_api_equivalent_usd
    );
    assert_eq!(after_cost.estimated_api_equivalent_usd, Some(167));
}

#[test]
fn applied_metadata_advances_only_after_success() {
    let (store, source) = store_with_source("/tmp/codex-reprice-success-meta");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    store
        .insert_event(&test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        ))
        .expect("insert");

    assert_eq!(
        store.applied_pricing_ruleset_version().expect("before"),
        None
    );
    store.ensure_current_pricing().expect("reprice");
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("after"),
        Some(PRICING_RULESET_VERSION)
    );
}

#[test]
fn second_invocation_at_the_same_version_is_a_noop() {
    let (store, source) = store_with_source("/tmp/codex-reprice-noop");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    store
        .insert_event(&test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        ))
        .expect("insert");

    let first = store.ensure_current_pricing().expect("first");
    let payload_before = store.events().expect("events");
    let second = store.ensure_current_pricing().expect("second");

    assert_eq!(first.changed_events, 1);
    assert!(second.already_current);
    assert_eq!(second.changed_events, 0);
    assert_eq!(second.examined_events, 0);
    assert_eq!(store.events().expect("events after noop"), payload_before);
}

#[test]
fn provider_reported_cost_and_provenance_survive_repricing() {
    let (store, source) = store_with_source("/tmp/codex-provider-reported");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let mut cost = missing_cost();
    cost.provider_reported_usd = Some(99);
    cost.provider_reported_micro_usd = Some(990_000);
    cost.pricing_source = Some("provider_invoice".to_string());
    cost.confidence = Confidence::High;
    let event = test_event(
        &source,
        started_at,
        "reported",
        "codex-auto-review",
        million_token_usage(),
        cost,
    );
    store.insert_event(&event).expect("insert");

    store.ensure_current_pricing().expect("reprice");
    let stored = stored_event(&store, &event.event_id.0);
    assert_eq!(stored.cost.provider_reported_usd, Some(99));
    assert_eq!(stored.cost.provider_reported_micro_usd, Some(990_000));
    assert_eq!(
        stored.cost.pricing_source.as_deref(),
        Some("provider_invoice")
    );
    assert_eq!(stored.cost.confidence, Confidence::High);
    assert_eq!(
        stored.cost.estimated_api_equivalent_usd,
        expected_review_cost(started_at).estimated_api_equivalent_usd
    );
    assert_eq!(
        stored.cost.pricing_version.as_deref(),
        Some(PRICING_CATALOG_VERSION)
    );
}

#[test]
fn apply_current_estimated_pricing_overlays_stale_estimated_only_summary() {
    let source = test_source("/tmp/codex-import-overlay");
    let start = parse_utc("2026-07-29T00:00:00Z");
    let end = parse_utc("2026-07-29T23:59:59Z");
    let mut summary = test_summary(&source, "codex-auto-review", start, end, missing_cost());
    summary.cost.estimated_api_equivalent_usd = Some(1);
    summary.cost.estimated_api_equivalent_micro_usd = Some(10_000);
    summary.cost.pricing_source = Some("official:stale".to_string());
    summary.cost.pricing_version = Some("official:stale".to_string());

    let priced = apply_current_estimated_pricing(summary);
    assert_eq!(priced.cost, expected_review_cost(end));
}

#[test]
fn apply_current_estimated_pricing_keeps_provider_reported_amount() {
    let source = test_source("/tmp/codex-import-provider-reported");
    let start = parse_utc("2026-07-29T00:00:00Z");
    let end = parse_utc("2026-07-29T23:59:59Z");
    let mut cost = missing_cost();
    cost.provider_reported_usd = Some(99);
    cost.provider_reported_micro_usd = Some(990_000);
    cost.pricing_source = Some("provider_invoice".to_string());
    cost.confidence = Confidence::High;
    let summary = test_summary(&source, "codex-auto-review", start, end, cost);

    let priced = apply_current_estimated_pricing(summary);
    assert_eq!(priced.cost.provider_reported_usd, Some(99));
    assert_eq!(priced.cost.provider_reported_micro_usd, Some(990_000));
    assert_eq!(
        priced.cost.pricing_source.as_deref(),
        Some("provider_invoice")
    );
    assert_eq!(
        priced.cost.estimated_api_equivalent_usd,
        expected_review_cost(end).estimated_api_equivalent_usd
    );
}

#[test]
fn unknown_model_stays_unknown_while_ruleset_is_marked_applied() {
    let (store, source) = store_with_source("/tmp/codex-unknown-model");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let event = test_event(
        &source,
        started_at,
        "unknown",
        "not-a-real-model",
        million_token_usage(),
        missing_cost(),
    );
    store.insert_event(&event).expect("insert");

    let report = store.ensure_current_pricing().expect("reprice");
    let stored = stored_event(&store, &event.event_id.0);
    assert_eq!(report.changed_events, 0);
    assert!(stored.cost.estimated_api_equivalent_usd.is_none());
    assert_eq!(stored.cost.pricing_source.as_deref(), Some("unknown"));
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        Some(PRICING_RULESET_VERSION)
    );
}

#[test]
fn summary_spanning_a_pricing_boundary_remains_unknown() {
    let (store, source) = store_with_source("/tmp/codex-boundary-summary");
    let start = parse_utc("2026-07-29T00:00:00Z");
    let end = parse_utc("2026-07-31T00:00:00Z");
    let mut summary = UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: summary_id("codex", &source.source_id, "crossing"),
        device_id: "device".to_string(),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalSummary,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "stats-cache.json".to_string(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some("crossing".to_string()),
            parse_confidence: Confidence::Medium,
        },
        model: Some(test_model("codex-auto-review")),
        models: Vec::new(),
        usage: million_token_usage(),
        cost: missing_cost(),
        parse_evidence: None,
        project: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        metrics: None,
        period_start: Some(start),
        period_end: Some(end),
        observed_at: end,
        metadata: SummaryMetadata {
            summary_format: "claude_stats_cache".to_string(),
            summary_version: Some("1".to_string()),
            total_sessions: Some(1),
            total_messages: Some(2),
            last_computed_at: Some(end),
        },
        imported_at: end,
    };
    summary.cost.estimated_api_equivalent_usd = Some(999);
    store.upsert_summary(&summary).expect("summary");

    let report = store.ensure_current_pricing().expect("reprice");
    let stored = store
        .summaries()
        .expect("summaries")
        .into_iter()
        .next()
        .expect("one summary");
    assert_eq!(report.changed_summaries, 1);
    assert!(stored.cost.estimated_api_equivalent_usd.is_none());
    assert_eq!(stored.cost.pricing_source.as_deref(), Some("unknown"));
}

#[test]
fn changed_sync_rollups_are_refreshed_and_marked_dirty() {
    let (store, source) = store_with_source("/tmp/codex-reprice-rollups");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let event = test_event(
        &source,
        started_at,
        "legacy-review",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    store.insert_event(&event).expect("insert");
    store.rebuild_sync_rollups().expect("rebuild");
    let rollups = store.all_sync_rollup_summaries().expect("rollups");
    store
        .mark_sync_rollups_synced(
            &rollups
                .iter()
                .map(|summary| summary.summary_id.clone())
                .collect::<Vec<_>>(),
        )
        .expect("mark synced");
    assert!(store
        .dirty_sync_rollup_summaries()
        .expect("clean")
        .is_empty());

    let report = store.ensure_current_pricing().expect("reprice");
    let dirty = store.dirty_sync_rollup_summaries().expect("dirty");
    assert_eq!(report.refreshed_rollups, 1);
    assert_eq!(dirty.len(), 1);
    assert_eq!(
        dirty[0].cost.estimated_api_equivalent_usd,
        expected_review_cost(started_at).estimated_api_equivalent_usd
    );
}

#[test]
fn injected_mid_operation_error_rolls_back_payloads_rollups_and_metadata() {
    let (store, source) = store_with_source("/tmp/codex-reprice-rollback");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let first = test_event(
        &source,
        started_at,
        "first",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    let second = test_event(
        &source,
        started_at + chrono::Duration::seconds(1),
        "second",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    store
        .insert_events(&[first.clone(), second.clone()])
        .expect("insert");
    store.rebuild_sync_rollups().expect("rebuild");
    let rollups = store.all_sync_rollup_summaries().expect("rollups");
    store
        .mark_sync_rollups_synced(
            &rollups
                .iter()
                .map(|summary| summary.summary_id.clone())
                .collect::<Vec<_>>(),
        )
        .expect("mark synced");
    let payloads_before = store.events().expect("events before");

    fail_repricing_after_event_writes(Some(1));
    let error = store
        .ensure_current_pricing()
        .expect_err("injected failure");
    fail_repricing_after_event_writes(None);

    assert!(error.to_string().contains("injected repricing failure"));
    assert_eq!(store.events().expect("events after"), payloads_before);
    assert!(store
        .dirty_sync_rollup_summaries()
        .expect("dirty after rollback")
        .is_empty());
    assert_eq!(store.applied_pricing_ruleset_version().expect("meta"), None);
}

#[test]
fn older_ruleset_refuses_a_newer_store_and_does_not_mutate_it() {
    let (store, source) = store_with_source("/tmp/codex-forward-pricing");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let event = test_event(
        &source,
        started_at,
        "legacy-review",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    store.insert_event(&event).expect("insert");
    store
        .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "99")
        .expect("future ruleset");
    store
        .set_metadata_value(APPLIED_PRICING_CATALOG_VERSION_KEY, "future")
        .expect("future catalog");
    let payload_before =
        serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize");

    let error = store
        .ensure_pricing_ruleset(1, PRICING_CATALOG_VERSION)
        .expect_err("forward pricing must refuse");

    assert!(error
        .to_string()
        .contains("pricing ruleset version 99 is newer than this StatsAI binary supports (1)"));
    assert_eq!(
        serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize"),
        payload_before
    );
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("unchanged"),
        Some(99)
    );
}

#[test]
fn concurrent_callers_do_not_publish_partial_or_duplicate_repricing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("statsai.sqlite");
    let setup = Store::open(&path).expect("create store");
    let source = test_source("/tmp/codex-concurrent-reprice");
    setup.upsert_source(&source).expect("source");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    setup
        .insert_event(&test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        ))
        .expect("insert");
    drop(setup);

    let barrier = Arc::new(Barrier::new(2));
    let reports = std::thread::scope(|scope| {
        let first_barrier = Arc::clone(&barrier);
        let second_barrier = Arc::clone(&barrier);
        let first_path = path.clone();
        let second_path = path.clone();
        let first = scope.spawn(move || {
            let store = Store::open(&first_path).expect("open first");
            first_barrier.wait();
            store.ensure_current_pricing()
        });
        let second = scope.spawn(move || {
            let store = Store::open(&second_path).expect("open second");
            second_barrier.wait();
            store.ensure_current_pricing()
        });
        vec![
            first.join().expect("first thread"),
            second.join().expect("second thread"),
        ]
    });

    let reports = reports
        .into_iter()
        .map(|result| result.expect("repricing"))
        .collect::<Vec<_>>();
    let workers = reports
        .iter()
        .filter(|report| !report.already_current)
        .count();
    let noops = reports
        .iter()
        .filter(|report| report.already_current)
        .count();
    assert_eq!(workers, 1);
    assert_eq!(noops, 1);
    assert_eq!(
        reports
            .iter()
            .map(|report| report.changed_events)
            .sum::<u64>(),
        1
    );

    let store = Store::open(&path).expect("reopen");
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        Some(PRICING_RULESET_VERSION)
    );
    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cost, expected_review_cost(started_at));
}

#[test]
fn daily_rollups_are_unused_by_report_and_sync_paths() {
    let (store, source) = store_with_source("/tmp/codex-daily-rollup-exclusion");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    store
        .insert_event(&test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        ))
        .expect("insert");
    let stale = store
        .compute_daily_rollup("2026-07-29", "device")
        .expect("compute");
    store
        .upsert_daily_rollup(&stale)
        .expect("seed unused table");
    let before = store
        .daily_rollups_between("2026-07-29", "2026-07-29")
        .expect("before");

    store.ensure_current_pricing().expect("reprice");

    let after = store
        .daily_rollups_between("2026-07-29", "2026-07-29")
        .expect("after");
    assert_eq!(before, after);
    let events = store.events().expect("events");
    assert_eq!(events[0].cost, expected_review_cost(started_at));
    // Pin: bump this when CURRENT_SCHEMA_VERSION changes, after confirming
    // the daily_rollups table is still unused by report/sync/snapshot.
    assert_eq!(CURRENT_SCHEMA_VERSION, 22);
}

#[test]
fn task_spans_with_linked_events_are_repriced_from_persisted_events() {
    let (store, source) = store_with_source("/tmp/codex-task-span-reprice");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let event = test_event(
        &source,
        started_at,
        "legacy-review",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    store.insert_event(&event).expect("insert");
    store
        .upsert_task_spans(&[test_span(
            &source,
            started_at,
            "span",
            vec![event.event_id.clone()],
            Some(0),
            Some(0),
        )])
        .expect("span");

    let report = store.ensure_current_pricing().expect("reprice");
    let stored = store.task_spans().expect("spans");
    assert_eq!(report.changed_task_spans, 1);
    assert_eq!(
        stored[0].estimated_cost_usd,
        expected_review_cost(started_at).estimated_api_equivalent_usd
    );
}

#[test]
fn stale_task_spans_are_repriced_when_events_already_match() {
    let (store, source) = store_with_source("/tmp/codex-stale-span-current-event");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let expected = expected_review_cost(started_at);
    let event = test_event(
        &source,
        started_at,
        "already-priced",
        "codex-auto-review",
        million_token_usage(),
        expected.clone(),
    );
    store.insert_event(&event).expect("insert");
    store
        .upsert_task_spans(&[test_span(
            &source,
            started_at,
            "stale-span",
            vec![event.event_id.clone()],
            Some(0),
            Some(0),
        )])
        .expect("span");

    let report = store.ensure_current_pricing().expect("reprice");
    let stored = store.task_spans().expect("spans");
    assert_eq!(report.changed_events, 0);
    assert_eq!(report.changed_task_spans, 1);
    assert!(!report.already_current);
    assert_eq!(
        stored[0].estimated_cost_usd,
        expected.estimated_api_equivalent_usd
    );
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        Some(PRICING_RULESET_VERSION)
    );
}

#[test]
fn unlinked_task_spans_are_left_unchanged() {
    let (store, source) = store_with_source("/tmp/codex-unlinked-span");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    store
        .insert_event(&test_event(
            &source,
            started_at,
            "priced",
            "codex-auto-review",
            million_token_usage(),
            expected_review_cost(started_at),
        ))
        .expect("insert");
    store
        .upsert_task_spans(&[test_span(
            &source,
            started_at,
            "unlinked",
            Vec::new(),
            Some(0),
            Some(0),
        )])
        .expect("span");

    let report = store.ensure_current_pricing().expect("reprice");
    let stored = store.task_spans().expect("spans");
    assert_eq!(report.changed_task_spans, 0);
    assert_eq!(stored[0].estimated_cost_usd, Some(0));
    assert_eq!(stored[0].estimated_cost_micro_usd, Some(0));
}

#[test]
fn unreadable_usage_payloads_are_skipped_without_blocking_repricing() {
    let (store, source) = store_with_source("/tmp/codex-corrupt-usage-payload");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let event = test_event(
        &source,
        started_at,
        "legacy-review",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    store.insert_event(&event).expect("insert");
    store
        .conn
        .execute(
            "INSERT INTO usage_events (
                   event_id, provider, source_id, started_at, total_tokens, payload
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "aaa-corrupt-event",
                "codex",
                source.source_id.0.as_str(),
                started_at.to_rfc3339(),
                0,
                "{ this is not json",
            ],
        )
        .expect("corrupt event");
    store
        .conn
        .execute(
            "INSERT INTO usage_summaries (
                   summary_id, provider, source_id, observed_at, total_tokens, payload
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "aaa-corrupt-summary",
                "codex",
                source.source_id.0.as_str(),
                started_at.to_rfc3339(),
                0,
                "{ also not json",
            ],
        )
        .expect("corrupt summary");

    let report = store.ensure_current_pricing().expect("reprice");
    assert_eq!(report.skipped_unreadable_events, 1);
    assert_eq!(report.skipped_unreadable_summaries, 1);
    assert_eq!(report.changed_events, 1);
    assert_eq!(report.refreshed_rollups, 1);
    assert_eq!(
        stored_event(&store, &event.event_id.0).cost,
        expected_review_cost(started_at)
    );
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        Some(PRICING_RULESET_VERSION)
    );
    let corrupt = store
        .conn
        .query_row(
            "SELECT payload FROM usage_events WHERE event_id = ?1",
            ["aaa-corrupt-event"],
            |row| row.get::<_, String>(0),
        )
        .expect("corrupt row remains");
    assert_eq!(corrupt, "{ this is not json");
}

#[test]
fn dangling_task_span_links_clear_stale_cost() {
    let (store, source) = store_with_source("/tmp/codex-dangling-span-link");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let event = test_event(
        &source,
        started_at,
        "missing-later",
        "codex-auto-review",
        million_token_usage(),
        expected_review_cost(started_at),
    );
    store.insert_event(&event).expect("insert");
    store
        .upsert_task_spans(&[test_span(
            &source,
            started_at,
            "dangling",
            vec![event.event_id.clone()],
            Some(99),
            Some(990_000),
        )])
        .expect("span");
    store
        .conn
        .execute(
            "DELETE FROM usage_events WHERE event_id = ?1",
            [&event.event_id.0],
        )
        .expect("drop linked event");

    let report = store.ensure_current_pricing().expect("reprice");
    let stored = store.task_spans().expect("spans");
    assert_eq!(report.changed_task_spans, 1);
    assert!(stored[0].estimated_cost_usd.is_none());
    assert!(stored[0].estimated_cost_micro_usd.is_none());
}

#[test]
fn summary_inside_one_pricing_window_is_repriced() {
    let (store, source) = store_with_source("/tmp/codex-window-summary");
    let start = parse_utc("2026-07-28T00:00:00Z");
    let end = parse_utc("2026-07-29T23:59:59Z");
    store
        .upsert_summary(&test_summary(
            &source,
            "codex-auto-review",
            start,
            end,
            missing_cost(),
        ))
        .expect("summary");

    let report = store.ensure_current_pricing().expect("reprice");
    let stored = store
        .summaries()
        .expect("summaries")
        .into_iter()
        .next()
        .expect("one summary");
    assert_eq!(report.changed_summaries, 1);
    assert_eq!(stored.cost, expected_review_cost(end));
}

#[test]
fn invalid_applied_ruleset_metadata_fails_closed() {
    let (store, source) = store_with_source("/tmp/codex-invalid-ruleset-meta");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let event = test_event(
        &source,
        started_at,
        "legacy-review",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    store.insert_event(&event).expect("insert");
    store
        .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "not-a-number")
        .expect("write invalid metadata");
    let payload_before =
        serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize");

    let error = store
        .ensure_current_pricing()
        .expect_err("invalid metadata must fail");

    assert!(error
        .to_string()
        .contains("invalid pricing.applied_ruleset_version"));
    assert_eq!(
        serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize"),
        payload_before
    );
}

#[test]
fn repriced_task_buckets_are_marked_dirty_for_incremental_sync() {
    let (store, source) = store_with_source("/tmp/codex-task-bucket-dirty");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let event = test_event(
        &source,
        started_at,
        "legacy-review",
        "codex-auto-review",
        million_token_usage(),
        missing_cost(),
    );
    store.insert_event(&event).expect("insert");
    store
        .upsert_task_spans(&[test_span(
            &source,
            started_at,
            "span",
            vec![event.event_id.clone()],
            Some(0),
            Some(0),
        )])
        .expect("span");
    store
        .rebuild_task_work_items_for_project_buckets(&BTreeSet::from(["none".to_string()]))
        .expect("seed work items");
    let snapshots = store
        .pending_task_bucket_snapshots_for_sync(
            "http",
            "https://example.invalid/api/sync/batches",
            "device",
            true,
            None,
        )
        .expect("initial snapshots");
    assert!(!snapshots.is_empty());
    store
        .record_task_bucket_snapshots_synced(
            "http",
            "https://example.invalid/api/sync/batches",
            "device",
            &snapshots,
        )
        .expect("mark synced");
    assert!(store
        .pending_task_bucket_snapshots_for_sync(
            "http",
            "https://example.invalid/api/sync/batches",
            "device",
            false,
            None,
        )
        .expect("clean pending")
        .is_empty());

    let report = store.ensure_current_pricing().expect("reprice");
    assert_eq!(report.changed_task_spans, 1);
    assert!(report.rebuilt_work_items > 0);
    let pending = store
        .pending_task_bucket_snapshots_for_sync(
            "http",
            "https://example.invalid/api/sync/batches",
            "device",
            false,
            None,
        )
        .expect("dirty pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].project_bucket, "none");
    let work_items = store.work_items().expect("work items");
    assert_eq!(
        work_items[0].estimated_cost_usd,
        expected_review_cost(started_at).estimated_api_equivalent_usd
    );
}

#[test]
fn representative_fixture_streams_events_instead_of_loading_the_whole_store() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("large.sqlite");
    let store = Store::open(&path).expect("open");
    let source = test_source("/tmp/codex-large-reprice");
    store.upsert_source(&source).expect("source");
    let started_at = parse_utc("2026-07-29T12:00:00Z");
    let events = (0..512)
        .map(|index| {
            test_event(
                &source,
                started_at + chrono::Duration::seconds(index),
                &format!("event-{index}"),
                "codex-auto-review",
                million_token_usage(),
                missing_cost(),
            )
        })
        .collect::<Vec<_>>();
    store.insert_events(&events).expect("insert");

    let started = std::time::Instant::now();
    let report = store.ensure_current_pricing().expect("reprice");
    let elapsed = started.elapsed();

    assert_eq!(report.examined_events, 512);
    assert_eq!(report.changed_events, 512);
    assert!(
        elapsed.as_secs() < 30,
        "repricing 512 events should stay well under 30s, took {elapsed:?}"
    );
    eprintln!(
        "representative fixture: examined={} changed={} elapsed={elapsed:?}",
        report.examined_events, report.changed_events
    );
}

#[test]
fn read_only_applied_version_probe_does_not_open_or_reprice() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("probe.sqlite");
    assert_eq!(
        crate::database_applied_pricing_ruleset_version(&path).expect("missing"),
        None
    );
    let store = Store::open(&path).expect("create");
    assert_eq!(
        crate::database_applied_pricing_ruleset_version(&path).expect("legacy"),
        None
    );
    store
        .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "7")
        .expect("write");
    drop(store);
    assert_eq!(
        crate::database_applied_pricing_ruleset_version(&path).expect("present"),
        Some(7)
    );
}
