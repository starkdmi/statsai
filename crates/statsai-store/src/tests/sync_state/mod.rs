pub(super) use super::support::*;
pub(crate) use super::*;

mod pending;

#[test]
fn task_bucket_sync_status_counts_tracked_and_local_bucket_union() {
    let store = Store::in_memory().expect("store");
    store
        .conn
        .execute(
            r#"
                INSERT INTO task_spans (
                  span_id, provider, source_id, project_bucket, started_at, ended_at, title,
                  normalized_title, is_meta, confidence, source_file_path_hash, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, NULL, ?10)
                "#,
            params![
                "span-local",
                "codex",
                "source-local",
                "bucket-local",
                "2026-07-05T10:00:00Z",
                "2026-07-05T10:05:00Z",
                "Local span",
                "local span",
                "medium",
                r#"{"span_id":"span-local","project_bucket":"bucket-local"}"#,
            ],
        )
        .expect("insert local task span");
    store
        .conn
        .execute(
            r#"
                INSERT INTO task_bucket_sync_state (
                  sink, target, device_id, project_bucket, dirty, payload_hash, updated_at
                )
                VALUES (?1, ?2, ?3, ?4, 1, NULL, ?5)
                "#,
            params![
                "http",
                "target",
                "device-1",
                "bucket-tracked",
                "2026-07-05T11:00:00Z",
            ],
        )
        .expect("insert tracked bucket state");

    let status = store
        .task_bucket_sync_status("http", "target", "device-1")
        .expect("task bucket sync status");
    assert_eq!(status.total, 2);
    assert_eq!(status.dirty, 2);
}

#[test]
fn sync_state_tracks_success_and_filters_after_cursor() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sync-state"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let first = test_store_event(&source, now, "record-a");
    let second = test_store_event(&source, now + chrono::Duration::seconds(1), "record-b");
    let first_event_id = first.event_id.0.clone();
    store
        .insert_events(&[first.clone(), second.clone()])
        .expect("events");

    store
        .record_sync_success(
            "http",
            "http://localhost/sync",
            "batch_1",
            &[first],
            &[],
            None,
        )
        .expect("record success");
    let state = store
        .sync_state("http", "http://localhost/sync")
        .expect("state")
        .expect("present");

    assert_eq!(state.last_batch_id, "batch_1");
    assert_eq!(
        state.last_event_id.as_deref(),
        Some(first_event_id.as_str())
    );
    let remaining = store
        .events_after(
            state
                .last_event_started_at
                .as_ref()
                .zip(state.last_event_id.as_deref()),
        )
        .expect("remaining");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].event_id, second.event_id);
}

#[test]
fn record_rollup_chunk_sync_success_retries_busy_database() {
    use tempfile::tempdir;

    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("statsai.sqlite");
    let store = Store::open(&db_path).expect("open store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sync-rollup-retry"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let now = Utc
        .with_ymd_and_hms(2026, 7, 9, 10, 0, 0)
        .single()
        .expect("now");
    let event = test_store_event(&source, now, "record-a");
    assert!(store.insert_event(&event).expect("insert event"));
    let summaries = store
        .all_sync_rollup_summaries()
        .expect("all sync rollup summaries");
    assert_eq!(summaries.len(), 1);

    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_retry_chunk_1".to_string(),
        device_id: "device".to_string(),
        sources: Vec::new(),
        accounts: Vec::new(),
        source_account_assignments: Vec::new(),
        subscriptions: Vec::new(),
        account_plan_observations: Vec::new(),
        account_evidence_summaries: Vec::new(),
        events: Vec::new(),
        summaries: summaries.clone(),
        task_buckets: Vec::new(),
        task_verifications: Vec::new(),
        code_change_metrics: Vec::new(),
        quota_cycle_contributions: Vec::new(),
        authoritative_snapshot: None,
        created_at: now,
    };

    let db_path_for_lock = db_path.clone();
    let (lock_ready_tx, lock_ready_rx) = std::sync::mpsc::channel();
    let lock_thread = std::thread::spawn(move || {
        let conn = Connection::open(&db_path_for_lock).expect("open lock connection");
        conn.busy_timeout(Duration::from_millis(1))
            .expect("lock busy timeout");
        conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")
            .expect("begin lock");
        lock_ready_tx.send(()).expect("signal lock ready");
        std::thread::sleep(Duration::from_millis(200));
        conn.execute_batch("COMMIT").expect("commit lock");
    });
    lock_ready_rx.recv().expect("wait for lock");

    store
        .record_rollup_chunk_sync_success(
            "http",
            "https://api.example.com/api/sync/batches",
            "batch_retry_chunk",
            &batch,
        )
        .expect("record rollup chunk sync success");

    lock_thread.join().expect("join lock thread");

    assert!(store
        .dirty_sync_rollup_summaries()
        .expect("dirty summaries after retry")
        .is_empty());
    let state = store
        .sync_state("http", "https://api.example.com/api/sync/batches")
        .expect("sync state")
        .expect("sync state present");
    assert_eq!(state.last_batch_id, "batch_retry_chunk");
    assert_eq!(
        state.pending_resume_batch_id.as_deref(),
        Some("batch_retry_chunk")
    );
}

