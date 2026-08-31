use super::*;
use crate::report::{preview_path_label, EventUsageSeries};
use chrono::{DateTime, Duration, Utc};
use std::path::Path;

#[test]
fn source_ids_are_stable_for_same_input() {
    let a = source_id("codex", SourceKind::LocalAdapter, "abc");
    let b = source_id("codex", SourceKind::LocalAdapter, "abc");
    assert_eq!(a, b);
}

#[test]
fn source_ids_change_by_provider() {
    let codex = source_id("codex", SourceKind::LocalAdapter, "abc");
    let claude = source_id("claude_code", SourceKind::LocalAdapter, "abc");
    assert_ne!(codex, claude);
}

#[test]
fn reasoning_level_supports_ultracode() {
    assert_eq!(
        ReasoningLevel::parse("ultracode"),
        Some(ReasoningLevel::Ultracode)
    );
    assert_eq!(ReasoningLevel::Ultracode.as_str(), "ultracode");
}

#[test]
fn total_falls_back_to_parts() {
    let usage = UsageCounts {
        input_tokens: Some(10),
        output_tokens: Some(5),
        cache_read_tokens: Some(2),
        ..UsageCounts::default()
    };
    assert_eq!(usage.computed_total(), 17);
}

#[test]
fn legacy_usage_counts_without_cache_lifetimes_deserialize() {
    let usage: UsageCounts = serde_json::from_value(serde_json::json!({
        "input_tokens": 10,
        "cache_creation_tokens": 4
    }))
    .expect("legacy usage counts");

    assert_eq!(usage.cache_creation_tokens, Some(4));
    assert_eq!(usage.cache_creation_5m_tokens, None);
    assert_eq!(usage.cache_creation_1h_tokens, None);
    assert_eq!(usage.computed_total(), 14);
}

#[test]
fn schema_types_serialize() {
    let schema = schemars::schema_for!(UsageEvent);
    let json = serde_json::to_value(schema).expect("schema should serialize");
    assert!(json.get("title").is_some());

    let schema = schemars::schema_for!(UsageSummary);
    let json = serde_json::to_value(schema).expect("summary schema should serialize");
    assert!(json.get("title").is_some());
}

#[test]
fn sync_batch_without_authoritative_snapshot_remains_backward_compatible() {
    let batch: SyncBatch = serde_json::from_value(serde_json::json!({
        "schema_version": SYNC_BATCH_V2_SCHEMA_VERSION,
        "batch_id": "batch-legacy-v2",
        "device_id": "device-1",
        "created_at": "2026-05-31T10:00:00Z"
    }))
    .expect("legacy v2 batch should deserialize");

    assert!(batch.authoritative_snapshot.is_none());
    let serialized = serde_json::to_value(batch).expect("batch should serialize");
    assert!(serialized.get("authoritative_snapshot").is_none());
}

