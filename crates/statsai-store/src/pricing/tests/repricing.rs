use super::support::*;
use super::*;

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
    assert_eq!(CURRENT_SCHEMA_VERSION, 23);
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

#[test]
fn a_store_left_on_an_older_ruleset_repairs_stale_and_missing_catalog_costs() {
    let (store, source) = store_with_source("/tmp/pricing-ruleset-bump");
    // Sol's promotional rate started 2026-08-21; usage after it was stored under
    // the pre-promotion rates.
    let sol_at = parse_utc("2026-08-22T12:00:00Z");
    let sol = test_event(
        &source,
        sol_at,
        "sol-after-promo",
        "gpt-5.6-sol",
        million_token_usage(),
        CostInfo {
            estimated_api_equivalent_usd: Some(4_175),
            estimated_api_equivalent_micro_usd: Some(41_750_000),
            pricing_source: Some("codex_api_pricing:gpt-5.6-sol".to_string()),
            pricing_version: Some("official:2026-08-19".to_string()),
            confidence: Confidence::Medium,
            ..missing_cost()
        },
    );
    // Astra had no catalog entry at all, so its cost was recorded as unknown.
    let astra_at = parse_utc("2026-09-01T12:00:00Z");
    let astra = test_event(
        &source,
        astra_at,
        "astra-unpriced",
        "gpt-6-astra",
        million_token_usage(),
        missing_cost(),
    );
    store.insert_event(&sol).expect("insert sol");
    store.insert_event(&astra).expect("insert astra");
    store
        .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "1")
        .expect("mark ruleset 1");

    let report = store.ensure_current_pricing().expect("reprice");

    assert!(!report.already_current);
    assert_eq!(report.changed_events, 2);
    // 4.00 input + 5.00 cache write + 0.40 cached + 20.00 output.
    assert_eq!(
        stored_event(&store, &sol.event_id.0)
            .cost
            .estimated_api_equivalent_micro_usd,
        Some(29_400_000)
    );
    // 10.00 + 12.50 + 1.00 + 50.00.
    assert_eq!(
        stored_event(&store, &astra.event_id.0)
            .cost
            .estimated_api_equivalent_micro_usd,
        Some(73_500_000)
    );
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        Some(PRICING_RULESET_VERSION)
    );
}

#[test]
fn a_stored_event_with_a_stale_normalized_name_is_renormalized_and_repriced() {
    let (store, source) = store_with_source("/tmp/pricing-stale-normalization");
    let started_at = parse_utc("2026-09-01T12:00:00Z");
    let mut event = test_event(
        &source,
        started_at,
        "stale-fable",
        "claude-fable-5-1-thinking-max",
        UsageCounts {
            cache_read_tokens: Some(1_000_000),
            total_tokens: Some(1_000_000),
            ..UsageCounts::default()
        },
        missing_cost(),
    );
    // Written when the normalizer still folded every 5.1 name into Fable 5.
    event.model = Some(ModelInfo {
        normalized_name: Some("claude-fable-5".to_string()),
        ..event.model.clone().expect("model")
    });
    store.insert_event(&event).expect("insert");
    store
        .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "1")
        .expect("mark ruleset 1");

    store.ensure_current_pricing().expect("reprice");

    let stored = stored_event(&store, &event.event_id.0);
    let model = stored.model.expect("model");
    assert_eq!(model.normalized_name.as_deref(), Some("claude-fable-5-1"));
    assert_eq!(model.name.as_deref(), Some("claude-fable-5-1-thinking-max"));
    // Fable 5.1 reads cache at $0.25/M, not Fable 5's $1.00/M.
    assert_eq!(
        stored.cost.estimated_api_equivalent_micro_usd,
        Some(250_000)
    );
}