#[test]
fn failed_commit_restores_autocommit_before_the_next_transaction() {
    use tempfile::tempdir;

    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("statsai.sqlite");
    let store = Store::open(&db_path).expect("open store");
    store
        .conn
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode = DELETE;")
        .expect("switch test database to rollback journal mode");
    store
        .conn
        .busy_timeout(std::time::Duration::from_millis(1))
        .expect("short writer timeout");

    let reader = Connection::open(&db_path).expect("open reader");
    reader
        .execute_batch("BEGIN DEFERRED TRANSACTION")
        .expect("begin reader transaction");
    let mut statement = reader
        .prepare("SELECT COUNT(*) FROM local_metadata")
        .expect("prepare reader");
    let mut rows = statement.query([]).expect("query reader");
    assert!(rows.next().expect("read first row").is_some());

    let error = store
        .with_immediate_transaction(|| {
            store.set_metadata_value("commit-test", "pending")?;
            Ok(())
        })
        .expect_err("reader must block commit");
    assert!(
        error
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(is_sqlite_busy_or_locked),
        "expected busy commit error, got {error:#}"
    );
    assert!(
        store.conn.is_autocommit(),
        "failed commit must not leave the connection inside a transaction"
    );

    drop(rows);
    drop(statement);
    reader.execute_batch("ROLLBACK").expect("release reader");

    store
        .with_immediate_transaction(|| {
            store.set_metadata_value("commit-test", "committed")?;
            Ok(())
        })
        .expect("next transaction commits independently");
    assert_eq!(
        store.metadata_value("commit-test").expect("metadata"),
        Some("committed".to_string())
    );
}

#[test]
fn clear_sync_tracking_for_target_only_removes_matching_target() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-clear-sync-target"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    store
        .record_sources_synced(
            "http",
            "https://api.example.com/api/sync/batches",
            std::slice::from_ref(&source),
        )
        .expect("record synced source");
    store
        .record_sources_synced(
            "http",
            "https://other.example.com/api/sync/batches",
            std::slice::from_ref(&source),
        )
        .expect("record synced source other target");
    store
        .record_sync_success(
            "http",
            "https://api.example.com/api/sync/batches",
            "batch_1",
            &[],
            &[],
            None,
        )
        .expect("record success");
    store
        .record_sync_success(
            "http",
            "https://other.example.com/api/sync/batches",
            "batch_2",
            &[],
            &[],
            None,
        )
        .expect("record success other target");

    store
        .clear_sync_tracking_for_target("http", "https://api.example.com/api/sync/batches")
        .expect("clear target tracking");

    assert!(store
        .sync_state("http", "https://api.example.com/api/sync/batches")
        .expect("state")
        .is_none());
    assert!(store
        .sync_state("http", "https://other.example.com/api/sync/batches")
        .expect("other state")
        .is_some());

    assert_eq!(
        store
            .pending_sources_for_sync(
                "http",
                "https://api.example.com/api/sync/batches",
                std::slice::from_ref(&source),
            )
            .expect("pending sources")
            .len(),
        1
    );
    assert_eq!(
        store
            .pending_sources_for_sync(
                "http",
                "https://other.example.com/api/sync/batches",
                std::slice::from_ref(&source),
            )
            .expect("other pending sources")
            .len(),
        0
    );
}