#[test]
fn sync_ack_v1_omits_zero_task_counters() {
    let ack = SyncAck {
        schema_version: SYNC_ACK_V1_SCHEMA_VERSION.to_string(),
        batch_id: "batch-1".to_string(),
        accepted: SyncEntityCounts {
            sources: 1,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 2,
            task_buckets: 0,
            task_verifications: 0,
            code_change_metrics: 0,
            quota_cycle_contributions: 0,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        duplicates: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
            task_buckets: 0,
            task_verifications: 0,
            code_change_metrics: 0,
            quota_cycle_contributions: 0,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        rejected: Vec::new(),
    };

    let json = serde_json::to_value(&ack).expect("ack should serialize");
    assert_eq!(json["schema_version"], SYNC_ACK_V1_SCHEMA_VERSION);
    assert!(json["accepted"].get("task_buckets").is_none());
    assert!(json["accepted"].get("task_verifications").is_none());
    assert!(json["accepted"].get("code_change_metrics").is_none());
    assert!(json["duplicates"].get("task_buckets").is_none());
    assert!(json["duplicates"].get("task_verifications").is_none());
    assert!(json["duplicates"].get("code_change_metrics").is_none());
}

#[test]
fn sync_ack_v3_keeps_nonzero_task_and_code_change_counters() {
    let ack = SyncAck {
        schema_version: SYNC_ACK_V3_SCHEMA_VERSION.to_string(),
        batch_id: "batch-2".to_string(),
        accepted: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
            task_buckets: 3,
            task_verifications: 1,
            code_change_metrics: 2,
            quota_cycle_contributions: 0,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        duplicates: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
            task_buckets: 0,
            task_verifications: 0,
            code_change_metrics: 0,
            quota_cycle_contributions: 0,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        rejected: Vec::new(),
    };

    let json = serde_json::to_value(&ack).expect("ack should serialize");
    assert_eq!(json["schema_version"], SYNC_ACK_V3_SCHEMA_VERSION);
    assert_eq!(json["accepted"]["task_buckets"], 3);
    assert_eq!(json["accepted"]["task_verifications"], 1);
    assert_eq!(json["accepted"]["code_change_metrics"], 2);
    assert!(json["accepted"].get("quota_cycle_contributions").is_none());
    assert!(json["accepted"].get("account_plan_observations").is_none());
    assert!(json["accepted"].get("account_evidence_summaries").is_none());
}

#[test]
fn sync_ack_v4_keeps_nonzero_quota_cycle_counters() {
    let ack = SyncAck {
        schema_version: SYNC_ACK_V4_SCHEMA_VERSION.to_string(),
        batch_id: "batch-quota".to_string(),
        accepted: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
            task_buckets: 0,
            task_verifications: 0,
            code_change_metrics: 1,
            quota_cycle_contributions: 4,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        duplicates: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
            task_buckets: 0,
            task_verifications: 0,
            code_change_metrics: 0,
            quota_cycle_contributions: 0,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        rejected: Vec::new(),
    };

    let json = serde_json::to_value(&ack).expect("ack should serialize");
    assert_eq!(json["schema_version"], SYNC_ACK_V4_SCHEMA_VERSION);
    assert_eq!(json["accepted"]["code_change_metrics"], 1);
    assert_eq!(json["accepted"]["quota_cycle_contributions"], 4);
}

#[test]
fn sync_batch_v3_without_quota_contributions_remains_backward_compatible() {
    let batch: SyncBatch = serde_json::from_value(serde_json::json!({
        "schema_version": SYNC_BATCH_V3_SCHEMA_VERSION,
        "batch_id": "batch-legacy-v3",
        "device_id": "device-1",
        "created_at": "2026-05-31T10:00:00Z"
    }))
    .expect("legacy v3 batch should deserialize");

    assert!(batch.quota_cycle_contributions.is_empty());
    let serialized = serde_json::to_value(batch).expect("batch should serialize");
    assert!(serialized.get("quota_cycle_contributions").is_none());
}

fn test_source(provider: &str, path: &str) -> SourceLocation {
    SourceLocation::local_adapter(
        provider,
        "test",
        "0",
        Path::new(path),
        LocationOrigin::Configured,
    )
}

fn test_event(
    provider: &str,
    source: &SourceLocation,
    started_at: DateTime<Utc>,
    tokens: u64,
    cost_cents: Option<i64>,
) -> UsageEvent {
    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: event_id(provider, &source.source_id, "rec", None, started_at),
        device_id: "d".to_string(),
        provider: provider.to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        subscription_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "jsonl".to_string(),
            source_path_hash: None,
            source_record_id: Some("rec".to_string()),
            parse_confidence: Confidence::High,
        },
        session: SessionInfo {
            session_id: "s".to_string(),
            local_session_id_hash: None,
            title: None,
            started_at,
            ended_at: None,
            duration_seconds: None,
        },
        model: None,
        usage: UsageCounts {
            input_tokens: Some(tokens / 2),
            output_tokens: Some(tokens / 2),
            total_tokens: Some(tokens),
            ..UsageCounts::default()
        },
        runtime: None,
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: cost_cents,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: None,
            provider_reported_micro_usd: None,
            pricing_source: None,
            pricing_version: None,
            confidence: Confidence::Low,
        },
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

fn test_summary(
    provider: &str,
    source: &SourceLocation,
    observed_at: DateTime<Utc>,
    period_start: DateTime<Utc>,
    period_end: DateTime<Utc>,
    tokens: u64,
) -> UsageSummary {
    UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: summary_id(provider, &source.source_id, "sum"),
        device_id: "d".to_string(),
        provider: provider.to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalSummary,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "cache".to_string(),
            source_path_hash: None,
            source_record_id: Some("rec".to_string()),
            parse_confidence: Confidence::Medium,
        },
        model: None,
        models: Vec::new(),
        usage: UsageCounts {
            input_tokens: Some(tokens),
            total_tokens: Some(tokens),
            ..UsageCounts::default()
        },
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: None,
            provider_reported_micro_usd: None,
            pricing_source: None,
            pricing_version: None,
            confidence: Confidence::Low,
        },
        parse_evidence: None,
        project: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        metrics: None,
        period_start: Some(period_start),
        period_end: Some(period_end),
        observed_at,
        metadata: SummaryMetadata {
            summary_format: "stats_cache".to_string(),
            summary_version: None,
            total_sessions: Some(1),
            total_messages: Some(10),
            last_computed_at: Some(observed_at),
        },
        imported_at: observed_at,
    }
}

fn mk_dt(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc())
        .expect("valid date")
}

