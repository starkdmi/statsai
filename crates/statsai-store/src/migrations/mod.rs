use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};

mod v1;
mod v2;

pub(crate) use v1::*;
pub(crate) use v2::*;

pub const CURRENT_SCHEMA_VERSION: i64 = 22;

pub fn migrate(conn: &Connection) -> Result<()> {
    if let Some(current) = existing_schema_version(conn)? {
        ensure_supported_schema(current)?;
    }

    ensure_migrations_table(conn)?;
    stamp_legacy_database(conn)?;

    let current = current_schema_version(conn)?;
    ensure_supported_schema(current)?;
    for version in (current + 1)..=CURRENT_SCHEMA_VERSION {
        apply_migration(conn, version)?;
        record_migration(conn, version)?;
    }
    if current != CURRENT_SCHEMA_VERSION {
        conn.execute_batch("PRAGMA optimize;")?;
    }

    Ok(())
}

fn existing_schema_version(conn: &Connection) -> Result<Option<i64>> {
    let has_migrations_table = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !has_migrations_table {
        return Ok(None);
    }
    current_schema_version(conn).map(Some)
}

fn ensure_supported_schema(current: i64) -> Result<()> {
    if current > CURRENT_SCHEMA_VERSION {
        bail!(
            "database schema version {current} is newer than this StatsAI binary supports ({CURRENT_SCHEMA_VERSION}); upgrade StatsAI or use a compatible database"
        );
    }
    Ok(())
}

fn ensure_migrations_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA busy_timeout = 5000;
        CREATE TABLE IF NOT EXISTS schema_migrations (
          version INTEGER PRIMARY KEY,
          applied_at TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

fn current_schema_version(conn: &Connection) -> Result<i64> {
    let version = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten()
        .unwrap_or(0);
    Ok(version)
}

fn record_migration(conn: &Connection, version: i64) -> Result<()> {
    let applied_at = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        (version, applied_at),
    )?;
    Ok(())
}

fn stamp_legacy_database(conn: &Connection) -> Result<()> {
    let migration_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })?;
    if migration_count > 0 {
        return Ok(());
    }

    let has_sources: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'sources'",
        [],
        |row| row.get(0),
    )?;
    if has_sources == 0 {
        return Ok(());
    }

    record_migration(conn, 1)?;
    if sync_state_has_pending_resume_batch_id(conn)? {
        record_migration(conn, 2)?;
    }
    Ok(())
}

