use super::support::{test_event, test_source};
use super::*;
use chrono::{DateTime, Duration, FixedOffset, TimeZone, Utc};

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 2, 10, 0, 0).unwrap() + Duration::seconds(seconds)
}

/// One single-call event in session `s`, transcript `main`, at `seconds`.
fn call(index: u64, seconds: i64, input: u64, read: u64, write: Option<u64>) -> UsageEvent {
    let source = test_source("claude_code", "/fixture/claude");
    let mut event = test_event("claude_code", &source, at(seconds), 0, None);
    event.event_id = EventId(format!("event_{index:03}"));
    event.model = Some(ModelInfo {
        normalized_name: Some("claude-model".to_string()),
        ..ModelInfo::default()
    });
    event.usage = UsageCounts {
        input_tokens: Some(input),
        cache_read_tokens: Some(read),
        cache_creation_tokens: write,
        output_tokens: Some(10),
        requests: Some(1),
        ..UsageCounts::default()
    };
    event.parse_evidence = Some(ParseEvidence {
        event_key_version: "test".to_string(),
        source_file_path_hash: Some("main".to_string()),
        source_line_number: Some(index),
        source_record_id: None,
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::Unresolved,
    });
    event
}

fn classes(calls: &[AnalyzedCacheCall]) -> Vec<CacheCallClass> {
    calls.iter().map(|call| call.verdict.class).collect()
}

fn losses(calls: &[AnalyzedCacheCall]) -> Vec<Option<CacheLoss>> {
    calls.iter().map(|call| call.verdict.loss).collect()
}

fn health(calls: &[AnalyzedCacheCall]) -> CacheHealthV1 {
    let mut health = CacheHealthV1::default();
    for call in calls {
        health.add_call(call);
    }
    health
}

#[test]
fn gap_bins_are_closed_below_and_open_above() {
    let cases = [
        (0, 0),
        (59, 0),
        (60, 1),
        (299, 1),
        (300, 2),
        (599, 2),
        (600, 3),
        (899, 3),
        (900, 4),
        (1_799, 4),
        (1_800, 5),
        (2_699, 5),
        (2_700, 6),
        (3_599, 6),
        (3_600, 7),
        (86_400, 7),
    ];
    for (seconds, bin) in cases {
        assert_eq!(cache_gap_bin(seconds), bin, "{seconds}s");
    }
    let labels = (0..CACHE_GAP_BIN_COUNT)
        .map(cache_gap_bin_label)
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        ["<1m", "1–5m", "5–10m", "10–15m", "15–30m", "30–45m", "45–60m", "60m+"]
    );
}

#[test]
fn hit_ratio_counts_cache_writes_in_the_denominator() {
    assert_eq!(cache_hit_ratio(100, 300, 600), Some(0.6));
    assert_eq!(cache_hit_ratio(0, 0, 0), None);
}

#[test]
fn growing_context_is_reuse_not_loss() {
    let events = [
        call(1, 0, 10, 0, Some(20_000)),
        call(2, 30, 10, 20_000, Some(2_000)),
        call(3, 60, 10, 22_000, Some(3_000)),
        call(4, 90, 10, 25_000, Some(500)),
    ];
    let calls = analyze_cache_calls(&events);
    assert_eq!(
        classes(&calls),
        [
            CacheCallClass::FirstObserved,
            CacheCallClass::Comparable,
            CacheCallClass::Comparable,
            CacheCallClass::Comparable
        ]
    );
    assert!(losses(&calls).iter().all(Option::is_none));
    assert_eq!(calls[2].verdict.baseline_tokens, Some(22_000));
    let health = health(&calls);
    assert_eq!((health.first_observed, health.first_cold), (1, 1));
    assert_eq!(health.gap_comparable[0], 3);
}

#[test]
fn losses_need_half_the_baseline_and_the_token_floor() {
    // Each pair: a call leaving 10,000 reusable tokens, then the probe, which
    // writes to the cache whatever it did not read.
    let probe = |read: u64| {
        let events = [
            call(1, 0, 0, 10_000, Some(0)),
            call(2, 10, 0, read, Some(20_000 - read)),
        ];
        analyze_cache_calls(&events)[1].verdict.loss
    };
    assert_eq!(probe(0), Some(CacheLoss::Full));
    assert_eq!(probe(5_000), Some(CacheLoss::Partial));
    assert_eq!(probe(5_001), None, "a drop under half is not a loss");

    // A drop of more than half that is still under the token floor.
    let events = [
        call(1, 0, 0, 8_000, Some(0)),
        call(2, 10, 8_000, 3_905, Some(0)),
    ];
    assert_eq!(analyze_cache_calls(&events)[1].verdict.loss, None);
    let events = [
        call(1, 0, 0, 8_192, Some(0)),
        call(2, 10, 8_192, 4_096, Some(0)),
    ];
    assert_eq!(
        analyze_cache_calls(&events)[1].verdict.loss,
        Some(CacheLoss::Partial)
    );
}