/// A cost an adapter derived by summing per-request samples, which prices each
/// request against its own context-size tier. Only the session total is stored,
/// and its request count is not 1, so the generic estimator cannot reproduce it.
fn source_derived_cost() -> CostInfo {
    CostInfo {
        estimated_api_equivalent_usd: Some(3_242),
        estimated_api_equivalent_micro_usd: Some(32_426_240),
        pricing_source: Some("xai_api_pricing:grok-4.6:unified_log_inference_usage".to_string()),
        pricing_version: Some("official:2026-08-19".to_string()),
        confidence: Confidence::Medium,
        ..missing_cost()
    }
}

#[test]
fn repricing_keeps_a_summary_an_adapter_priced_from_source_records() {
    let (store, source) = store_with_source("/tmp/pricing-source-derived-summary");
    let start = parse_utc("2026-09-01T00:00:00Z");
    let end = parse_utc("2026-09-01T23:59:59Z");
    let mut summary = test_summary(&source, "grok-4.6", start, end, source_derived_cost());
    summary.provider = "grok_build".to_string();
    // A whole session's worth of requests, so the per-request context tiers the
    // adapter applied are not recoverable from this aggregate.
    summary.usage.requests = Some(37);
    store.upsert_summary(&summary).expect("insert");
    store
        .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "1")
        .expect("mark ruleset 1");

    store.ensure_current_pricing().expect("reprice");

    let stored = &store.summaries().expect("summaries")[0];
    assert_eq!(stored.cost, source_derived_cost());
}

#[test]
fn repricing_still_updates_a_summary_the_generic_estimator_owns() {
    let (store, source) = store_with_source("/tmp/pricing-generic-summary");
    let start = parse_utc("2026-09-01T00:00:00Z");
    let end = parse_utc("2026-09-01T23:59:59Z");
    // Same shape, but priced by the generic estimator, so it is safe to redo.
    let generic = CostInfo {
        pricing_source: Some("codex_api_pricing:gpt-5.6-sol".to_string()),
        ..source_derived_cost()
    };
    let summary = test_summary(&source, "gpt-5.6-sol", start, end, generic.clone());
    store.upsert_summary(&summary).expect("insert");
    store
        .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "1")
        .expect("mark ruleset 1");

    store.ensure_current_pricing().expect("reprice");

    let stored = &store.summaries().expect("summaries")[0];
    assert_ne!(stored.cost, generic);
    assert_eq!(
        stored.cost.pricing_version.as_deref(),
        Some(PRICING_CATALOG_VERSION)
    );
}

#[test]
fn a_fast_mode_qualifier_is_not_mistaken_for_source_derived_pricing() {
    let (store, source) = store_with_source("/tmp/pricing-fast-summary");
    let start = parse_utc("2026-09-01T00:00:00Z");
    let end = parse_utc("2026-09-01T23:59:59Z");
    // `:fast` is written by the generic estimator itself, so it must not be
    // read as an adapter fingerprint and freeze the estimate.
    let fast = CostInfo {
        pricing_source: Some("claude_code_api_pricing:claude-opus-5:fast".to_string()),
        ..source_derived_cost()
    };
    let mut summary = test_summary(&source, "claude-opus-5", start, end, fast.clone());
    summary.model = Some(ModelInfo {
        speed: Some("fast".to_string()),
        ..test_model("claude-opus-5")
    });
    store.upsert_summary(&summary).expect("insert");
    store
        .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "1")
        .expect("mark ruleset 1");

    store.ensure_current_pricing().expect("reprice");

    let stored = &store.summaries().expect("summaries")[0];
    assert_ne!(
        stored.cost.estimated_api_equivalent_micro_usd,
        fast.estimated_api_equivalent_micro_usd
    );
    // Recomputed, and still charged at the fast-mode rate.
    assert!(
        stored
            .cost
            .pricing_source
            .as_deref()
            .is_some_and(|source| source.ends_with(":claude-opus-5:fast")),
        "unexpected pricing source: {:?}",
        stored.cost.pricing_source
    );
}