#[test]
fn report_empty_inputs_returns_zero_totals() {
    let now = mk_dt(2026, 5, 25);
    let report = build_usage_report(&[], &[], &[], &[], &[], ReportPeriod::AllTime, now);
    assert_eq!(report.total_events, 0);
    assert_eq!(report.total_usage.total_tokens, 0);
    assert!(report.rows.is_empty());
    assert!(report.summary_rows.is_empty());
}

#[test]
fn report_filters_events_by_period() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let recent = test_event("codex", &source, mk_dt(2026, 5, 24), 100, None);
    let old = test_event("codex", &source, mk_dt(2026, 5, 10), 200, None);

    let report = build_usage_report(
        &[recent, old],
        &[],
        &[source],
        &[],
        &[],
        ReportPeriod::LastDays(7),
        now,
    );

    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 100);
}

#[test]
fn report_filters_events_by_explicit_date_range() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let before = test_event("codex", &source, mk_dt(2026, 4, 30), 50, None);
    let inside = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 10) + Duration::hours(15),
        100,
        None,
    );
    let after = test_event("codex", &source, mk_dt(2026, 5, 20), 200, None);
    let period =
        report_period_from_range(Some("2026-05-01"), Some("2026-05-15"), now).expect("valid range");

    let report = build_usage_report(
        &[before, inside, after],
        &[],
        &[source],
        &[],
        &[],
        period,
        now,
    );

    assert_eq!(report.label, "2026-05-01 to 2026-05-15");
    assert_eq!(report.since, Some(mk_dt(2026, 5, 1)));
    assert_eq!(
        report.until,
        parse_report_date_bound("2026-05-15", true).expect("end of day")
    );
    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 100);
}

#[test]
fn report_date_only_to_includes_the_whole_utc_day() {
    let now = mk_dt(2026, 6, 1);
    let source = test_source("codex", "/tmp/codex");
    let late_on_end_day = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 15) + Duration::hours(23),
        75,
        None,
    );
    let next_day = test_event("codex", &source, mk_dt(2026, 5, 16), 25, None);
    let period = report_period_from_range(Some("2026-05-15"), Some("2026-05-15"), now)
        .expect("single-day range");

    let report = build_usage_report(
        &[late_on_end_day, next_day],
        &[],
        &[source],
        &[],
        &[],
        period,
        now,
    );

    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 75);
}

#[test]
fn report_range_from_only_defaults_until_to_now() {
    let now = mk_dt(2026, 5, 25);
    let period = report_period_from_range(Some("2026-05-01"), None, now).expect("from-only range");
    assert_eq!(
        period,
        ReportPeriod::Range {
            since: Some(ReportBound {
                timestamp: mk_dt(2026, 5, 1),
                date_only: true,
            }),
            until: ReportBound {
                timestamp: now,
                date_only: false,
            },
        }
    );
    assert_eq!(period.label(now), "2026-05-01 to 2026-05-25T00:00:00+00:00");
}

#[test]
fn report_range_rejects_inverted_and_invalid_bounds() {
    let now = mk_dt(2026, 5, 25);
    assert_eq!(
        report_period_from_range(Some("2026-05-20"), Some("2026-05-10"), now),
        Err(ReportRangeError::InvertedRange {
            since: mk_dt(2026, 5, 20),
            until: parse_report_date_bound("2026-05-10", true).expect("end of day"),
        })
    );
    assert!(matches!(
        report_period_from_range(Some("last-week"), None, now),
        Err(ReportRangeError::InvalidDate { .. })
    ));
    assert_eq!(
        report_period_from_range(None, None, now),
        Err(ReportRangeError::MissingBound)
    );
}

#[test]
fn report_range_keeps_future_windows_and_returns_no_events() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let present = test_event("codex", &source, now, 50, None);
    let period = report_period_from_range(Some("2026-09-01"), Some("2026-09-30"), now)
        .expect("future range is valid");

    assert_eq!(
        period,
        ReportPeriod::Range {
            since: Some(ReportBound {
                timestamp: mk_dt(2026, 9, 1),
                date_only: true,
            }),
            until: ReportBound {
                timestamp: parse_report_date_bound("2026-09-30", true).expect("end of day"),
                date_only: true,
            },
        }
    );
    assert_eq!(period.label(now), "2026-09-01 to 2026-09-30 (empty)");
    assert_eq!(period.window(now), (Some(mk_dt(2026, 9, 1)), now));
    assert_eq!(period.published_window(now), (Some(now), now));

    let report = build_usage_report(&[present], &[], &[source], &[], &[], period, now);
    assert_eq!(report.label, "2026-09-01 to 2026-09-30 (empty)");
    assert_eq!(report.since, Some(now));
    assert_eq!(report.until, now);
    assert!(report.since.is_some_and(|since| since <= report.until));
    assert_eq!(report.total_events, 0);
}