#[test]
fn the_baseline_is_bounded_by_the_current_input() {
    let events = [
        call(1, 0, 0, 100_000, Some(0)),
        // The context shrank to 20,000 tokens; 15,000 of them were reused.
        call(2, 10, 5_000, 15_000, Some(0)),
    ];
    let verdict = &analyze_cache_calls(&events)[1].verdict;
    assert_eq!(verdict.baseline_tokens, Some(20_000));
    assert_eq!(verdict.loss, None);
}

#[test]
fn without_write_telemetry_previous_reads_are_the_baseline() {
    let events = [
        call(1, 0, 40_000, 50_000, None),
        call(2, 10, 45_000, 50_000, None),
    ];
    let calls = analyze_cache_calls(&events);
    assert_eq!(calls[1].verdict.baseline_tokens, Some(50_000));
    assert_eq!(calls[1].verdict.loss, None);
}

#[test]
fn a_tiny_previous_context_is_too_small_to_judge() {
    let events = [
        call(1, 0, 100, 0, Some(3_000)),
        call(2, 10, 100, 0, Some(3_000)),
    ];
    let calls = analyze_cache_calls(&events);
    assert_eq!(calls[1].verdict.class, CacheCallClass::BelowThreshold);
    assert_eq!(health(&calls).below_threshold, 1);
}

#[test]
fn sub_agents_are_separate_streams() {
    let mut events = vec![
        call(1, 0, 0, 100_000, Some(1_000)),
        call(2, 20, 0, 0, Some(20_000)),
        call(3, 40, 0, 101_000, Some(1_000)),
        call(4, 60, 0, 20_000, Some(500)),
    ];
    // In one stream, the small agent context reads as a drop.
    assert!(analyze_cache_calls(&events)
        .iter()
        .any(|call| call.verdict.loss.is_some()));

    for index in [1, 3] {
        events[index].context = Some(CallContext {
            agent_hash: Some("agent".to_string()),
            ..CallContext::default()
        });
    }
    let calls = analyze_cache_calls(&events);
    assert!(losses(&calls).iter().all(Option::is_none));
    assert_eq!(health(&calls).first_observed, 2);

    // A different transcript is a different stream too.
    let mut events = vec![
        call(1, 0, 0, 100_000, Some(1_000)),
        call(2, 20, 0, 5_000, Some(500)),
    ];
    if let Some(evidence) = events[1].parse_evidence.as_mut() {
        evidence.source_file_path_hash = Some("agent-file".to_string());
    }
    assert_eq!(health(&analyze_cache_calls(&events)).first_observed, 2);
}

#[test]
fn model_changes_and_compaction_restart_the_comparison() {
    let mut events = vec![
        call(1, 0, 0, 100_000, Some(0)),
        call(2, 10, 0, 0, Some(100_000)),
        call(3, 20, 0, 100_000, Some(0)),
        call(4, 30, 0, 0, Some(30_000)),
    ];
    events[1].model = Some(ModelInfo {
        normalized_name: Some("other-model".to_string()),
        ..ModelInfo::default()
    });
    events[2].model = events[1].model.clone();
    events[3].model = events[1].model.clone();
    events[3].context = Some(CallContext {
        after_compaction: true,
        ..CallContext::default()
    });
    let calls = analyze_cache_calls(&events);
    assert_eq!(
        calls[1].verdict.boundary,
        Some(CacheBoundaryReason::ModelChange)
    );
    assert_eq!(
        calls[1].verdict.previous_model.as_deref(),
        Some("claude-model")
    );
    assert_eq!(calls[2].verdict.class, CacheCallClass::Comparable);
    assert_eq!(
        calls[3].verdict.boundary,
        Some(CacheBoundaryReason::Compaction)
    );
    assert!(losses(&calls).iter().all(Option::is_none));
    assert_eq!(health(&calls).boundaries, 2);
}

