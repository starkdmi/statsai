use super::*;

pub(crate) fn apply_migration_012(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS archive_items_source_record_idx
          ON archive_items (source_record_id, item_id);
        "#,
    )?;
    Ok(())
}

pub(crate) fn apply_migration_013(conn: &Connection) -> Result<()> {
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

pub(crate) fn apply_migration_014(conn: &Connection) -> Result<()> {
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

pub(crate) fn apply_migration_015(conn: &Connection) -> Result<()> {
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
pub(crate) fn apply_migration_016(conn: &Connection) -> Result<()> {
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
pub(crate) fn apply_migration_017(conn: &Connection) -> Result<()> {
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

pub(crate) fn apply_migration_018(conn: &Connection) -> Result<()> {
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

pub(crate) fn apply_migration_019(conn: &Connection) -> Result<()> {
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

pub(crate) fn apply_migration_020(conn: &Connection) -> Result<()> {
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

pub(crate) fn apply_migration_021(conn: &Connection) -> Result<()> {
    ensure_column(
        conn,
        "account_evidence_checkpoints",
        "checkpoint_row_fingerprint",
        "TEXT",
    )
}

pub(crate) fn apply_migration_022(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS quota_window_observations_observation_idx
          ON quota_window_observations
             (observation_id, resets_at, window_observation_id);
        "#,
    )?;
    Ok(())
}