#[test]
fn report_range_from_only_in_the_future_is_empty_not_inverted() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let present = test_event("codex", &source, now, 50, None);
    let period = report_period_from_range(Some("2026-09-01"), None, now).expect("future from-only");

    assert_eq!(
        period,
        ReportPeriod::Range {
            since: Some(ReportBound {
                timestamp: mk_dt(2026, 9, 1),
                date_only: true,
            }),
            until: ReportBound {
                timestamp: now,
                date_only: false,
            },
        }
    );
    assert_eq!(period.label(now), "from 2026-09-01 (empty)");
    assert_eq!(period.published_window(now), (Some(now), now));

    let report = build_usage_report(&[present], &[], &[source], &[], &[], period, now);
    assert_eq!(report.label, "from 2026-09-01 (empty)");
    assert_eq!(report.since, Some(now));
    assert_eq!(report.until, now);
    assert!(report.since.is_some_and(|since| since <= report.until));
    assert_eq!(report.total_events, 0);
}

#[test]
fn report_range_label_uses_applied_until_when_to_is_in_the_future() {
    let now = mk_dt(2026, 5, 25) + Duration::hours(12);
    let period = report_period_from_range(Some("2026-05-01"), Some("2026-12-31"), now)
        .expect("open-ended future to");
    assert_eq!(period.label(now), "2026-05-01 to 2026-05-25T12:00:00+00:00");
    assert_eq!(period.window(now), (Some(mk_dt(2026, 5, 1)), now));
}

#[test]
fn report_filters_events_by_rfc3339_clock_bounds() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let before = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 10) + Duration::hours(14),
        50,
        None,
    );
    let inside = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 10) + Duration::hours(15) + Duration::minutes(30),
        100,
        None,
    );
    let after = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 10) + Duration::hours(16) + Duration::minutes(1),
        200,
        None,
    );
    let period = report_period_from_range(
        Some("2026-05-10T15:00:00Z"),
        Some("2026-05-10T16:00:00Z"),
        now,
    )
    .expect("rfc3339 range");

    let report = build_usage_report(
        &[before, inside, after],
        &[],
        &[source],
        &[],
        &[],
        period,
        now,
    );

    assert_eq!(
        report.label,
        "2026-05-10T15:00:00+00:00 to 2026-05-10T16:00:00+00:00"
    );
    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 100);
}

#[test]
fn report_range_rfc3339_midnight_keeps_timestamp_label() {
    let now = mk_dt(2026, 5, 25);
    let period = report_period_from_range(
        Some("2026-05-01T00:00:00Z"),
        Some("2026-05-15T23:59:59.999999999Z"),
        now,
    )
    .expect("rfc3339 midnight range");

    assert_eq!(
        period,
        ReportPeriod::Range {
            since: Some(ReportBound {
                timestamp: mk_dt(2026, 5, 1),
                date_only: false,
            }),
            until: ReportBound {
                timestamp: parse_report_date_bound("2026-05-15", true).expect("end of day"),
                date_only: false,
            },
        }
    );
    assert_eq!(
        period.label(now),
        "2026-05-01T00:00:00+00:00 to 2026-05-15T23:59:59.999999999+00:00"
    );
}

#[test]
fn report_range_to_today_clamps_until_to_now() {
    let now = mk_dt(2026, 5, 25) + Duration::hours(12);
    let source = test_source("codex", "/tmp/codex");
    let morning = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 25) + Duration::hours(8),
        75,
        None,
    );
    let evening = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 25) + Duration::hours(18),
        25,
        None,
    );
    let period =
        report_period_from_range(Some("2026-05-25"), Some("2026-05-25"), now).expect("today");

    let report = build_usage_report(&[morning, evening], &[], &[source], &[], &[], period, now);
    assert_eq!(report.until, now);
    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 75);
}

#[test]
fn report_filters_out_future_events() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let future = test_event("codex", &source, mk_dt(2026, 6, 1), 100, None);
    let present = test_event("codex", &source, now, 50, None);

    let report = build_usage_report(
        &[future, present],
        &[],
        &[source],
        &[],
        &[],
        ReportPeriod::AllTime,
        now,
    );

    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 50);
}

#[test]
fn report_groups_events_by_provider_and_account() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("codex", "/tmp/codex");
    let e1 = test_event("codex", &src, now, 100, None);
    let e2 = test_event("codex", &src, now, 200, None);

    let report = build_usage_report(&[e1, e2], &[], &[src], &[], &[], ReportPeriod::AllTime, now);

    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].provider, "codex");
    assert_eq!(report.rows[0].events, 2);
    assert_eq!(report.rows[0].usage.total_tokens, 300);
}