#[test]
fn a_call_without_a_timestamp_is_counted_but_not_placed() {
    let mut events = vec![
        call(1, 0, 0, 50_000, Some(0)),
        call(2, 10, 0, 0, Some(0)),
        call(3, 20, 0, 50_000, Some(0)),
    ];
    if let Some(evidence) = events[1].parse_evidence.as_mut() {
        evidence.timestamp_inferred = true;
    }
    let calls = analyze_cache_calls(&events);
    let unplaced = calls
        .iter()
        .find(|call| call.event_id.0 == "event_002")
        .expect("unplaced call");
    assert_eq!(
        unplaced.verdict.unclassifiable,
        Some(CacheUnclassifiableReason::TimestampMissing)
    );
    let third = calls
        .iter()
        .find(|call| call.event_id.0 == "event_003")
        .expect("third call");
    assert_eq!(third.verdict.class, CacheCallClass::Comparable);
    assert_eq!(third.verdict.loss, None);
}

#[test]
fn calls_with_one_timestamp_and_no_recorded_order_are_unclassifiable() {
    // The cold call sorts first by event id, but either could have come first,
    // so neither is judged against the call before them.
    let mut events = vec![
        call(1, 0, 0, 50_000, Some(0)),
        call(2, 10, 0, 0, Some(0)),
        call(3, 10, 0, 50_000, Some(0)),
        call(4, 20, 0, 50_000, Some(0)),
    ];
    for event in &mut events[1..3] {
        if let Some(evidence) = event.parse_evidence.as_mut() {
            evidence.source_line_number = None;
        }
    }
    let calls = analyze_cache_calls(&events);
    for tied in &calls[1..3] {
        assert_eq!(
            tied.verdict.unclassifiable,
            Some(CacheUnclassifiableReason::AmbiguousOrder)
        );
    }
    assert_eq!(
        calls[3].verdict.boundary,
        Some(CacheBoundaryReason::EvidenceGap)
    );
    assert!(losses(&calls).iter().all(Option::is_none));

    // The same timestamps keep their order when the lines say it.
    let events = [
        call(1, 0, 0, 50_000, Some(0)),
        call(2, 10, 0, 50_000, Some(0)),
        call(3, 10, 0, 0, Some(50_000)),
    ];
    assert_eq!(
        analyze_cache_calls(&events)[2].verdict.loss,
        Some(CacheLoss::Full)
    );
}

#[test]
fn aggregated_usage_is_unclassifiable_and_breaks_the_chain() {
    let mut events = vec![
        call(1, 0, 0, 80_000, Some(0)),
        call(2, 60, 3_000, 240_000, Some(0)),
        call(3, 120, 0, 0, Some(80_000)),
    ];
    events[1].usage.requests = Some(3);
    let calls = analyze_cache_calls(&events);
    assert_eq!(
        calls[1].verdict.unclassifiable,
        Some(CacheUnclassifiableReason::Aggregated)
    );
    assert_eq!(calls[1].requests, 3);
    assert_eq!(
        calls[2].verdict.boundary,
        Some(CacheBoundaryReason::EvidenceGap)
    );
    let health = health(&calls);
    assert_eq!(
        (health.requests, health.unclassifiable, health.analyzed()),
        (5, 3, 2)
    );
}

#[test]
fn recorded_calls_of_a_turn_are_judged_one_by_one() {
    let mut turn = call(1, 0, 0, 0, Some(0));
    turn.provider = "codex".to_string();
    turn.usage.requests = Some(3);
    turn.context = Some(CallContext {
        calls: vec![
            ModelCall {
                completed_at: at(10),
                requested_at: Some(at(0)),
                input_tokens: Some(30_000),
                cache_read_tokens: Some(0),
                cache_creation_tokens: Some(0),
                after_compaction: false,
                model: None,
            },
            ModelCall {
                completed_at: at(20),
                requested_at: Some(at(12)),
                input_tokens: Some(2_000),
                cache_read_tokens: Some(29_952),
                cache_creation_tokens: Some(0),
                after_compaction: false,
                model: None,
            },
            ModelCall {
                completed_at: at(400),
                requested_at: Some(at(390)),
                input_tokens: Some(31_000),
                cache_read_tokens: Some(1_024),
                cache_creation_tokens: Some(0),
                after_compaction: false,
                model: None,
            },
        ],
        ..CallContext::default()
    });
    let calls = analyze_cache_calls([&turn]);
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].verdict.loss, None);
    assert_eq!(calls[2].verdict.loss, Some(CacheLoss::Partial));
    assert_eq!(calls[2].verdict.gap_seconds, Some(370));
    assert_eq!(
        calls[2].verdict.gap_timing,
        Some(CacheGapTiming::RequestStart)
    );

    // A breakdown that does not cover every call is not used.
    if let Some(context) = turn.context.as_mut() {
        context.calls.pop();
    }
    let calls = analyze_cache_calls([&turn]);
    assert_eq!(
        calls[0].verdict.unclassifiable,
        Some(CacheUnclassifiableReason::Aggregated)
    );
}