#[test]
fn entity_sync_state_only_returns_changed_sources() {
    let store = Store::in_memory().expect("store");
    let mut source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sync-entities"),
        LocationOrigin::Configured,
    );

    let changed = store
        .pending_sources_for_sync(
            "http",
            "https://api.example.com/api/sync/batches",
            &[source.clone()],
        )
        .expect("initial changed");
    assert_eq!(changed.len(), 1);

    store
        .record_sources_synced(
            "http",
            "https://api.example.com/api/sync/batches",
            &[source.clone()],
        )
        .expect("record synced");
    assert!(store
        .pending_sources_for_sync(
            "http",
            "https://api.example.com/api/sync/batches",
            &[source.clone()]
        )
        .expect("unchanged")
        .is_empty());

    source.enabled = false;
    source.updated_at += chrono::Duration::seconds(1);
    let changed = store
        .pending_sources_for_sync(
            "http",
            "https://api.example.com/api/sync/batches",
            &[source],
        )
        .expect("changed after update");
    assert_eq!(changed.len(), 1);
}

#[test]
fn sync_preferences_round_trip_and_normalize_tasks() {
    let store = Store::in_memory().expect("store");

    assert_eq!(
        store.sync_preferences().expect("default sync preferences"),
        SyncPreferences::default()
    );

    store
        .set_sync_preferences(SyncPreferences {
            include_projects: false,
            include_tasks: true,
        })
        .expect("save sync preferences");

    assert_eq!(
        store.sync_preferences().expect("stored sync preferences"),
        SyncPreferences {
            include_projects: true,
            include_tasks: true,
        }
    );
}

#[test]
fn adopting_sync_tracking_moves_all_three_tables_together() {
    let previous_dir = tempfile::tempdir().expect("previous directory");
    let current_dir = tempfile::tempdir().expect("current directory");
    let previous_path = previous_dir.path().join("previous.sqlite");
    let current_path = current_dir.path().join("current.sqlite");
    let dev_target = "https://dev-api.example.test/api/sync/batches";
    let stale_target = "https://api.example.test/api/sync/batches";
    let now = Utc::now();

    let previous = Store::open(&previous_path).expect("open previous store");
    previous
        .restore_sync_states(&[cursor_at(dev_target, now, "batch-dev-latest")])
        .expect("seed previous dev cursor");
    previous
        .restore_sync_states(&[cursor_at(
            stale_target,
            now - chrono::Duration::hours(12),
            "batch-stale",
        )])
        .expect("seed previous stale cursor");
    // A batch cursor without its entity rows leaves every entity pending, which
    // re-uploads most of what carrying the cursor was supposed to spare.
    previous
        .record_entity_synced("http", dev_target, "account", "account-1", "hash-1")
        .expect("seed entity tracking");
    previous
        .record_entity_synced("http", stale_target, "account", "account-2", "hash-2")
        .expect("seed stale entity tracking");
    drop(previous);

    let current = Store::open(&current_path).expect("open current store");
    current
        .restore_sync_states(&[cursor_at(stale_target, now, "batch-fresh")])
        .expect("seed current cursor");

    let adopted = current
        .adopt_sync_tracking_from(&previous_path)
        .expect("adopt tracking");

    assert_eq!(adopted, 1, "only the target the previous store leads on");
    assert_eq!(
        current
            .sync_state("http", dev_target)
            .expect("dev cursor")
            .expect("dev cursor exists")
            .last_batch_id,
        "batch-dev-latest"
    );
    assert!(
        !current
            .entity_requires_sync("http", dev_target, "account", "account-1", "hash-1")
            .expect("read adopted entity tracking"),
        "entity tracking must travel with the cursor it belongs to"
    );
    // The fresher local cursor wins, and its companion rows are left alone rather
    // than replaced by the stale store's.
    assert_eq!(
        current
            .sync_state("http", stale_target)
            .expect("stale cursor")
            .expect("stale cursor exists")
            .last_batch_id,
        "batch-fresh"
    );
    assert!(
        current
            .entity_requires_sync("http", stale_target, "account", "account-2", "hash-2")
            .expect("read untouched entity tracking"),
        "a losing target must not import the previous store's entity rows"
    );
}

