//! `Store::open_read_only`: what a process that must never write can open and
//! read, including while another process scans into the same store.

use chrono::{DateTime, Duration, TimeZone, Utc};
use rusqlite::Connection;
use statsai_core::{
    Confidence, CostInfo, EventId, EventSource, LocationOrigin, ModelInfo, PrivacyInfo,
    PrivacyMode, SessionInfo, SourceKind, SourceLocation, UsageCounts, UsageEvent,
    USAGE_EVENT_SCHEMA_VERSION,
};
use statsai_store::{
    CacheReportQuery, QuotaQuery, ReadOnlyOpenError, ReadStore, SessionFilter, SessionSort, Store,
    CURRENT_SCHEMA_VERSION,
};
use std::path::{Path, PathBuf};

fn started_at(index: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0)
        .single()
        .expect("time")
        + Duration::minutes(i64::from(index))
}

fn source() -> SourceLocation {
    SourceLocation::local_adapter(
        "codex",
        "read-only-test",
        "0",
        Path::new("/tmp/statsai-read-only-test"),
        LocationOrigin::Configured,
    )
}

fn event(index: u32) -> UsageEvent {
    let at = started_at(index);
    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: EventId(format!("event_read_only_{index:04}")),
        device_id: "device_read_only".to_string(),
        provider: "codex".to_string(),
        source_id: source().source_id,
        provider_account_id: None,
        subscription_id: None,
        source: EventSource {
            adapter_id: "read-only-test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "jsonl".to_string(),
            source_path_hash: None,
            source_record_id: None,
            parse_confidence: Confidence::High,
        },
        session: SessionInfo {
            session_id: format!("session_read_only_{}", index % 3),
            local_session_id_hash: Some(format!("session_hash_read_only_{}", index % 3)),
            // Session rollups list only sessions with a title or a project.
            title: Some(format!("Read-only session {}", index % 3)),
            started_at: at,
            ended_at: None,
            duration_seconds: None,
            turn_started_at: None,
        },
        model: Some(ModelInfo {
            normalized_name: Some("gpt-6-sol".to_string()),
            ..ModelInfo::default()
        }),
        usage: UsageCounts {
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            cache_read_tokens: Some(500),
            requests: Some(1),
            ..UsageCounts::default()
        },
        runtime: None,
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: None,
            provider_reported_micro_usd: None,
            pricing_source: None,
            pricing_version: None,
            confidence: Confidence::Low,
        },
        parse_evidence: None,
        project: None,
        git: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        created_at: at,
        imported_at: at,
        context: None,
    }
}

fn store_path(directory: &tempfile::TempDir) -> PathBuf {
    directory.path().join("statsai.sqlite")
}

/// A store as the CLI leaves it: written, then closed.
fn written_store(path: &Path, events: u32) {
    let store = Store::open(path).expect("create store");
    store.upsert_source(&source()).expect("source");
    let events = (0..events).map(event).collect::<Vec<_>>();
    store.insert_events(&events).expect("events");
}

fn recorded_schema_version(path: &Path) -> Option<i64> {
    statsai_store::database_schema_version(path).expect("schema version")
}

fn open_error(path: &Path) -> ReadOnlyOpenError {
    match Store::open_read_only(path) {
        Ok(_) => panic!("{} must not open read-only", path.display()),
        Err(error) => error,
    }
}