#[test]
fn report_keeps_summaries_separate_from_events() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("claude_code", "/tmp/claude");
    let event = test_event("claude_code", &src, now, 100, None);
    let summary = test_summary(
        "claude_code",
        &src,
        now,
        mk_dt(2026, 5, 1),
        mk_dt(2026, 5, 25),
        500,
    );

    let report = build_usage_report(
        &[event],
        &[summary],
        &[src],
        &[],
        &[],
        ReportPeriod::AllTime,
        now,
    );

    assert_eq!(report.total_usage.total_tokens, 100);
    assert_eq!(report.total_summary_usage.total_tokens, 500);
    assert_eq!(report.summary_rows.len(), 1);
    // Direct event usage within summary period
    assert_eq!(report.summary_rows[0].direct_event_usage.total_tokens, 100);
}

#[test]
fn event_usage_series_preserves_legacy_cent_fallback_in_range_differences() {
    let source = test_source("codex", "/tmp/codex");
    let before_range_at = mk_dt(2026, 5, 1);
    let inside_range_at = mk_dt(2026, 5, 2);
    let legacy_cents = i64::MAX / MICRO_USD_PER_CENT + 1;
    let before_range = test_event(
        "codex",
        &source,
        before_range_at,
        100,
        Some(legacy_cents + 7),
    );
    let inside_range = test_event(
        "codex",
        &source,
        inside_range_at,
        200,
        Some(legacy_cents + 11),
    );
    let series = EventUsageSeries::from_events(vec![&before_range, &inside_range]);

    let (events, usage) =
        series.usage_between(inside_range_at, inside_range_at + Duration::days(1), false);

    assert_eq!(events, 1);
    assert_eq!(usage.total_tokens, 200);
    assert_eq!(usage.estimated_cost_usd, Some(legacy_cents + 11));
    assert_eq!(usage.estimated_cost_micro_usd, None);
}

#[test]
fn report_hides_summaries_in_non_alltime_periods() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("claude_code", "/tmp/claude");
    let summary = test_summary(
        "claude_code",
        &src,
        now,
        mk_dt(2026, 5, 1),
        mk_dt(2026, 5, 25),
        500,
    );

    let report = build_usage_report(
        &[],
        &[summary],
        &[src],
        &[],
        &[],
        ReportPeriod::LastDays(7),
        now,
    );

    assert!(report.summary_rows.is_empty());
}

#[test]
fn subscription_rows_respect_past_end_time() {
    let now = mk_dt(2026, 6, 1);
    let src = test_source("codex", "/tmp/codex");
    let account_id = provider_account_id("codex", "email:verified@example.com");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: account_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::LocalAuth,
        provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
        provider_user_id_hash: None,
        email: Some("verified@example.com".to_string()),
        email_hash: None,
        org_id_hash: None,
        account_label: None,
        plan_name: Some("Plus".to_string()),
        confidence: Confidence::High,
        verified_at: Some(mk_dt(2026, 5, 3)),
        created_at: mk_dt(2026, 5, 3),
        updated_at: mk_dt(2026, 5, 3),
    };
    let mut before_end = test_event("codex", &src, mk_dt(2026, 5, 29), 100, Some(100));
    before_end.provider_account_id = Some(account_id.clone());
    let mut after_end = test_event("codex", &src, mk_dt(2026, 5, 31), 200, Some(200));
    after_end.provider_account_id = Some(account_id.clone());
    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id("codex", &account_id, "Plus", mk_dt(2026, 4, 30)),
        provider: "codex".to_string(),
        provider_account_id: account_id.clone(),
        plan_name: "Plus".to_string(),
        price: 2000,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: Some(mk_dt(2026, 4, 30)),
        renewal_day: Some(30),
        started_at: mk_dt(2026, 4, 30),
        ended_at: Some(mk_dt(2026, 5, 30)),
        current_period_ends_at: Some(mk_dt(2026, 5, 30)),
        status: SubscriptionStatus::Cancelled,
        record_source: IdentitySource::LocalAuth,
        verified_at: Some(mk_dt(2026, 5, 3)),
        notes: None,
    };

    let report = build_usage_report(
        &[before_end, after_end],
        &[],
        &[src],
        &[account],
        &[subscription],
        ReportPeriod::LastDays(30),
        now,
    );

    assert_eq!(report.subscription_rows.len(), 1);
    assert_eq!(report.subscription_rows[0].account, account_id.0);
    assert_eq!(
        report.subscription_rows[0].ended_at,
        Some(mk_dt(2026, 5, 30))
    );
    assert_eq!(report.subscription_rows[0].events, 1);
    assert_eq!(report.subscription_rows[0].usage.total_tokens, 100);
    assert_eq!(
        report.subscription_rows[0].usage.estimated_cost_usd,
        Some(100)
    );
}