#[test]
fn a_model_change_inside_a_turn_is_a_boundary_not_a_loss() {
    let mut turn = call(1, 0, 0, 0, Some(0));
    turn.provider = "codex".to_string();
    turn.usage.requests = Some(2);
    let model_call = |seconds, read, write, model: &str| ModelCall {
        completed_at: at(seconds),
        requested_at: None,
        input_tokens: Some(1_000),
        cache_read_tokens: Some(read),
        cache_creation_tokens: Some(write),
        after_compaction: false,
        model: Some(model.to_string()),
    };
    turn.context = Some(CallContext {
        calls: vec![
            model_call(10, 50_000, 0, "gpt-5"),
            model_call(20, 0, 0, "gpt-5.1"),
        ],
        ..CallContext::default()
    });
    let calls = analyze_cache_calls([&turn]);
    assert_eq!(
        calls[1].verdict.boundary,
        Some(CacheBoundaryReason::ModelChange)
    );
    assert_eq!(calls[1].verdict.previous_model.as_deref(), Some("gpt-5"));
    assert_eq!(calls[1].verdict.loss, None);
}

#[test]
fn a_turn_call_without_input_counts_is_unclassifiable() {
    let mut turn = call(1, 0, 0, 0, Some(0));
    turn.provider = "codex".to_string();
    turn.usage.requests = Some(3);
    let model_call = |seconds, input: Option<u64>, read: Option<u64>| ModelCall {
        completed_at: at(seconds),
        requested_at: None,
        input_tokens: input,
        cache_read_tokens: read,
        cache_creation_tokens: None,
        after_compaction: false,
        model: None,
    };
    turn.context = Some(CallContext {
        calls: vec![
            model_call(10, Some(1_000), Some(50_000)),
            model_call(20, None, None),
            model_call(30, Some(1_000), Some(0)),
        ],
        ..CallContext::default()
    });
    let calls = analyze_cache_calls([&turn]);
    assert_eq!(
        calls[1].verdict.unclassifiable,
        Some(CacheUnclassifiableReason::NoInputTelemetry)
    );
    // The call after it has no known predecessor, so it is not a loss.
    assert_eq!(
        calls[2].verdict.boundary,
        Some(CacheBoundaryReason::EvidenceGap)
    );
    assert_eq!(calls[2].verdict.loss, None);
}

#[test]
fn gaps_prefer_the_recorded_request_start() {
    let mut events = vec![
        call(1, 0, 0, 50_000, Some(0)),
        call(2, 400, 0, 50_000, Some(0)),
        call(3, 800, 0, 50_000, Some(0)),
    ];
    events[1].context = Some(CallContext {
        requested_at: Some(at(330)),
        ..CallContext::default()
    });
    // A request start before the previous response belongs to another exchange.
    events[2].context = Some(CallContext {
        requested_at: Some(at(390)),
        ..CallContext::default()
    });
    let calls = analyze_cache_calls(&events);
    assert_eq!(calls[1].verdict.gap_seconds, Some(330));
    assert_eq!(
        calls[1].verdict.gap_timing,
        Some(CacheGapTiming::RequestStart)
    );
    assert_eq!(calls[2].verdict.gap_seconds, Some(400));
    assert_eq!(
        calls[2].verdict.gap_timing,
        Some(CacheGapTiming::RecordedEvents)
    );
    let health = health(&calls);
    assert_eq!(health.timing_estimated, 1);
    assert_eq!(health.gap_comparable[2], 2);
}

