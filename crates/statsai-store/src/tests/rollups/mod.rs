pub(super) use super::support::*;
pub(crate) use super::*;

mod build;

#[test]
fn renaming_a_repository_keeps_one_rollup_for_the_day() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-repo-rename"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let day = Utc
        .with_ymd_and_hms(2026, 6, 5, 9, 0, 0)
        .single()
        .expect("day");
    let account_id = statsai_core::provider_account_id("codex", "personal");

    let checkout = |remote: &str, label: &str| ProjectInfo {
        project_id: format!("project-{remote}"),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some(remote.to_string()),
        repo_label: Some(label.to_string()),
        branch_hash: Some("branch-main".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-checkout".to_string()),
        path_label: Some("/work/ai-stats".to_string()),
    };

    // Same checkout, same branch, same day — the remote was renamed midway.
    let mut before = test_store_event(&source, day, "before-rename");
    before.provider_account_id = Some(account_id.clone());
    before.usage.total_tokens = Some(10);
    before.project = Some(checkout("remote-before", "owner/ai-stats"));

    let mut after = test_store_event(&source, day + chrono::Duration::hours(1), "after-rename");
    after.provider_account_id = Some(account_id);
    after.usage.total_tokens = Some(20);
    after.project = Some(checkout("remote-after", "owner/statsai"));

    assert!(store.insert_event(&before).expect("insert before"));
    assert!(store.insert_event(&after).expect("insert after"));

    let dirty = store.dirty_sync_rollup_summaries().expect("dirty rollups");
    assert_eq!(dirty.len(), 1, "a rename must not split the day in two");
    let rollup = &dirty[0];
    assert_eq!(rollup.usage.total_tokens, Some(30));

    // The remote still travels with the rollup, so the backend can key the
    // project on it and relink this location's history across the rename.
    let project = rollup.project.as_ref().expect("project metadata");
    assert_eq!(project.path_hash.as_deref(), Some("path-checkout"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("owner/statsai"),
        "the newest event names the remote the checkout has now"
    );
}

#[test]
fn sync_rollups_track_dirty_daily_buckets() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sync-rollups"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let day = Utc
        .with_ymd_and_hms(2026, 5, 28, 9, 0, 0)
        .single()
        .expect("day");
    let account_id = statsai_core::provider_account_id("codex", "personal");
    let mut first = test_store_event(&source, day, "record-a");
    first.provider_account_id = Some(account_id.clone());
    first.usage.total_tokens = Some(15);
    first.model = Some(ModelInfo {
        name: Some("gpt-5".to_string()),
        normalized_name: Some("gpt-5".to_string()),
        provider_model_id: Some("gpt-5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    first.cost.provider_reported_usd = Some(11);

    assert!(store.insert_event(&first).expect("insert first"));
    let dirty = store
        .dirty_sync_rollup_summaries()
        .expect("dirty rollups after first");
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].usage.total_tokens, Some(15));
    assert_eq!(dirty[0].metadata.summary_format, "daily_rollup.v1");
    assert_eq!(
        dirty[0].metadata.summary_version.as_deref(),
        Some(SYNC_ROLLUP_SUMMARY_VERSION)
    );
    assert_eq!(
        dirty[0]
            .period_start
            .expect("period start")
            .date_naive()
            .to_string(),
        "2026-05-28"
    );
    assert_eq!(dirty[0].models.len(), 1);
    assert_eq!(
        dirty[0].models[0].model.normalized_name.as_deref(),
        Some("gpt-5")
    );
    assert_eq!(dirty[0].models[0].usage.total_tokens, Some(15));
    assert_eq!(dirty[0].cost.provider_reported_usd, Some(11));

    store
        .mark_sync_rollups_synced(&[dirty[0].summary_id.clone()])
        .expect("mark clean");
    assert!(store
        .dirty_sync_rollup_summaries()
        .expect("no dirty after clean")
        .is_empty());

    let mut second = test_store_event(&source, day + chrono::Duration::hours(1), "record-b");
    second.provider_account_id = Some(account_id);
    second.usage.total_tokens = Some(25);
    second.model = Some(ModelInfo {
        name: Some("gpt-4.1".to_string()),
        normalized_name: Some("gpt-4.1".to_string()),
        provider_model_id: Some("gpt-4.1".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    second.cost.provider_reported_usd = Some(22);

    assert!(store.insert_event(&second).expect("insert second"));
    let dirty = store
        .dirty_sync_rollup_summaries()
        .expect("dirty rollups after second");
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].usage.total_tokens, Some(40));
    assert_eq!(dirty[0].usage.requests, Some(2));
    assert_eq!(dirty[0].cost.provider_reported_usd, Some(33));
    assert_eq!(dirty[0].models.len(), 2);
    assert_eq!(dirty[0].models[0].usage.total_tokens, Some(25));
    assert_eq!(dirty[0].models[1].usage.total_tokens, Some(15));
    assert_eq!(dirty[0].metadata.total_sessions, Some(1));
}

#[test]
fn dirty_sync_rollups_rebuild_stale_summary_versions() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-stale-rollups"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc
        .with_ymd_and_hms(2026, 6, 4, 10, 0, 0)
        .single()
        .expect("now");

    let mut event = test_store_event(&source, now, "stale-a");
    event.usage = UsageCounts {
        output_tokens: Some(12),
        reasoning_tokens: Some(3),
        total_tokens: Some(15),
        requests: Some(1),
        ..UsageCounts::default()
    };
    event.runtime = Some(statsai_core::RuntimeInfo {
        runtime_name: None,
        host_id: None,
        latency_ms: Some(2000),
        latency_source: Some(LatencySource::Explicit),
        time_to_first_token_ms: Some(500),
        prompt_eval_duration_ms: None,
        eval_duration_ms: None,
        total_messages: Some(2),
        user_messages: Some(1),
        assistant_messages: Some(1),
        developer_messages: Some(0),
    });
    assert!(store.insert_event(&event).expect("insert"));

    let initial = store.dirty_sync_rollup_summaries().expect("dirty initial");
    assert_eq!(
        initial[0].metadata.summary_version.as_deref(),
        Some(SYNC_ROLLUP_SUMMARY_VERSION)
    );
    store
        .mark_sync_rollups_synced(&[initial[0].summary_id.clone()])
        .expect("mark synced");
    store
            .conn
            .execute(
                "UPDATE sync_rollups SET payload = json_set(payload, '$.metadata.summary_version', '3'), dirty = 0",
                [],
            )
            .expect("downgrade payload version");

    let rebuilt = store
        .dirty_sync_rollup_summaries()
        .expect("dirty after rebuild");
    assert_eq!(rebuilt.len(), 1);
    assert_eq!(
        rebuilt[0].metadata.summary_version.as_deref(),
        Some(SYNC_ROLLUP_SUMMARY_VERSION)
    );
    let metrics = rebuilt[0].metrics.as_ref().expect("metrics");
    assert_eq!(metrics.tracked_requests, Some(1));
    assert_eq!(metrics.tracked_output_tokens, Some(12));
    assert_eq!(metrics.tracked_reasoning_tokens, Some(3));
    assert_eq!(metrics.overall_generated_tps, Some(7.5));
    assert_eq!(metrics.overall_visible_tps, Some(6.0));
}

#[test]
fn menu_usage_totals_by_provider_uses_fast_rollups_and_reportable_summaries() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-menu-provider-totals"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let mut event = test_store_event(&source, now, "event");
    event.usage.total_tokens = Some(25);
    event.cost.estimated_api_equivalent_usd = Some(15);
    store.insert_event(&event).expect("insert event");

    let mut reportable = test_store_summary(&source, now, 100);
    reportable.summary_id = summary_id(&source.provider, &source.source_id, "reportable");
    reportable.source.source_kind = SourceKind::LocalAdapter;
    reportable.metadata.summary_format = "ccusage_daily".to_string();
    reportable.usage.requests = Some(3);
    reportable.cost.provider_reported_usd = Some(45);
    store
        .upsert_summary(&reportable)
        .expect("reportable summary");

    let mut requestless = test_store_summary(&source, now, 50);
    requestless.summary_id = summary_id(&source.provider, &source.source_id, "requestless");
    requestless.source.source_kind = SourceKind::LocalAdapter;
    requestless.metadata.summary_format = "ccusage_daily".to_string();
    requestless.cost.provider_reported_usd = Some(5);
    store
        .upsert_summary(&requestless)
        .expect("requestless summary");

    let mut local_summary = test_store_summary(&source, now, 1_000);
    local_summary.summary_id = summary_id(&source.provider, &source.source_id, "local");
    local_summary.metadata.summary_format = "claude_stats_cache".to_string();
    local_summary.cost.provider_reported_usd = Some(9_999);
    store.upsert_summary(&local_summary).expect("local summary");

    let totals = store
        .menu_usage_totals_by_provider()
        .expect("provider totals");
    let provider_totals = totals.get("codex").expect("codex totals");

    assert_eq!(
        *provider_totals,
        SourceUsageTotals {
            events: 5,
            tokens: 175,
            estimated_cost_cents: Some(65),
        }
    );
}
