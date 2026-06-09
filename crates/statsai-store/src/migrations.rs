use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};

pub const CURRENT_SCHEMA_VERSION: i64 = 2;

pub fn migrate(conn: &Connection) -> Result<()> {
    ensure_migrations_table(conn)?;
    stamp_legacy_database(conn)?;

    let current = current_schema_version(conn)?;
    for version in (current + 1)..=CURRENT_SCHEMA_VERSION {
        apply_migration(conn, version)?;
        record_migration(conn, version)?;
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
    let migration_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
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
        assert_eq!(schema_version(&conn).expect("read version"), CURRENT_SCHEMA_VERSION);
        assert!(sync_state_has_pending_resume_batch_id(&conn).expect("inspect sync_state"));
    }

    #[test]
    fn legacy_database_without_migration_history_is_stamped_and_upgraded() {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        apply_migration_001(&conn).expect("apply legacy baseline schema");

        migrate(&conn).expect("migrate legacy database");
        assert_eq!(schema_version(&conn).expect("read version"), CURRENT_SCHEMA_VERSION);
        assert!(sync_state_has_pending_resume_batch_id(&conn).expect("inspect sync_state"));
    }
}