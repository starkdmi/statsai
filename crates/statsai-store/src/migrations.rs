use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};

pub const CURRENT_SCHEMA_VERSION: i64 = 21;

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

fn apply_migration_008(conn: &Connection) -> Result<()> {
    ensure_column(
        conn,
        "scan_file_state",
        "tasks_collected",
        "INTEGER NOT NULL DEFAULT 0",
    )
}

fn apply_migration_009(conn: &Connection) -> Result<()> {
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

fn apply_migration_010(conn: &Connection) -> Result<()> {
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

fn apply_migration_011(conn: &Connection) -> Result<()> {
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

fn apply_migration_012(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS archive_items_source_record_idx
          ON archive_items (source_record_id, item_id);
        "#,
    )?;
    Ok(())
}

fn apply_migration_013(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS filtered_conversations (
          conversation_id TEXT PRIMARY KEY,
          dataset_key TEXT NOT NULL UNIQUE,
          input_fingerprint TEXT NOT NULL,
          policy_fingerprint TEXT NOT NULL,
          payload TEXT NOT NULL,
          finding_count INTEGER NOT NULL,
          succeeded_at TEXT NOT NULL,
          FOREIGN KEY (conversation_id) REFERENCES archive_conversations(conversation_id)
            ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS filtered_conversations_policy_idx
          ON filtered_conversations (policy_fingerprint, succeeded_at, dataset_key);

        CREATE TABLE IF NOT EXISTS privacy_findings (
          conversation_id TEXT NOT NULL,
          field_path TEXT NOT NULL,
          start_offset INTEGER NOT NULL,
          end_offset INTEGER NOT NULL,
          category TEXT NOT NULL,
          detector TEXT NOT NULL,
          confidence TEXT,
          replacement TEXT NOT NULL,
          PRIMARY KEY (conversation_id, field_path, start_offset, end_offset, category, detector),
          FOREIGN KEY (conversation_id) REFERENCES filtered_conversations(conversation_id)
            ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS privacy_pseudonyms (
          category TEXT NOT NULL,
          value_hmac TEXT NOT NULL,
          alias INTEGER NOT NULL,
          created_at TEXT NOT NULL,
          PRIMARY KEY (category, value_hmac),
          UNIQUE (category, alias)
        );

        CREATE TABLE IF NOT EXISTS privacy_filter_failures (
          failure_id INTEGER PRIMARY KEY AUTOINCREMENT,
          conversation_id TEXT NOT NULL,
          input_fingerprint TEXT NOT NULL,
          policy_fingerprint TEXT NOT NULL,
          failed_stage TEXT NOT NULL,
          error_code TEXT NOT NULL,
          attempted_at TEXT NOT NULL,
          FOREIGN KEY (conversation_id) REFERENCES archive_conversations(conversation_id)
            ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS privacy_filter_failures_conversation_idx
          ON privacy_filter_failures (conversation_id, attempted_at DESC);

        CREATE TRIGGER IF NOT EXISTS archive_conversations_privacy_delete
        AFTER DELETE ON archive_conversations
        BEGIN
          DELETE FROM privacy_filter_failures WHERE conversation_id = OLD.conversation_id;
          DELETE FROM privacy_findings WHERE conversation_id = OLD.conversation_id;
          DELETE FROM filtered_conversations WHERE conversation_id = OLD.conversation_id;
        END;
        "#,
    )?;
    Ok(())
}

fn apply_migration_014(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS privacy_dataset_identity (
          singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
          key_verifier TEXT NOT NULL,
          created_at TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

fn apply_migration_015(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS code_trace_edits (
          trace_edit_id TEXT PRIMARY KEY,
          source_id TEXT NOT NULL,
          conversation_id TEXT NOT NULL,
          source_record_id TEXT NOT NULL,
          occurred_at TEXT,
          project_id TEXT,
          repository_path TEXT,
          relative_path TEXT NOT NULL,
          payload TEXT NOT NULL,
          FOREIGN KEY (conversation_id) REFERENCES archive_conversations(conversation_id)
            ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS code_trace_edits_source_record_idx
          ON code_trace_edits (source_id, source_record_id, trace_edit_id);
        CREATE INDEX IF NOT EXISTS code_trace_edits_repository_idx
          ON code_trace_edits (repository_path, occurred_at, trace_edit_id);

        CREATE TABLE IF NOT EXISTS code_trace_coverage (
          source_id TEXT NOT NULL,
          cache_key TEXT NOT NULL,
          coverage TEXT NOT NULL,
          updated_at TEXT NOT NULL,
          PRIMARY KEY (source_id, cache_key)
        );

        CREATE TABLE IF NOT EXISTS code_git_scans (
          repository_hash TEXT PRIMARY KEY,
          repository_path TEXT NOT NULL,
          coverage TEXT NOT NULL,
          scanned_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS code_git_commits (
          deduplication_id TEXT PRIMARY KEY,
          repository_hash TEXT NOT NULL,
          commit_hash TEXT NOT NULL,
          committed_at TEXT NOT NULL,
          project_id TEXT,
          payload TEXT NOT NULL,
          UNIQUE (repository_hash, commit_hash),
          FOREIGN KEY (repository_hash) REFERENCES code_git_scans(repository_hash)
            ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS code_git_commits_day_idx
          ON code_git_commits (committed_at, repository_hash, commit_hash);

        CREATE TABLE IF NOT EXISTS code_change_matches (
          match_id TEXT PRIMARY KEY,
          trace_edit_id TEXT NOT NULL,
          commit_deduplication_id TEXT NOT NULL,
          confidence TEXT NOT NULL,
          payload TEXT NOT NULL,
          FOREIGN KEY (trace_edit_id) REFERENCES code_trace_edits(trace_edit_id)
            ON DELETE CASCADE,
          FOREIGN KEY (commit_deduplication_id) REFERENCES code_git_commits(deduplication_id)
            ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS code_change_matches_commit_idx
          ON code_change_matches (commit_deduplication_id, confidence, match_id);

        CREATE TABLE IF NOT EXISTS code_change_metrics (
          metric_id TEXT PRIMARY KEY,
          device_id TEXT NOT NULL,
          day TEXT NOT NULL,
          project_id TEXT,
          repository_hash TEXT,
          commit_hash TEXT,
          kind TEXT NOT NULL,
          payload TEXT NOT NULL,
          dirty INTEGER NOT NULL DEFAULT 1
        );
        CREATE INDEX IF NOT EXISTS code_change_metrics_sync_idx
          ON code_change_metrics (dirty, day, metric_id);
        CREATE INDEX IF NOT EXISTS code_change_metrics_dashboard_idx
          ON code_change_metrics (day, project_id, device_id, kind);
        "#,
    )?;
    Ok(())
}

/// Gives reconstructed edits the scan cache key they belong to.
///
/// Reconciliation previously removed an archive file's edits by matching a
/// `{cache_key}:` prefix on `source_record_id`. That made the record-id shape
/// an unwritten schema contract, could not use an index, and would silently
/// stop deleting for any provider that numbered its records differently.
/// Existing rows are backfilled from the prefix they were written with, which
/// is correct for every shape recorded up to this version.
fn apply_migration_016(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        ALTER TABLE code_trace_edits ADD COLUMN cache_key TEXT;
        UPDATE code_trace_edits
        SET cache_key = (
          SELECT s.cache_key
          FROM archive_import_state s
          WHERE s.source_id = code_trace_edits.source_id
            AND substr(code_trace_edits.source_record_id, 1, length(s.cache_key) + 1)
                = s.cache_key || ':'
        )
        WHERE cache_key IS NULL;
        -- An edit whose archive file is no longer on record cannot be
        -- reconciled by any key, so it would linger unreachable and be counted
        -- again beside its replacement the next time that file is imported.
        DELETE FROM code_trace_edits WHERE cache_key IS NULL;
        CREATE INDEX IF NOT EXISTS code_trace_edits_cache_key_idx
          ON code_trace_edits (source_id, cache_key);
        "#,
    )?;
    Ok(())
}

/// Remembers which committer identities a repository has been scanned under.
///
/// Scans previously recognised only the address `user.email` held at the moment
/// they ran, so changing it turned every earlier commit into someone else's
/// work: the scan succeeded with zero commits, and refresh, treating a
/// successful scan as authoritative, deleted the commit rows it had already
/// measured and retired their metrics remotely.
///
/// Addresses are stored blinded because equality is all matching needs and an
/// email identifies a person, matching how commit hashes and repository
/// identity are already held. The table is local-only and never synced.
///
/// Existing stores need no backfill: their next scan runs under the address the
/// previous filter already matched, which is exactly the identity to remember.
fn apply_migration_017(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS code_git_identities (
          repository_hash TEXT NOT NULL,
          identity_hash TEXT NOT NULL,
          first_seen_at TEXT NOT NULL,
          PRIMARY KEY (repository_hash, identity_hash),
          FOREIGN KEY (repository_hash) REFERENCES code_git_scans(repository_hash)
            ON DELETE CASCADE
        );
        "#,
    )?;
    Ok(())
}

fn apply_migration_018(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS quota_payloads (
          payload_hash TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          payload TEXT NOT NULL,
          created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS quota_observations (
          observation_id TEXT PRIMARY KEY,
          semantic_fingerprint TEXT NOT NULL,
          provider TEXT NOT NULL,
          source_id TEXT NOT NULL,
          provider_account_id TEXT,
          observed_at TEXT NOT NULL,
          source_file_path_hash TEXT NOT NULL,
          source_record_id TEXT NOT NULL,
          usage_event_id TEXT,
          usage_link_kind TEXT NOT NULL,
          payload_hash TEXT NOT NULL,
          payload TEXT NOT NULL,
          FOREIGN KEY (payload_hash) REFERENCES quota_payloads(payload_hash)
        );
        CREATE INDEX IF NOT EXISTS quota_observations_scope_idx
          ON quota_observations (provider, provider_account_id, observed_at, observation_id);
        CREATE INDEX IF NOT EXISTS quota_observations_source_idx
          ON quota_observations (source_id, source_file_path_hash, observed_at, observation_id);
        CREATE INDEX IF NOT EXISTS quota_observations_semantic_idx
          ON quota_observations (semantic_fingerprint, observation_id);

        CREATE TABLE IF NOT EXISTS quota_window_observations (
          window_observation_id TEXT PRIMARY KEY,
          observation_id TEXT NOT NULL,
          provider_slot TEXT NOT NULL,
          limit_id TEXT,
          window_minutes INTEGER NOT NULL,
          used_percent REAL NOT NULL,
          resets_at INTEGER NOT NULL,
          payload TEXT NOT NULL,
          FOREIGN KEY (observation_id) REFERENCES quota_observations(observation_id)
            ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS quota_window_observations_reconstruct_idx
          ON quota_window_observations (window_minutes, limit_id, resets_at, observation_id);
        "#,
    )?;
    Ok(())
}

fn apply_migration_019(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS account_identity_observations (
          observation_id TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          source_id TEXT NOT NULL,
          provider_account_id TEXT,
          observed_at TEXT NOT NULL,
          evidence_kind TEXT NOT NULL,
          conversation_id_hash TEXT,
          payload TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS account_identity_observations_source_idx
          ON account_identity_observations (source_id, observed_at, observation_id);
        CREATE INDEX IF NOT EXISTS account_identity_observations_account_idx
          ON account_identity_observations
             (provider, provider_account_id, observed_at, observation_id);

        CREATE TABLE IF NOT EXISTS account_plan_observations (
          observation_id TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          source_id TEXT NOT NULL,
          provider_account_id TEXT,
          observed_at TEXT NOT NULL,
          active_from TEXT,
          active_until TEXT,
          plan_name TEXT NOT NULL,
          evidence_kind TEXT NOT NULL,
          payload TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS account_plan_observations_account_idx
          ON account_plan_observations
             (provider, provider_account_id, observed_at, observation_id);
        CREATE INDEX IF NOT EXISTS account_plan_observations_source_idx
          ON account_plan_observations (source_id, observed_at, observation_id);

        CREATE TABLE IF NOT EXISTS conversation_account_bindings (
          binding_id TEXT PRIMARY KEY,
          provider TEXT NOT NULL,
          source_id TEXT NOT NULL,
          provider_account_id TEXT NOT NULL,
          conversation_id_hash TEXT NOT NULL,
          turn_id_hash TEXT,
          observed_at TEXT NOT NULL,
          payload TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS conversation_account_bindings_lookup_idx
          ON conversation_account_bindings
             (source_id, conversation_id_hash, turn_id_hash, observed_at);
        "#,
    )?;
    Ok(())
}

fn apply_migration_020(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS account_evidence_checkpoints (
          source_id TEXT NOT NULL,
          artifact_path_hash TEXT NOT NULL,
          parser_version TEXT NOT NULL,
          maximum_row_id INTEGER NOT NULL,
          database_size INTEGER NOT NULL,
          database_modified_nanos INTEGER NOT NULL,
          wal_size INTEGER NOT NULL,
          wal_modified_nanos INTEGER NOT NULL,
          payload TEXT NOT NULL,
          updated_at TEXT NOT NULL,
          PRIMARY KEY (source_id, artifact_path_hash, parser_version)
        );
        CREATE INDEX IF NOT EXISTS account_evidence_checkpoints_source_idx
          ON account_evidence_checkpoints (source_id, artifact_path_hash, parser_version);
        "#,
    )?;
    Ok(())
}

fn apply_migration_021(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "ALTER TABLE account_evidence_checkpoints ADD COLUMN checkpoint_row_fingerprint TEXT;",
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
            VALUES (22, '2026-08-23T00:00:00Z');
            "#,
        )
        .expect("create future schema marker");

        let error = migrate(&conn).expect_err("schema 22 must be rejected by schema 21 binary");

        assert_eq!(
            error.to_string(),
            "database schema version 22 is newer than this StatsAI binary supports (21); upgrade StatsAI or use a compatible database"
        );
        assert_eq!(
            current_schema_version(&conn).expect("read unchanged version"),
            22
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