fn cursor_at(target: &str, at: DateTime<Utc>, batch: &str) -> SyncState {
    SyncState {
        sink: "http".to_string(),
        target: target.to_string(),
        last_success_at: at,
        last_batch_id: batch.to_string(),
        last_event_started_at: None,
        last_event_id: None,
        last_summary_observed_at: None,
        last_summary_id: None,
        last_task_verification_updated_at: None,
        last_task_verification_id: None,
        failure_count: 0,
        pending_resume_batch_id: None,
    }
}

#[test]
fn restoring_tracking_brings_back_entity_rows_not_just_the_cursor() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = Store::open(&directory.path().join("store.sqlite")).expect("open store");
    let target = "https://api.example.test/api/sync/batches";
    store
        .restore_sync_states(&[cursor_at(target, Utc::now(), "batch-local")])
        .expect("seed cursor");
    store
        .record_entity_synced("http", target, "account", "account-1", "hash-1")
        .expect("seed entity tracking");

    let snapshot = store
        .capture_sync_tracking("http", target)
        .expect("capture tracking");
    assert!(!snapshot.is_empty());
    store
        .clear_sync_tracking_for_target("http", target)
        .expect("clear tracking");
    assert!(
        store
            .entity_requires_sync("http", target, "account", "account-1", "hash-1")
            .expect("read entity tracking"),
        "clearing really removed the entity row"
    );

    store
        .restore_sync_tracking(&snapshot)
        .expect("restore tracking");

    assert_eq!(
        store
            .sync_state("http", target)
            .expect("read cursor")
            .expect("cursor exists")
            .last_batch_id,
        "batch-local"
    );
    // Restoring the cursor alone would leave every entity pending, resending the
    // metadata the cursor exists to spare.
    assert!(
        !store
            .entity_requires_sync("http", target, "account", "account-1", "hash-1")
            .expect("read restored entity tracking"),
        "entity tracking must come back with the cursor"
    );
}

#[test]
fn restoring_declines_once_chunks_have_landed_since_the_capture() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = Store::open(&directory.path().join("store.sqlite")).expect("open store");
    let target = "https://api.example.test/api/sync/batches";
    let captured_at = Utc::now() - chrono::Duration::minutes(5);
    store
        .restore_sync_states(&[cursor_at(target, captured_at, "batch-old")])
        .expect("seed cursor");
    store
        .record_entity_synced("http", target, "account", "account-old", "hash-old")
        .expect("seed entity tracking");

    let snapshot = store
        .capture_sync_tracking("http", target)
        .expect("capture tracking");
    store
        .clear_sync_tracking_for_target("http", target)
        .expect("clear tracking");

    // A chunked upload records progress and a resume point as each chunk lands. Those
    // records describe what the remote actually has; the snapshot predates them.
    store
        .restore_sync_states(&[cursor_at(target, Utc::now(), "batch-chunk-1")])
        .expect("record chunk progress");
    store
        .mark_pending_sync_resume("http", target, "batch-chunk-1")
        .expect("mark resume point");

    let restored = store
        .restore_sync_tracking(&snapshot)
        .expect("restore is attempted");

    assert!(!restored, "restoring must decline when the target moved on");
    assert_eq!(
        store
            .sync_state("http", target)
            .expect("read cursor")
            .expect("cursor exists")
            .last_batch_id,
        "batch-chunk-1",
        "chunk progress must not be rewound to the captured cursor"
    );
    // Re-marking the old entity as synced would make the retry skip metadata this
    // run never actually sent.
    assert!(
        store
            .entity_requires_sync("http", target, "account", "account-old", "hash-old")
            .expect("read entity tracking"),
        "stale entity tracking must not be reinstated over newer progress"
    );
}