fn sync_state_has_pending_resume_batch_id(conn: &Connection) -> Result<bool> {
    let mut stmt = conn.prepare("PRAGMA table_info(sync_state)")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == "pending_resume_batch_id" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn apply_migration(conn: &Connection, version: i64) -> Result<()> {
    match version {
        1 => apply_migration_001(conn),
        2 => apply_migration_002(conn),
        3 => apply_migration_003(conn),
        4 => apply_migration_004(conn),
        5 => apply_migration_005(conn),
        6 => apply_migration_006(conn),
        7 => apply_migration_007(conn),
        8 => apply_migration_008(conn),
        9 => apply_migration_009(conn),
        10 => apply_migration_010(conn),
        11 => apply_migration_011(conn),
        12 => apply_migration_012(conn),
        13 => apply_migration_013(conn),
        14 => apply_migration_014(conn),
        15 => apply_migration_015(conn),
        16 => apply_migration_016(conn),
        17 => apply_migration_017(conn),
        18 => apply_migration_018(conn),
        19 => apply_migration_019(conn),
        20 => apply_migration_020(conn),
        21 => apply_migration_021(conn),
        22 => apply_migration_022(conn),
        _ => bail!("unsupported schema migration version {version}"),
    }
}

fn ensure_local_task_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS task_spans (
          span_id TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          source_id TEXT NOT NULL,
          project_bucket TEXT NOT NULL,
          started_at TEXT NOT NULL,
          ended_at TEXT,
          title TEXT NOT NULL,
          normalized_title TEXT NOT NULL,
          is_meta INTEGER NOT NULL DEFAULT 0,
          confidence TEXT NOT NULL,
          source_file_path_hash TEXT,
          payload TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS task_spans_bucket_idx
          ON task_spans (project_bucket, started_at, span_id);
        CREATE INDEX IF NOT EXISTS task_spans_source_idx
          ON task_spans (source_id, started_at, span_id);
        CREATE INDEX IF NOT EXISTS task_spans_source_file_idx
          ON task_spans (source_id, source_file_path_hash, started_at, span_id);

        CREATE TABLE IF NOT EXISTS task_span_event_links (
          span_id TEXT NOT NULL,
          event_id TEXT NOT NULL,
          PRIMARY KEY (span_id, event_id)
        );
        CREATE INDEX IF NOT EXISTS task_span_event_links_event_idx
          ON task_span_event_links (event_id);

        CREATE TABLE IF NOT EXISTS task_work_items (
          work_item_id TEXT PRIMARY KEY,
          anchor_span_id TEXT NOT NULL,
          project_bucket TEXT NOT NULL,
          started_at TEXT NOT NULL,
          ended_at TEXT NOT NULL,
          status TEXT NOT NULL,
          confidence TEXT NOT NULL,
          total_tokens INTEGER NOT NULL,
          payload TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS task_work_items_bucket_idx
          ON task_work_items (project_bucket, ended_at, work_item_id);
        CREATE INDEX IF NOT EXISTS task_work_items_bucket_start_idx
          ON task_work_items (project_bucket, started_at, work_item_id);
        CREATE INDEX IF NOT EXISTS task_work_items_status_idx
          ON task_work_items (status, confidence, total_tokens, ended_at);

        CREATE TABLE IF NOT EXISTS task_work_item_members (
          work_item_id TEXT NOT NULL,
          span_id TEXT NOT NULL,
          ordinal INTEGER NOT NULL,
          PRIMARY KEY (work_item_id, span_id)
        );
        CREATE INDEX IF NOT EXISTS task_work_item_members_span_idx
          ON task_work_item_members (span_id, ordinal);
        CREATE INDEX IF NOT EXISTS task_work_item_members_work_item_ordinal_idx
          ON task_work_item_members (work_item_id, ordinal, span_id);

        CREATE TABLE IF NOT EXISTS task_verifications (
          verification_id TEXT PRIMARY KEY,
          action_kind TEXT NOT NULL,
          action_key TEXT NOT NULL UNIQUE,
          updated_at TEXT NOT NULL,
          payload TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS task_verifications_kind_idx
          ON task_verifications (action_kind, updated_at, verification_id);
        "#,
    )?;
    ensure_column(
        conn,
        "task_spans",
        "event_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "task_spans",
        "has_usage_evidence",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "task_spans",
        "total_messages",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "task_spans",
        "user_messages",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "task_spans",
        "assistant_messages",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "task_spans",
        "developer_messages",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = conn.prepare(&pragma)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(());
        }
    }
    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
    conn.execute(&sql, [])?;
    Ok(())
}

pub fn schema_version(conn: &Connection) -> Result<i64> {
    ensure_migrations_table(conn)?;
    current_schema_version(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn fresh_database_applies_all_schema_migrations() {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        migrate(&conn).expect("migrate fresh database");
        assert_eq!(
            schema_version(&conn).expect("read version"),
            CURRENT_SCHEMA_VERSION
        );
        assert!(sync_state_has_pending_resume_batch_id(&conn).expect("inspect sync_state"));
        assert!(table_exists(&conn, "task_bucket_sync_state"));
        assert!(column_exists(&conn, "scan_file_state", "tasks_collected"));
        assert!(table_exists(&conn, "archive_missing_content_state"));
        assert!(table_exists(&conn, "archive_artifact_dependencies"));
        assert!(index_exists(&conn, "archive_items_source_record_idx"));
        assert!(table_exists(&conn, "filtered_conversations"));
        assert!(table_exists(&conn, "privacy_findings"));
        assert!(table_exists(&conn, "privacy_pseudonyms"));
        assert!(table_exists(&conn, "privacy_filter_failures"));
        assert!(table_exists(&conn, "privacy_dataset_identity"));
        assert!(table_exists(&conn, "code_trace_edits"));
        assert!(table_exists(&conn, "code_git_scans"));
        assert!(table_exists(&conn, "code_git_commits"));
        assert!(table_exists(&conn, "code_git_identities"));
        assert!(table_exists(&conn, "code_change_matches"));
        assert!(table_exists(&conn, "code_change_metrics"));
        assert!(table_exists(&conn, "account_identity_observations"));
        assert!(table_exists(&conn, "account_plan_observations"));
        assert!(table_exists(&conn, "conversation_account_bindings"));
        assert!(table_exists(&conn, "account_evidence_checkpoints"));
        assert!(column_exists(
            &conn,
            "account_evidence_checkpoints",
            "checkpoint_row_fingerprint"
        ));
        assert!(index_exists(
            &conn,
            "quota_window_observations_observation_idx"
        ));
    }

    #[test]
    fn newer_database_schema_is_rejected_without_modification() {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        conn.execute_batch(
            r#"
            CREATE TABLE schema_migrations (
              version INTEGER PRIMARY KEY,
              applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations (version, applied_at)
            VALUES (23, '2026-08-23T00:00:00Z');
            "#,
        )
        .expect("create future schema marker");

        let error = migrate(&conn).expect_err("schema 23 must be rejected by schema 22 binary");

        assert_eq!(
            error.to_string(),
            "database schema version 23 is newer than this StatsAI binary supports (22); upgrade StatsAI or use a compatible database"
        );
        assert_eq!(
            current_schema_version(&conn).expect("read unchanged version"),
            23
        );
    }

    #[test]
    fn legacy_database_without_migration_history_is_stamped_and_upgraded() {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        apply_migration_001(&conn).expect("apply legacy baseline schema");

        migrate(&conn).expect("migrate legacy database");
        assert_eq!(
            schema_version(&conn).expect("read version"),
            CURRENT_SCHEMA_VERSION
        );
        assert!(sync_state_has_pending_resume_batch_id(&conn).expect("inspect sync_state"));
        assert!(table_exists(&conn, "task_bucket_sync_state"));
        assert!(column_exists(&conn, "scan_file_state", "tasks_collected"));
    }

    #[test]
    fn migration_twenty_one_retries_after_column_was_added_without_history() {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        ensure_migrations_table(&conn).expect("ensure migrations table");
        for version in 1..=20 {
            apply_migration(&conn, version).expect("apply migration");
            record_migration(&conn, version).expect("record migration");
        }
        apply_migration_021(&conn).expect("interrupted migration schema change");
        assert_eq!(
            current_schema_version(&conn).expect("version before retry"),
            20
        );

        migrate(&conn).expect("retry migration");

        assert_eq!(
            schema_version(&conn).expect("version after retry"),
            CURRENT_SCHEMA_VERSION
        );
        assert!(column_exists(
            &conn,
            "account_evidence_checkpoints",
            "checkpoint_row_fingerprint"
        ));
    }

    #[test]
    fn version_eleven_archive_receives_source_record_index() {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        ensure_migrations_table(&conn).expect("ensure migrations table");
        for version in 1..=11 {
            apply_migration(&conn, version).expect("apply pre-index migration");
            record_migration(&conn, version).expect("record pre-index migration");
        }
        assert!(!index_exists(&conn, "archive_items_source_record_idx"));

        migrate(&conn).expect("migrate version eleven database");

        assert_eq!(
            schema_version(&conn).expect("read version"),
            CURRENT_SCHEMA_VERSION
        );
        assert!(index_exists(&conn, "archive_items_source_record_idx"));
    }

    #[test]
    fn version_twelve_archive_receives_privacy_dataset_tables() {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        ensure_migrations_table(&conn).expect("ensure migrations table");
        for version in 1..=12 {
            apply_migration(&conn, version).expect("apply pre-privacy migration");
            record_migration(&conn, version).expect("record pre-privacy migration");
        }
        assert!(!table_exists(&conn, "filtered_conversations"));

        migrate(&conn).expect("migrate version twelve database");

        assert_eq!(
            schema_version(&conn).expect("read version"),
            CURRENT_SCHEMA_VERSION
        );
        assert!(table_exists(&conn, "filtered_conversations"));
        assert!(table_exists(&conn, "privacy_findings"));
        assert!(table_exists(&conn, "privacy_pseudonyms"));
        assert!(table_exists(&conn, "privacy_filter_failures"));
        assert!(table_exists(&conn, "privacy_dataset_identity"));
    }

    #[test]
    fn version_thirteen_privacy_schema_receives_dataset_identity() {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        ensure_migrations_table(&conn).expect("ensure migrations table");
        for version in 1..=13 {
            apply_migration(&conn, version).expect("apply pre-identity migration");
            record_migration(&conn, version).expect("record pre-identity migration");
        }
        assert!(!table_exists(&conn, "privacy_dataset_identity"));

        migrate(&conn).expect("migrate version thirteen database");

        assert_eq!(
            schema_version(&conn).expect("read version"),
            CURRENT_SCHEMA_VERSION
        );
        assert!(table_exists(&conn, "privacy_dataset_identity"));
    }

    #[test]
    fn version_four_legacy_task_schema_receives_local_task_tables() {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        apply_migration_001(&conn).expect("apply migration 001");
        apply_migration_002(&conn).expect("apply migration 002");
        ensure_migrations_table(&conn).expect("ensure migrations table");
        conn.execute_batch(
            r#"
            CREATE TABLE tasks (
              task_id TEXT PRIMARY KEY,
              payload TEXT NOT NULL
            );
            CREATE TABLE task_rollups (
              task_rollup_id TEXT PRIMARY KEY,
              payload TEXT NOT NULL
            );
            CREATE TABLE task_evidence (
              evidence_id TEXT PRIMARY KEY,
              payload TEXT NOT NULL
            );
            "#,
        )
        .expect("create legacy task tables");
        for version in 1..=4 {
            record_migration(&conn, version).expect("record migration");
        }

        migrate(&conn).expect("migrate version four legacy database");

        assert_eq!(
            schema_version(&conn).expect("read version"),
            CURRENT_SCHEMA_VERSION
        );
        assert!(table_exists(&conn, "task_spans"));
        assert!(table_exists(&conn, "task_span_event_links"));
        assert!(table_exists(&conn, "task_work_items"));
        assert!(table_exists(&conn, "task_work_item_members"));
        assert!(table_exists(&conn, "task_verifications"));
        assert!(table_exists(&conn, "task_bucket_sync_state"));
        assert!(column_exists(&conn, "scan_file_state", "tasks_collected"));
        assert!(table_exists(&conn, "tasks"));
        assert!(table_exists(&conn, "task_rollups"));
        assert!(table_exists(&conn, "task_evidence"));
    }

    fn table_exists(conn: &Connection, table_name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table_name],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .expect("read sqlite_master")
    }

    fn column_exists(conn: &Connection, table_name: &str, column_name: &str) -> bool {
        let sql = format!("PRAGMA table_info({table_name})");
        let mut statement = conn.prepare(&sql).expect("prepare table_info");
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query table_info");
        for column in columns {
            if column.expect("read column") == column_name {
                return true;
            }
        }
        false
    }

    fn index_exists(conn: &Connection, index_name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
            [index_name],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .expect("read sqlite_master")
    }
}