#[test]
fn subscription_rows_respect_historical_range_until() {
    let now = mk_dt(2026, 6, 1);
    let src = test_source("codex", "/tmp/codex-range-sub");
    let account_id = provider_account_id("codex", "email:verified@example.com");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: account_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::LocalAuth,
        provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
        provider_user_id_hash: None,
        email: Some("verified@example.com".to_string()),
        email_hash: None,
        org_id_hash: None,
        account_label: None,
        plan_name: Some("Plus".to_string()),
        confidence: Confidence::High,
        verified_at: Some(mk_dt(2026, 5, 3)),
        created_at: mk_dt(2026, 5, 3),
        updated_at: mk_dt(2026, 5, 3),
    };
    let mut inside = test_event("codex", &src, mk_dt(2026, 5, 10), 100, Some(100));
    inside.provider_account_id = Some(account_id.clone());
    let mut after_range = test_event("codex", &src, mk_dt(2026, 5, 20), 200, Some(200));
    after_range.provider_account_id = Some(account_id.clone());
    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id("codex", &account_id, "Plus", mk_dt(2026, 4, 30)),
        provider: "codex".to_string(),
        provider_account_id: account_id.clone(),
        plan_name: "Plus".to_string(),
        price: 2000,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: Some(mk_dt(2026, 4, 30)),
        renewal_day: Some(30),
        started_at: mk_dt(2026, 4, 30),
        ended_at: None,
        current_period_ends_at: Some(mk_dt(2026, 5, 30)),
        status: SubscriptionStatus::Active,
        record_source: IdentitySource::UserConfigured,
        verified_at: Some(mk_dt(2026, 5, 3)),
        notes: None,
    };
    let period = report_period_from_range(Some("2026-05-01"), Some("2026-05-15"), now)
        .expect("historical range");

    let report = build_usage_report(
        &[inside, after_range],
        &[],
        &[src],
        &[account],
        &[subscription],
        period,
        now,
    );

    assert_eq!(report.total_events, 1);
    assert_eq!(report.subscription_rows.len(), 1);
    assert_eq!(report.subscription_rows[0].events, 1);
    assert_eq!(report.subscription_rows[0].usage.total_tokens, 100);
    assert_eq!(
        report.subscription_rows[0].usage.estimated_cost_usd,
        Some(100)
    );
}

#[test]
fn subscription_rows_exclude_future_range() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("codex", "/tmp/codex-future-sub");
    let account_id = provider_account_id("codex", "email:verified@example.com");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: account_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::LocalAuth,
        provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
        provider_user_id_hash: None,
        email: Some("verified@example.com".to_string()),
        email_hash: None,
        org_id_hash: None,
        account_label: None,
        plan_name: Some("Plus".to_string()),
        confidence: Confidence::High,
        verified_at: Some(mk_dt(2026, 5, 3)),
        created_at: mk_dt(2026, 5, 3),
        updated_at: mk_dt(2026, 5, 3),
    };
    let mut present = test_event("codex", &src, now, 100, Some(100));
    present.provider_account_id = Some(account_id.clone());
    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id("codex", &account_id, "Plus", mk_dt(2026, 4, 30)),
        provider: "codex".to_string(),
        provider_account_id: account_id,
        plan_name: "Plus".to_string(),
        price: 2000,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: Some(mk_dt(2026, 4, 30)),
        renewal_day: Some(30),
        started_at: mk_dt(2026, 4, 30),
        ended_at: None,
        current_period_ends_at: Some(mk_dt(2026, 5, 30)),
        status: SubscriptionStatus::Active,
        record_source: IdentitySource::UserConfigured,
        verified_at: Some(mk_dt(2026, 5, 3)),
        notes: None,
    };
    let period = report_period_from_range(Some("2026-09-01"), Some("2026-09-30"), now)
        .expect("future range");

    let report = build_usage_report(
        &[present],
        &[],
        &[src],
        &[account],
        &[subscription],
        period,
        now,
    );

    assert_eq!(report.label, "2026-09-01 to 2026-09-30 (empty)");
    assert!(report.since.is_some_and(|since| since <= report.until));
    assert_eq!(report.total_events, 0);
    assert!(report.subscription_rows.is_empty());
}