/// Runs every read the read-only store exposes, so a write hidden in any of
/// them fails the test that calls this.
fn read_everything(reader: &ReadStore) {
    let now = started_at(0) + Duration::days(1);
    reader.schema_version().expect("schema version");
    reader.data_version().expect("data version");
    reader
        .applied_pricing_ruleset_version()
        .expect("pricing ruleset");
    reader.event_count().expect("event count");
    reader.token_total().expect("token total");
    reader
        .session_rollups_in_period(
            None,
            now,
            &SessionFilter::default(),
            SessionSort::default(),
            100,
            0,
        )
        .expect("session rollups");
    reader
        .session_stats_in_period(None, now, &SessionFilter::default())
        .expect("session stats");
    reader
        .all_sync_rollup_summaries()
        .expect("sync rollup summaries");
    reader
        .daily_rollups_between("2026-01-01", "2026-12-31")
        .expect("daily rollups");
    reader
        .cache_report(&CacheReportQuery::all_time(now), &Utc)
        .expect("cache report");
    reader
        .quota_status(&QuotaQuery::default())
        .expect("quota status");
    reader
        .quota_windows(&QuotaQuery::default())
        .expect("quota windows");
    reader
        .quota_observations(&QuotaQuery::default(), true)
        .expect("quota observations");
    reader
        .weekly_reset_anchor(
            "claude_code",
            &statsai_core::ProviderAccountId("acct_read_only".to_string()),
        )
        .expect("weekly reset anchor");
    reader.list_sources().expect("sources");
    reader.list_accounts().expect("accounts");
    reader
        .list_source_account_assignments()
        .expect("source account assignments");
    reader.list_subscriptions().expect("subscriptions");
    reader.list_sync_states().expect("sync states");
    reader
        .with_read_snapshot(|snapshot| snapshot.event_count())
        .expect("read snapshot");
}

#[test]
fn a_missing_store_is_reported_and_not_created() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join(".statsai").join("statsai.sqlite");

    match open_error(&path) {
        ReadOnlyOpenError::NotFound { path: reported } => assert_eq!(reported, path),
        other => panic!("expected NotFound, got {other:?}"),
    }
    assert!(!path.exists(), "a read-only open created the database");
    assert!(
        !path.parent().expect("parent").exists(),
        "a read-only open created the store directory"
    );
}

#[test]
fn a_fresh_store_opens_and_reads_empty() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    drop(Store::open(&path).expect("create store"));

    let reader = Store::open_read_only(&path).expect("open fresh store read-only");

    assert_eq!(
        reader.schema_version().expect("schema"),
        CURRENT_SCHEMA_VERSION
    );
    assert_eq!(reader.event_count().expect("events"), 0);
    assert!(reader.list_sources().expect("sources").is_empty());
    assert!(reader
        .all_sync_rollup_summaries()
        .expect("summaries")
        .is_empty());
    read_everything(&reader);
}

/// Schema 31 is current as of this change; the test follows
/// `CURRENT_SCHEMA_VERSION`, so it keeps covering "a store this binary wrote".
#[test]
fn a_current_v31_store_reads_like_the_writer_and_is_left_byte_for_byte_unchanged() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 6);
    #[cfg(unix)]
    {
        // Store::open would tighten this to 0600; a reader must not.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen permissions");
    }
    let before = std::fs::read(&path).expect("database bytes");
    assert_eq!(recorded_schema_version(&path), Some(CURRENT_SCHEMA_VERSION));

    let reader = Store::open_read_only(&path).expect("open read-only");
    read_everything(&reader);
    assert_eq!(reader.event_count().expect("events"), 6);
    assert_eq!(reader.list_sources().expect("sources").len(), 1);
    // No writer has repriced this store, and a reader must not either.
    assert_eq!(
        reader.applied_pricing_ruleset_version().expect("ruleset"),
        None
    );
    let reader_summaries = reader.all_sync_rollup_summaries().expect("summaries");
    let reader_sessions = reader
        .session_rollups_in_period(
            None,
            started_at(0) + Duration::days(1),
            &SessionFilter::default(),
            SessionSort::default(),
            100,
            0,
        )
        .expect("sessions");
    assert!(!reader_summaries.is_empty());
    assert_eq!(reader_sessions.len(), 3);
    drop(reader);

    assert_eq!(
        std::fs::read(&path).expect("database bytes"),
        before,
        "reading changed the database file"
    );
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    if wal.exists() {
        assert_eq!(
            std::fs::metadata(&wal).expect("wal").len(),
            0,
            "a reader wrote to the WAL"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644, "a read-only open changed permissions");
    }

    // The writer sees the same rows the reader did.
    let writer = Store::open(&path).expect("writer");
    assert_eq!(
        writer.all_sync_rollup_summaries().expect("summaries"),
        reader_summaries
    );
    assert_eq!(
        writer
            .session_rollups_in_period(
                None,
                started_at(0) + Duration::days(1),
                &SessionFilter::default(),
                SessionSort::default(),
                100,
                0,
            )
            .expect("sessions"),
        reader_sessions
    );
}