#[test]
fn a_short_gap_loss_is_counted_in_its_bin() {
    let events = [
        call(1, 0, 2, 350_000, Some(1_000)),
        call(2, 40, 2, 27_000, Some(325_000)),
        call(3, 80, 2, 352_000, Some(2_000)),
    ];
    let calls = analyze_cache_calls(&events);
    assert_eq!(losses(&calls), [None, Some(CacheLoss::Partial), None]);
    let health = health(&calls);
    assert_eq!((health.gap_comparable[0], health.gap_losses[0]), (2, 1));
    assert_eq!((health.losses, health.full_losses), (1, 0));
}

#[test]
fn a_loss_after_a_long_pause_is_counted_in_the_open_bin() {
    let events = [
        call(1, 0, 2, 120_000, Some(1_000)),
        call(2, 2 * 3_600, 2, 0, Some(121_500)),
    ];
    let calls = analyze_cache_calls(&events);
    assert_eq!(calls[1].verdict.loss, Some(CacheLoss::Full));
    let health = health(&calls);
    assert_eq!((health.gap_comparable[7], health.gap_losses[7]), (1, 1));
}

#[test]
fn previous_write_lifetimes_are_kept_as_evidence() {
    let mut events = vec![
        call(1, 0, 2, 10_000, Some(9_000)),
        call(2, 600, 2, 0, Some(19_000)),
    ];
    events[0].usage.cache_creation_5m_tokens = Some(4_000);
    events[0].usage.cache_creation_1h_tokens = Some(5_000);
    let calls = analyze_cache_calls(&events);
    assert_eq!(calls[0].write_ttl, Some(CacheWriteTtl::Mixed));
    assert_eq!(
        calls[1].verdict.previous_write_ttl,
        Some(CacheWriteTtl::Mixed)
    );
}

#[test]
fn health_counts_add_up_and_merge() {
    let mut events = vec![
        call(1, 0, 0, 0, Some(40_000)),
        call(2, 30, 0, 40_000, Some(1_000)),
        call(3, 400, 41_000, 0, Some(0)),
        call(4, 460, 0, 41_000, Some(0)),
        call(5, 520, 9_000, 0, Some(0)),
    ];
    events[4].usage.requests = Some(2);
    let calls = analyze_cache_calls(&events);
    let health = health(&calls);
    assert_eq!(health.uncached, 1, "the third call skipped the cache");
    assert_eq!(
        health.requests,
        health.unclassifiable
            + health.first_observed
            + health.boundaries
            + health.below_threshold
            + health.uncached
            + health.comparable
    );
    assert_eq!(health.gap_comparable.iter().sum::<u64>(), health.comparable);
    assert_eq!(health.gap_losses.iter().sum::<u64>(), health.losses);
    assert_eq!(
        health.gap_missed_tokens.iter().sum::<u64>(),
        health.missed_tokens
    );

    let mut merged = CacheHealthV1::default();
    merged.merge(&health);
    merged.merge(&health);
    assert_eq!(merged.requests, health.requests * 2);
    assert_eq!(merged.gap_comparable[2], health.gap_comparable[2] * 2);
}

#[test]
fn providers_without_call_order_are_left_out() {
    let mut event = call(1, 0, 0, 1_000, Some(0));
    event.provider = "cursor".to_string();
    assert!(analyze_cache_calls([&event]).is_empty());
}

#[test]
fn the_analysis_does_not_depend_on_input_order() {
    let events = vec![
        call(1, 0, 0, 0, Some(30_000)),
        call(2, 20, 0, 30_000, Some(0)),
        call(3, 700, 30_000, 0, Some(0)),
    ];
    let mut reversed = events.clone();
    reversed.reverse();
    assert_eq!(analyze_cache_calls(&events), analyze_cache_calls(&reversed));
}