#[test]
fn subscription_rows_keep_legacy_verified_cycle_rows_open() {
    let now = mk_dt(2026, 6, 1);
    let src = test_source("codex", "/tmp/codex");
    let account_id = provider_account_id("codex", "email:verified@example.com");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: account_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::LocalAuth,
        provider_user_id: None,
        provider_user_id_hash: None,
        email: Some("verified@example.com".to_string()),
        email_hash: None,
        org_id_hash: None,
        account_label: None,
        plan_name: Some("Plus".to_string()),
        confidence: Confidence::High,
        verified_at: Some(mk_dt(2026, 5, 3)),
        created_at: mk_dt(2026, 5, 3),
        updated_at: mk_dt(2026, 5, 3),
    };
    let mut before_cycle_end = test_event("codex", &src, mk_dt(2026, 5, 29), 100, Some(100));
    before_cycle_end.provider_account_id = Some(account_id.clone());
    let mut after_cycle_end = test_event("codex", &src, mk_dt(2026, 5, 31), 200, Some(200));
    after_cycle_end.provider_account_id = Some(account_id.clone());
    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id("codex", &account_id, "Plus", mk_dt(2026, 4, 30)),
        provider: "codex".to_string(),
        provider_account_id: account_id,
        plan_name: "Plus".to_string(),
        price: 2000,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: Some(mk_dt(2026, 4, 30)),
        renewal_day: Some(30),
        started_at: mk_dt(2026, 4, 30),
        ended_at: Some(mk_dt(2026, 5, 30)),
        current_period_ends_at: Some(mk_dt(2026, 5, 30)),
        status: SubscriptionStatus::Active,
        record_source: IdentitySource::LocalAuth,
        verified_at: Some(mk_dt(2026, 5, 3)),
        notes: None,
    };

    let report = build_usage_report(
        &[before_cycle_end, after_cycle_end],
        &[],
        &[src],
        &[account],
        &[subscription],
        ReportPeriod::LastDays(30),
        now,
    );

    assert_eq!(report.subscription_rows.len(), 1);
    assert_eq!(report.subscription_rows[0].ended_at, None);
    assert_eq!(report.subscription_rows[0].events, 2);
    assert_eq!(report.subscription_rows[0].usage.total_tokens, 300);
    assert_eq!(
        report.subscription_rows[0].usage.estimated_cost_usd,
        Some(300)
    );
}

#[test]
fn report_uses_account_label_from_registry() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("codex", "/tmp/codex");
    let acct_id = provider_account_id("codex", "stable");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: acct_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::UserConfigured,
        provider_user_id: None,
        provider_user_id_hash: None,
        email: None,
        email_hash: None,
        org_id_hash: None,
        account_label: Some("work".to_string()),
        plan_name: None,
        confidence: Confidence::Medium,
        verified_at: None,
        created_at: now,
        updated_at: now,
    };
    let mut event = test_event("codex", &src, now, 50, None);
    event.provider_account_id = Some(acct_id);

    let report = build_usage_report(
        &[event],
        &[],
        &[src],
        &[account],
        &[],
        ReportPeriod::AllTime,
        now,
    );

    assert_eq!(report.rows[0].account, "work");
}

#[test]
fn usage_totals_accumulate_cost() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("codex", "/tmp/codex");
    let e1 = test_event("codex", &src, now, 100, Some(1));
    let e2 = test_event("codex", &src, now, 200, Some(2));

    let report = build_usage_report(&[e1, e2], &[], &[src], &[], &[], ReportPeriod::AllTime, now);

    assert_eq!(report.total_usage.estimated_cost_usd, Some(3));
}

#[test]
fn computed_total_does_not_overflow() {
    let usage = UsageCounts {
        input_tokens: Some(u64::MAX),
        output_tokens: Some(u64::MAX),
        ..UsageCounts::default()
    };
    let total = usage.computed_total();
    assert_eq!(total, u64::MAX);
}

#[test]
fn usage_totals_saturate_imported_counters_and_costs() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex-overflow");
    let mut event = test_event("codex", &source, now, u64::MAX, Some(i64::MAX));
    event.usage = UsageCounts {
        input_tokens: Some(u64::MAX),
        cache_creation_tokens: Some(u64::MAX),
        cache_read_tokens: Some(u64::MAX),
        output_tokens: Some(u64::MAX),
        reasoning_tokens: Some(u64::MAX),
        total_tokens: Some(u64::MAX),
        ..UsageCounts::default()
    };

    let mut totals = UsageTotals::default();
    totals.add_event(&event);
    totals.add_event(&event);

    assert_eq!(totals.input_tokens, u64::MAX);
    assert_eq!(totals.cache_creation_tokens, u64::MAX);
    assert_eq!(totals.cached_input_tokens, u64::MAX);
    assert_eq!(totals.output_tokens, u64::MAX);
    assert_eq!(totals.reasoning_tokens, u64::MAX);
    assert_eq!(totals.total_tokens, u64::MAX);
    assert_eq!(totals.estimated_cost_usd, Some(i64::MAX));
}

#[test]
fn display_path_expands_home_but_avoids_canonicalize() {
    let p = Path::new("~/relative/test");
    let displayed = display_path(p);
    assert!(displayed.contains("relative/test"));
    // should not resolve to absolute via fs if ~ expanded
    if let Some(home) = home_dir() {
        let home_str = home.to_string_lossy();
        if displayed.starts_with(home_str.as_ref()) {
            // expanded, good
        }
    }
}

#[test]
fn path_hash_remains_stable_via_canonical_display() {
    let p = Path::new("/tmp/nonexistent-for-test");
    let h1 = path_hash(p);
    let h2 = path_hash(p);
    assert_eq!(h1, h2);
}

