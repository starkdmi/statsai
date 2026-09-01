use super::support::*;
use super::*;

#[test]
fn upserts_usage_summaries_idempotently() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-summary"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let mut summary = test_store_summary(&source, now, 100);

    assert!(store.upsert_summary(&summary).expect("insert"));
    summary.usage.input_tokens = Some(150);
    summary.usage.total_tokens = Some(150);
    assert!(store.upsert_summary(&summary).expect("update"));

    let summaries = store.summaries().expect("summaries");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].usage.total_tokens, Some(150));
    assert_eq!(store.summary_count().expect("count"), 1);
}

#[test]
fn reportable_summary_period_stats_include_summary_only_usage() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "grok_build",
        "test",
        "0",
        Path::new("/tmp/grok-summary-period-stats"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let mut recent = test_store_summary(&source, now, 70);
    recent.summary_id = summary_id(&source.provider, &source.source_id, "recent");
    recent.source.source_kind = SourceKind::LocalAdapter;
    recent.metadata.summary_format = "grok_build_session_summary".to_string();
    recent.period_start = Some(now);
    recent.period_end = Some(now);
    store.upsert_summary(&recent).expect("recent summary");

    let mut explicit_requests = test_store_summary(&source, now, 30);
    explicit_requests.summary_id =
        summary_id(&source.provider, &source.source_id, "explicit-requests");
    explicit_requests.source.source_kind = SourceKind::LocalAdapter;
    explicit_requests.metadata.summary_format = "grok_build_session_summary".to_string();
    explicit_requests.period_start = Some(now);
    explicit_requests.period_end = Some(now);
    explicit_requests.usage.requests = Some(4);
    store
        .upsert_summary(&explicit_requests)
        .expect("explicit request summary");

    let mut old = test_store_summary(&source, now - chrono::Duration::days(10), 1_000);
    old.summary_id = summary_id(&source.provider, &source.source_id, "old");
    old.source.source_kind = SourceKind::LocalAdapter;
    old.metadata.summary_format = "grok_build_session_summary".to_string();
    old.period_start = Some(now - chrono::Duration::days(10));
    old.period_end = Some(now - chrono::Duration::days(10));
    store.upsert_summary(&old).expect("old summary");

    let mut rollup = test_store_summary(&source, now, 2_000);
    rollup.summary_id = summary_id(&source.provider, &source.source_id, "rollup");
    rollup.source.source_kind = SourceKind::LocalAdapter;
    rollup.metadata.summary_format = "daily_rollup.v1".to_string();
    rollup.period_start = Some(now);
    rollup.period_end = Some(now);
    store.upsert_summary(&rollup).expect("rollup summary");

    let stats = store
        .reportable_summary_period_stats_since(now - chrono::Duration::hours(1))
        .expect("summary stats");
    assert_eq!(
        stats,
        RollupPeriodStats {
            tokens: 100,
            requests: 5,
        }
    );

    let day_stats = store
        .reportable_summary_period_stats_since_day(now.date_naive())
        .expect("summary day stats");
    assert_eq!(day_stats, stats);
}