#[test]
fn an_older_schema_is_refused_and_not_migrated() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 1);
    let previous = CURRENT_SCHEMA_VERSION - 1;
    Connection::open(&path)
        .expect("raw connection")
        .execute(
            "DELETE FROM schema_migrations WHERE version > ?1",
            [previous],
        )
        .expect("roll the recorded schema back");

    match open_error(&path) {
        ReadOnlyOpenError::SchemaTooOld {
            path: reported,
            found,
            expected,
        } => {
            assert_eq!(reported, path);
            assert_eq!(found, previous);
            assert_eq!(expected, CURRENT_SCHEMA_VERSION);
        }
        other => panic!("expected SchemaTooOld, got {other:?}"),
    }
    assert_eq!(recorded_schema_version(&path), Some(previous));
}

#[test]
fn a_store_from_before_schema_tracking_is_refused_as_version_zero() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    Connection::open(&path)
        .expect("raw connection")
        .execute_batch("CREATE TABLE sources (source_id TEXT PRIMARY KEY);")
        .expect("legacy table");

    match open_error(&path) {
        ReadOnlyOpenError::SchemaTooOld {
            found, expected, ..
        } => {
            assert_eq!((found, expected), (0, CURRENT_SCHEMA_VERSION));
        }
        other => panic!("expected SchemaTooOld, got {other:?}"),
    }
    // A writer's open would have stamped and migrated it.
    assert_eq!(recorded_schema_version(&path), Some(0));
}

#[test]
fn a_newer_schema_is_refused_with_both_versions() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 1);
    let future = CURRENT_SCHEMA_VERSION + 1;
    Connection::open(&path)
        .expect("raw connection")
        .execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, '2026-10-01T00:00:00Z')",
            [future],
        )
        .expect("record a future schema");

    let error = open_error(&path);
    assert!(
        error
            .to_string()
            .contains("newer than this binary supports"),
        "{error}"
    );
    match error {
        ReadOnlyOpenError::SchemaTooNew {
            found, expected, ..
        } => assert_eq!((found, expected), (future, CURRENT_SCHEMA_VERSION)),
        other => panic!("expected SchemaTooNew, got {other:?}"),
    }
    assert_eq!(recorded_schema_version(&path), Some(future));
}

#[test]
fn a_file_that_is_not_a_database_is_an_open_error() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    std::fs::write(
        &path,
        b"definitely not sqlite, but long enough to have a header....",
    )
    .expect("write junk");

    let error = open_error(&path);
    assert!(matches!(error, ReadOnlyOpenError::Open { .. }), "{error:?}");
    // The message says why, on its own and not again through `source`.
    let message = error.to_string();
    assert!(message.contains("file is not a database"), "{message}");
    assert!(std::error::Error::source(&error).is_none());
}

/// Reading the version must not set the journal mode or create
/// `schema_migrations`, as the writer's version check does: on a store in
/// rollback-journal mode, or inside a read transaction, that is a write.
#[test]
fn the_schema_version_is_read_without_writing_even_from_a_rollback_journal_store() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 1);
    let journal_mode: String = Connection::open(&path)
        .expect("raw connection")
        .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))
        .expect("leave WAL mode");
    assert_eq!(journal_mode, "delete");
    let before = std::fs::read(&path).expect("database bytes");

    let reader = Store::open_read_only(&path).expect("open read-only");
    assert_eq!(
        reader.schema_version().expect("schema version"),
        CURRENT_SCHEMA_VERSION
    );
    assert_eq!(
        reader
            .with_read_snapshot(|snapshot| snapshot.schema_version())
            .expect("schema version in a snapshot"),
        CURRENT_SCHEMA_VERSION
    );
    reader.check_schema().expect("current schema");
    read_everything(&reader);
    drop(reader);

    assert_eq!(
        std::fs::read(&path).expect("database bytes"),
        before,
        "reading the schema version changed the database file"
    );
}