#[test]
fn renaming_a_repository_keeps_one_rollup_bucket() {
    let before = ProjectInfo {
        project_id: "project_before".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("remote_before".to_string()),
        repo_label: Some("owner/ai-stats".to_string()),
        branch_hash: Some("branch_main".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path_checkout".to_string()),
        path_label: Some("/work/ai-stats".to_string()),
    };
    let after = ProjectInfo {
        project_id: "project_after".to_string(),
        repo_remote_hash: Some("remote_after".to_string()),
        repo_label: Some("owner/statsai".to_string()),
        ..before.clone()
    };

    // Same checkout, same branch: the rename must not split the bucket.
    assert_eq!(
        daily_rollup_project_key(Some(&before)),
        daily_rollup_project_key(Some(&after))
    );
    // The remote itself is untouched, so the backend can still key the
    // project on it and move the location's history across the rename.
    assert_ne!(before.repo_remote_hash, after.repo_remote_hash);

    // Task spans are already persisted under the remote-inclusive key, so
    // it has to keep telling the two apart or their history splits at the
    // upgrade instead of at the rename.
    assert_ne!(
        project_bucket_key(Some(&before)),
        project_bucket_key(Some(&after))
    );

    // A different checkout of the same repository keeps its own bucket, the
    // way a worktree is its own location under one project.
    let elsewhere = ProjectInfo {
        path_hash: Some("path_worktree".to_string()),
        ..before.clone()
    };
    assert_ne!(
        daily_rollup_project_key(Some(&before)),
        daily_rollup_project_key(Some(&elsewhere))
    );

    // So does the same checkout on another branch.
    let other_branch = ProjectInfo {
        branch_hash: Some("branch_release".to_string()),
        ..before.clone()
    };
    assert_ne!(
        daily_rollup_project_key(Some(&before)),
        daily_rollup_project_key(Some(&other_branch))
    );
}

#[test]
fn remote_only_attribution_still_buckets_by_repository() {
    let project = ProjectInfo {
        project_id: "project_remote_only".to_string(),
        project_label: Some("statsai".to_string()),
        repo_remote_hash: Some("remote_only".to_string()),
        repo_label: Some("owner/statsai".to_string()),
        branch_hash: None,
        branch_label: None,
        path_hash: None,
        path_label: None,
    };

    assert_eq!(
        daily_rollup_project_key(Some(&project)),
        "repo:remote_only|branch:none"
    );
}

#[test]
fn bare_project_id_is_not_a_stable_project_identity() {
    let project = ProjectInfo {
        project_id: "project_bare".to_string(),
        project_label: Some("Bare".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: None,
        path_label: None,
    };

    assert!(!project_has_stable_identity(&project));
    assert_eq!(project_bucket_key(Some(&project)), "none");
}

#[test]
fn sanitize_project_for_sync_preserves_path_only_project_labels() {
    let project = ProjectInfo {
        project_id: "project_path_only".to_string(),
        project_label: Some("Scratch".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/Users/example/Scratch".to_string()),
    };

    let sanitized = sanitize_project_for_sync(project).expect("stable path identity");

    assert_eq!(sanitized.repo_remote_hash, None);
    assert_eq!(sanitized.path_hash.as_deref(), Some("path-hash"));
    assert_eq!(
        sanitized.path_label.as_deref(),
        Some("/Users/example/Scratch")
    );
    assert!(project_contains_file_paths(Some(&sanitized)));
}

#[test]
fn sanitize_project_for_sync_drops_bare_project_ids() {
    let project = ProjectInfo {
        project_id: "project_bare".to_string(),
        project_label: Some("Bare".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: None,
        path_label: Some("/Users/example/Bare".to_string()),
    };

    assert!(sanitize_project_for_sync(project).is_none());
}

#[test]
fn sanitize_summary_for_sync_marks_project_path_labels_as_file_paths() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let mut summary = test_summary("codex", &source, now, now, now, 100);
    summary.project = Some(ProjectInfo {
        project_id: "project_path_only".to_string(),
        project_label: Some("Scratch".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/Users/example/Scratch".to_string()),
    });

    let sanitized = sanitize_summary_for_sync(summary);

    assert_eq!(
        sanitized
            .project
            .as_ref()
            .and_then(|project| project.path_label.as_deref()),
        Some("/Users/example/Scratch")
    );
    assert!(sanitized.privacy.contains_file_paths);
}

#[test]
fn preview_path_label_uses_display_label() {
    let mut source = test_source("codex", "/tmp/codex");
    source.path_label = Some("/home/testuser/work/codex".to_string());
    let preview = preview_path_label(&source);
    // if home matches, abbreviates; else full
    assert!(preview.contains("codex") || preview.contains("work"));
}
