use super::support::*;
use super::*;

#[test]
fn source_lifecycle_updates_enabled_and_removes_scan_cache() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-source-lifecycle"),
        LocationOrigin::Configured,
    );
    let source_id = source.source_id.clone();
    store.upsert_source(&source).expect("source");
    store
        .record_scan_file_entries(
            &source_id,
            &[ScanFileStateEntry {
                cache_key: "/tmp/a.jsonl".to_string(),
                cache_signature: "sig-a-1".to_string(),
            }],
        )
        .expect("record scan cache");

    let disabled = store
        .set_source_enabled(&source_id, false)
        .expect("disable")
        .expect("existing source");
    assert!(!disabled.enabled);
    assert!(store
        .pending_scan_file_entries(
            &source_id,
            &[ScanFileStateEntry {
                cache_key: "/tmp/a.jsonl".to_string(),
                cache_signature: "sig-a-1".to_string(),
            }],
        )
        .expect("cached")
        .is_empty());

    let deleted_scan_cache = store
        .delete_scan_file_entries_for_sources(std::slice::from_ref(&source_id))
        .expect("delete scan cache");
    assert_eq!(deleted_scan_cache, 1);
    assert!(
        store
            .pending_scan_file_entries(
                &source_id,
                &[ScanFileStateEntry {
                    cache_key: "/tmp/a.jsonl".to_string(),
                    cache_signature: "sig-a-1".to_string(),
                }],
            )
            .expect("pending after delete")
            .len()
            == 1
    );

    assert!(store.delete_source(&source_id).expect("delete source"));
    assert!(store.source(&source_id).expect("reload").is_none());
}

#[test]
fn usage_totals_by_source_groups_tokens_and_cost() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-source-totals"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let mut first = test_store_event(&source, now - chrono::Duration::minutes(5), "first");
    first.cost.estimated_api_equivalent_usd = Some(10);
    let mut second = test_store_event(&source, now, "second");
    second.usage.total_tokens = Some(25);
    second.cost.estimated_api_equivalent_usd = Some(15);
    store
        .insert_events(&[first, second])
        .expect("insert events");
    let mut summary = test_store_summary(&source, now, 100);
    summary.cost.estimated_api_equivalent_usd = Some(40);
    summary.cost.provider_reported_usd = Some(45);
    store.upsert_summary(&summary).expect("summary");

    let totals = store.usage_totals_by_source().expect("source totals");
    let source_totals = totals.get(&source.source_id.0).expect("source entry");

    assert_eq!(
        *source_totals,
        SourceUsageTotals {
            events: 1,
            tokens: 100,
            estimated_cost_cents: Some(45),
        }
    );
}
