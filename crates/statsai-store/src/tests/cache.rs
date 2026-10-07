use super::support::*;
use super::*;
use statsai_core::{
    CacheHealthV1, CallContext, EventId, IdentitySource, ModelCall, CACHE_HEALTH_VERSION,
};

fn claude_source() -> SourceLocation {
    SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/fixture/claude"),
        LocationOrigin::Configured,
    )
}

fn at(day: u32, hour: u32, minute: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, day, hour, minute, 0)
        .single()
        .expect("time")
}

/// One Claude Code call in session `session_cache`, transcript `main`.
fn call(
    source: &SourceLocation,
    index: u64,
    time: chrono::DateTime<Utc>,
    input: u64,
    read: u64,
    write: u64,
) -> UsageEvent {
    let mut event = test_store_event(source, time, &format!("call-{index}"));
    event.provider = "claude_code".to_string();
    event.event_id = EventId(format!("event_cache_{index:03}"));
    event.session.session_id = "session_cache".to_string();
    event.usage = UsageCounts {
        input_tokens: Some(input),
        cache_read_tokens: Some(read),
        cache_creation_tokens: Some(write),
        output_tokens: Some(10),
        total_tokens: Some(input + read + write + 10),
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

fn store_with_source() -> (Store, SourceLocation) {
    let store = Store::in_memory().expect("store");
    let source = claude_source();
    store.upsert_source(&source).expect("source");
    (store, source)
}

fn health_by_day(store: &Store) -> BTreeMap<String, CacheHealthV1> {
    store
        .all_sync_rollup_summaries()
        .expect("summaries")
        .into_iter()
        .filter_map(|summary| {
            let day = summary.period_start?.date_naive().to_string();
            Some((day, summary.metrics?.cache_health?))
        })
        .collect()
}

fn payload_hashes(store: &Store) -> BTreeMap<String, (String, String)> {
    let mut statement = store
        .conn
        .prepare("SELECT summary_id, payload_hash, updated_at FROM sync_rollups")
        .expect("statement");
    statement
        .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("hashes")
}

#[test]
fn daily_summaries_carry_cache_health_for_covered_providers_only() {
    let (store, source) = store_with_source();
    let mut events = vec![
        call(&source, 1, at(1, 10, 0), 10, 0, 30_000),
        call(&source, 2, at(1, 10, 1), 10, 30_000, 500),
        call(&source, 3, at(1, 10, 20), 10, 0, 30_600),
    ];
    let mut cursor = test_store_event(&source, at(1, 11, 0), "cursor-row");
    cursor.provider = "cursor".to_string();
    events.push(cursor);
    store.insert_events(&events).expect("insert");

    let summaries = store.all_sync_rollup_summaries().expect("summaries");
    let claude = summaries
        .iter()
        .find(|summary| summary.provider == "claude_code")
        .expect("claude summary");
    let health = claude
        .metrics
        .as_ref()
        .and_then(|metrics| metrics.cache_health.as_ref())
        .expect("cache health");
    assert_eq!(health.version, CACHE_HEALTH_VERSION);
    assert_eq!(
        (health.requests, health.first_observed, health.first_cold),
        (3, 1, 1)
    );
    assert_eq!(
        (health.comparable, health.losses, health.full_losses),
        (2, 1, 1)
    );
    assert_eq!(health.gap_comparable[1], 1);
    assert_eq!(health.gap_losses[4], 1);
    // Writes count in the denominator of the per-call ratio.
    assert_eq!(
        claude
            .metrics
            .as_ref()
            .and_then(|metrics| metrics.cache_hit_ratio.as_ref())
            .and_then(|ratio| ratio.max),
        Some(30_000.0 / 30_510.0)
    );

    let cursor = summaries
        .iter()
        .find(|summary| summary.provider == "cursor")
        .expect("cursor summary");
    assert!(cursor
        .metrics
        .as_ref()
        .is_none_or(|metrics| metrics.cache_health.is_none()));
    assert!(
        serde_json::to_string(claude).expect("json").len()
            - serde_json::to_string(&claude.clone().without_cache_health())
                .expect("json")
                .len()
            < 2_048,
        "the synced object stays under 2 KiB"
    );
}

#[test]
fn a_predecessor_before_midnight_is_compared_with() {
    let (store, source) = store_with_source();
    store
        .insert_events(&[
            call(&source, 1, at(1, 23, 50), 10, 0, 50_000),
            call(&source, 2, at(1, 23, 58), 10, 50_000, 100),
            call(&source, 3, at(2, 0, 2), 10, 0, 50_100),
        ])
        .expect("insert");

    let health = health_by_day(&store);
    assert_eq!(health["2026-07-01"].first_observed, 1);
    let next_day = &health["2026-07-02"];
    assert_eq!(
        next_day.first_observed, 0,
        "the previous call is yesterday's"
    );
    assert_eq!((next_day.comparable, next_day.losses), (1, 1));
    assert_eq!(next_day.gap_losses[1], 1);
}

#[test]
fn a_late_predecessor_refreshes_the_later_day() {
    let (store, source) = store_with_source();
    store
        .insert_events(&[call(&source, 3, at(2, 0, 2), 10, 0, 50_100)])
        .expect("later call");
    assert_eq!(health_by_day(&store)["2026-07-02"].first_observed, 1);
    let before = payload_hashes(&store);

    store
        .insert_events(&[
            call(&source, 1, at(1, 23, 50), 10, 0, 50_000),
            call(&source, 2, at(1, 23, 58), 10, 50_000, 100),
        ])
        .expect("late arrivals");

    let health = health_by_day(&store);
    assert_eq!(health["2026-07-02"].first_observed, 0);
    assert_eq!(health["2026-07-02"].losses, 1);
    let after = payload_hashes(&store);
    let changed = before
        .iter()
        .filter(|(id, (hash, _))| after.get(*id).is_some_and(|(next, _)| next != hash))
        .count();
    assert_eq!(changed, 1, "the later day's summary was rebuilt");
}

#[test]
fn a_corrected_predecessor_changes_the_verdict_after_it() {
    let (store, source) = store_with_source();
    store
        .insert_events(&[
            call(&source, 1, at(1, 23, 58), 10, 50_000, 100),
            call(&source, 2, at(2, 0, 2), 10, 0, 50_100),
        ])
        .expect("insert");
    assert_eq!(health_by_day(&store)["2026-07-02"].losses, 1);

    // The predecessor turns out to have cached almost nothing.
    store
        .insert_event(&call(&source, 1, at(1, 23, 58), 50_100, 0, 10))
        .expect("correction");
    let next_day = &health_by_day(&store)["2026-07-02"];
    assert_eq!(next_day.losses, 0);
    assert_eq!(next_day.below_threshold, 1);
}

#[test]
fn sessions_split_by_sub_agent_are_compared_within_their_stream() {
    let (store, source) = store_with_source();
    let mut agent = call(&source, 2, at(1, 10, 1), 0, 0, 20_000);
    agent.context = Some(CallContext {
        agent_hash: Some("agent".to_string()),
        ..CallContext::default()
    });
    store
        .insert_events(&[
            call(&source, 1, at(1, 10, 0), 0, 100_000, 1_000),
            agent,
            call(&source, 3, at(1, 10, 2), 0, 101_000, 1_000),
        ])
        .expect("insert");
    let health = &health_by_day(&store)["2026-07-01"];
    assert_eq!((health.first_observed, health.losses), (2, 0));
}

#[test]
fn the_one_time_refresh_adds_cache_health_once_and_leaves_other_summaries_alone() {
    let (store, source) = store_with_source();
    let mut cursor = test_store_event(&source, at(1, 11, 0), "cursor-row");
    cursor.provider = "cursor".to_string();
    // Another provider's day with cache writes, whose hit ratio now counts them.
    let mut opencode = call(&source, 3, at(1, 12, 0), 100, 100, 100);
    opencode.provider = "opencode".to_string();
    opencode.event_id = EventId("event_opencode".to_string());
    opencode.session.session_id = "session_opencode".to_string();
    store
        .insert_events(&[
            call(&source, 1, at(1, 10, 0), 10, 0, 30_000),
            call(&source, 2, at(1, 10, 1), 10, 30_000, 500),
            cursor,
            opencode,
        ])
        .expect("insert");
    store
        .all_sync_rollup_summaries()
        .expect("marker set on first read");
    let current = payload_hashes(&store);

    // A store written before this version: no cache_health, hit ratios
    // without cache writes, and no marker.
    for summary in store.all_sync_rollup_summaries().expect("summaries") {
        let mut legacy = summary.without_cache_health();
        if legacy.provider == "opencode" {
            let metrics = legacy.metrics.as_mut().expect("metrics");
            let ratio = metrics.cache_hit_ratio.as_mut().expect("hit ratio");
            ratio.p50 = Some(0.5);
        }
        store
            .conn
            .execute(
                "UPDATE sync_rollups SET payload = ?2, payload_hash = ?3, updated_at = 'legacy', dirty = 0
                 WHERE summary_id = ?1",
                params![
                    &legacy.summary_id.0,
                    serde_json::to_string(&legacy).expect("json"),
                    summary_sync_payload_hash(&legacy).expect("hash")
                ],
            )
            .expect("legacy row");
    }
    store
        .conn
        .execute(
            "DELETE FROM local_metadata WHERE key = 'sync_rollups.cache_health_version'",
            [],
        )
        .expect("clear marker");

    assert_eq!(health_by_day(&store).len(), 1, "the refresh ran");
    let refreshed = payload_hashes(&store);
    for (id, (hash, _)) in &refreshed {
        assert_eq!(hash, &current[id].0, "content matches a fresh build");
    }
    let untouched = refreshed
        .values()
        .filter(|(_, updated_at)| updated_at == "legacy")
        .count();
    assert_eq!(untouched, 1, "the cursor summary was not rewritten");

    // Running again changes nothing.
    store.all_sync_rollup_summaries().expect("second read");
    assert_eq!(payload_hashes(&store), refreshed);
}

#[test]
fn the_report_agrees_with_the_daily_summaries() {
    let (store, source) = store_with_source();
    store
        .insert_events(&[
            call(&source, 1, at(1, 23, 50), 10, 0, 50_000),
            call(&source, 2, at(1, 23, 58), 10, 50_000, 100),
            call(&source, 3, at(2, 0, 2), 10, 0, 50_100),
            call(&source, 4, at(2, 1, 30), 10, 50_110, 200),
        ])
        .expect("insert");
    let mut query =
        CacheReportQuery::for_range(Some("2026-07-02"), Some("2026-07-02"), at(3, 0, 0))
            .expect("query");
    query.details = true;
    let report = store.cache_report(&query, &Utc).expect("report");

    let daily = &health_by_day(&store)["2026-07-02"];
    assert_eq!(&report.diagnostics, daily);
    assert_eq!(report.totals.requests, 2);
    assert_eq!(report.details.as_ref().map(Vec::len), Some(2));
    assert_eq!(report.days.len(), 1);

    let mut filtered = query.clone();
    filtered.account = Some("unassigned".to_string());
    filtered.session = Some("session_ca".to_string());
    assert_eq!(
        store
            .cache_report(&filtered, &Utc)
            .expect("report")
            .totals
            .requests,
        2
    );
    filtered.provider = Some("codex".to_string());
    assert_eq!(
        store
            .cache_report(&filtered, &Utc)
            .expect("report")
            .totals
            .requests,
        0
    );
}

#[test]
fn the_report_counts_other_providers_in_its_token_totals_only() {
    let (store, source) = store_with_source();
    let other_source = SourceLocation::local_adapter(
        "opencode",
        "test",
        "0",
        Path::new("/fixture/opencode"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&other_source).expect("source");
    let mut other = test_store_event(&other_source, at(2, 9, 0), "other");
    other.provider = "opencode".to_string();
    other.usage.requests = Some(1);
    store
        .insert_events(&[
            call(&source, 1, at(1, 23, 50), 10, 0, 50_000),
            call(&source, 2, at(2, 0, 2), 10, 50_000, 100),
            other,
        ])
        .expect("insert");
    let query = CacheReportQuery::for_range(Some("2026-07-02"), Some("2026-07-02"), at(3, 0, 0))
        .expect("query");

    let report = store.cache_report(&query, &Utc).expect("report");
    assert_eq!(report.totals.requests, 2);
    assert_eq!(report.diagnostics.requests, 1);
    assert_eq!(report.diagnostics.comparable, 1);

    let mut other_only = query.clone();
    other_only.provider = Some("opencode".to_string());
    let report = store.cache_report(&other_only, &Utc).expect("report");
    assert_eq!(report.totals.requests, 1);
    assert_eq!(report.diagnostics.requests, 0);
}

#[test]
fn a_receiver_without_cache_health_is_not_offered_summaries_that_differ_only_by_it() {
    let (store, source) = store_with_source();
    store
        .insert_events(&[
            call(&source, 1, at(1, 10, 0), 10, 0, 30_000),
            call(&source, 2, at(1, 10, 1), 10, 30_000, 500),
        ])
        .expect("insert");
    let target = "https://receiver.test/api/sync/batches";
    assert!(!store
        .sync_target_accepts_cache_health("http", target)
        .expect("default"));
    assert!(store
        .sync_target_accepts_cache_health("file", "out.json")
        .expect("local sink"));

    let stripped = store
        .all_sync_rollup_summaries()
        .expect("summaries")
        .into_iter()
        .map(|summary| {
            summary_for_sync_target(sanitize_summary_for_default_http_sync(summary), false)
        })
        .collect::<Vec<_>>();
    assert!(stripped.iter().all(|summary| summary
        .metrics
        .as_ref()
        .is_none_or(|metrics| metrics.cache_health.is_none())));
    store
        .record_summaries_synced("http", target, &stripped)
        .expect("record");
    assert_eq!(
        store
            .pending_http_sync_summary_counts_with_projects(target, "device", false)
            .expect("pending")
            .rollups,
        0
    );

    store
        .record_sync_target_cache_health_support("http", target, true)
        .expect("support");
    assert_eq!(
        store
            .pending_http_sync_summary_counts_with_projects(target, "device", false)
            .expect("pending")
            .rollups,
        1,
        "support turning on queues the diagnostics once"
    );
}

#[test]
fn the_report_counts_a_multi_day_summary_by_its_start_day() {
    let (store, source) = store_with_source();
    let backfill = |key: &str, start: chrono::DateTime<Utc>, reads: u64| {
        let mut summary = test_store_summary(&source, start, reads);
        summary.summary_id = summary_id(&source.provider, &source.source_id, key);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "manual_period_summary".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(start + chrono::Duration::days(5));
        summary.usage = UsageCounts {
            cache_read_tokens: Some(reads),
            total_tokens: Some(reads),
            ..UsageCounts::default()
        };
        summary
    };
    store
        .upsert_summary(&backfill("starts-inside", at(3, 0, 0), 1_000))
        .expect("summary");
    store
        .upsert_summary(&backfill("starts-before", at(1, 0, 0), 50_000))
        .expect("summary");
    let query = CacheReportQuery::for_range(Some("2026-07-02"), Some("2026-07-09"), at(10, 0, 0))
        .expect("query");

    let report = store.cache_report(&query, &Utc).expect("report");
    assert_eq!(report.totals.cache_read_tokens, 1_000);
}

#[test]
fn missed_reuse_is_priced_as_rewritten_context_minus_cache_reads() {
    let (store, source) = store_with_source();
    let model = |event: &mut UsageEvent, name: &str| {
        event.model = Some(statsai_core::ModelInfo {
            normalized_name: Some(name.to_string()),
            ..statsai_core::ModelInfo::default()
        });
    };
    let mut events = vec![
        call(&source, 1, at(1, 9, 0), 10, 0, 100_000),
        // An hour and more later the context is written again, with the
        // one-hour lifetime: 100,000 tokens at twice the input rate.
        call(&source, 2, at(1, 10, 10), 10, 0, 100_000),
    ];
    events[1].usage.cache_creation_1h_tokens = Some(100_000);
    for event in &mut events {
        model(event, "claude-sonnet-4-5");
    }
    store.insert_events(&events).expect("insert");
    let query = CacheReportQuery::for_range(Some("2026-07-01"), Some("2026-07-01"), at(2, 0, 0))
        .expect("query");

    let diagnostics = store
        .cache_report(&query, &Utc)
        .expect("report")
        .diagnostics;
    assert_eq!(diagnostics.missed_tokens, 100_000);
    // $6.00 per million written for an hour, less $0.30 per million read.
    assert_eq!(diagnostics.missed_cost_micro_usd, 570_000);
    assert_eq!(diagnostics.unpriced_losses, 0);
    assert_eq!(
        health_by_day(&store)["2026-07-01"].missed_cost_micro_usd,
        570_000
    );

    // A model without known pricing is counted, not priced.
    let (store, source) = store_with_source();
    let mut events = vec![
        call(&source, 1, at(1, 9, 0), 10, 0, 100_000),
        call(&source, 2, at(1, 10, 10), 10, 0, 100_000),
    ];
    for event in &mut events {
        model(event, "unpriced-model");
    }
    store.insert_events(&events).expect("insert");
    let diagnostics = store
        .cache_report(&query, &Utc)
        .expect("report")
        .diagnostics;
    assert_eq!(
        (
            diagnostics.losses,
            diagnostics.unpriced_losses,
            diagnostics.missed_cost_micro_usd
        ),
        (1, 1, 0)
    );
}

#[test]
fn an_event_sharing_a_session_with_another_provider_is_counted_once() {
    let (store, source) = store_with_source();
    let other_source = SourceLocation::local_adapter(
        "opencode",
        "test",
        "0",
        Path::new("/fixture/opencode"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&other_source).expect("source");
    let mut other = test_store_event(&other_source, at(2, 9, 0), "other");
    other.provider = "opencode".to_string();
    other.session.session_id = "session_cache".to_string();
    other.usage.requests = Some(1);
    store
        .insert_events(&[call(&source, 1, at(2, 8, 0), 10, 0, 50_000), other])
        .expect("insert");
    let query = CacheReportQuery::for_range(Some("2026-07-02"), Some("2026-07-02"), at(3, 0, 0))
        .expect("query");

    let report = store.cache_report(&query, &Utc).expect("report");
    assert_eq!(report.totals.requests, 2);
    assert_eq!(
        report
            .providers
            .iter()
            .map(|row| (row.provider.as_str(), row.totals.requests))
            .collect::<Vec<_>>(),
        [("claude_code", 1), ("opencode", 1)]
    );
}

#[test]
fn missed_reuse_is_priced_per_request_and_by_the_calls_own_model() {
    let codex_source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/fixture/codex"),
        LocationOrigin::Configured,
    );
    let codex = |index: u64, minutes: u32, input: u64, read: u64| {
        let mut event = call(&codex_source, index, at(1, 9, minutes), input, read, 0);
        event.provider = "codex".to_string();
        event.usage.cache_creation_tokens = None;
        event.model = Some(statsai_core::ModelInfo {
            normalized_name: Some("gpt-6-astra".to_string()),
            provider_model_id: Some("gpt-6-astra".to_string()),
            ..statsai_core::ModelInfo::default()
        });
        event
    };
    let query = CacheReportQuery::for_range(Some("2026-07-01"), Some("2026-07-01"), at(2, 0, 0))
        .expect("query");

    // A prompt over 272k tokens is priced at the long-context rates: $20 per
    // million of input and $2 per million of cached input, not $10 and $1.
    let store = Store::in_memory().expect("store");
    store.upsert_source(&codex_source).expect("source");
    store
        .insert_events(&[codex(1, 0, 10, 300_000), codex(2, 1, 300_010, 0)])
        .expect("insert");
    let diagnostics = store
        .cache_report(&query, &Utc)
        .expect("report")
        .diagnostics;
    assert_eq!(diagnostics.missed_tokens, 300_000);
    assert_eq!(diagnostics.missed_cost_micro_usd, 5_400_000);

    // A turn's call on a model without pricing stays unpriced, even though
    // the turn's own model is priced.
    let store = Store::in_memory().expect("store");
    store.upsert_source(&codex_source).expect("source");
    let mut turn = codex(1, 0, 50_010, 50_000);
    turn.usage.requests = Some(2);
    let model_call = |minute: i64, input: u64, read: u64, model: &str| ModelCall {
        completed_at: at(1, 9, 0) + chrono::Duration::minutes(minute),
        requested_at: None,
        input_tokens: Some(input),
        cache_read_tokens: Some(read),
        cache_creation_tokens: None,
        after_compaction: false,
        model: Some(model.to_string()),
    };
    turn.context = Some(CallContext {
        calls: vec![
            model_call(0, 10, 50_000, "unpriced-model"),
            model_call(1, 50_000, 0, "unpriced-model"),
        ],
        ..CallContext::default()
    });
    store.insert_events(&[turn]).expect("insert");
    let diagnostics = store
        .cache_report(&query, &Utc)
        .expect("report")
        .diagnostics;
    assert_eq!(
        (
            diagnostics.losses,
            diagnostics.unpriced_losses,
            diagnostics.missed_cost_micro_usd
        ),
        (1, 1, 0)
    );
}
