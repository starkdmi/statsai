use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};

pub const CURRENT_SCHEMA_VERSION: i64 = 7;

pub fn migrate(conn: &Connection) -> Result<()> {
    ensure_migrations_table(conn)?;
    stamp_legacy_database(conn)?;

    let current = current_schema_version(conn)?;
    for version in (current + 1)..=CURRENT_SCHEMA_VERSION {
        apply_migration(conn, version)?;
        record_migration(conn, version)?;
    }
    if current != CURRENT_SCHEMA_VERSION {
        conn.execute_batch("PRAGMA optimize;")?;
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
        _ => bail!("unsupported schema migration version {version}"),
    }
}

fn apply_migration_001(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sources (
          source_id TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          source_kind TEXT NOT NULL,
          location_origin TEXT NOT NULL,
          payload TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS provider_accounts (
          provider_account_id TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          payload TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS source_account_assignments (
          assignment_id TEXT PRIMARY KEY,
          source_id TEXT NOT NULL,
          provider TEXT NOT NULL,
          provider_account_id TEXT NOT NULL,
          started_at TEXT NOT NULL,
          ended_at TEXT,
          payload TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS source_account_assignments_lookup_idx
          ON source_account_assignments (source_id, started_at, ended_at, provider_account_id);
        CREATE TABLE IF NOT EXISTS subscriptions (
          subscription_id TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          provider_account_id TEXT,
          payload TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS local_metadata (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS usage_events (
          event_id TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          source_id TEXT NOT NULL,
          provider_account_id TEXT,
          started_at TEXT NOT NULL,
          total_tokens INTEGER NOT NULL,
          semantic_fingerprint TEXT,
          payload TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS usage_events_semantic_lookup_idx
          ON usage_events (provider, source_id, started_at, total_tokens);
        CREATE INDEX IF NOT EXISTS usage_events_semantic_fingerprint_idx
          ON usage_events (provider, source_id, semantic_fingerprint);
        CREATE TABLE IF NOT EXISTS usage_summaries (
          summary_id TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          source_id TEXT NOT NULL,
          provider_account_id TEXT,
          period_start TEXT,
          period_end TEXT,
          observed_at TEXT NOT NULL,
          total_tokens INTEGER NOT NULL,
          payload TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS usage_summaries_lookup_idx
          ON usage_summaries (provider, source_id, period_end, observed_at);
        CREATE TABLE IF NOT EXISTS daily_rollups (
          date TEXT NOT NULL,
          device_id TEXT NOT NULL,
          total_tokens INTEGER NOT NULL,
          total_events INTEGER NOT NULL,
          total_sessions INTEGER NOT NULL,
          estimated_cost_usd REAL,
          payload TEXT NOT NULL,
          PRIMARY KEY (date, device_id)
        );
        CREATE INDEX IF NOT EXISTS daily_rollups_date_idx ON daily_rollups (date);
        CREATE TABLE IF NOT EXISTS sync_rollups (
          summary_id TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          source_id TEXT NOT NULL,
          provider_account_id TEXT,
          day_key TEXT NOT NULL,
          observed_at TEXT NOT NULL,
          updated_at TEXT NOT NULL,
          payload_hash TEXT NOT NULL,
          dirty INTEGER NOT NULL DEFAULT 1,
          payload TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS sync_rollups_dirty_idx
          ON sync_rollups (dirty, updated_at, summary_id);
        CREATE INDEX IF NOT EXISTS sync_rollups_lookup_idx
          ON sync_rollups (provider, source_id, provider_account_id, day_key);
        CREATE TABLE IF NOT EXISTS scan_file_state (
          source_id TEXT NOT NULL,
          cache_key TEXT NOT NULL,
          cache_signature TEXT NOT NULL,
          synced_at TEXT NOT NULL,
          PRIMARY KEY (source_id, cache_key)
        );
        CREATE TABLE IF NOT EXISTS entity_sync_state (
          sink TEXT NOT NULL,
          target TEXT NOT NULL,
          entity_kind TEXT NOT NULL,
          entity_id TEXT NOT NULL,
          payload_hash TEXT NOT NULL,
          synced_at TEXT NOT NULL,
          PRIMARY KEY (sink, target, entity_kind, entity_id)
        );
        CREATE TABLE IF NOT EXISTS sync_state (
          sink TEXT NOT NULL,
          target TEXT NOT NULL,
          last_success_at TEXT NOT NULL,
          last_batch_id TEXT NOT NULL,
          last_event_started_at TEXT,
          last_event_id TEXT,
          last_summary_observed_at TEXT,
          last_summary_id TEXT,
          failure_count INTEGER NOT NULL DEFAULT 0,
          PRIMARY KEY (sink, target)
        );
        "#,
    )
    .context("apply schema migration 001")?;
    Ok(())
}

fn apply_migration_002(conn: &Connection) -> Result<()> {
    match conn.execute(
        "ALTER TABLE sync_state ADD COLUMN pending_resume_batch_id TEXT",
        [],
    ) {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("duplicate column name") => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn apply_migration_003(conn: &Connection) -> Result<()> {
    ensure_local_task_tables(conn)
}

fn apply_migration_004(_conn: &Connection) -> Result<()> {
    Ok(())
}

fn apply_migration_005(conn: &Connection) -> Result<()> {
    ensure_local_task_tables(conn)
}

fn apply_migration_006(conn: &Connection) -> Result<()> {
    ensure_local_task_tables(conn)?;
    conn.execute_batch("PRAGMA optimize;")?;
    Ok(())
}

fn apply_migration_007(conn: &Connection) -> Result<()> {
    ensure_local_task_tables(conn)?;
    ensure_column(
        conn,
        "sync_state",
        "last_task_verification_updated_at",
        "TEXT",
    )?;
    ensure_column(conn, "sync_state", "last_task_verification_id", "TEXT")?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS task_bucket_sync_state (
          sink TEXT NOT NULL,
          target TEXT NOT NULL,
          device_id TEXT NOT NULL,
          project_bucket TEXT NOT NULL,
          dirty INTEGER NOT NULL DEFAULT 1,
          payload_hash TEXT,
          updated_at TEXT NOT NULL,
          PRIMARY KEY (sink, target, device_id, project_bucket)
        );
        CREATE INDEX IF NOT EXISTS task_bucket_sync_state_dirty_idx
          ON task_bucket_sync_state (sink, target, device_id, dirty, project_bucket);
        "#,
    )?;
    Ok(())
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
}