#[test]
fn the_report_splits_totals_by_provider_and_utc_day() {
    let mut events = vec![
        call(1, 13 * 3_600 + 59 * 60, 100, 0, Some(20_000)),
        call(2, 14 * 3_600 + 1, 100, 20_000, Some(100)),
    ];
    let mut cursor = call(3, 0, 1_000, 3_000, None);
    cursor.provider = "cursor".to_string();
    events.push(cursor);
    let calls = analyze_cache_calls(&events);
    let report = build_cache_report(
        CacheReportInput {
            label: "fixture".to_string(),
            since: None,
            until: at(86_400),
            filters: CacheReportFilters::default(),
            events: &events,
            summaries: &[],
            calls,
            include_details: false,
        },
        Some(&FixedOffset::east_opt(5 * 3_600 + 30 * 60).unwrap()),
    );
    assert_eq!(report.totals.requests, 3);
    assert_eq!(report.totals.logical_input_tokens, 44_300);
    assert_eq!(report.providers.len(), 2);
    assert!(report.providers[1].diagnostics.is_none(), "cursor");
    assert_eq!(
        report.diagnostics.write_split_unavailable_input_tokens, 0,
        "only covered providers count unsplit input"
    );
    assert_eq!(
        report
            .days
            .iter()
            .map(|day| day.day.as_str())
            .collect::<Vec<_>>(),
        ["2026-03-02", "2026-03-03"]
    );
    let timeline = report.timeline.expect("timeline");
    assert_eq!(timeline.len(), 2);
    assert_eq!(timeline[0].start, "2026-03-03T05:20:00+05:30");
    assert_eq!(report.gap_histogram[0].comparable, 0);
    assert_eq!(report.gap_histogram[1].label, "1–5m");
    assert!(report.details.is_none());
}

#[test]
fn call_context_is_omitted_when_empty_and_optional_on_read() {
    let event = call(1, 0, 10, 20, Some(30));
    let value = serde_json::to_value(&event).expect("event json");
    assert!(value.get("context").is_none());
    let mut legacy = value.clone();
    legacy.as_object_mut().expect("object").remove("context");
    let read: UsageEvent = serde_json::from_value(legacy).expect("legacy event");
    assert_eq!(read.context, None);

    let context = CallContext {
        requested_at: Some(at(0)),
        ..CallContext::default()
    };
    assert_eq!(
        serde_json::to_value(&context).expect("context json"),
        serde_json::json!({ "requested_at": "2026-03-02T10:00:00Z" })
    );
}

#[test]
fn removing_cache_health_restores_the_summary_a_receiver_stores() {
    let metrics = |cache_health| SummaryMetrics {
        active_seconds: None,
        tracked_requests: None,
        tracked_output_tokens: None,
        tracked_reasoning_tokens: None,
        latency_ms: None,
        time_to_first_token_ms: None,
        generated_tps: None,
        visible_tps: None,
        overall_generated_tps: None,
        overall_visible_tps: None,
        cache_hit_ratio: None,
        reasoning_share: None,
        total_messages: Some(4),
        user_messages: None,
        assistant_messages: None,
        developer_messages: None,
        cache_health,
    };
    let only_health = metrics(Some(CacheHealthV1::default()));
    let mut kept = only_health.clone();
    kept.total_messages = Some(4);
    let mut empty = only_health;
    empty.total_messages = None;
    assert!(!empty.is_empty());

    let summary = |metrics| UsageSummary {
        metrics,
        ..serde_json::from_value::<UsageSummary>(fixture_summary_json()).expect("summary")
    };
    assert_eq!(
        summary(Some(empty)).without_cache_health().metrics,
        None,
        "metrics holding only cache_health go away"
    );
    assert_eq!(
        summary(Some(kept)).without_cache_health().metrics,
        Some(metrics(None))
    );
}

fn fixture_summary_json() -> serde_json::Value {
    serde_json::json!({
        "schema_version": USAGE_SUMMARY_SCHEMA_VERSION,
        "summary_id": "summary_fixture",
        "device_id": "device",
        "provider": "claude_code",
        "source_id": "source",
        "provider_account_id": null,
        "source": {
            "adapter_id": "test",
            "adapter_version": "0",
            "source_kind": "local_adapter",
            "location_origin": null,
            "source_type": "jsonl",
            "source_path_hash": null,
            "source_record_id": null,
            "parse_confidence": "high"
        },
        "model": null,
        "usage": {},
        "cost": { "currency": "USD", "estimated_api_equivalent_usd": null, "provider_reported_usd": null, "pricing_source": null, "pricing_version": null, "confidence": "low" },
        "parse_evidence": null,
        "project": null,
        "privacy": { "mode": "metadata_only", "contains_prompt_text": false, "contains_response_text": false, "contains_file_paths": false },
        "metrics": null,
        "period_start": null,
        "period_end": null,
        "observed_at": "2026-03-02T10:00:00Z",
        "metadata": { "summary_format": "daily_rollup.v1", "summary_version": null, "total_sessions": null, "total_messages": null, "last_computed_at": null },
        "imported_at": "2026-03-02T10:00:00Z"
    })
}

