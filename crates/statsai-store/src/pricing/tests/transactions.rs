use super::support::*;
use super::*;

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
