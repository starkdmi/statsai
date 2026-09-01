use super::support::*;
use super::*;

#[test]
fn replacing_task_bucket_snapshot_marks_existing_sync_state_dirty() {
    let store = Store::in_memory().expect("store");
    let initial_snapshot = test_task_bucket_snapshot(
        "bucket-a",
        "span-a",
        "Implement sync dirty tracking",
        Utc.with_ymd_and_hms(2026, 7, 6, 10, 0, 0).unwrap(),
    );
    store
        .replace_task_bucket_snapshot(&initial_snapshot)
        .expect("replace initial snapshot");
    store
        .record_task_bucket_snapshots_synced(
            "http",
            "https://example.invalid/api/sync/batches",
            "device-1",
            std::slice::from_ref(&initial_snapshot),
        )
        .expect("record synced snapshot");

    let clean_pending = store
        .pending_task_bucket_snapshots_for_sync(
            "http",
            "https://example.invalid/api/sync/batches",
            "device-1",
            false,
            None,
        )
        .expect("pending snapshots before replacement");
    assert!(clean_pending.is_empty());

    let updated_snapshot = test_task_bucket_snapshot(
        "bucket-a",
        "span-a",
        "Implement sync dirty tracking v2",
        Utc.with_ymd_and_hms(2026, 7, 6, 11, 0, 0).unwrap(),
    );
    store
        .replace_task_bucket_snapshot(&updated_snapshot)
        .expect("replace updated snapshot");

    let pending = store
        .pending_task_bucket_snapshots_for_sync(
            "http",
            "https://example.invalid/api/sync/batches",
            "device-1",
            false,
            None,
        )
        .expect("pending snapshots after replacement");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].project_bucket, "bucket-a");
    assert_eq!(pending[0].spans.len(), 1);
    assert_eq!(
        pending[0].spans[0].title,
        "Implement sync dirty tracking v2"
    );
}
