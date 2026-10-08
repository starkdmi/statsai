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

#[test]
fn buckets_acknowledged_before_a_sanitizer_change_are_resent_once() {
    let target = "https://example.invalid/api/sync/batches";
    let home = std::path::Path::new("/Users/alice");
    let path = tempfile::tempdir()
        .expect("tempdir")
        .keep()
        .join("store.db");
    let mut snapshot = test_task_bucket_snapshot(
        "bucket-home",
        "span-home",
        "Collapse home paths",
        Utc.with_ymd_and_hms(2026, 7, 6, 10, 0, 0).unwrap(),
    );
    snapshot.spans[0].project = Some(statsai_core::ProjectInfo {
        project_id: "project-home".to_string(),
        project_label: Some("app".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/Users/alice/code/app".to_string()),
    });
    snapshot.work_items[0].path_label = Some("/Users/alice/code/app".to_string());

    {
        // A collector from before the sanitizer change syncs the bucket with
        // its full path: its schema has no sanitizer version yet.
        let store = Store::open(&path).expect("store");
        store
            .replace_task_bucket_snapshot(&snapshot)
            .expect("replace snapshot");
        store
            .record_task_bucket_snapshots_synced(
                "http",
                target,
                "device-1",
                std::slice::from_ref(&snapshot),
            )
            .expect("record legacy sync");
        store
            .conn
            .execute_batch(
                "ALTER TABLE task_bucket_sync_state DROP COLUMN sanitizer_version;
                 DELETE FROM schema_migrations WHERE version = 31;",
            )
            .expect("roll back to the previous schema");
    }

    // Upgrading reopens the store and migrates it.
    let store = Store::open(&path).expect("reopened store");
    assert_eq!(
        store
            .task_bucket_sync_status("http", target, "device-1")
            .expect("status")
            .dirty,
        1
    );
    let pending = store
        .pending_task_bucket_snapshots_for_sync("http", target, "device-1", false, None)
        .expect("incremental selection after upgrade");
    assert_eq!(
        pending.len(),
        1,
        "the hosted bucket still has the full path"
    );
    let sent = statsai_core::sanitize_task_bucket_for_sync_with_home(
        pending.into_iter().next().expect("bucket"),
        Some(home),
    );
    assert_eq!(sent.work_items[0].path_label.as_deref(), Some("~/code/app"));
    assert_eq!(
        sent.spans[0]
            .project
            .as_ref()
            .and_then(|project| project.path_label.as_deref()),
        Some("~/code/app")
    );

    store
        .record_task_bucket_snapshots_synced("http", target, "device-1", &[sent])
        .expect("record sync");
    assert!(store
        .pending_task_bucket_snapshots_for_sync("http", target, "device-1", false, None)
        .expect("incremental selection after resend")
        .is_empty());
    assert_eq!(
        store
            .task_bucket_sync_status("http", target, "device-1")
            .expect("status")
            .dirty,
        0
    );
}

#[test]
fn acknowledged_tombstones_stay_clean_across_a_sanitizer_change() {
    let target = "https://example.invalid/api/sync/batches";
    let path = tempfile::tempdir()
        .expect("tempdir")
        .keep()
        .join("store.db");
    let started_at = Utc.with_ymd_and_hms(2026, 7, 6, 10, 0, 0).unwrap();
    let kept = test_task_bucket_snapshot("bucket-kept", "span-kept", "Keep me", started_at);
    let gone = test_task_bucket_snapshot("bucket-gone", "span-gone", "Delete me", started_at);

    {
        // A collector from before the sanitizer change syncs both buckets,
        // deletes one locally, and syncs its tombstone.
        let store = Store::open(&path).expect("store");
        for snapshot in [&kept, &gone] {
            store
                .replace_task_bucket_snapshot(snapshot)
                .expect("replace snapshot");
        }
        store
            .record_task_bucket_snapshots_synced(
                "http",
                target,
                "device-1",
                &[kept.clone(), gone.clone()],
            )
            .expect("record buckets");
        store
            .replace_task_bucket_snapshot(&TaskBucketSnapshot {
                project_bucket: "bucket-gone".to_string(),
                generated_at: started_at,
                applied_verification_cursor: None,
                work_items: Vec::new(),
                members: Vec::new(),
                spans: Vec::new(),
            })
            .expect("delete bucket locally");
        let tombstones = store
            .pending_task_bucket_snapshots_for_sync("http", target, "device-1", false, None)
            .expect("tombstone selection");
        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].project_bucket, "bucket-gone");
        assert!(tombstones[0].spans.is_empty());
        store
            .record_task_bucket_snapshots_synced("http", target, "device-1", &tombstones)
            .expect("record tombstone");
        store
            .conn
            .execute_batch(
                "ALTER TABLE task_bucket_sync_state DROP COLUMN sanitizer_version;
                 DELETE FROM schema_migrations WHERE version = 31;",
            )
            .expect("roll back to the previous schema");
    }

    let store = Store::open(&path).expect("reopened store");
    let status = store
        .task_bucket_sync_status("http", target, "device-1")
        .expect("status");
    assert_eq!((status.dirty, status.total), (1, 2));
    let pending = store
        .pending_task_bucket_snapshots_for_sync("http", target, "device-1", false, None)
        .expect("incremental selection");
    assert_eq!(
        pending
            .iter()
            .map(|snapshot| snapshot.project_bucket.as_str())
            .collect::<Vec<_>>(),
        vec!["bucket-kept"],
        "only the bucket with content is re-sent"
    );
    let full = store
        .pending_task_bucket_snapshots_for_sync("http", target, "device-1", true, None)
        .expect("full selection");
    assert_eq!(full.len(), 1);

    store
        .record_task_bucket_snapshots_synced("http", target, "device-1", &pending)
        .expect("record resend");
    let status = store
        .task_bucket_sync_status("http", target, "device-1")
        .expect("status after resend");
    assert_eq!((status.dirty, status.total), (0, 2));
    assert!(store
        .pending_task_bucket_snapshots_for_sync("http", target, "device-1", false, None)
        .expect("incremental selection after resend")
        .is_empty());
}
