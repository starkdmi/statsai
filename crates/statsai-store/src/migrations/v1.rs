use super::*;

pub(crate) fn apply_migration_001(conn: &Connection) -> Result<()> {
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

pub(crate) fn apply_migration_002(conn: &Connection) -> Result<()> {
    match conn.execute(
        "ALTER TABLE sync_state ADD COLUMN pending_resume_batch_id TEXT",
        [],
    ) {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("duplicate column name") => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn apply_migration_003(conn: &Connection) -> Result<()> {
    ensure_local_task_tables(conn)
}

pub(crate) fn apply_migration_004(_conn: &Connection) -> Result<()> {
    Ok(())
}

pub(crate) fn apply_migration_005(conn: &Connection) -> Result<()> {
    ensure_local_task_tables(conn)
}

pub(crate) fn apply_migration_006(conn: &Connection) -> Result<()> {
    ensure_local_task_tables(conn)?;
    conn.execute_batch("PRAGMA optimize;")?;
    Ok(())
}

pub(crate) fn apply_migration_007(conn: &Connection) -> Result<()> {
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

pub(crate) fn apply_migration_008(conn: &Connection) -> Result<()> {
    ensure_column(
        conn,
        "scan_file_state",
        "tasks_collected",
        "INTEGER NOT NULL DEFAULT 0",
    )
}

pub(crate) fn apply_migration_009(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS archive_conversations (
          conversation_id TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          source_id TEXT NOT NULL,
          native_conversation_id TEXT NOT NULL,
          title TEXT,
          project_json TEXT,
          started_at TEXT,
          updated_at TEXT,
          completeness TEXT NOT NULL,
          missing_content_count INTEGER NOT NULL DEFAULT 0,
          imported_at TEXT NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS archive_conversations_native_idx
          ON archive_conversations (provider, native_conversation_id);
        CREATE INDEX IF NOT EXISTS archive_conversations_source_idx
          ON archive_conversations (source_id, updated_at, conversation_id);
        CREATE INDEX IF NOT EXISTS archive_conversations_provider_idx
          ON archive_conversations (provider, updated_at, conversation_id);

        CREATE TABLE IF NOT EXISTS archive_items (
          item_id TEXT PRIMARY KEY,
          conversation_id TEXT NOT NULL,
          native_item_id TEXT,
          source_record_id TEXT,
          ordinal INTEGER NOT NULL,
          kind TEXT NOT NULL,
          role TEXT,
          created_at TEXT,
          model_json TEXT,
          tool_name TEXT,
          tool_call_id TEXT,
          status TEXT,
          usage_json TEXT,
          FOREIGN KEY (conversation_id) REFERENCES archive_conversations(conversation_id)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS archive_items_order_idx
          ON archive_items (conversation_id, ordinal, item_id);
        CREATE INDEX IF NOT EXISTS archive_items_created_idx
          ON archive_items (created_at, item_id);

        CREATE TABLE IF NOT EXISTS archive_content_parts (
          content_id TEXT PRIMARY KEY,
          item_id TEXT NOT NULL,
          ordinal INTEGER NOT NULL,
          kind TEXT NOT NULL,
          mime_type TEXT,
          name TEXT,
          text_content TEXT,
          binary_content BLOB,
          external_uri TEXT,
          content_hash TEXT NOT NULL,
          original_bytes INTEGER NOT NULL,
          truncated INTEGER NOT NULL DEFAULT 0,
          FOREIGN KEY (item_id) REFERENCES archive_items(item_id)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS archive_content_parts_order_idx
          ON archive_content_parts (item_id, ordinal, content_id);
        CREATE INDEX IF NOT EXISTS archive_content_parts_hash_idx
          ON archive_content_parts (content_hash);

        CREATE VIRTUAL TABLE IF NOT EXISTS archive_content_fts USING fts5(
          text_content,
          content='archive_content_parts',
          content_rowid='rowid',
          tokenize='unicode61'
        );
        CREATE TRIGGER IF NOT EXISTS archive_content_parts_ai AFTER INSERT ON archive_content_parts
        WHEN new.text_content IS NOT NULL BEGIN
          INSERT INTO archive_content_fts(rowid, text_content)
          VALUES (new.rowid, new.text_content);
        END;
        CREATE TRIGGER IF NOT EXISTS archive_content_parts_ad AFTER DELETE ON archive_content_parts
        WHEN old.text_content IS NOT NULL BEGIN
          INSERT INTO archive_content_fts(archive_content_fts, rowid, text_content)
          VALUES ('delete', old.rowid, old.text_content);
        END;
        CREATE TRIGGER IF NOT EXISTS archive_content_parts_au AFTER UPDATE ON archive_content_parts
        BEGIN
          INSERT INTO archive_content_fts(archive_content_fts, rowid, text_content)
          SELECT 'delete', old.rowid, old.text_content
          WHERE old.text_content IS NOT NULL;
          INSERT INTO archive_content_fts(rowid, text_content)
          SELECT new.rowid, new.text_content
          WHERE new.text_content IS NOT NULL;
        END;

        CREATE TABLE IF NOT EXISTS archive_import_state (
          source_id TEXT NOT NULL,
          cache_key TEXT NOT NULL,
          cache_signature TEXT NOT NULL,
          collected_at TEXT NOT NULL,
          PRIMARY KEY (source_id, cache_key)
        );
        "#,
    )?;
    Ok(())
}

pub(crate) fn apply_migration_010(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS archive_missing_content_state (
          conversation_id TEXT NOT NULL,
          scope_id TEXT NOT NULL,
          missing_content_count INTEGER NOT NULL,
          updated_at TEXT NOT NULL,
          PRIMARY KEY (conversation_id, scope_id),
          FOREIGN KEY (conversation_id) REFERENCES archive_conversations(conversation_id)
        );
        "#,
    )?;
    Ok(())
}

pub(crate) fn apply_migration_011(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS archive_artifact_dependencies (
          source_id TEXT NOT NULL,
          cache_key TEXT NOT NULL,
          artifact_path TEXT NOT NULL,
          metadata_signature TEXT NOT NULL,
          PRIMARY KEY (source_id, cache_key, artifact_path)
        );
        "#,
    )?;
    Ok(())
}