#[test]
fn restored_task_buckets_come_back_dirty() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = Store::open(&directory.path().join("store.sqlite")).expect("open store");
    let target = "https://api.example.test/api/sync/batches";
    store
        .restore_sync_states(&[cursor_at(target, Utc::now(), "batch-old")])
        .expect("seed cursor");
    store
        .conn
        .execute(
            "INSERT INTO task_bucket_sync_state \
             (sink, target, device_id, project_bucket, dirty, payload_hash, updated_at) \
             VALUES ('http', ?1, 'device-1', 'bucket-1', 0, 'hash-1', '2026-01-01T00:00:00Z')",
            params![target],
        )
        .expect("seed a clean bucket");

    let snapshot = store
        .capture_sync_tracking("http", target)
        .expect("capture tracking");
    store
        .clear_sync_tracking_for_target("http", target)
        .expect("clear tracking");

    // While the rows are gone a scan can rebuild or delete the bucket, and the only
    // record of that is an UPDATE on the very rows that do not exist -- so it leaves
    // no trace at all. Coming back clean would declare the change already synced.
    store
        .restore_sync_tracking(&snapshot)
        .expect("restore tracking");

    let dirty: i64 = store
        .conn
        .query_row(
            "SELECT dirty FROM task_bucket_sync_state WHERE sink = 'http' AND target = ?1",
            params![target],
            |row| row.get(0),
        )
        .expect("read restored bucket");
    assert_eq!(dirty, 1, "a restored bucket must not claim to be clean");
}

#[test]
fn adopted_task_buckets_arrive_dirty() {
    let previous_dir = tempfile::tempdir().expect("previous directory");
    let current_dir = tempfile::tempdir().expect("current directory");
    let previous_path = previous_dir.path().join("previous.sqlite");
    let current_path = current_dir.path().join("current.sqlite");
    let target = "https://dev-api.example.test/api/sync/batches";

    let previous = Store::open(&previous_path).expect("open previous store");
    previous
        .restore_sync_states(&[cursor_at(target, Utc::now(), "batch-dev")])
        .expect("seed cursor");
    previous
        .conn
        .execute(
            "INSERT INTO task_bucket_sync_state \
             (sink, target, device_id, project_bucket, dirty, payload_hash, updated_at) \
             VALUES ('http', ?1, 'device-1', 'bucket-1', 0, 'hash-1', '2026-01-01T00:00:00Z')",
            params![target],
        )
        .expect("seed a clean bucket");
    drop(previous);

    let current = Store::open(&current_path).expect("open current store");
    current
        .adopt_sync_tracking_from(&previous_path)
        .expect("adopt tracking");

    // The refreshed database can hold buckets that changed or vanished since the
    // capture, and incremental selection trusts the flag alone.
    let dirty: i64 = current
        .conn
        .query_row(
            "SELECT dirty FROM task_bucket_sync_state WHERE sink = 'http' AND target = ?1",
            params![target],
            |row| row.get(0),
        )
        .expect("read adopted bucket");
    assert_eq!(dirty, 1, "a carried bucket must not claim to be clean");
}

#[test]
fn a_newer_schema_database_reports_a_version_but_refuses_to_open() {
    // The two facts `statsai-dev` relies on to tell "nothing to lose" apart from
    // "cannot read it": a clone left ahead by a schema-changing PR still reports a
    // real schema version, so it must not be mistaken for an empty or corrupt file
    // and have its sync cursors discarded.
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("newer.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).expect("create database");
        conn.execute_batch(
            r#"
            CREATE TABLE schema_migrations (
              version INTEGER PRIMARY KEY,
              applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations (version, applied_at)
            VALUES (9999, '2026-08-23T00:00:00Z');
            "#,
        )
        .expect("mark a future schema");
    }

    assert_eq!(
        crate::database_schema_version(&path).expect("probe schema"),
        Some(9999),
        "a newer clone is a real database, not an empty one"
    );
    assert!(
        Store::open(&path).is_err(),
        "and it cannot be opened, so its cursors are unreadable rather than absent"
    );
}

#[test]
fn schema_zero_does_not_mean_there_is_nothing_to_read() {
    // `statsai-dev` routes schema zero into an open-and-read rather than writing it
    // off as empty, because `stamp_legacy_database` upgrades a pre-`schema_migrations`
    // store that can already hold cursors. This pins the other half: a genuinely empty
    // file reports the same version and must still open cleanly with no cursors, so
    // the common case is not turned into a refresh-blocking error.
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("empty.sqlite");
    std::fs::write(&path, b"").expect("create an empty file");

    assert_eq!(
        crate::database_schema_version(&path).expect("probe schema"),
        Some(0)
    );
    let store = Store::open(&path).expect("an empty file is still openable");
    assert!(
        store.list_sync_states().expect("read cursors").is_empty(),
        "nothing to carry, and nothing to refuse over"
    );
}