#[test]
fn check_schema_reports_a_migration_made_after_the_open() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 1);
    let reader = Store::open_read_only(&path).expect("open read-only");
    reader.check_schema().expect("current schema at open");

    let future = CURRENT_SCHEMA_VERSION + 1;
    Connection::open(&path)
        .expect("writer connection")
        .execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, '2026-10-01T00:00:00Z')",
            [future],
        )
        .expect("a newer binary migrates the store");

    assert_eq!(reader.schema_version().expect("schema version"), future);
    match reader.check_schema() {
        Err(ReadOnlyOpenError::SchemaTooNew {
            path: reported,
            found,
            expected,
        }) => {
            assert_eq!(reported, path);
            assert_eq!((found, expected), (future, CURRENT_SCHEMA_VERSION));
        }
        other => panic!("expected SchemaTooNew, got {other:?}"),
    }
}

#[test]
fn every_read_refuses_a_store_migrated_after_the_open() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 1);
    let reader = Store::open_read_only(&path).expect("open read-only");
    assert_eq!(reader.event_count().expect("read at open"), 1);

    let future = CURRENT_SCHEMA_VERSION + 1;
    Connection::open(&path)
        .expect("writer connection")
        .execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, '2026-10-01T00:00:00Z')",
            [future],
        )
        .expect("a newer binary migrates the store");

    let refused = |error: anyhow::Error| match error.downcast_ref::<ReadOnlyOpenError>() {
        Some(ReadOnlyOpenError::SchemaTooNew { found, .. }) => assert_eq!(*found, future),
        _ => panic!("expected SchemaTooNew, got {error:#}"),
    };
    refused(reader.event_count().expect_err("a single read"));
    refused(reader.list_sources().expect_err("a list read"));
    refused(
        reader
            .with_read_snapshot(|reader| reader.event_count())
            .expect_err("a read snapshot"),
    );
}

#[test]
fn reads_proceed_during_a_write_and_see_it_once_committed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    let writer = Store::open(&path).expect("writer");
    let reader = Store::open_read_only(&path).expect("reader");
    let version_before = reader.data_version().expect("data version");

    writer
        .apply_scan_update(|store| {
            store.upsert_source(&source())?;
            store.insert_events(&[event(0), event(1)])?;
            // The writer holds an open write transaction here. WAL lets the
            // reader through, at the last committed state.
            assert_eq!(reader.event_count()?, 0);
            assert!(reader.list_sources()?.is_empty());
            Ok(())
        })
        .expect("scan update");

    assert_eq!(reader.event_count().expect("events"), 2);
    assert_eq!(reader.list_sources().expect("sources").len(), 1);
    assert_ne!(
        reader.data_version().expect("data version"),
        version_before,
        "data_version must move when another connection commits"
    );

    // A snapshot keeps its point in time while the writer commits more.
    reader
        .with_read_snapshot(|snapshot| {
            assert_eq!(snapshot.event_count()?, 2);
            writer.insert_events(&[event(2)])?;
            assert_eq!(snapshot.event_count()?, 2);
            assert_eq!(snapshot.all_sync_rollup_summaries()?.len(), 1);
            Ok(())
        })
        .expect("snapshot");
    assert_eq!(reader.event_count().expect("events"), 3);
}

#[test]
fn a_reader_thread_keeps_reading_while_a_writer_thread_commits() {
    const WRITES: u32 = 25;
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    let writer = Store::open(&path).expect("writer");
    writer.upsert_source(&source()).expect("source");
    let done = std::sync::atomic::AtomicBool::new(false);

    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let reader = Store::open_read_only(&path).expect("reader");
            let mut last = 0;
            let mut reads = 0u32;
            loop {
                let finished = done.load(std::sync::atomic::Ordering::Acquire);
                let count = reader.event_count().expect("read during writes");
                assert!(count >= last, "event count went backwards");
                last = count;
                reader
                    .all_sync_rollup_summaries()
                    .expect("summaries during writes");
                reads += 1;
                if finished {
                    return (last, reads);
                }
            }
        });
        for index in 0..WRITES {
            writer.insert_events(&[event(index)]).expect("write");
        }
        done.store(true, std::sync::atomic::Ordering::Release);
        let (last, reads) = reader.join().expect("reader thread");
        assert_eq!(last, u64::from(WRITES));
        assert!(reads > 0);
    });
}