#[test]
fn a_call_that_neither_reads_nor_writes_the_cache_is_uncached_not_a_loss() {
    // A router serving the request without the cache, between cached calls.
    let events = [
        call(1, 0, 10, 0, Some(50_000)),
        call(2, 10, 50_010, 0, Some(0)),
        call(3, 20, 51_000, 0, Some(0)),
        call(4, 30, 10, 50_000, Some(1_000)),
    ];
    let calls = analyze_cache_calls(&events);
    assert_eq!(
        calls
            .iter()
            .map(|call| call.verdict.class)
            .collect::<Vec<_>>(),
        [
            CacheCallClass::FirstObserved,
            CacheCallClass::Uncached,
            CacheCallClass::Uncached,
            CacheCallClass::BelowThreshold,
        ]
    );
    assert!(losses(&calls).iter().all(Option::is_none));
    let health = health(&calls);
    assert_eq!((health.uncached, health.losses), (2, 0));

    // Codex reports zero writes on calls that use the cache, so a cold Codex
    // call is still judged.
    let mut codex = events.clone();
    for event in &mut codex {
        event.provider = "codex".to_string();
    }
    assert_eq!(
        analyze_cache_calls(&codex)[1].verdict.loss,
        Some(CacheLoss::Full)
    );
}

#[test]
fn a_loss_records_the_reusable_context_it_missed() {
    let events = [
        call(1, 0, 10, 0, Some(80_000)),
        call(2, 30, 10, 80_000, Some(0)),
        call(3, 3_900, 10, 20_000, Some(60_000)),
    ];
    let calls = analyze_cache_calls(&events);
    assert_eq!(calls[1].verdict.missed_tokens, None);
    assert_eq!(calls[2].verdict.loss, Some(CacheLoss::Partial));
    assert_eq!(calls[2].verdict.missed_tokens, Some(60_000));

    let health = health(&calls);
    assert_eq!(health.missed_tokens, 60_000);
    assert_eq!(health.gap_missed_tokens[CACHE_GAP_BIN_COUNT - 1], 60_000);
    let mut doubled = health.clone();
    doubled.merge(&health);
    assert_eq!(doubled.missed_tokens, 120_000);
    assert_eq!(
        gap_histogram(&doubled)[CACHE_GAP_BIN_COUNT - 1].missed_tokens,
        120_000
    );
}

#[test]
fn an_inferred_model_does_not_make_a_model_change() {
    let mut events = vec![
        call(1, 0, 0, 30_000, Some(0)),
        call(2, 10, 0, 1_000, Some(29_000)),
    ];
    // The first model is the parser's fallback, the second one the transcript named.
    if let Some(evidence) = events[0].parse_evidence.as_mut() {
        evidence.model_inferred = true;
    }
    events[1].model = Some(ModelInfo {
        normalized_name: Some("other-model".to_string()),
        ..ModelInfo::default()
    });
    let calls = analyze_cache_calls(&events);
    assert_eq!(calls[1].verdict.boundary, None);
    assert_eq!(calls[1].verdict.loss, Some(CacheLoss::Partial));
    assert_eq!(calls[1].verdict.missed_tokens, Some(29_000));
}

#[test]
fn unsplit_input_is_counted_per_call_when_writes_are_partly_reported() {
    let mut turn = call(1, 0, 3_000, 0, Some(0));
    turn.provider = "codex".to_string();
    turn.usage.requests = Some(2);
    let model_call = |seconds, input, write| ModelCall {
        completed_at: at(seconds),
        requested_at: None,
        input_tokens: Some(input),
        cache_read_tokens: Some(0),
        cache_creation_tokens: write,
        after_compaction: false,
        model: None,
    };
    // The turn's summed usage says zero writes; one call never reported them.
    turn.context = Some(CallContext {
        calls: vec![model_call(10, 1_000, Some(0)), model_call(20, 2_000, None)],
        ..CallContext::default()
    });
    let mut health = CacheHealthV1::default();
    health.add_event_input(&turn);
    assert_eq!(health.write_split_unavailable_input_tokens, 2_000);

    // Without a call list the event's own counts decide.
    let mut single = call(2, 30, 4_000, 0, None);
    single.provider = "codex".to_string();
    health.add_event_input(&single);
    assert_eq!(health.write_split_unavailable_input_tokens, 6_000);
}
