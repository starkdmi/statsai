//! Local SQLite storage for `statsai`.

mod account_plan;
mod accounts;
mod archive;
mod code_changes;
mod dedupe;
mod events;
mod migrations;
mod pricing;
mod privacy;
mod quota;
mod rollups;
mod scan_state;
mod snapshot;
mod sources;
mod sql;
mod summaries;
mod sync_state;
mod tasks;
mod verified;

use accounts::deserialize_subscription_payload;
pub(crate) use rollups::{
    collect_pending_summary_days, event_with_valid_project, is_daily_rollup_summary,
    is_http_rollup_passthrough_summary, sanitize_summary_for_http_sync, summary_period_bounds,
    summary_sync_payload_hash, sync_rollup_bucket_key, sync_rollup_project_key,
    SyncRollupBucketKey,
};
pub(crate) use sql::{
    begin_immediate_transaction_with_retry, commit_transaction, restrict_dir_permissions,
    restrict_file_permissions, rollback, safe_u64_to_i64, sync_state_from_row,
};
pub use verified::{
    apply_source_account_resolution, apply_verified_source_state,
    close_active_verified_source_assignments, close_active_verified_source_linkages,
    find_existing_provider_account, reconcile_verified_source_state, upsert_provider_account,
    verified_source_observation_hash, verified_source_state_hash, UpsertProviderAccountInput,
};
pub(crate) use verified::{
    assignment_for_timestamp, is_verified_source_assignment, reattribute_source_records,
    validate_time_window,
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use statsai_core::{
    daily_rollup_project_key, hash_text, micro_usd_to_cents_rounded, normalize_email,
    normalize_provider_user_id, periods_overlap, project_contains_file_paths,
    project_has_stable_identity, provider_account_id, provider_account_id_from_identity,
    sanitize_code_change_metric_for_sync, sanitize_summary_for_sync, semantic_event_fingerprint,
    source_account_assignment_id, subscription_id, summary_id, timestamp_in_period,
    AccountEvidenceSummaryV1, AccountPlanProjectionV1, BillingPeriod, CodeChangeMetric, Confidence,
    CostAccumulator, CostInfo, DailyRollup, EventId, EventSource, IdentitySource, LatencySource,
    MetricStats, ModelInfo, PrivacyInfo, PrivacyMode, ProviderAccount, ProviderAccountId,
    SemanticFingerprintInput, SourceAccountAssignment, SourceAccountAssignmentId, SourceId,
    SourceKind, SourceLocation, SourceVerificationMode, Subscription, SubscriptionId,
    SubscriptionStatus, SummaryId, SummaryMetadata, SummaryMetricTotals, SummaryMetrics,
    SummaryModelMetrics, SummaryModelUsage, SyncAuthoritativeSnapshot, SyncBatch,
    TaskVerificationCursor, TaskVerificationId, UsageCounts, UsageEvent, UsageSummary,
    VerifiedSourceObservation, VerifiedSourceState, VerifiedSubscriptionState,
    PROVIDER_ACCOUNT_SCHEMA_VERSION, SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION,
    SUBSCRIPTION_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

pub use account_plan::AccountEvidenceReferenceCounts;
pub use migrations::CURRENT_SCHEMA_VERSION;
pub use pricing::{
    apply_current_estimated_pricing, RepricingReport, APPLIED_PRICING_CATALOG_VERSION_KEY,
    APPLIED_PRICING_RULESET_VERSION_KEY,
};
pub use snapshot::{
    clone_database_to, database_applied_pricing_ruleset_version, database_schema_version,
    DatabaseClone,
};
pub use statsai_pricing::{PRICING_CATALOG_VERSION, PRICING_RULESET_VERSION};
use std::time::Duration;

#[cfg(test)]
use statsai_core::SourceIdentityInference;

const ATTRIBUTION_BLOCKED_OBSERVATION_HASH_PREFIX: &str = "attribution_blocked.v2:";
const INFERRED_SOURCE_OBSERVATION_HASH_PREFIX: &str = "inferred_source.v1:";
const VERIFIED_SOURCE_OBSERVATION_HASH_PREFIX: &str = "verified_source.v2:";

pub use archive::{ArchiveConversationSummary, ArchiveSearchHit, ArchiveStats, ArchiveWriteResult};
pub use code_changes::CodeChangeRefreshReport;
pub use privacy::{
    FilteredConversationMetadata, FilteredConversationRecord, PrivacyDatasetStatus,
    PrivacyFailureRecord, PrivacyFindingRecord,
};
pub use quota::{QuotaDateRange, QuotaQuery, QuotaStatus};
pub use tasks::{
    derive_task_work_items, NamedTaskBenchmark, TaskBenchmarkMetrics, TaskBenchmarkReport,
    TaskDeletionImpact, TaskRebuildReport, TaskRebuildTimings, TaskStats,
};

// 13: project identity dropped the git remote, so every bucket that carries a
// path has to be rebuilt for a repository rename to stop splitting a day.
const SYNC_ROLLUP_SUMMARY_VERSION: &str = "13";
const SYNC_INCLUDE_PROJECTS_METADATA_KEY: &str = "sync.include_projects";
const SYNC_INCLUDE_TASKS_METADATA_KEY: &str = "sync.include_tasks";
const LEGACY_CODEX_PLAN_CONVERSION_METADATA_KEY: &str = "migration.legacy_codex_plan_evidence.v1";
const SQLITE_BUSY_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(5)
};
const SQLITE_BUSY_RETRY_DELAY: Duration = if cfg!(test) {
    Duration::from_millis(75)
} else {
    Duration::from_millis(250)
};
const SQLITE_BUSY_RETRY_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsagePeriodStats {
    pub events: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SourceUsageTotals {
    pub events: u64,
    pub tokens: u64,
    pub estimated_cost_cents: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RollupPeriodStats {
    pub tokens: u64,
    pub requests: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRollupView {
    pub pending_count: u64,
    pub pending_days: u64,
    pub today: RollupPeriodStats,
    pub week: RollupPeriodStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PendingSyncSummaryCounts {
    pub rollups: u64,
    pub passthrough_summaries: u64,
    pub retired_entities: u64,
    pub quota_cycle_contributions: u64,
    pub total: u64,
    pub days: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncPreferences {
    pub include_projects: bool,
    pub include_tasks: bool,
}

impl SyncPreferences {
    #[must_use]
    pub fn normalized(self) -> Self {
        let include_projects = self.include_projects || self.include_tasks;
        let include_tasks = self.include_tasks && include_projects;
        Self {
            include_projects,
            include_tasks,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncState {
    pub sink: String,
    pub target: String,
    pub last_success_at: DateTime<Utc>,
    pub last_batch_id: String,
    pub last_event_started_at: Option<DateTime<Utc>>,
    pub last_event_id: Option<String>,
    pub last_summary_observed_at: Option<DateTime<Utc>>,
    pub last_summary_id: Option<String>,
    pub last_task_verification_updated_at: Option<DateTime<Utc>>,
    pub last_task_verification_id: Option<String>,
    pub failure_count: u64,
    pub pending_resume_batch_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskBucketSyncStatus {
    pub total: u64,
    pub dirty: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFileStateEntry {
    pub cache_key: String,
    pub cache_signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanFileStateSelection {
    pub pending_entries: Vec<ScanFileStateEntry>,
    pub compatible_entries_to_upgrade: Vec<ScanFileStateEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventInsertBatchResult {
    pub inserted: u64,
    pub canonical_event_ids: HashMap<EventId, EventId>,
}

pub struct ScanFileReplacement<'a> {
    pub source_id: &'a SourceId,
    pub reconciled_file_hashes: &'a [String],
    pub events: &'a [UsageEvent],
    pub summaries: &'a [UsageSummary],
    pub pending_entries: &'a [ScanFileStateEntry],
    pub compatible_entries_to_upgrade: &'a [ScanFileStateEntry],
    pub removed_cache_keys: &'a [String],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanFileReplacementResult {
    pub inserted_events: u64,
    pub written_summaries: u64,
}

/// Counts produced while atomically applying an incoming sync batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncBatchIngestResult {
    /// New events inserted after deduplication.
    pub inserted_events: u64,
    /// Summaries inserted or updated.
    pub written_summaries: u64,
    /// Task verifications that superseded the stored state.
    pub merged_task_verifications: u64,
}

pub struct Store {
    conn: Connection,
}

/// Restores the store's commit durability when dropped.
///
/// Held for the length of a bulk import. Restoring on drop rather than at the
/// end of the import keeps a failed import from leaving a long-lived process
/// writing everything else at reduced durability.
pub struct BulkImportDurability<'a> {
    store: &'a Store,
    restore_to: i64,
}

impl Drop for BulkImportDurability<'_> {
    fn drop(&mut self) {
        let _ = self
            .store
            .conn
            .execute_batch(&format!("PRAGMA synchronous = {}", self.restore_to));
    }
}

impl Store {
    /// Opens a store and applies migrations.
    ///
    /// # Errors
    ///
    /// Returns an error if SQLite cannot open the path or migrations fail.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let parent_existed = parent.exists();
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
            if !parent_existed {
                restrict_dir_permissions(parent)?;
            }
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        restrict_file_permissions(path)?;
        conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        let store = Self { conn };
        store.migrate()?;
        store.configure_connection()?;
        store.conn.execute_batch("PRAGMA optimize=0x10002;")?;
        Ok(store)
    }

    /// Applies the per-connection settings the write paths rely on.
    ///
    /// Commit durability is deliberately left alone: this connection also
    /// writes state that exists nowhere else — verifications a person entered,
    /// subscriptions, account assignments, privacy identity — and none of that
    /// can be collected again from local files. Relaxing durability is scoped
    /// to the imports that can, in [`Store::relax_durability_for_bulk_import`].
    ///
    /// The page cache is raised because these archives are far larger than the
    /// 2MB default, which turns index maintenance into a stream of single-page
    /// reads.
    fn configure_connection(&self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA cache_size = -65536;
             PRAGMA temp_store = MEMORY;
             CREATE TEMP TABLE IF NOT EXISTS incoming_records (
               source_record_id TEXT,
               item_id TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS incoming_records_source_idx
               ON incoming_records (source_record_id);
             CREATE INDEX IF NOT EXISTS incoming_records_item_idx
               ON incoming_records (item_id);",
        )?;
        // Batched writes issue one statement per batch size, and the archive
        // paths alternate between a handful of them.
        self.conn.set_prepared_statement_cache_capacity(64);
        Ok(())
    }

    /// Opens an independent connection to the same file-backed store.
    ///
    /// This is useful for background readers that must inspect cache state
    /// without holding an application-wide mutex during parsing. Reconciliation
    /// writes should still be serialized through the primary connection.
    ///
    /// # Errors
    ///
    /// Returns an error for in-memory stores or when the file cannot be opened.
    pub fn reopen(&self) -> Result<Self> {
        let path = self
            .conn
            .path()
            .filter(|path| !path.trim().is_empty())
            .context("cannot reopen an in-memory statsai store")?;
        Self::open(Path::new(path))
    }

    /// Returns SQLite's connection-local database generation.
    ///
    /// The value changes when another connection commits, which lets a
    /// background reader detect that the state it parsed from has gone stale
    /// before reconciling through the primary connection.
    ///
    /// # Errors
    ///
    /// Returns an error if SQLite cannot read the generation counter.
    pub fn data_version(&self) -> Result<i64> {
        self.conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .context("read SQLite data version")
    }

    pub fn in_memory() -> Result<Self> {
        let store = Self {
            conn: Connection::open_in_memory()?,
        };
        store.conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        store.migrate()?;
        store.configure_connection()?;
        store.conn.execute_batch("PRAGMA optimize=0x10002;")?;
        Ok(store)
    }

    /// Relaxes commit durability until the returned guard is dropped.
    ///
    /// In WAL mode `synchronous = NORMAL` cannot corrupt the database; it only
    /// means a power loss may cost the most recently committed transactions.
    /// That is an acceptable trade for importing a provider's archive, because
    /// each file's rows and the cache entry recording it commit together, so a
    /// lost commit is collected again on the next run rather than going
    /// silently missing.
    ///
    /// It is not an acceptable trade for the rest of the store, which holds
    /// state that no local file can reproduce, so the relaxation is scoped to
    /// the import rather than applied to the connection for good.
    ///
    /// # Errors
    ///
    /// Returns an error if SQLite rejects the durability change.
    pub fn relax_durability_for_bulk_import(&self) -> Result<BulkImportDurability<'_>> {
        let restore_to = self
            .conn
            .query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
            .context("read commit durability")?;
        self.conn.execute_batch("PRAGMA synchronous = NORMAL")?;
        Ok(BulkImportDurability {
            store: self,
            restore_to,
        })
    }

    fn with_immediate_transaction<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        if !self.conn.is_autocommit() {
            return operation();
        }
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = operation();
        match result {
            Ok(value) => {
                commit_transaction(&self.conn)?;
                Ok(value)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    /// Reads a coherent point-in-time view across multiple store operations.
    ///
    /// Nested calls join the caller's existing transaction. The callback must
    /// not perform writes when it is used as a read snapshot.
    pub fn with_read_snapshot<T>(&self, operation: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        if !self.conn.is_autocommit() {
            return operation(self);
        }
        self.conn.execute_batch("BEGIN DEFERRED TRANSACTION")?;
        let result = operation(self);
        match result {
            Ok(value) => {
                commit_transaction(&self.conn)?;
                Ok(value)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    /// Applies all persistence work for one scanner source in one transaction.
    ///
    /// Nested store operations join this transaction rather than committing
    /// independently.
    ///
    /// # Errors
    ///
    /// Returns the operation error and rolls back every scanner write, or an
    /// error if the transaction cannot be started or committed.
    pub fn apply_scan_update<T>(&self, operation: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        self.with_immediate_transaction(|| operation(self))
    }

    /// Applies a complete incoming sync batch in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error and rolls back every batch component when any write or
    /// task rebuild fails.
    pub fn ingest_sync_batch(&self, batch: &SyncBatch) -> Result<SyncBatchIngestResult> {
        self.with_immediate_transaction(|| {
            for source in &batch.sources {
                self.upsert_source(source)?;
            }
            for account in &batch.accounts {
                self.upsert_account(account)?;
            }
            for assignment in &batch.source_account_assignments {
                self.upsert_source_account_assignment(assignment)?;
            }
            for subscription in &batch.subscriptions {
                self.upsert_subscription(subscription)?;
            }
            let inserted_events = self.insert_events(&batch.events)?;
            let written_summaries = self.upsert_summaries(&batch.summaries)?;
            let mut buckets_needing_rebuild = BTreeSet::new();
            for snapshot in &batch.task_buckets {
                let had_newer_local_verifications = self.task_bucket_has_newer_verifications(
                    &snapshot.project_bucket,
                    snapshot.applied_verification_cursor.as_ref(),
                )?;
                self.replace_task_bucket_snapshot(snapshot)?;
                let has_newer_local_verifications = had_newer_local_verifications
                    || self.task_bucket_has_newer_verifications(
                        &snapshot.project_bucket,
                        snapshot.applied_verification_cursor.as_ref(),
                    )?;
                if has_newer_local_verifications {
                    buckets_needing_rebuild.insert(snapshot.project_bucket.clone());
                }
            }
            let mut merged_task_verifications = 0u64;
            for verification in &batch.task_verifications {
                if self.merge_task_verification(verification)? {
                    merged_task_verifications += 1;
                    buckets_needing_rebuild
                        .extend(self.project_buckets_for_task_verification(verification)?);
                }
            }
            if !buckets_needing_rebuild.is_empty() {
                self.rebuild_task_work_items_for_project_buckets(&buckets_needing_rebuild)?;
            }
            self.ingest_code_change_metrics_inner(&batch.code_change_metrics)?;

            Ok(SyncBatchIngestResult {
                inserted_events,
                written_summaries,
                merged_task_verifications,
            })
        })
    }

    pub fn migrate(&self) -> Result<()> {
        migrations::migrate(&self.conn)?;
        if self
            .metadata_value(LEGACY_CODEX_PLAN_CONVERSION_METADATA_KEY)?
            .as_deref()
            != Some("1")
        {
            self.with_immediate_transaction(|| {
                if self
                    .metadata_value(LEGACY_CODEX_PLAN_CONVERSION_METADATA_KEY)?
                    .as_deref()
                    == Some("1")
                {
                    return Ok(());
                }
                self.migrate_legacy_codex_local_auth_subscriptions_to_plan_evidence()?;
                self.set_metadata_value(LEGACY_CODEX_PLAN_CONVERSION_METADATA_KEY, "1")?;
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        migrations::schema_version(&self.conn)
    }

    pub fn checkpoint_wal(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedupe::model_key;
    use crate::rollups::{build_sync_rollup_summary, sanitize_summary_for_default_http_sync};
    use crate::sql::is_sqlite_busy_or_locked;
    use crate::verified::upsert_verified_source_assignment;
    // Task spans still key on the remote-inclusive bucket; rollups do not.
    use chrono::{TimeZone, Utc};
    use statsai_core::project_bucket_key;
    use statsai_core::{
        event_id, summary_id, Confidence, CostInfo, EventSource, LocationOrigin, ModelInfo,
        ParseEvidence, PrivacyInfo, PrivacyMode, ProjectInfo, ReasoningLevel, SessionInfo,
        SourceKind, SummaryMetadata, UsageCounts, UsageSummary, SYNC_BATCH_SCHEMA_VERSION,
        USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
    };
    use std::path::Path;

    /// Importing an archive may trade durability for speed because a lost
    /// commit is simply collected again. The rest of the store holds work that
    /// no local file can reproduce, so the trade must not outlive the import.
    #[test]
    fn relaxed_durability_is_scoped_to_the_bulk_import() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join("statsai.sqlite")).expect("store");
        let durability = || {
            store
                .conn
                .query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
                .expect("read durability")
        };

        let opened_with = durability();
        assert_ne!(
            opened_with, 0,
            "a store must never open with durability disabled"
        );

        {
            let _relaxed = store
                .relax_durability_for_bulk_import()
                .expect("relax durability");
            assert_eq!(durability(), 1, "the import did not get NORMAL durability");
        }

        assert_eq!(
            durability(),
            opened_with,
            "durability stayed relaxed after the import"
        );
    }

    #[test]
    #[cfg(unix)]
    fn open_restricts_store_directory_and_database_permissions() {
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let store_dir = dir.path().join(".statsai");
        let db_path = store_dir.join("statsai.sqlite");

        let store = Store::open(&db_path).expect("open store");
        drop(store);

        let dir_mode = std::fs::metadata(&store_dir)
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(&db_path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn open_preserves_existing_parent_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let shared_dir = dir.path().join("shared");
        std::fs::create_dir(&shared_dir).expect("create shared dir");
        std::fs::set_permissions(&shared_dir, std::fs::Permissions::from_mode(0o750))
            .expect("set shared dir mode");

        let store = Store::open(&shared_dir.join("statsai.sqlite")).expect("open store");
        drop(store);

        let mode = std::fs::metadata(&shared_dir)
            .expect("shared dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o750);
    }

    #[test]
    fn reopen_uses_an_independent_connection_to_the_same_file_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("statsai.sqlite");
        let store = Store::open(&db_path).expect("open store");
        store
            .conn
            .execute(
                "INSERT INTO local_metadata (key, value, updated_at)
                 VALUES ('reopen-test', 'visible', '2026-07-25T00:00:00Z')",
                [],
            )
            .expect("insert metadata");

        let reopened = store.reopen().expect("reopen file store");
        let value = reopened
            .conn
            .query_row(
                "SELECT value FROM local_metadata WHERE key = 'reopen-test'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read metadata from independent connection");

        assert_eq!(value, "visible");
    }

    #[test]
    fn reopen_rejects_an_in_memory_store() {
        let store = Store::in_memory().expect("store");

        let error = store
            .reopen()
            .err()
            .expect("in-memory store cannot be reopened");

        assert!(error.to_string().contains("in-memory"));
    }

    #[test]
    fn blocked_auth_reattributes_usage_from_the_evidence_boundary() {
        let store = Store::in_memory().expect("store");
        let mut source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-delayed-auth-block"),
            LocationOrigin::Configured,
        );
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
            .single()
            .expect("assignment start");
        let blocked_since = Utc
            .with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
            .single()
            .expect("auth block boundary");
        let before_block = Utc
            .with_ymd_and_hms(2026, 5, 10, 0, 0, 0)
            .single()
            .expect("event before block");
        let after_block = Utc
            .with_ymd_and_hms(2026, 5, 20, 0, 0, 0)
            .single()
            .expect("event after block");
        store.upsert_source(&source).expect("source");
        apply_verified_source_state(
            &store,
            &source,
            Some(&VerifiedSourceState {
                provider_user_id: Some("oauth-account".to_string()),
                email: Some("oauth@example.com".to_string()),
                account_label: None,
                plan_name: None,
                authenticated_at: Some(started_at),
                verified_at: Some(started_at),
                subscription: None,
            }),
        )
        .expect("verified source state");
        let mut events = vec![
            test_store_event(&source, before_block, "before-block"),
            test_store_event(&source, after_block, "after-block"),
        ];
        apply_source_account_resolution(&store, &source, &mut events, &mut [])
            .expect("initial account resolution");
        assert!(events
            .iter()
            .all(|event| event.provider_account_id.is_some()));
        store.insert_events(&events).expect("usage events");
        let observation = VerifiedSourceObservation::AttributionBlocked {
            blocked_since: Some(blocked_since),
        };
        let next_hash = verified_source_observation_hash(&observation).expect("observation hash");

        reconcile_verified_source_state(&store, &mut source, &observation, next_hash)
            .expect("reconcile blocked auth");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].ended_at, Some(blocked_since));
        let events = store
            .events_for_source(&source.source_id)
            .expect("reattributed events");
        assert!(events[0].provider_account_id.is_some());
        assert_eq!(events[1].provider_account_id, None);
    }

    #[test]
    fn cached_profile_inference_backfills_without_interpreting_the_previous_block_hash() {
        let store = Store::in_memory().expect("store");
        let authenticated_at = Utc
            .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
            .single()
            .expect("authenticated at");
        let usage_at = authenticated_at + chrono::Duration::days(1);
        let mut source = SourceLocation::local_adapter(
            "claude_code",
            "claude-code-local-jsonl",
            "0.3.3",
            Path::new("/tmp/claude-broken-profile-block-migration"),
            LocationOrigin::Default,
        );
        source.verified_state_hash =
            verified_source_observation_hash(&VerifiedSourceObservation::AttributionBlocked {
                blocked_since: None,
            })
            .expect("blocked observation hash");
        assert_eq!(
            source.verified_state_hash.as_deref(),
            Some(
                "attribution_blocked.v2:8fee6869306fd2707a21c0aa54affa2d1b1c726dd6dd23a20e61edbf7891e860"
            )
        );
        store.upsert_source(&source).expect("source");
        store
            .insert_event(&test_store_event(
                &source,
                usage_at,
                "unassigned-claude-usage",
            ))
            .expect("unassigned usage");

        let inferred_observation = VerifiedSourceObservation::Inferred {
            identity: Box::new(VerifiedSourceState {
                provider_user_id: Some("claude-account".to_string()),
                email: Some("claude@example.com".to_string()),
                account_label: None,
                plan_name: None,
                authenticated_at: Some(authenticated_at),
                verified_at: Some(authenticated_at),
                subscription: None,
            }),
            basis: SourceIdentityInference::CachedLocalProfile,
            settings_modified_at: None,
        };
        let inferred_hash = verified_source_observation_hash(&inferred_observation)
            .expect("inferred observation hash");

        reconcile_verified_source_state(&store, &mut source, &inferred_observation, inferred_hash)
            .expect("inferred profile reconciliation");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].started_at, authenticated_at);
        assert_eq!(assignments[0].record_source, IdentitySource::SourceConfig);
        let accounts = store.list_accounts().expect("accounts");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].identity_source, IdentitySource::SourceConfig);
        assert_eq!(accounts[0].confidence, Confidence::Medium);
        let events = store
            .events_for_source(&source.source_id)
            .expect("reattributed events");
        assert!(events[0].provider_account_id.is_some());
    }

    #[test]
    fn repaired_settings_bound_cached_profile_inference_after_legacy_block() {
        let store = Store::in_memory().expect("store");
        let blocked_observed_at = Utc
            .with_ymd_and_hms(2026, 8, 5, 0, 0, 0)
            .single()
            .expect("blocked observation");
        let settings_repaired_at = blocked_observed_at + chrono::Duration::days(1);
        let authenticated_at = blocked_observed_at - chrono::Duration::days(10);
        let mut source = SourceLocation::local_adapter(
            "claude_code",
            "claude-code-local-jsonl",
            "0.3.3",
            Path::new("/tmp/claude-repaired-settings-inference"),
            LocationOrigin::Default,
        );
        source.updated_at = blocked_observed_at;
        source.verified_state_hash =
            verified_source_observation_hash(&VerifiedSourceObservation::AttributionBlocked {
                blocked_since: None,
            })
            .expect("blocked observation hash");
        store.upsert_source(&source).expect("source");
        store
            .insert_events(&[
                test_store_event(
                    &source,
                    settings_repaired_at - chrono::Duration::hours(1),
                    "before-settings-repair",
                ),
                test_store_event(
                    &source,
                    settings_repaired_at + chrono::Duration::hours(1),
                    "after-settings-repair",
                ),
            ])
            .expect("unassigned usage");
        let observation = VerifiedSourceObservation::Inferred {
            identity: Box::new(VerifiedSourceState {
                provider_user_id: Some("claude-account".to_string()),
                email: Some("claude@example.com".to_string()),
                account_label: None,
                plan_name: None,
                authenticated_at: Some(authenticated_at),
                verified_at: Some(authenticated_at),
                subscription: None,
            }),
            basis: SourceIdentityInference::CachedLocalProfile,
            settings_modified_at: Some(settings_repaired_at),
        };
        let next_hash =
            verified_source_observation_hash(&observation).expect("inferred observation hash");

        reconcile_verified_source_state(&store, &mut source, &observation, next_hash)
            .expect("inferred profile reconciliation");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].started_at, settings_repaired_at);
        let events = store
            .events_for_source(&source.source_id)
            .expect("reattributed events");
        assert_eq!(events[0].provider_account_id, None);
        assert!(events[1].provider_account_id.is_some());
    }

    #[test]
    fn clearing_auth_override_and_later_profile_changes_preserve_the_blocked_interval() {
        let store = Store::in_memory().expect("store");
        let mut source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-auth-override-recovery"),
            LocationOrigin::Configured,
        );
        let recovery_probe_at = Utc::now();
        let authenticated_at = recovery_probe_at - chrono::Duration::days(30);
        let blocked_since = recovery_probe_at - chrono::Duration::days(20);
        let override_usage_at = recovery_probe_at - chrono::Duration::days(10);
        store.upsert_source(&source).expect("source");

        let verified_state = VerifiedSourceState {
            provider_user_id: Some("oauth-account".to_string()),
            email: Some("oauth@example.com".to_string()),
            account_label: None,
            plan_name: None,
            authenticated_at: Some(authenticated_at),
            verified_at: Some(authenticated_at),
            subscription: None,
        };
        let initial_observation =
            VerifiedSourceObservation::Verified(Box::new(verified_state.clone()));
        let initial_hash =
            verified_source_observation_hash(&initial_observation).expect("initial hash");
        reconcile_verified_source_state(&store, &mut source, &initial_observation, initial_hash)
            .expect("initial OAuth reconciliation");

        let mut events = vec![test_store_event(
            &source,
            override_usage_at,
            "override-usage",
        )];
        apply_source_account_resolution(&store, &source, &mut events, &mut [])
            .expect("initial account resolution");
        assert!(events[0].provider_account_id.is_some());
        store.insert_events(&events).expect("usage event");

        let blocked_observation = VerifiedSourceObservation::AttributionBlocked {
            blocked_since: Some(blocked_since),
        };
        let blocked_hash =
            verified_source_observation_hash(&blocked_observation).expect("blocked hash");
        reconcile_verified_source_state(&store, &mut source, &blocked_observation, blocked_hash)
            .expect("blocked auth reconciliation");

        let refreshed_during_block = blocked_since + chrono::Duration::days(5);
        let refreshed_state = VerifiedSourceState {
            authenticated_at: Some(refreshed_during_block),
            verified_at: Some(refreshed_during_block),
            ..verified_state
        };
        let clear_observation =
            VerifiedSourceObservation::Verified(Box::new(refreshed_state.clone()));
        let clear_hash = verified_source_observation_hash(&clear_observation).expect("clear hash");
        let recovery_not_before = Utc::now();
        reconcile_verified_source_state(&store, &mut source, &clear_observation, clear_hash)
            .expect("cleared auth reconciliation");
        let recovery_not_after = Utc::now();

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0].started_at, authenticated_at);
        assert_eq!(assignments[0].ended_at, Some(blocked_since));
        assert!(assignments[1].started_at >= recovery_not_before);
        assert!(assignments[1].started_at <= recovery_not_after);
        assert_eq!(assignments[1].ended_at, None);
        let recovered_at = assignments[1].started_at;

        let events = store
            .events_for_source(&source.source_id)
            .expect("reattributed events");
        assert_eq!(events[0].provider_account_id, None);

        let changed_profile_observation =
            VerifiedSourceObservation::Verified(Box::new(VerifiedSourceState {
                account_label: Some("Personal".to_string()),
                ..refreshed_state
            }));
        let changed_profile_hash = verified_source_observation_hash(&changed_profile_observation)
            .expect("changed profile hash");
        reconcile_verified_source_state(
            &store,
            &mut source,
            &changed_profile_observation,
            changed_profile_hash,
        )
        .expect("changed profile reconciliation");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments after profile change");
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0].ended_at, Some(blocked_since));
        assert_eq!(assignments[1].started_at, recovered_at);
        assert_eq!(assignments[1].ended_at, None);
        let events = store
            .events_for_source(&source.source_id)
            .expect("events after profile change");
        assert_eq!(events[0].provider_account_id, None);
    }

    #[test]
    fn clearing_current_and_legacy_unknown_auth_blocks_preserves_invalidated_history() {
        for use_legacy_hash in [false, true] {
            let store = Store::in_memory().expect("store");
            let source_path = format!("/tmp/claude-unknown-auth-recovery-{use_legacy_hash}");
            let mut source = SourceLocation::local_adapter(
                "claude_code",
                "test",
                "0",
                Path::new(&source_path),
                LocationOrigin::Configured,
            );
            let recovery_probe_at = Utc::now();
            let authenticated_at = recovery_probe_at - chrono::Duration::days(30);
            let usage_at = recovery_probe_at - chrono::Duration::days(10);
            store.upsert_source(&source).expect("source");

            let verified_state = VerifiedSourceState {
                provider_user_id: Some("oauth-account".to_string()),
                email: Some("oauth@example.com".to_string()),
                account_label: None,
                plan_name: None,
                authenticated_at: Some(authenticated_at),
                verified_at: Some(authenticated_at),
                subscription: None,
            };
            let initial_observation =
                VerifiedSourceObservation::Verified(Box::new(verified_state.clone()));
            let initial_hash =
                verified_source_observation_hash(&initial_observation).expect("initial hash");
            reconcile_verified_source_state(
                &store,
                &mut source,
                &initial_observation,
                initial_hash,
            )
            .expect("initial OAuth reconciliation");
            let mut events = vec![test_store_event(&source, usage_at, "uncertain-usage")];
            apply_source_account_resolution(&store, &source, &mut events, &mut [])
                .expect("initial account resolution");
            store.insert_events(&events).expect("usage event");

            let blocked_observation = VerifiedSourceObservation::AttributionBlocked {
                blocked_since: None,
            };
            let blocked_hash = if use_legacy_hash {
                let legacy_payload = serde_json::to_string(&(
                    "verified_source_observation.attribution_blocked.v2",
                    Option::<DateTime<Utc>>::None,
                ))
                .expect("legacy blocked payload");
                Some(hash_text(&legacy_payload))
            } else {
                verified_source_observation_hash(&blocked_observation).expect("blocked hash")
            };
            reconcile_verified_source_state(
                &store,
                &mut source,
                &blocked_observation,
                blocked_hash,
            )
            .expect("unknown auth block reconciliation");
            assert!(store
                .list_source_account_assignments_for_source(&source.source_id)
                .expect("blocked assignments")
                .is_empty());

            let clear_observation = VerifiedSourceObservation::Verified(Box::new(verified_state));
            let clear_hash =
                verified_source_observation_hash(&clear_observation).expect("clear hash");
            let recovery_not_before = Utc::now();
            reconcile_verified_source_state(&store, &mut source, &clear_observation, clear_hash)
                .expect("cleared auth reconciliation");

            let assignments = store
                .list_source_account_assignments_for_source(&source.source_id)
                .expect("recovered assignments");
            assert_eq!(assignments.len(), 1);
            assert!(assignments[0].started_at >= recovery_not_before);
            let events = store
                .events_for_source(&source.source_id)
                .expect("reattributed events");
            assert_eq!(events[0].provider_account_id, None);
        }
    }

    #[test]
    fn clearing_legacy_timestamped_auth_block_without_history_starts_at_recovery() {
        let store = Store::in_memory().expect("store");
        let mut source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-legacy-timestamped-auth-recovery"),
            LocationOrigin::Configured,
        );
        let authenticated_at = Utc::now() - chrono::Duration::days(30);
        let blocked_since = authenticated_at + chrono::Duration::days(10);
        let legacy_payload = serde_json::to_string(&(
            "verified_source_observation.attribution_blocked.v2",
            Some(blocked_since),
        ))
        .expect("legacy blocked payload");
        source.verified_state_hash = Some(hash_text(&legacy_payload));
        store.upsert_source(&source).expect("source");

        let clear_observation =
            VerifiedSourceObservation::Verified(Box::new(VerifiedSourceState {
                provider_user_id: Some("oauth-account".to_string()),
                email: Some("oauth@example.com".to_string()),
                account_label: None,
                plan_name: None,
                authenticated_at: Some(authenticated_at),
                verified_at: Some(authenticated_at),
                subscription: None,
            }));
        let clear_hash = verified_source_observation_hash(&clear_observation).expect("clear hash");
        let recovery_not_before = Utc::now();

        reconcile_verified_source_state(&store, &mut source, &clear_observation, clear_hash)
            .expect("cleared auth reconciliation");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("recovered assignments");
        assert_eq!(assignments.len(), 1);
        assert!(assignments[0].started_at >= recovery_not_before);
    }

    #[test]
    fn migrating_legacy_verified_hash_preserves_active_assignment_continuity() {
        let store = Store::in_memory().expect("store");
        let mut source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-legacy-verified-hash-migration"),
            LocationOrigin::Configured,
        );
        let authenticated_at = Utc::now() - chrono::Duration::days(30);
        let verified_state = VerifiedSourceState {
            provider_user_id: Some("oauth-account".to_string()),
            email: Some("oauth@example.com".to_string()),
            account_label: None,
            plan_name: None,
            authenticated_at: Some(authenticated_at),
            verified_at: Some(authenticated_at),
            subscription: None,
        };
        source.verified_state_hash =
            verified_source_state_hash(Some(&verified_state)).expect("legacy verified hash");
        store.upsert_source(&source).expect("source");
        apply_verified_source_state(&store, &source, Some(&verified_state))
            .expect("legacy verified assignment");

        let observation = VerifiedSourceObservation::Verified(Box::new(verified_state));
        let typed_hash = verified_source_observation_hash(&observation).expect("typed hash");
        reconcile_verified_source_state(&store, &mut source, &observation, typed_hash.clone())
            .expect("verified hash migration");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].started_at, authenticated_at);
        assert_eq!(assignments[0].ended_at, None);
        assert_eq!(source.verified_state_hash, typed_hash);
    }

    #[test]
    fn earlier_blocked_auth_boundary_shortens_closed_assignment_and_reattributes_usage() {
        let store = Store::in_memory().expect("store");
        let mut source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-corrected-auth-block"),
            LocationOrigin::Configured,
        );
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
            .single()
            .expect("assignment start");
        let earlier_boundary = Utc
            .with_ymd_and_hms(2026, 5, 10, 0, 0, 0)
            .single()
            .expect("earlier auth block boundary");
        let first_boundary = Utc
            .with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
            .single()
            .expect("first auth block boundary");
        let before_earlier_boundary = Utc
            .with_ymd_and_hms(2026, 5, 8, 0, 0, 0)
            .single()
            .expect("event before earlier boundary");
        let between_boundaries = Utc
            .with_ymd_and_hms(2026, 5, 12, 0, 0, 0)
            .single()
            .expect("event between boundaries");
        let later_assignment_start = Utc
            .with_ymd_and_hms(2026, 5, 20, 0, 0, 0)
            .single()
            .expect("later assignment start");
        let later_usage = Utc
            .with_ymd_and_hms(2026, 5, 21, 0, 0, 0)
            .single()
            .expect("later usage");
        store.upsert_source(&source).expect("source");
        apply_verified_source_state(
            &store,
            &source,
            Some(&VerifiedSourceState {
                provider_user_id: Some("oauth-account".to_string()),
                email: Some("oauth@example.com".to_string()),
                account_label: None,
                plan_name: None,
                authenticated_at: Some(started_at),
                verified_at: Some(started_at),
                subscription: None,
            }),
        )
        .expect("verified source state");
        let mut events = vec![
            test_store_event(&source, before_earlier_boundary, "before-earlier-boundary"),
            test_store_event(&source, between_boundaries, "between-boundaries"),
        ];
        apply_source_account_resolution(&store, &source, &mut events, &mut [])
            .expect("initial account resolution");
        store.insert_events(&events).expect("usage events");

        let first_observation = VerifiedSourceObservation::AttributionBlocked {
            blocked_since: Some(first_boundary),
        };
        let first_hash =
            verified_source_observation_hash(&first_observation).expect("first observation hash");
        reconcile_verified_source_state(&store, &mut source, &first_observation, first_hash)
            .expect("first blocked auth reconciliation");
        apply_verified_source_state(
            &store,
            &source,
            Some(&VerifiedSourceState {
                provider_user_id: Some("later-oauth-account".to_string()),
                email: Some("later-oauth@example.com".to_string()),
                account_label: None,
                plan_name: None,
                authenticated_at: Some(later_assignment_start),
                verified_at: Some(later_assignment_start),
                subscription: None,
            }),
        )
        .expect("later verified source state");
        let mut later_events = vec![test_store_event(&source, later_usage, "later-usage")];
        apply_source_account_resolution(&store, &source, &mut later_events, &mut [])
            .expect("later account resolution");
        assert!(later_events[0].provider_account_id.is_some());
        store
            .insert_events(&later_events)
            .expect("later usage event");
        let corrected_observation = VerifiedSourceObservation::AttributionBlocked {
            blocked_since: Some(earlier_boundary),
        };
        let corrected_hash = verified_source_observation_hash(&corrected_observation)
            .expect("corrected observation hash");

        reconcile_verified_source_state(
            &store,
            &mut source,
            &corrected_observation,
            corrected_hash,
        )
        .expect("corrected blocked auth reconciliation");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].ended_at, Some(earlier_boundary));
        let events = store
            .events_for_source(&source.source_id)
            .expect("reattributed events");
        assert!(events[0].provider_account_id.is_some());
        assert_eq!(events[1].provider_account_id, None);
        assert_eq!(events[2].provider_account_id, None);
    }

    #[test]
    fn blocked_auth_without_evidence_invalidates_the_uncertain_assignment_interval() {
        let store = Store::in_memory().expect("store");
        let mut source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-unknown-auth-block-boundary"),
            LocationOrigin::Configured,
        );
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
            .single()
            .expect("assignment start");
        store.upsert_source(&source).expect("source");
        apply_verified_source_state(
            &store,
            &source,
            Some(&VerifiedSourceState {
                provider_user_id: Some("oauth-account".to_string()),
                email: Some("oauth@example.com".to_string()),
                account_label: None,
                plan_name: None,
                authenticated_at: Some(started_at),
                verified_at: Some(started_at),
                subscription: None,
            }),
        )
        .expect("verified source state");
        let mut events = vec![test_store_event(&source, started_at, "uncertain-event")];
        apply_source_account_resolution(&store, &source, &mut events, &mut [])
            .expect("initial account resolution");
        assert!(events[0].provider_account_id.is_some());
        store.insert_events(&events).expect("usage event");
        let observation = VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        };
        let next_hash = verified_source_observation_hash(&observation).expect("observation hash");

        reconcile_verified_source_state(&store, &mut source, &observation, next_hash)
            .expect("reconcile blocked auth");

        assert!(store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments")
            .is_empty());
        let events = store
            .events_for_source(&source.source_id)
            .expect("reattributed events");
        assert_eq!(events[0].provider_account_id, None);
    }

    #[test]
    fn blocked_auth_hash_includes_the_evidence_boundary() {
        let first_boundary = Utc
            .with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
            .single()
            .expect("first boundary");
        let second_boundary = Utc
            .with_ymd_and_hms(2026, 5, 16, 0, 0, 0)
            .single()
            .expect("second boundary");

        let first_hash =
            verified_source_observation_hash(&VerifiedSourceObservation::AttributionBlocked {
                blocked_since: Some(first_boundary),
            })
            .expect("first hash");
        let second_hash =
            verified_source_observation_hash(&VerifiedSourceObservation::AttributionBlocked {
                blocked_since: Some(second_boundary),
            })
            .expect("second hash");

        assert_ne!(first_hash, second_hash);
    }

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
    fn reads_legacy_subscription_payloads_with_missing_account_and_start() {
        let store = Store::in_memory().expect("store");
        let payload = r#"{
            "schema_version":"subscription.v1",
            "subscription_id":"sub_legacy",
            "provider":"codex",
            "plan_name":"Plus",
            "price":20.0,
            "currency":"USD",
            "billing_period":"monthly",
            "paid_at":"2026-05-01T00:00:00Z",
            "status":"active"
        }"#;
        store
            .conn
            .execute(
                "INSERT INTO subscriptions (subscription_id, provider, provider_account_id, payload) VALUES (?1, ?2, ?3, ?4)",
                params!["sub_legacy", "codex", Option::<String>::None, payload],
            )
            .expect("insert legacy subscription");

        let subscription = store
            .subscription(&SubscriptionId("sub_legacy".to_string()))
            .expect("read legacy subscription")
            .expect("subscription exists");
        let subscriptions = store
            .list_subscriptions()
            .expect("list legacy subscriptions");

        assert_eq!(subscriptions, vec![subscription.clone()]);
        assert_eq!(subscription.provider, "codex");
        assert_eq!(
            subscription.provider_account_id,
            provider_account_id("codex", "legacy_subscription:sub_legacy")
        );
        assert_eq!(
            subscription.started_at,
            Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
                .single()
                .expect("started_at")
        );
    }

    #[test]
    fn migrates_only_codex_local_auth_subscriptions_into_plan_evidence() {
        let store = Store::in_memory().expect("store");
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("started_at");
        let active_until = Utc
            .with_ymd_and_hms(2026, 7, 1, 0, 0, 0)
            .single()
            .expect("active_until");
        let account_id = provider_account_id("codex", "provider-user");
        let synthetic = Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: SubscriptionId("synthetic-codex-plan".to_string()),
            provider: "codex".to_string(),
            provider_account_id: account_id.clone(),
            plan_name: "future_ultra".to_string(),
            price: 20_00,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: Some(started_at),
            renewal_day: Some(1),
            started_at,
            ended_at: None,
            current_period_ends_at: Some(active_until),
            status: SubscriptionStatus::Active,
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(started_at),
            notes: None,
        };
        let manual = Subscription {
            subscription_id: SubscriptionId("manual-codex-billing".to_string()),
            record_source: IdentitySource::UserConfigured,
            price: 12_34,
            ..synthetic.clone()
        };
        store.upsert_subscription(&synthetic).expect("synthetic");
        store.upsert_subscription(&manual).expect("manual");

        assert_eq!(
            store
                .migrate_legacy_codex_local_auth_subscriptions_to_plan_evidence()
                .expect("migration"),
            1
        );
        assert_eq!(
            store
                .migrate_legacy_codex_local_auth_subscriptions_to_plan_evidence()
                .expect("repeat migration"),
            0
        );

        assert_eq!(store.list_subscriptions().expect("billing"), vec![manual]);
        let observations = store.account_plan_observations().expect("plan evidence");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].provider_account_id, Some(account_id));
        assert_eq!(observations[0].raw_plan_name, "future_ultra");
        assert_eq!(observations[0].plan_name, "Future Ultra");
        assert_eq!(observations[0].active_from, Some(started_at));
        assert_eq!(observations[0].active_until, Some(active_until));
        assert_eq!(
            observations[0].evidence_kind,
            statsai_core::AccountEvidenceKind::LegacyLocalAuth
        );
    }

    #[test]
    fn an_unreadable_legacy_payload_does_not_make_the_store_unopenable() {
        let path = tempfile::tempdir()
            .expect("tempdir")
            .keep()
            .join("store.db");
        {
            let store = Store::open(&path).expect("initial store");
            store
                .conn
                .execute(
                    "INSERT INTO subscriptions
                       (subscription_id, provider, provider_account_id, payload)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        "corrupt-subscription",
                        "codex",
                        "codex:corrupt",
                        "{ this is not json",
                    ],
                )
                .expect("corrupt subscription row");
            store
                .conn
                .execute(
                    "INSERT INTO provider_accounts
                       (provider_account_id, provider, payload, updated_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        "codex:corrupt-account",
                        "codex",
                        "{ also not json",
                        Utc::now().to_rfc3339(),
                    ],
                )
                .expect("corrupt account row");
            store
                .conn
                .execute(
                    "DELETE FROM local_metadata WHERE key = ?1",
                    params![LEGACY_CODEX_PLAN_CONVERSION_METADATA_KEY],
                )
                .expect("clear conversion flag");
        }

        // The conversion runs inside `migrate`, so a hard error here would roll
        // back before the completion flag is written and fail every later open.
        let reopened = Store::open(&path).expect("a corrupt legacy row must not brick the store");
        assert_eq!(
            reopened
                .metadata_value(LEGACY_CODEX_PLAN_CONVERSION_METADATA_KEY)
                .expect("conversion flag")
                .as_deref(),
            Some("1"),
            "the one-shot conversion must be recorded as done, not retried forever"
        );
        Store::open(&path).expect("a second open still succeeds");
    }

    #[test]
    fn migrates_legacy_codex_account_plan_without_subscription() {
        let store = Store::in_memory().expect("store");
        let observed_at = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("observed at");
        let account_id = provider_account_id("codex", "legacy-plan-account");
        store
            .upsert_account(&ProviderAccount {
                schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
                provider_account_id: account_id.clone(),
                provider: "codex".to_string(),
                identity_source: IdentitySource::LocalAuth,
                provider_user_id: None,
                provider_user_id_hash: Some("a".repeat(64)),
                email: None,
                email_hash: None,
                org_id_hash: None,
                account_label: Some("Codex account".to_string()),
                plan_name: Some("future_ultra".to_string()),
                confidence: Confidence::High,
                verified_at: Some(observed_at),
                created_at: observed_at,
                updated_at: observed_at,
            })
            .expect("legacy account");

        assert_eq!(
            store
                .migrate_legacy_codex_local_auth_subscriptions_to_plan_evidence()
                .expect("migration"),
            0
        );

        let account = store
            .account(&account_id)
            .expect("account")
            .expect("exists");
        assert_eq!(account.plan_name, None);
        assert_eq!(account.account_label.as_deref(), Some("Codex account"));
        let observations = store.account_plan_observations().expect("plan evidence");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].provider_account_id, Some(account_id));
        assert_eq!(observations[0].raw_plan_name, "future_ultra");
        assert_eq!(observations[0].plan_name, "Future Ultra");
        assert_eq!(observations[0].active_from, None);
        assert_eq!(observations[0].active_until, None);
        assert_eq!(
            observations[0].evidence_kind,
            statsai_core::AccountEvidenceKind::LegacyLocalAuth
        );
        assert_eq!(
            store
                .migrate_legacy_codex_local_auth_subscriptions_to_plan_evidence()
                .expect("repeat migration"),
            0
        );
        assert_eq!(
            store
                .account_plan_observations()
                .expect("repeated plan evidence")
                .len(),
            1
        );
    }

    #[test]
    fn completed_legacy_plan_conversion_is_not_repeated_on_migrate() {
        let store = Store::in_memory().expect("store");
        store
            .conn
            .execute(
                r#"
                INSERT INTO account_plan_observations (
                  observation_id, provider, source_id, provider_account_id,
                  observed_at, active_from, active_until, plan_name, evidence_kind, payload
                ) VALUES (?1, ?2, ?3, NULL, ?4, NULL, NULL, ?5, ?6, ?7)
                "#,
                params![
                    "post-conversion-malformed-observation",
                    "codex",
                    "source-after-conversion",
                    "2026-08-23T00:00:00Z",
                    "Pro",
                    "quota_status",
                    "not-json"
                ],
            )
            .expect("insert evidence that must not be rescanned");

        store.migrate().expect("completed conversion stays skipped");
    }

    #[test]
    fn direct_conversation_evidence_overrides_only_that_event_and_preserves_manual_interval() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-direct-account-binding"),
            LocationOrigin::Configured,
        );
        let observed_at = Utc
            .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
            .single()
            .expect("observed_at");
        let manual_account = ProviderAccountId("manual-account".to_string());
        let directly_bound_account = ProviderAccountId("direct-account".to_string());
        store.upsert_source(&source).expect("source");
        let manual_assignment = SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: SourceAccountAssignmentId("manual-assignment".to_string()),
            source_id: source.source_id.clone(),
            provider: "codex".to_string(),
            provider_account_id: manual_account.clone(),
            started_at: observed_at - chrono::Duration::days(1),
            ended_at: None,
            record_source: IdentitySource::UserConfigured,
            verified_at: None,
            created_at: observed_at,
            updated_at: observed_at,
        };
        store
            .upsert_source_account_assignment(&manual_assignment)
            .expect("manual assignment");
        let direct_binding = statsai_core::ConversationAccountBindingV1 {
            schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
            binding_id: "direct-binding".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: directly_bound_account.clone(),
            conversation_id_hash: "same-session".to_string(),
            turn_id_hash: None,
            observed_at,
            evidence_kind: statsai_core::AccountEvidenceKind::ResetHistory,
            confidence: Confidence::High,
        };
        assert_eq!(
            store
                .upsert_conversation_account_bindings(std::slice::from_ref(&direct_binding))
                .expect("direct binding"),
            1
        );
        assert_eq!(
            store
                .upsert_conversation_account_bindings(&[direct_binding])
                .expect("repeat direct binding"),
            0
        );
        store
            .upsert_account_identity_observations(&[statsai_core::AccountIdentityObservationV1 {
                schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION
                    .to_string(),
                observation_id: "direct-identity".to_string(),
                provider: "codex".to_string(),
                source_id: source.source_id.clone(),
                provider_account_id: Some(directly_bound_account.clone()),
                provider_user_id_hash: Some("provider-id-hash".to_string()),
                email_hash: None,
                conversation_id_hash: Some("same-session".to_string()),
                turn_id_hash: None,
                observed_at,
                evidence_kind: statsai_core::AccountEvidenceKind::ResetHistory,
                confidence: Confidence::High,
                auth_mode: None,
                application_version: None,
                parser_version: "test.v1".to_string(),
                artifact_kind: "test".to_string(),
                artifact_path_hash: "path-hash".to_string(),
                record_fingerprint: "record-hash".to_string(),
            }])
            .expect("identity observation");
        let mut directly_bound = test_store_event(&source, observed_at, "direct");
        directly_bound.provider_account_id = Some(manual_account.clone());
        let mut unrelated = test_store_event(&source, observed_at, "unrelated");
        unrelated.session.local_session_id_hash = Some("other-session".to_string());
        unrelated.provider_account_id = Some(manual_account.clone());
        let mut events = vec![directly_bound, unrelated];

        store
            .apply_conversation_account_bindings(&source.source_id, &mut events)
            .expect("apply binding");

        assert_eq!(events[0].provider_account_id, Some(directly_bound_account));
        assert_eq!(events[1].provider_account_id, Some(manual_account.clone()));
        assert_eq!(
            store
                .list_source_account_assignments_for_source(&source.source_id)
                .expect("manual interval"),
            vec![manual_assignment]
        );
        let summaries = store
            .account_evidence_summaries("device")
            .expect("evidence summary");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].directly_bound_conversations, 1);
        assert_eq!(summaries[0].uncovered_gap_count, 0);
        assert_eq!(summaries[0].conflict_count, 1);
    }

    #[test]
    fn confirmed_auth_reload_boundaries_repair_switches_conservatively() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-account-evidence-switch"),
            LocationOrigin::Configured,
        );
        let base = Utc
            .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
            .single()
            .expect("base");
        let account_a = ProviderAccountId("account-a".to_string());
        let account_b = ProviderAccountId("account-b".to_string());
        store.upsert_source(&source).expect("source");
        store
            .upsert_source_account_assignment(&SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: SourceAccountAssignmentId("broad-account-a".to_string()),
                source_id: source.source_id.clone(),
                provider: "codex".to_string(),
                provider_account_id: account_a.clone(),
                started_at: base,
                ended_at: None,
                record_source: IdentitySource::LocalAuth,
                verified_at: Some(base),
                created_at: base,
                updated_at: base,
            })
            .expect("broad assignment");
        let observation = |id: &str,
                           account: ProviderAccountId,
                           observed_at: chrono::DateTime<Utc>,
                           kind: statsai_core::AccountEvidenceKind| {
            statsai_core::AccountIdentityObservationV1 {
                schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION
                    .to_string(),
                observation_id: id.to_string(),
                provider: "codex".to_string(),
                source_id: source.source_id.clone(),
                provider_account_id: Some(account),
                provider_user_id_hash: Some(format!("hash-{id}")),
                email_hash: None,
                conversation_id_hash: None,
                turn_id_hash: None,
                observed_at,
                evidence_kind: kind,
                confidence: Confidence::High,
                auth_mode: Some("chatgpt".to_string()),
                application_version: None,
                parser_version: "test.v1".to_string(),
                artifact_kind: "test".to_string(),
                artifact_path_hash: "path-hash".to_string(),
                record_fingerprint: format!("fingerprint-{id}"),
            }
        };
        let account_a_reload = base + chrono::Duration::days(1);
        let account_a_confirmation = base + chrono::Duration::days(2);
        let account_b_reload = base + chrono::Duration::days(3);
        let account_b_confirmation = base + chrono::Duration::days(4);
        store
            .upsert_account_identity_observations(&[
                observation(
                    "a-reload",
                    account_a.clone(),
                    account_a_reload,
                    statsai_core::AccountEvidenceKind::AuthReload,
                ),
                observation(
                    "a-confirm",
                    account_a.clone(),
                    account_a_confirmation,
                    statsai_core::AccountEvidenceKind::TelemetryIdentity,
                ),
                observation(
                    "b-reload",
                    account_b.clone(),
                    account_b_reload,
                    statsai_core::AccountEvidenceKind::AuthReload,
                ),
                observation(
                    "b-confirm",
                    account_b.clone(),
                    account_b_confirmation,
                    statsai_core::AccountEvidenceKind::AuthSnapshot,
                ),
            ])
            .expect("identity evidence");

        assert!(
            store
                .reconcile_source_account_evidence_assignments(&source.source_id)
                .expect("repair intervals")
                > 0
        );
        assert_eq!(
            store
                .reconcile_source_account_evidence_assignments(&source.source_id)
                .expect("repeat repair"),
            0
        );
        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 2);
        let repaired_a = assignments
            .iter()
            .find(|assignment| assignment.provider_account_id == account_a)
            .expect("account a interval");
        assert_eq!(repaired_a.started_at, base);
        assert_eq!(repaired_a.ended_at, Some(account_b_reload));
        let repaired_b = assignments
            .iter()
            .find(|assignment| assignment.provider_account_id == account_b)
            .expect("account b interval");
        assert_eq!(repaired_b.started_at, account_b_reload);
        assert_eq!(repaired_b.ended_at, None);
    }

    #[test]
    fn inserts_events_idempotently() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let id = event_id("codex", &source.source_id, "record", None, now);
        let mut event = UsageEvent {
            schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
            event_id: id,
            device_id: "device".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id,
            provider_account_id: None,
            subscription_id: None,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalAdapter,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "jsonl".to_string(),
                source_path_hash: None,
                source_record_id: Some("record".to_string()),
                parse_confidence: Confidence::High,
            },
            session: SessionInfo {
                session_id: "session".to_string(),
                local_session_id_hash: None,
                title: None,
                started_at: now,
                ended_at: None,
                duration_seconds: None,
            },
            model: None,
            usage: UsageCounts {
                total_tokens: Some(10),
                ..UsageCounts::default()
            },
            runtime: None,
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: None,
                provider_reported_usd: None,
                estimated_api_equivalent_micro_usd: None,
                provider_reported_micro_usd: None,
                pricing_source: Some("unknown".to_string()),
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
            created_at: now,
            imported_at: now,
        };

        assert!(store.insert_event(&event).expect("insert"));
        assert!(!store.insert_event(&event).expect("dedupe"));
        assert_eq!(store.event_count().expect("count"), 1);

        event.usage.input_tokens = Some(12);
        event.usage.output_tokens = Some(3);
        event.usage.total_tokens = Some(15);
        event.cost.estimated_api_equivalent_usd = Some(1);

        assert!(!store.insert_event(&event).expect("refresh duplicate"));
        assert_eq!(store.event_count().expect("count after refresh"), 1);
        assert_eq!(store.token_total().expect("tokens after refresh"), 15);

        let events = store.events().expect("events");
        assert_eq!(events[0].usage.input_tokens, Some(12));
        assert_eq!(events[0].cost.estimated_api_equivalent_usd, Some(1));
    }

    #[test]
    fn store_strips_bare_project_identity_from_events_and_rollups() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-bare-project"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 5, 9, 0, 0)
            .single()
            .expect("now");
        let mut event = test_store_event(&source, now, "bare-project");
        event.project = Some(ProjectInfo {
            project_id: "project_bare".to_string(),
            project_label: Some("Bare".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: None,
            path_label: None,
        });

        assert!(store.insert_event(&event).expect("insert"));
        let events = store.events().expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].project, None);

        let rollups = store.dirty_sync_rollup_summaries().expect("rollups");
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].project, None);
    }

    #[test]
    fn renaming_a_repository_keeps_one_rollup_for_the_day() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-repo-rename"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let day = Utc
            .with_ymd_and_hms(2026, 6, 5, 9, 0, 0)
            .single()
            .expect("day");
        let account_id = statsai_core::provider_account_id("codex", "personal");

        let checkout = |remote: &str, label: &str| ProjectInfo {
            project_id: format!("project-{remote}"),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some(remote.to_string()),
            repo_label: Some(label.to_string()),
            branch_hash: Some("branch-main".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-checkout".to_string()),
            path_label: Some("/work/ai-stats".to_string()),
        };

        // Same checkout, same branch, same day — the remote was renamed midway.
        let mut before = test_store_event(&source, day, "before-rename");
        before.provider_account_id = Some(account_id.clone());
        before.usage.total_tokens = Some(10);
        before.project = Some(checkout("remote-before", "owner/ai-stats"));

        let mut after = test_store_event(&source, day + chrono::Duration::hours(1), "after-rename");
        after.provider_account_id = Some(account_id);
        after.usage.total_tokens = Some(20);
        after.project = Some(checkout("remote-after", "owner/statsai"));

        assert!(store.insert_event(&before).expect("insert before"));
        assert!(store.insert_event(&after).expect("insert after"));

        let dirty = store.dirty_sync_rollup_summaries().expect("dirty rollups");
        assert_eq!(dirty.len(), 1, "a rename must not split the day in two");
        let rollup = &dirty[0];
        assert_eq!(rollup.usage.total_tokens, Some(30));

        // The remote still travels with the rollup, so the backend can key the
        // project on it and relink this location's history across the rename.
        let project = rollup.project.as_ref().expect("project metadata");
        assert_eq!(project.path_hash.as_deref(), Some("path-checkout"));
        assert_eq!(
            project.repo_label.as_deref(),
            Some("owner/statsai"),
            "the newest event names the remote the checkout has now"
        );
    }

    #[test]
    fn sync_rollups_export_path_only_project_metadata() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-path-only-projects"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let day = Utc
            .with_ymd_and_hms(2026, 6, 5, 9, 0, 0)
            .single()
            .expect("day");
        let account_id = statsai_core::provider_account_id("codex", "personal");

        let mut first = test_store_event(&source, day, "path-only-project-a");
        first.provider_account_id = Some(account_id.clone());
        first.usage.total_tokens = Some(10);
        first.project = Some(ProjectInfo {
            project_id: "project-path-a".to_string(),
            project_label: Some("hi".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-a".to_string()),
            path_label: Some("/Users/example/Documents/Codex/2026-05-29/hi".to_string()),
        });

        let mut second = test_store_event(
            &source,
            day + chrono::Duration::hours(1),
            "path-only-project-b",
        );
        second.provider_account_id = Some(account_id);
        second.usage.total_tokens = Some(20);
        second.project = Some(ProjectInfo {
            project_id: "project-path-b".to_string(),
            project_label: Some("hi".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-b".to_string()),
            path_label: Some("/Users/example/Documents/Codex/2026-05-28/hi".to_string()),
        });

        assert!(store.insert_event(&first).expect("insert first"));
        assert!(store.insert_event(&second).expect("insert second"));

        let dirty = store.dirty_sync_rollup_summaries().expect("dirty rollups");
        assert_eq!(dirty.len(), 2);
        let projects = dirty
            .iter()
            .map(|summary| summary.project.as_ref().expect("project metadata"))
            .collect::<Vec<_>>();
        assert!(projects
            .iter()
            .all(|project| project.repo_remote_hash.is_none()));
        assert_eq!(
            projects
                .iter()
                .filter_map(|project| project.path_label.as_deref())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "/Users/example/Documents/Codex/2026-05-28/hi",
                "/Users/example/Documents/Codex/2026-05-29/hi",
            ])
        );
        assert_eq!(
            dirty
                .iter()
                .map(|summary| summary.usage.total_tokens.unwrap_or(0))
                .sum::<u64>(),
            30
        );
    }

    #[test]
    fn refreshes_semantic_duplicate_with_new_event_id_without_double_counting() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-semantic"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let old_event = test_store_event(&source, now, "old-record");
        let old_event_id = old_event.event_id.clone();
        let mut new_event = old_event.clone();
        new_event.event_id = event_id("codex", &source.source_id, "semantic-record", None, now);
        new_event.source.source_record_id = Some("usage_key_new".to_string());
        new_event.parse_evidence = None;

        assert!(store.insert_event(&old_event).expect("insert old"));
        assert!(!store.insert_event(&new_event).expect("refresh semantic"));
        assert_eq!(store.event_count().expect("count"), 1);

        assert_eq!(store.events().expect("events")[0].event_id, old_event_id);
    }

    #[test]
    fn refreshes_legacy_reasoning_level_upgrade_without_double_counting() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-reasoning-upgrade"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let mut old_event = test_store_event(&source, now, "legacy-record");
        old_event.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });

        let mut new_event = old_event.clone();
        new_event.event_id = event_id("codex", &source.source_id, "reasoning-record", None, now);
        new_event.source.source_record_id = Some("usage_key_reasoning".to_string());
        new_event.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::Low),
            reasoning_level_raw: Some("low".to_string()),
        });

        assert!(store.insert_event(&old_event).expect("insert old"));
        assert!(!store
            .insert_event(&new_event)
            .expect("refresh reasoning upgrade"));

        let events = store.events().expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, old_event.event_id);
        assert_eq!(
            events[0]
                .model
                .as_ref()
                .and_then(|model| model.reasoning_level),
            Some(ReasoningLevel::Low)
        );
        assert_eq!(
            events[0]
                .model
                .as_ref()
                .and_then(|model| model.reasoning_level_raw.as_deref()),
            Some("low")
        );
    }

    #[test]
    fn refresh_duplicate_without_reasoning_does_not_erase_enriched_reasoning() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-reasoning-preserve"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let mut enriched = test_store_event(&source, now, "enriched-record");
        enriched.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::Low),
            reasoning_level_raw: Some("low".to_string()),
        });

        let mut less_enriched = enriched.clone();
        less_enriched.event_id = event_id("codex", &source.source_id, "less-enriched", None, now);
        less_enriched.source.source_record_id = Some("usage_key_less_enriched".to_string());
        less_enriched.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });

        assert!(store.insert_event(&enriched).expect("insert enriched"));
        assert!(!store
            .insert_event(&less_enriched)
            .expect("refresh less-enriched duplicate"));

        let events = store.events().expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0]
                .model
                .as_ref()
                .and_then(|model| model.reasoning_level),
            Some(ReasoningLevel::Low)
        );
        assert_eq!(
            events[0]
                .model
                .as_ref()
                .and_then(|model| model.reasoning_level_raw.as_deref()),
            Some("low")
        );
    }

    #[test]
    fn exact_event_id_refresh_without_reasoning_does_not_erase_enriched_reasoning() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-reasoning-exact-id-preserve"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let mut enriched = test_store_event(&source, now, "same-record");
        enriched.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::Medium),
            reasoning_level_raw: Some("medium".to_string()),
        });

        let mut less_enriched = enriched.clone();
        less_enriched.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });

        assert!(store.insert_event(&enriched).expect("insert enriched"));
        assert!(!store
            .insert_event(&less_enriched)
            .expect("refresh exact-id duplicate"));

        let events = store.events().expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0]
                .model
                .as_ref()
                .and_then(|model| model.reasoning_level),
            Some(ReasoningLevel::Medium)
        );
        assert_eq!(
            events[0]
                .model
                .as_ref()
                .and_then(|model| model.reasoning_level_raw.as_deref()),
            Some("medium")
        );
    }

    #[test]
    fn keeps_explicit_reasoning_levels_as_distinct_events() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-reasoning-distinct"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let mut low = test_store_event(&source, now, "low-record");
        low.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::Low),
            reasoning_level_raw: Some("low".to_string()),
        });

        let mut high = low.clone();
        high.event_id = event_id("codex", &source.source_id, "high-record", None, now);
        high.source.source_record_id = Some("high-record".to_string());
        high.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::High),
            reasoning_level_raw: Some("high".to_string()),
        });

        assert!(store.insert_event(&low).expect("insert low"));
        assert!(store.insert_event(&high).expect("insert high"));

        let events = store.events().expect("events");
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|event| {
            event.model.as_ref().and_then(|model| model.reasoning_level)
                == Some(ReasoningLevel::Low)
        }));
        assert!(events.iter().any(|event| {
            event.model.as_ref().and_then(|model| model.reasoning_level)
                == Some(ReasoningLevel::High)
        }));
    }

    #[test]
    fn insert_events_keeps_existing_reasoning_variants_distinct() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-reasoning-batch"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let mut low = test_store_event(&source, now, "low-record");
        low.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::Low),
            reasoning_level_raw: Some("low".to_string()),
        });
        assert!(store.insert_event(&low).expect("insert low"));

        let mut high = low.clone();
        high.event_id = event_id("codex", &source.source_id, "high-record", None, now);
        high.source.source_record_id = Some("high-record".to_string());
        high.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::High),
            reasoning_level_raw: Some("high".to_string()),
        });

        assert_eq!(
            store.insert_events(&[high]).expect("insert batched high"),
            1
        );
        assert_eq!(store.event_count().expect("count"), 2);
    }

    #[test]
    fn insert_events_preserves_existing_reasoning_on_less_enriched_duplicate() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-reasoning-batch-preserve"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let mut enriched = test_store_event(&source, now, "enriched-record");
        enriched.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::High),
            reasoning_level_raw: Some("high".to_string()),
        });
        assert!(store.insert_event(&enriched).expect("insert enriched"));

        let mut less_enriched = enriched.clone();
        less_enriched.event_id = event_id("codex", &source.source_id, "less-enriched", None, now);
        less_enriched.source.source_record_id = Some("less-enriched".to_string());
        less_enriched.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });

        assert_eq!(
            store
                .insert_events(&[less_enriched])
                .expect("insert batched less-enriched duplicate"),
            0
        );

        let events = store.events().expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0]
                .model
                .as_ref()
                .and_then(|model| model.reasoning_level),
            Some(ReasoningLevel::High)
        );
        assert_eq!(
            events[0]
                .model
                .as_ref()
                .and_then(|model| model.reasoning_level_raw.as_deref()),
            Some("high")
        );
    }

    #[test]
    fn insert_events_with_resolution_returns_canonical_duplicate_event_ids() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-batch-resolution"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let existing = test_store_event(&source, now, "existing-record");
        let mut duplicate = existing.clone();
        duplicate.event_id = event_id("codex", &source.source_id, "duplicate-record", None, now);
        duplicate.source.source_record_id = Some("duplicate-record".to_string());
        duplicate.parse_evidence = None;

        assert!(store.insert_event(&existing).expect("insert existing"));
        let result = store
            .insert_events_with_resolution(&[duplicate.clone()])
            .expect("insert duplicate");

        assert_eq!(result.inserted, 0);
        assert_eq!(
            result.canonical_event_ids.get(&duplicate.event_id),
            Some(&existing.event_id)
        );
        assert_eq!(store.event_count().expect("count"), 1);
    }

    #[test]
    fn insert_events_refreshes_preloaded_conflicts_before_matching_new_reasoning_variant() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-reasoning-batch-refresh"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let mut legacy = test_store_event(&source, now, "legacy-record");
        legacy.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        assert!(store.insert_event(&legacy).expect("insert legacy"));

        let mut low = legacy.clone();
        low.event_id = event_id("codex", &source.source_id, "low-record", None, now);
        low.source.source_record_id = Some("low-record".to_string());
        low.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::Low),
            reasoning_level_raw: Some("low".to_string()),
        });

        let mut high = low.clone();
        high.event_id = event_id("codex", &source.source_id, "high-record", None, now);
        high.source.source_record_id = Some("high-record".to_string());
        high.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::High),
            reasoning_level_raw: Some("high".to_string()),
        });

        assert_eq!(
            store
                .insert_events(&[low, high])
                .expect("insert batched variants"),
            1
        );

        let events = store.events().expect("events");
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|event| {
            event.model.as_ref().and_then(|model| model.reasoning_level)
                == Some(ReasoningLevel::Low)
        }));
        assert!(events.iter().any(|event| {
            event.model.as_ref().and_then(|model| model.reasoning_level)
                == Some(ReasoningLevel::High)
        }));
    }

    #[test]
    fn refreshes_legacy_codex_token_count_duplicate_without_double_counting() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-token-count"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let mut old_event = test_store_event(&source, now, "legacy-record");
        old_event.session.session_id = "session-a".to_string();
        old_event.session.local_session_id_hash = Some("session-a".to_string());
        old_event.model = Some(ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        old_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v1".to_string(),
            source_file_path_hash: Some("active-hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some(
                "semantic_usage_event.v1:codex_token_count:session-a:1715510400000:gpt-5:12:0:3:0:15"
                    .to_string(),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

        let mut new_event = old_event.clone();
        new_event.event_id = event_id("codex", &source.source_id, "modern-record", None, now);
        new_event.session.session_id = "session-b".to_string();
        new_event.session.local_session_id_hash = Some("session-b".to_string());
        new_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v2".to_string(),
            source_file_path_hash: Some("branch-hash".to_string()),
            source_line_number: Some(48),
            source_record_id: Some(
                "semantic_usage_event.v2:codex_token_count:1715510400000:gpt-5:12:0:3:0:15"
                    .to_string(),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

        assert!(store.insert_event(&old_event).expect("insert old"));
        let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
            provider: &old_event.provider,
            source_id: &old_event.source_id,
            started_at: old_event.session.started_at,
            session_hash: old_event.session.local_session_id_hash.as_deref(),
            project_key: None,
            model_name: model_key(&old_event),
            input_tokens: old_event.usage.input_tokens,
            cache_read_tokens: old_event.usage.cache_read_tokens,
            cache_creation_tokens: old_event.usage.cache_creation_tokens,
            output_tokens: old_event.usage.output_tokens,
            reasoning_tokens: old_event.usage.reasoning_tokens,
            total_tokens: old_event.usage.computed_total(),
        });
        store
            .conn
            .execute(
                "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
                params![legacy_fingerprint, &old_event.event_id.0],
            )
            .expect("downgrade fingerprint");

        assert!(!store
            .insert_event(&new_event)
            .expect("refresh legacy duplicate"));
        assert_eq!(store.event_count().expect("count"), 1);
        assert_eq!(
            store.events().expect("events")[0].event_id,
            old_event.event_id
        );
    }

    #[test]
    fn refreshes_legacy_projectless_codex_token_count_without_double_counting() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-token-count-project-upgrade"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let project = ProjectInfo {
            project_id: "project_shared".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("example/statsai".to_string()),
            branch_hash: Some("branch-hash".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/workspace/ai-stats".to_string()),
        };

        let mut old_event = test_store_event(&source, now, "legacy-projectless-record");
        old_event.session.session_id = "session-a".to_string();
        old_event.session.local_session_id_hash = Some("session-a".to_string());
        old_event.model = Some(ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        old_event.project = None;
        old_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v2".to_string(),
            source_file_path_hash: Some("active-hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some(
                "semantic_usage_event.v2:codex_token_count:1715510400000:gpt-5:12:0:3:0:15"
                    .to_string(),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

        let mut new_event = old_event.clone();
        new_event.event_id = event_id(
            "codex",
            &source.source_id,
            "modern-projectful-record",
            None,
            now,
        );
        new_event.session.session_id = "session-b".to_string();
        new_event.session.local_session_id_hash = Some("session-b".to_string());
        new_event.project = Some(project.clone());
        new_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("branch-hash".to_string()),
            source_line_number: Some(48),
            source_record_id: Some(format!(
                "semantic_usage_event.v4:codex_token_count:{}:1715510400000:gpt-5:12:0:3:0:15",
                project_bucket_key(Some(&project))
            )),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

        assert!(store.insert_event(&old_event).expect("insert old"));
        let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
            provider: &old_event.provider,
            source_id: &old_event.source_id,
            started_at: old_event.session.started_at,
            session_hash: old_event.session.local_session_id_hash.as_deref(),
            project_key: None,
            model_name: model_key(&old_event),
            input_tokens: old_event.usage.input_tokens,
            cache_read_tokens: old_event.usage.cache_read_tokens,
            cache_creation_tokens: old_event.usage.cache_creation_tokens,
            output_tokens: old_event.usage.output_tokens,
            reasoning_tokens: old_event.usage.reasoning_tokens,
            total_tokens: old_event.usage.computed_total(),
        });
        store
            .conn
            .execute(
                "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
                params![legacy_fingerprint, &old_event.event_id.0],
            )
            .expect("downgrade fingerprint");

        assert_eq!(
            store
                .insert_events(std::slice::from_ref(&new_event))
                .expect("refresh legacy projectless duplicate"),
            0
        );
        assert_eq!(store.event_count().expect("count"), 1);
        assert_eq!(
            store.events().expect("events")[0].event_id,
            old_event.event_id
        );
    }

    #[test]
    fn refreshes_legacy_codex_turn_usage_duplicate_without_double_counting() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-turn-usage"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let project = ProjectInfo {
            project_id: "project_shared".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("example/statsai".to_string()),
            branch_hash: Some("branch-hash".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/workspace/ai-stats".to_string()),
        };

        let mut old_event = test_store_event(&source, now, "legacy-record");
        old_event.session.session_id = "session-a".to_string();
        old_event.session.local_session_id_hash = Some("session-a".to_string());
        old_event.project = Some(project.clone());
        old_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v3".to_string(),
            source_file_path_hash: Some("active-hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some(
                format!(
                    "semantic_usage_event.v3:codex_turn_usage:{}:1715510400000:1715510405000:gpt-5:12:0:3:0:15",
                    project.project_id
                ),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });
        old_event.model = Some(ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        old_event.session.ended_at = Some(now + chrono::Duration::seconds(5));
        old_event.session.duration_seconds = Some(5);

        let mut new_event = old_event.clone();
        new_event.event_id = event_id("codex", &source.source_id, "modern-record", None, now);
        new_event.session.session_id = "session-b".to_string();
        new_event.session.local_session_id_hash = Some("session-b".to_string());
        new_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("branch-hash".to_string()),
            source_line_number: Some(48),
            source_record_id: Some(
                format!(
                    "semantic_usage_event.v4:codex_turn_usage:{}:1715510400000:1715510405000:gpt-5:12:0:3:0:15",
                    project_bucket_key(Some(&project))
                ),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

        assert!(store.insert_event(&old_event).expect("insert old"));
        let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
            provider: &old_event.provider,
            source_id: &old_event.source_id,
            started_at: old_event.session.started_at,
            session_hash: old_event.session.local_session_id_hash.as_deref(),
            project_key: Some("repo:repo-hash|path:path-hash"),
            model_name: model_key(&old_event),
            input_tokens: old_event.usage.input_tokens,
            cache_read_tokens: old_event.usage.cache_read_tokens,
            cache_creation_tokens: old_event.usage.cache_creation_tokens,
            output_tokens: old_event.usage.output_tokens,
            reasoning_tokens: old_event.usage.reasoning_tokens,
            total_tokens: old_event.usage.computed_total(),
        });
        store
            .conn
            .execute(
                "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
                params![legacy_fingerprint, &old_event.event_id.0],
            )
            .expect("downgrade fingerprint");

        assert!(!store
            .insert_event(&new_event)
            .expect("refresh legacy duplicate"));
        assert_eq!(store.event_count().expect("count"), 1);
        assert_eq!(
            store.events().expect("events")[0].event_id,
            old_event.event_id
        );
    }

    #[test]
    fn refreshes_legacy_projectless_codex_turn_usage_without_double_counting() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-turn-usage-project-upgrade"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let project = ProjectInfo {
            project_id: "project_shared".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("example/statsai".to_string()),
            branch_hash: Some("branch-hash".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/workspace/ai-stats".to_string()),
        };

        let mut old_event = test_store_event(&source, now, "legacy-projectless-turn");
        old_event.session.session_id = "session-a".to_string();
        old_event.session.local_session_id_hash = Some("session-a".to_string());
        old_event.project = None;
        old_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v3".to_string(),
            source_file_path_hash: Some("active-hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some(
                "semantic_usage_event.v3:codex_turn_usage:1715510400000:1715510405000:gpt-5:12:0:3:0:15"
                    .to_string(),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });
        old_event.model = Some(ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        old_event.session.ended_at = Some(now + chrono::Duration::seconds(5));
        old_event.session.duration_seconds = Some(5);

        let mut new_event = old_event.clone();
        new_event.event_id = event_id(
            "codex",
            &source.source_id,
            "modern-projectful-turn",
            None,
            now,
        );
        new_event.session.session_id = "session-b".to_string();
        new_event.session.local_session_id_hash = Some("session-b".to_string());
        new_event.project = Some(project.clone());
        new_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("branch-hash".to_string()),
            source_line_number: Some(48),
            source_record_id: Some(
                format!(
                    "semantic_usage_event.v4:codex_turn_usage:{}:1715510400000:1715510405000:gpt-5:12:0:3:0:15",
                    project_bucket_key(Some(&project))
                ),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

        assert!(store.insert_event(&old_event).expect("insert old"));
        let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
            provider: &old_event.provider,
            source_id: &old_event.source_id,
            started_at: old_event.session.started_at,
            session_hash: old_event.session.local_session_id_hash.as_deref(),
            project_key: None,
            model_name: model_key(&old_event),
            input_tokens: old_event.usage.input_tokens,
            cache_read_tokens: old_event.usage.cache_read_tokens,
            cache_creation_tokens: old_event.usage.cache_creation_tokens,
            output_tokens: old_event.usage.output_tokens,
            reasoning_tokens: old_event.usage.reasoning_tokens,
            total_tokens: old_event.usage.computed_total(),
        });
        store
            .conn
            .execute(
                "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
                params![legacy_fingerprint, &old_event.event_id.0],
            )
            .expect("downgrade fingerprint");

        assert!(!store
            .insert_event(&new_event)
            .expect("refresh legacy projectless turn duplicate"));
        assert_eq!(store.event_count().expect("count"), 1);
        assert_eq!(
            store.events().expect("events")[0].event_id,
            old_event.event_id
        );
    }

    #[test]
    fn refreshes_legacy_project_id_only_codex_token_count_without_double_counting() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-token-count-project-id-upgrade"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let legacy_project = ProjectInfo {
            project_id: "project_shared".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: None,
            path_label: None,
        };
        let project = ProjectInfo {
            project_id: "project_shared".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("example/statsai".to_string()),
            branch_hash: Some("branch-hash".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/workspace/ai-stats".to_string()),
        };

        let mut old_event = test_store_event(&source, now, "legacy-project-id-token-count");
        old_event.session.session_id = "session-a".to_string();
        old_event.session.local_session_id_hash = Some("session-a".to_string());
        old_event.model = Some(ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        old_event.project = Some(legacy_project.clone());
        old_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v2".to_string(),
            source_file_path_hash: Some("active-hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some(format!(
                "semantic_usage_event.v2:codex_token_count:{}:1715510400000:gpt-5:12:0:3:0:15",
                legacy_project.project_id
            )),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

        let mut new_event = old_event.clone();
        new_event.event_id = event_id(
            "codex",
            &source.source_id,
            "modern-projectful-token-count",
            None,
            now,
        );
        new_event.session.session_id = "session-b".to_string();
        new_event.session.local_session_id_hash = Some("session-b".to_string());
        new_event.project = Some(project.clone());
        new_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("branch-hash".to_string()),
            source_line_number: Some(48),
            source_record_id: Some(format!(
                "semantic_usage_event.v4:codex_token_count:{}:1715510400000:gpt-5:12:0:3:0:15",
                project_bucket_key(Some(&project))
            )),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

        assert!(store.insert_event(&old_event).expect("insert old"));
        let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
            provider: &old_event.provider,
            source_id: &old_event.source_id,
            started_at: old_event.session.started_at,
            session_hash: old_event.session.local_session_id_hash.as_deref(),
            project_key: Some(legacy_project.project_id.as_str()),
            model_name: model_key(&old_event),
            input_tokens: old_event.usage.input_tokens,
            cache_read_tokens: old_event.usage.cache_read_tokens,
            cache_creation_tokens: old_event.usage.cache_creation_tokens,
            output_tokens: old_event.usage.output_tokens,
            reasoning_tokens: old_event.usage.reasoning_tokens,
            total_tokens: old_event.usage.computed_total(),
        });
        store
            .conn
            .execute(
                "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
                params![legacy_fingerprint, &old_event.event_id.0],
            )
            .expect("downgrade fingerprint");

        assert!(!store
            .insert_event(&new_event)
            .expect("refresh legacy project-id duplicate"));
        assert_eq!(store.event_count().expect("count"), 1);
        assert_eq!(
            store.events().expect("events")[0].event_id,
            old_event.event_id
        );
    }

    #[test]
    fn refreshes_legacy_project_id_only_codex_turn_usage_without_double_counting() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-turn-usage-project-id-upgrade"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let legacy_project = ProjectInfo {
            project_id: "project_shared".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: None,
            path_label: None,
        };
        let project = ProjectInfo {
            project_id: "project_shared".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("example/statsai".to_string()),
            branch_hash: Some("branch-hash".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/workspace/ai-stats".to_string()),
        };

        let mut old_event = test_store_event(&source, now, "legacy-project-id-turn");
        old_event.session.session_id = "session-a".to_string();
        old_event.session.local_session_id_hash = Some("session-a".to_string());
        old_event.project = Some(legacy_project.clone());
        old_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v3".to_string(),
            source_file_path_hash: Some("active-hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some(
                format!(
                    "semantic_usage_event.v3:codex_turn_usage:{}:1715510400000:1715510405000:gpt-5:12:0:3:0:15",
                    legacy_project.project_id
                ),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });
        old_event.model = Some(ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        old_event.session.ended_at = Some(now + chrono::Duration::seconds(5));
        old_event.session.duration_seconds = Some(5);

        let mut new_event = old_event.clone();
        new_event.event_id = event_id(
            "codex",
            &source.source_id,
            "modern-projectful-turn",
            None,
            now,
        );
        new_event.session.session_id = "session-b".to_string();
        new_event.session.local_session_id_hash = Some("session-b".to_string());
        new_event.project = Some(project.clone());
        new_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("branch-hash".to_string()),
            source_line_number: Some(48),
            source_record_id: Some(
                format!(
                    "semantic_usage_event.v4:codex_turn_usage:{}:1715510400000:1715510405000:gpt-5:12:0:3:0:15",
                    project_bucket_key(Some(&project))
                ),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

        assert!(store.insert_event(&old_event).expect("insert old"));
        let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
            provider: &old_event.provider,
            source_id: &old_event.source_id,
            started_at: old_event.session.started_at,
            session_hash: old_event.session.local_session_id_hash.as_deref(),
            project_key: Some(legacy_project.project_id.as_str()),
            model_name: model_key(&old_event),
            input_tokens: old_event.usage.input_tokens,
            cache_read_tokens: old_event.usage.cache_read_tokens,
            cache_creation_tokens: old_event.usage.cache_creation_tokens,
            output_tokens: old_event.usage.output_tokens,
            reasoning_tokens: old_event.usage.reasoning_tokens,
            total_tokens: old_event.usage.computed_total(),
        });
        store
            .conn
            .execute(
                "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
                params![legacy_fingerprint, &old_event.event_id.0],
            )
            .expect("downgrade fingerprint");

        assert!(!store
            .insert_event(&new_event)
            .expect("refresh legacy project-id duplicate"));
        assert_eq!(store.event_count().expect("count"), 1);
        assert_eq!(
            store.events().expect("events")[0].event_id,
            old_event.event_id
        );
    }

    #[test]
    fn refreshes_legacy_codex_usage_shape_after_normalization_without_double_counting() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-normalized"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let mut old_event = test_store_event(&source, now, "legacy-inclusive");
        old_event.model = Some(ModelInfo {
            name: Some("gpt-5-codex".to_string()),
            normalized_name: Some("gpt-5-codex".to_string()),
            provider_model_id: Some("gpt-5-codex".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        old_event.usage = UsageCounts {
            input_tokens: Some(100),
            cache_read_tokens: Some(30),
            output_tokens: Some(10),
            reasoning_tokens: Some(5),
            total_tokens: Some(110),
            requests: Some(1),
            ..UsageCounts::default()
        };

        let mut new_event = old_event.clone();
        new_event.event_id = event_id("codex", &source.source_id, "normalized", None, now);
        new_event.usage = UsageCounts {
            input_tokens: Some(70),
            cache_read_tokens: Some(30),
            output_tokens: Some(5),
            reasoning_tokens: Some(5),
            total_tokens: Some(110),
            requests: Some(1),
            ..UsageCounts::default()
        };

        assert!(store.insert_event(&old_event).expect("insert old"));
        assert!(!store
            .insert_event(&new_event)
            .expect("refresh normalized duplicate"));

        let events = store.events().expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, old_event.event_id);
        assert_eq!(events[0].usage.input_tokens, Some(70));
        assert_eq!(events[0].usage.cache_read_tokens, Some(30));
        assert_eq!(events[0].usage.output_tokens, Some(5));
        assert_eq!(events[0].usage.reasoning_tokens, Some(5));
    }

    #[test]
    fn insert_events_batches_in_one_transaction() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-batch"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let events = vec![
            test_store_event(&source, now, "record-a"),
            test_store_event(&source, now + chrono::Duration::seconds(1), "record-b"),
        ];

        assert_eq!(store.insert_events(&events).expect("batch"), 2);
        assert_eq!(store.insert_events(&events).expect("batch duplicate"), 0);
        assert_eq!(store.event_count().expect("count"), 2);
    }

    #[test]
    fn daily_rollup_saturates_imported_usage_and_costs() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-rollup-overflow"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let mut first = test_store_event(&source, now, "overflow-a");
        first.usage = UsageCounts {
            input_tokens: Some(u64::MAX),
            cache_creation_tokens: Some(u64::MAX),
            cache_read_tokens: Some(u64::MAX),
            output_tokens: Some(u64::MAX),
            reasoning_tokens: Some(u64::MAX),
            total_tokens: Some(u64::MAX),
            ..UsageCounts::default()
        };
        first.cost.estimated_api_equivalent_usd = Some(i64::MAX);
        let mut second = first.clone();
        second.event_id = event_id(
            "codex",
            &source.source_id,
            "overflow-b",
            None,
            now + chrono::Duration::seconds(1),
        );
        second.session.started_at = now + chrono::Duration::seconds(1);
        second.source.source_record_id = Some("overflow-b".to_string());
        store.insert_events(&[first, second]).expect("events");

        let rollup = store
            .compute_daily_rollup(&now.format("%Y-%m-%d").to_string(), "device")
            .expect("rollup");

        assert_eq!(rollup.total_input_tokens, u64::MAX);
        assert_eq!(rollup.total_cache_creation_tokens, u64::MAX);
        assert_eq!(rollup.total_cache_read_tokens, u64::MAX);
        assert_eq!(rollup.total_output_tokens, u64::MAX);
        assert_eq!(rollup.total_reasoning_tokens, u64::MAX);
        assert_eq!(rollup.total_tokens, u64::MAX);
        assert_eq!(rollup.total_events, 2);
        assert_eq!(rollup.estimated_cost_usd, Some(i64::MAX));
        let by_provider: serde_json::Value =
            serde_json::from_str(rollup.by_provider.as_deref().expect("provider totals"))
                .expect("provider JSON");
        assert_eq!(by_provider["codex"]["tokens"].as_u64(), Some(u64::MAX));
    }

    #[test]
    fn scan_file_replacement_rolls_back_deletions_when_cache_update_fails() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-atomic-replacement"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let cache_key = "/tmp/codex-atomic-replacement/session.jsonl".to_string();
        let file_hash = hash_text(&cache_key);
        let mut old_event = test_store_event(&source, now, "old-record");
        old_event.parse_evidence = Some(statsai_core::ParseEvidence {
            event_key_version: "v1".to_string(),
            source_file_path_hash: Some(file_hash.clone()),
            source_line_number: Some(1),
            source_record_id: Some("old-record".to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::Unresolved,
        });
        let mut old_summary = test_store_summary(&source, now, 15);
        old_summary.parse_evidence = old_event.parse_evidence.clone();
        store.insert_event(&old_event).expect("old event");
        store.upsert_summary(&old_summary).expect("old summary");
        store
            .record_scan_file_entries(
                &source.source_id,
                &[ScanFileStateEntry {
                    cache_key: cache_key.clone(),
                    cache_signature: "old-signature".to_string(),
                }],
            )
            .expect("old cache entry");
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_scan_cache_update
                 BEFORE UPDATE ON scan_file_state
                 WHEN NEW.cache_signature = 'fail-signature'
                 BEGIN
                   SELECT RAISE(FAIL, 'injected scan cache failure');
                 END;",
            )
            .expect("failure trigger");

        let replacement = ScanFileStateEntry {
            cache_key,
            cache_signature: "fail-signature".to_string(),
        };
        let error = store
            .replace_scan_file_records(ScanFileReplacement {
                source_id: &source.source_id,
                reconciled_file_hashes: &[file_hash],
                events: &[],
                summaries: &[],
                pending_entries: &[replacement],
                compatible_entries_to_upgrade: &[],
                removed_cache_keys: &[],
            })
            .expect_err("replacement should fail");

        assert!(error.to_string().contains("injected scan cache failure"));
        assert_eq!(store.event_count().expect("event count"), 1);
        assert_eq!(store.summary_count().expect("summary count"), 1);
        assert_eq!(
            store
                .scan_file_entries(&source.source_id)
                .expect("cache entries")[0]
                .cache_signature,
            "old-signature"
        );
    }

    #[test]
    fn scan_update_rolls_back_nested_writes_when_cache_update_fails() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-atomic-scan"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let old_event = test_store_event(&source, Utc::now(), "old-record");
        store.insert_event(&old_event).expect("old event");
        let cache_key = "/tmp/codex-atomic-scan/session.jsonl".to_string();
        store
            .record_scan_file_entries(
                &source.source_id,
                &[ScanFileStateEntry {
                    cache_key: cache_key.clone(),
                    cache_signature: "old-signature".to_string(),
                }],
            )
            .expect("old cache entry");
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_atomic_scan_cache_update
                 BEFORE UPDATE ON scan_file_state
                 WHEN NEW.cache_signature = 'fail-signature'
                 BEGIN
                   SELECT RAISE(FAIL, 'injected atomic scan cache failure');
                 END;",
            )
            .expect("failure trigger");

        let error = store
            .apply_scan_update(|store| {
                store.delete_events_for_sources(std::slice::from_ref(&source.source_id))?;
                store.record_scan_file_entries(
                    &source.source_id,
                    &[ScanFileStateEntry {
                        cache_key,
                        cache_signature: "fail-signature".to_string(),
                    }],
                )?;
                Ok(())
            })
            .expect_err("scan update should fail");

        assert!(error
            .to_string()
            .contains("injected atomic scan cache failure"));
        assert_eq!(store.events().expect("events"), vec![old_event]);
        assert_eq!(
            store
                .scan_file_entries(&source.source_id)
                .expect("cache entries")[0]
                .cache_signature,
            "old-signature"
        );
    }

    #[test]
    fn upserts_usage_summaries_idempotently() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-summary"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let mut summary = test_store_summary(&source, now, 100);

        assert!(store.upsert_summary(&summary).expect("insert"));
        summary.usage.input_tokens = Some(150);
        summary.usage.total_tokens = Some(150);
        assert!(store.upsert_summary(&summary).expect("update"));

        let summaries = store.summaries().expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].usage.total_tokens, Some(150));
        assert_eq!(store.summary_count().expect("count"), 1);
    }

    #[test]
    fn scan_file_state_tracks_only_changed_entries() {
        let store = Store::in_memory().expect("store");
        let source_id = SourceId("src_scan_cache".to_string());
        let first = vec![
            ScanFileStateEntry {
                cache_key: "/tmp/a.jsonl".to_string(),
                cache_signature: "sig-a-1".to_string(),
            },
            ScanFileStateEntry {
                cache_key: "/tmp/b.jsonl".to_string(),
                cache_signature: "sig-b-1".to_string(),
            },
        ];

        let pending = store
            .pending_scan_file_entries(&source_id, &first)
            .expect("initial pending");
        assert_eq!(pending, first);
        store
            .record_scan_file_entries(&source_id, &pending)
            .expect("record");

        let unchanged = store
            .pending_scan_file_entries(&source_id, &first)
            .expect("unchanged");
        assert!(unchanged.is_empty());

        let changed = vec![
            ScanFileStateEntry {
                cache_key: "/tmp/a.jsonl".to_string(),
                cache_signature: "sig-a-2".to_string(),
            },
            ScanFileStateEntry {
                cache_key: "/tmp/b.jsonl".to_string(),
                cache_signature: "sig-b-1".to_string(),
            },
            ScanFileStateEntry {
                cache_key: "/tmp/c.jsonl".to_string(),
                cache_signature: "sig-c-1".to_string(),
            },
        ];
        let pending = store
            .pending_scan_file_entries(&source_id, &changed)
            .expect("changed pending");
        assert_eq!(pending.len(), 2);
        assert!(pending
            .iter()
            .any(|entry| entry.cache_key == "/tmp/a.jsonl"));
        assert!(pending
            .iter()
            .any(|entry| entry.cache_key == "/tmp/c.jsonl"));
    }

    #[test]
    fn scan_file_state_accepts_compatible_signatures() {
        let store = Store::in_memory().expect("store");
        let source_id = SourceId("src_scan_cache_compat".to_string());
        let legacy = ScanFileStateEntry {
            cache_key: "/tmp/a.jsonl".to_string(),
            cache_signature: "legacy-auth-signature".to_string(),
        };
        store
            .record_scan_file_entries(&source_id, std::slice::from_ref(&legacy))
            .expect("record legacy cache state");

        let current = ScanFileStateEntry {
            cache_key: legacy.cache_key.clone(),
            cache_signature: "current-signature".to_string(),
        };
        let compatible_signatures = HashMap::from([(
            current.cache_key.clone(),
            vec![legacy.cache_signature.clone()],
        )]);

        let selection = store
            .select_scan_file_state_entries_with_task_requirement_and_compatibility(
                &source_id,
                std::slice::from_ref(&current),
                false,
                &compatible_signatures,
            )
            .expect("compatible selection");

        assert!(selection.pending_entries.is_empty());
        assert_eq!(
            selection.compatible_entries_to_upgrade,
            vec![current.clone()]
        );

        store
            .upgrade_scan_file_entries(&source_id, &selection.compatible_entries_to_upgrade)
            .expect("upgrade compatible entries");

        let stored_entries = store
            .scan_file_entries(&source_id)
            .expect("stored scan file entries");
        assert_eq!(stored_entries, vec![current]);
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
    fn sync_rollup_sums_micro_usd_before_rounding_to_cents() {
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-micro-usd-rollup"),
            LocationOrigin::Configured,
        );
        let day = Utc
            .with_ymd_and_hms(2026, 5, 28, 9, 0, 0)
            .single()
            .expect("day");
        let mut event = test_store_event(&source, day, "record-a");
        event.cost.set_estimated_micro_usd(2_250);
        let events = vec![event; 1_000];

        let summary = build_sync_rollup_summary(&events);

        assert_eq!(
            summary.cost.estimated_api_equivalent_micro_usd,
            Some(2_250_000)
        );
        assert_eq!(summary.cost.estimated_api_equivalent_usd, Some(225));
    }

    #[test]
    fn sync_rollups_track_dirty_daily_buckets() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-sync-rollups"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let day = Utc
            .with_ymd_and_hms(2026, 5, 28, 9, 0, 0)
            .single()
            .expect("day");
        let account_id = statsai_core::provider_account_id("codex", "personal");
        let mut first = test_store_event(&source, day, "record-a");
        first.provider_account_id = Some(account_id.clone());
        first.usage.total_tokens = Some(15);
        first.model = Some(ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        first.cost.provider_reported_usd = Some(11);

        assert!(store.insert_event(&first).expect("insert first"));
        let dirty = store
            .dirty_sync_rollup_summaries()
            .expect("dirty rollups after first");
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].usage.total_tokens, Some(15));
        assert_eq!(dirty[0].metadata.summary_format, "daily_rollup.v1");
        assert_eq!(
            dirty[0].metadata.summary_version.as_deref(),
            Some(SYNC_ROLLUP_SUMMARY_VERSION)
        );
        assert_eq!(
            dirty[0]
                .period_start
                .expect("period start")
                .date_naive()
                .to_string(),
            "2026-05-28"
        );
        assert_eq!(dirty[0].models.len(), 1);
        assert_eq!(
            dirty[0].models[0].model.normalized_name.as_deref(),
            Some("gpt-5")
        );
        assert_eq!(dirty[0].models[0].usage.total_tokens, Some(15));
        assert_eq!(dirty[0].cost.provider_reported_usd, Some(11));

        store
            .mark_sync_rollups_synced(&[dirty[0].summary_id.clone()])
            .expect("mark clean");
        assert!(store
            .dirty_sync_rollup_summaries()
            .expect("no dirty after clean")
            .is_empty());

        let mut second = test_store_event(&source, day + chrono::Duration::hours(1), "record-b");
        second.provider_account_id = Some(account_id);
        second.usage.total_tokens = Some(25);
        second.model = Some(ModelInfo {
            name: Some("gpt-4.1".to_string()),
            normalized_name: Some("gpt-4.1".to_string()),
            provider_model_id: Some("gpt-4.1".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        second.cost.provider_reported_usd = Some(22);

        assert!(store.insert_event(&second).expect("insert second"));
        let dirty = store
            .dirty_sync_rollup_summaries()
            .expect("dirty rollups after second");
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].usage.total_tokens, Some(40));
        assert_eq!(dirty[0].usage.requests, Some(2));
        assert_eq!(dirty[0].cost.provider_reported_usd, Some(33));
        assert_eq!(dirty[0].models.len(), 2);
        assert_eq!(dirty[0].models[0].usage.total_tokens, Some(25));
        assert_eq!(dirty[0].models[1].usage.total_tokens, Some(15));
        assert_eq!(dirty[0].metadata.total_sessions, Some(1));
    }

    #[test]
    fn sync_rollups_preserve_cache_creation_lifetimes() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-sync-rollup-cache-ttl"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 7, 12, 10, 0, 0)
            .single()
            .expect("now");
        let mut event = test_store_event(&source, now, "record-cache-ttl");
        event.usage.cache_creation_tokens = Some(30);
        event.usage.cache_creation_5m_tokens = Some(18);
        event.usage.cache_creation_1h_tokens = Some(12);

        assert!(store.insert_event(&event).expect("insert event"));
        let rollups = store.dirty_sync_rollup_summaries().expect("dirty rollups");

        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].usage.cache_creation_tokens, Some(30));
        assert_eq!(rollups[0].usage.cache_creation_5m_tokens, Some(18));
        assert_eq!(rollups[0].usage.cache_creation_1h_tokens, Some(12));
        assert_eq!(rollups[0].models.len(), 1);
        assert_eq!(
            rollups[0].models[0].usage.cache_creation_5m_tokens,
            Some(18)
        );
        assert_eq!(
            rollups[0].models[0].usage.cache_creation_1h_tokens,
            Some(12)
        );
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
    fn sync_rollups_split_same_model_by_reasoning_level() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-sync-rollups-reasoning"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let day = Utc
            .with_ymd_and_hms(2026, 5, 29, 9, 0, 0)
            .single()
            .expect("day");
        let mut low = test_store_event(&source, day, "record-low");
        low.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::Low),
            reasoning_level_raw: Some("low".to_string()),
        });
        low.usage.total_tokens = Some(15);

        let mut high = test_store_event(&source, day + chrono::Duration::hours(1), "record-high");
        high.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::High),
            reasoning_level_raw: Some("high".to_string()),
        });
        high.usage.total_tokens = Some(25);

        assert!(store.insert_event(&low).expect("insert low"));
        assert!(store.insert_event(&high).expect("insert high"));

        let dirty = store.dirty_sync_rollup_summaries().expect("dirty rollups");
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].models.len(), 2);
        assert!(dirty[0].models.iter().any(|entry| {
            entry.model.reasoning_level == Some(ReasoningLevel::Low)
                && entry.usage.total_tokens == Some(15)
        }));
        assert!(dirty[0].models.iter().any(|entry| {
            entry.model.reasoning_level == Some(ReasoningLevel::High)
                && entry.usage.total_tokens == Some(25)
        }));
    }

    #[test]
    fn sync_rollups_split_same_model_by_speed() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-sync-rollups-speed"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let day = Utc
            .with_ymd_and_hms(2026, 8, 1, 9, 0, 0)
            .single()
            .expect("day");
        let mut standard = test_store_event(&source, day, "record-standard");
        standard.model = Some(ModelInfo {
            name: Some("claude-opus-5".to_string()),
            normalized_name: Some("claude-opus-5".to_string()),
            provider_model_id: Some("claude-opus-5".to_string()),
            speed: Some("standard".to_string()),
            reasoning_level: Some(ReasoningLevel::Medium),
            reasoning_level_raw: Some("medium".to_string()),
        });
        standard.usage.total_tokens = Some(15);

        let mut fast = test_store_event(&source, day + chrono::Duration::hours(1), "record-fast");
        fast.model = Some(ModelInfo {
            name: Some("claude-opus-5".to_string()),
            normalized_name: Some("claude-opus-5".to_string()),
            provider_model_id: Some("claude-opus-5".to_string()),
            speed: Some("fast".to_string()),
            reasoning_level: Some(ReasoningLevel::Medium),
            reasoning_level_raw: Some("medium".to_string()),
        });
        fast.usage.total_tokens = Some(25);

        assert!(store.insert_event(&standard).expect("insert standard"));
        assert!(store.insert_event(&fast).expect("insert fast"));

        let dirty = store.dirty_sync_rollup_summaries().expect("dirty rollups");
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].models.len(), 2);
        assert!(dirty[0].models.iter().any(|entry| {
            entry.model.speed.as_deref() == Some("standard") && entry.usage.total_tokens == Some(15)
        }));
        assert!(dirty[0].models.iter().any(|entry| {
            entry.model.speed.as_deref() == Some("fast") && entry.usage.total_tokens == Some(25)
        }));
    }

    #[test]
    fn sync_rollups_split_same_day_usage_by_project_location() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-sync-projects"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let day = Utc
            .with_ymd_and_hms(2026, 6, 1, 9, 0, 0)
            .single()
            .expect("day");
        let account_id = statsai_core::provider_account_id("codex", "personal");

        let mut first = test_store_event(&source, day, "record-project-a");
        first.provider_account_id = Some(account_id.clone());
        first.usage.total_tokens = Some(10);
        first.project = Some(statsai_core::ProjectInfo {
            project_id: "project-a".to_string(),
            project_label: Some("Project A".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: Some("branch-main".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-a".to_string()),
            path_label: Some("/tmp/project-a".to_string()),
        });

        let mut second = test_store_event(
            &source,
            day + chrono::Duration::hours(1),
            "record-project-b",
        );
        second.provider_account_id = Some(account_id);
        second.usage.total_tokens = Some(20);
        second.project = Some(statsai_core::ProjectInfo {
            project_id: "project-b".to_string(),
            project_label: Some("Project B".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: Some("branch-main".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-b".to_string()),
            path_label: Some("/tmp/project-b".to_string()),
        });

        assert!(store.insert_event(&first).expect("insert first"));
        assert!(store.insert_event(&second).expect("insert second"));

        let dirty = store
            .dirty_sync_rollup_summaries()
            .expect("dirty rollups after project split");
        assert_eq!(dirty.len(), 2);
        assert_ne!(dirty[0].summary_id, dirty[1].summary_id);
        assert_ne!(dirty[0].project, dirty[1].project);
        assert_eq!(
            dirty
                .iter()
                .map(|summary| summary.usage.total_tokens.unwrap_or(0))
                .sum::<u64>(),
            30
        );
    }

    #[test]
    fn sync_rollups_split_same_day_usage_by_branch() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-sync-branches"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let day = Utc
            .with_ymd_and_hms(2026, 6, 1, 9, 0, 0)
            .single()
            .expect("day");
        let account_id = statsai_core::provider_account_id("codex", "personal");

        let mut first = test_store_event(&source, day, "record-branch-main");
        first.provider_account_id = Some(account_id.clone());
        first.usage.total_tokens = Some(10);
        first.project = Some(statsai_core::ProjectInfo {
            project_id: "project-shared".to_string(),
            project_label: Some("Project".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: Some("branch-main".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-shared".to_string()),
            path_label: Some("/tmp/project".to_string()),
        });

        let mut second = test_store_event(
            &source,
            day + chrono::Duration::hours(1),
            "record-branch-feature",
        );
        second.provider_account_id = Some(account_id);
        second.usage.total_tokens = Some(20);
        second.project = Some(statsai_core::ProjectInfo {
            project_id: "project-shared".to_string(),
            project_label: Some("Project".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: Some("branch-feature".to_string()),
            branch_label: Some("feature-x".to_string()),
            path_hash: Some("path-shared".to_string()),
            path_label: Some("/tmp/project".to_string()),
        });

        assert!(store.insert_event(&first).expect("insert first"));
        assert!(store.insert_event(&second).expect("insert second"));

        let dirty = store
            .dirty_sync_rollup_summaries()
            .expect("dirty rollups after branch split");
        assert_eq!(dirty.len(), 2);

        let mut branches = dirty
            .iter()
            .map(|summary| {
                summary
                    .project
                    .as_ref()
                    .and_then(|project| project.branch_label.clone())
                    .expect("branch")
            })
            .collect::<Vec<_>>();
        branches.sort();

        assert_eq!(branches, vec!["feature-x".to_string(), "main".to_string()]);
    }

    #[test]
    fn path_independent_codex_events_keep_distinct_branches() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-branch-dedupe"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let mut main = test_store_event(&source, now, "branch-main");
        main.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("main-hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some(
                "semantic_usage_event.v4:codex_turn_usage:repo:repo-hash|path:path-shared|branch:branch-main:1715510400000:1715510405000:gpt-5:12:0:3:0:15"
                    .to_string(),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });
        main.model = Some(ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        main.project = Some(ProjectInfo {
            project_id: "project-shared".to_string(),
            project_label: Some("Project".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: Some("branch-main".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-shared".to_string()),
            path_label: Some("/tmp/project".to_string()),
        });

        let mut feature = main.clone();
        feature.event_id = event_id("codex", &source.source_id, "branch-feature", None, now);
        feature.source.source_record_id = Some("branch-feature".to_string());
        feature.project = Some(ProjectInfo {
            project_id: "project-shared".to_string(),
            project_label: Some("Project".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: Some("branch-feature".to_string()),
            branch_label: Some("feature-x".to_string()),
            path_hash: Some("path-shared".to_string()),
            path_label: Some("/tmp/project".to_string()),
        });
        feature.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("feature-hash".to_string()),
            source_line_number: Some(18),
            source_record_id: Some(
                "semantic_usage_event.v4:codex_turn_usage:repo:repo-hash|path:path-shared|branch:branch-feature:1715510400000:1715510405000:gpt-5:12:0:3:0:15"
                    .to_string(),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

        assert!(store.insert_event(&main).expect("insert main"));
        assert!(store.insert_event(&feature).expect("insert feature"));
        assert_eq!(store.event_count().expect("count"), 2);
    }

    #[test]
    fn sync_rollups_capture_daily_runtime_metrics() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-sync-metrics"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let day = Utc
            .with_ymd_and_hms(2026, 5, 29, 9, 0, 0)
            .single()
            .expect("day");
        let mut first = test_store_event(&source, day, "metrics-a");
        first.session.session_id = "session-a".to_string();
        first.session.local_session_id_hash = Some("session-a".to_string());
        first.model = Some(ModelInfo {
            name: Some("gpt-5.6-sol".to_string()),
            normalized_name: Some("gpt-5.6-sol".to_string()),
            provider_model_id: Some("gpt-5.6-sol".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::High),
            reasoning_level_raw: Some("high".to_string()),
        });
        first.usage = UsageCounts {
            input_tokens: Some(60),
            output_tokens: Some(30),
            cache_read_tokens: Some(20),
            reasoning_tokens: Some(10),
            total_tokens: Some(120),
            requests: Some(1),
            ..UsageCounts::default()
        };
        first.runtime = Some(statsai_core::RuntimeInfo {
            runtime_name: None,
            host_id: None,
            latency_ms: Some(5000),
            latency_source: Some(LatencySource::Explicit),
            time_to_first_token_ms: Some(1200),
            prompt_eval_duration_ms: None,
            eval_duration_ms: None,
            total_messages: Some(2),
            user_messages: Some(1),
            assistant_messages: Some(1),
            developer_messages: Some(0),
        });

        let mut second = test_store_event(&source, day + chrono::Duration::minutes(2), "metrics-b");
        second.session.session_id = "session-b".to_string();
        second.session.local_session_id_hash = Some("session-b".to_string());
        second.model = Some(ModelInfo {
            name: Some("codex-auto-review".to_string()),
            normalized_name: Some("codex-auto-review".to_string()),
            provider_model_id: Some("codex-auto-review".to_string()),
            speed: None,
            reasoning_level: Some(ReasoningLevel::Low),
            reasoning_level_raw: Some("low".to_string()),
        });
        second.usage = UsageCounts {
            input_tokens: Some(40),
            output_tokens: Some(20),
            cache_read_tokens: Some(10),
            reasoning_tokens: Some(0),
            total_tokens: Some(70),
            requests: Some(1),
            ..UsageCounts::default()
        };
        second.runtime = Some(statsai_core::RuntimeInfo {
            runtime_name: None,
            host_id: None,
            latency_ms: Some(3000),
            latency_source: Some(LatencySource::Explicit),
            time_to_first_token_ms: Some(800),
            prompt_eval_duration_ms: None,
            eval_duration_ms: None,
            total_messages: Some(3),
            user_messages: Some(1),
            assistant_messages: Some(2),
            developer_messages: Some(0),
        });

        assert!(store.insert_event(&first).expect("insert first"));
        assert!(store.insert_event(&second).expect("insert second"));

        let dirty = store
            .dirty_sync_rollup_summaries()
            .expect("dirty rollups after metrics");
        assert_eq!(dirty.len(), 1);
        assert_eq!(
            dirty[0].metadata.summary_version.as_deref(),
            Some(SYNC_ROLLUP_SUMMARY_VERSION)
        );
        assert_eq!(dirty[0].metadata.total_sessions, Some(2));
        assert_eq!(dirty[0].metadata.total_messages, Some(5));
        let metrics = dirty[0].metrics.as_ref().expect("metrics");
        assert_eq!(metrics.active_seconds, Some(8.0));
        assert_eq!(metrics.tracked_requests, Some(2));
        assert_eq!(metrics.tracked_output_tokens, Some(50));
        assert_eq!(metrics.tracked_reasoning_tokens, Some(10));
        assert_eq!(metrics.total_messages, Some(5));
        assert_eq!(metrics.user_messages, Some(2));
        assert_eq!(metrics.assistant_messages, Some(3));
        assert_eq!(
            metrics.latency_ms.as_ref().map(|value| value.samples),
            Some(2)
        );
        assert_eq!(
            metrics.latency_ms.as_ref().and_then(|value| value.min),
            Some(3000.0)
        );
        assert_eq!(
            metrics.latency_ms.as_ref().and_then(|value| value.max),
            Some(5000.0)
        );
        assert_eq!(
            metrics
                .time_to_first_token_ms
                .as_ref()
                .and_then(|value| value.avg),
            Some(1000.0)
        );
        assert_eq!(
            metrics.generated_tps.as_ref().and_then(|value| value.min),
            Some(20.0 / 3.0)
        );
        assert_eq!(metrics.overall_generated_tps, Some(7.5));
        assert_eq!(metrics.overall_visible_tps, Some(6.25));
        assert_eq!(dirty[0].models.len(), 2);
        let primary = dirty[0]
            .models
            .iter()
            .find(|entry| entry.model.normalized_name.as_deref() == Some("gpt-5.6-sol"))
            .expect("primary model metrics");
        assert_eq!(
            primary
                .metrics
                .as_ref()
                .and_then(|metrics| metrics.generated_tps.as_ref()),
            Some(&SummaryMetricTotals {
                samples: 1,
                sum: 8.0,
            })
        );
        let reviewer = dirty[0]
            .models
            .iter()
            .find(|entry| entry.model.normalized_name.as_deref() == Some("codex-auto-review"))
            .expect("review model metrics");
        assert_eq!(
            reviewer
                .metrics
                .as_ref()
                .and_then(|metrics| metrics.generated_tps.as_ref()),
            Some(&SummaryMetricTotals {
                samples: 1,
                sum: 20.0 / 3.0,
            })
        );
    }

    #[test]
    fn dirty_sync_rollups_rebuild_stale_summary_versions() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-stale-rollups"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 4, 10, 0, 0)
            .single()
            .expect("now");

        let mut event = test_store_event(&source, now, "stale-a");
        event.usage = UsageCounts {
            output_tokens: Some(12),
            reasoning_tokens: Some(3),
            total_tokens: Some(15),
            requests: Some(1),
            ..UsageCounts::default()
        };
        event.runtime = Some(statsai_core::RuntimeInfo {
            runtime_name: None,
            host_id: None,
            latency_ms: Some(2000),
            latency_source: Some(LatencySource::Explicit),
            time_to_first_token_ms: Some(500),
            prompt_eval_duration_ms: None,
            eval_duration_ms: None,
            total_messages: Some(2),
            user_messages: Some(1),
            assistant_messages: Some(1),
            developer_messages: Some(0),
        });
        assert!(store.insert_event(&event).expect("insert"));

        let initial = store.dirty_sync_rollup_summaries().expect("dirty initial");
        assert_eq!(
            initial[0].metadata.summary_version.as_deref(),
            Some(SYNC_ROLLUP_SUMMARY_VERSION)
        );
        store
            .mark_sync_rollups_synced(&[initial[0].summary_id.clone()])
            .expect("mark synced");
        store
            .conn
            .execute(
                "UPDATE sync_rollups SET payload = json_set(payload, '$.metadata.summary_version', '3'), dirty = 0",
                [],
            )
            .expect("downgrade payload version");

        let rebuilt = store
            .dirty_sync_rollup_summaries()
            .expect("dirty after rebuild");
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(
            rebuilt[0].metadata.summary_version.as_deref(),
            Some(SYNC_ROLLUP_SUMMARY_VERSION)
        );
        let metrics = rebuilt[0].metrics.as_ref().expect("metrics");
        assert_eq!(metrics.tracked_requests, Some(1));
        assert_eq!(metrics.tracked_output_tokens, Some(12));
        assert_eq!(metrics.tracked_reasoning_tokens, Some(3));
        assert_eq!(metrics.overall_generated_tps, Some(7.5));
        assert_eq!(metrics.overall_visible_tps, Some(6.0));
    }

    #[test]
    fn sync_rollups_exclude_inferred_latency_from_per_turn_sample_metrics() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-sync-inferred-latency"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let day = Utc
            .with_ymd_and_hms(2026, 6, 1, 9, 0, 0)
            .single()
            .expect("day");

        let mut explicit = test_store_event(&source, day, "explicit-runtime");
        explicit.session.session_id = "session-explicit".to_string();
        explicit.session.local_session_id_hash = Some("session-explicit".to_string());
        explicit.usage = UsageCounts {
            output_tokens: Some(30),
            reasoning_tokens: Some(10),
            total_tokens: Some(40),
            requests: Some(1),
            ..UsageCounts::default()
        };
        explicit.runtime = Some(statsai_core::RuntimeInfo {
            runtime_name: None,
            host_id: None,
            latency_ms: Some(5000),
            latency_source: Some(LatencySource::Explicit),
            time_to_first_token_ms: Some(1200),
            prompt_eval_duration_ms: None,
            eval_duration_ms: None,
            total_messages: Some(2),
            user_messages: Some(1),
            assistant_messages: Some(1),
            developer_messages: Some(0),
        });

        let mut inferred = test_store_event(
            &source,
            day + chrono::Duration::minutes(5),
            "inferred-runtime",
        );
        inferred.session.session_id = "session-inferred".to_string();
        inferred.session.local_session_id_hash = Some("session-inferred".to_string());
        inferred.usage = UsageCounts {
            output_tokens: Some(700),
            reasoning_tokens: Some(300),
            total_tokens: Some(1000),
            requests: Some(1),
            ..UsageCounts::default()
        };
        inferred.runtime = Some(statsai_core::RuntimeInfo {
            runtime_name: None,
            host_id: None,
            latency_ms: Some(100),
            latency_source: Some(LatencySource::Inferred),
            time_to_first_token_ms: None,
            prompt_eval_duration_ms: None,
            eval_duration_ms: None,
            total_messages: Some(2),
            user_messages: Some(1),
            assistant_messages: Some(1),
            developer_messages: Some(0),
        });

        assert!(store.insert_event(&explicit).expect("insert explicit"));
        assert!(store.insert_event(&inferred).expect("insert inferred"));

        let dirty = store
            .dirty_sync_rollup_summaries()
            .expect("dirty rollups after inferred metrics");
        assert_eq!(dirty.len(), 1);
        let metrics = dirty[0].metrics.as_ref().expect("metrics");
        assert_eq!(metrics.active_seconds, Some(5.1));
        assert_eq!(metrics.tracked_requests, Some(2));
        assert_eq!(metrics.tracked_output_tokens, Some(730));
        assert_eq!(metrics.tracked_reasoning_tokens, Some(310));
        assert_eq!(
            metrics.latency_ms.as_ref().map(|value| value.samples),
            Some(1)
        );
        assert_eq!(
            metrics.generated_tps.as_ref().map(|value| value.samples),
            Some(1)
        );
        assert_eq!(
            metrics.generated_tps.as_ref().and_then(|value| value.avg),
            Some(8.0)
        );
        assert_eq!(
            metrics.visible_tps.as_ref().and_then(|value| value.avg),
            Some(6.0)
        );
        assert_eq!(metrics.overall_generated_tps, Some(1040.0 / 5.1));
        assert_eq!(metrics.overall_visible_tps, Some(730.0 / 5.1));
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
    fn source_lifecycle_updates_enabled_and_removes_scan_cache() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-source-lifecycle"),
            LocationOrigin::Configured,
        );
        let source_id = source.source_id.clone();
        store.upsert_source(&source).expect("source");
        store
            .record_scan_file_entries(
                &source_id,
                &[ScanFileStateEntry {
                    cache_key: "/tmp/a.jsonl".to_string(),
                    cache_signature: "sig-a-1".to_string(),
                }],
            )
            .expect("record scan cache");

        let disabled = store
            .set_source_enabled(&source_id, false)
            .expect("disable")
            .expect("existing source");
        assert!(!disabled.enabled);
        assert!(store
            .pending_scan_file_entries(
                &source_id,
                &[ScanFileStateEntry {
                    cache_key: "/tmp/a.jsonl".to_string(),
                    cache_signature: "sig-a-1".to_string(),
                }],
            )
            .expect("cached")
            .is_empty());

        let deleted_scan_cache = store
            .delete_scan_file_entries_for_sources(std::slice::from_ref(&source_id))
            .expect("delete scan cache");
        assert_eq!(deleted_scan_cache, 1);
        assert!(
            store
                .pending_scan_file_entries(
                    &source_id,
                    &[ScanFileStateEntry {
                        cache_key: "/tmp/a.jsonl".to_string(),
                        cache_signature: "sig-a-1".to_string(),
                    }],
                )
                .expect("pending after delete")
                .len()
                == 1
        );

        assert!(store.delete_source(&source_id).expect("delete source"));
        assert!(store.source(&source_id).expect("reload").is_none());
    }

    #[test]
    fn usage_event_period_stats_since_counts_recent_events() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-period-stats"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let recent = test_store_event(&source, now - chrono::Duration::minutes(5), "recent");
        let old = test_store_event(&source, now - chrono::Duration::days(2), "old");
        store.insert_events(&[recent, old]).expect("insert events");

        let stats = store
            .usage_event_period_stats_since(now - chrono::Duration::hours(1))
            .expect("period stats");

        assert_eq!(stats.requests, 1);
        assert_eq!(stats.tokens, 15);
    }

    #[test]
    fn usage_totals_by_source_groups_tokens_and_cost() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-source-totals"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let mut first = test_store_event(&source, now - chrono::Duration::minutes(5), "first");
        first.cost.estimated_api_equivalent_usd = Some(10);
        let mut second = test_store_event(&source, now, "second");
        second.usage.total_tokens = Some(25);
        second.cost.estimated_api_equivalent_usd = Some(15);
        store
            .insert_events(&[first, second])
            .expect("insert events");
        let mut summary = test_store_summary(&source, now, 100);
        summary.cost.estimated_api_equivalent_usd = Some(40);
        summary.cost.provider_reported_usd = Some(45);
        store.upsert_summary(&summary).expect("summary");

        let totals = store.usage_totals_by_source().expect("source totals");
        let source_totals = totals.get(&source.source_id.0).expect("source entry");

        assert_eq!(
            *source_totals,
            SourceUsageTotals {
                events: 1,
                tokens: 100,
                estimated_cost_cents: Some(45),
            }
        );
    }

    #[test]
    fn menu_usage_totals_by_provider_uses_fast_rollups_and_reportable_summaries() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-menu-provider-totals"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let mut event = test_store_event(&source, now, "event");
        event.usage.total_tokens = Some(25);
        event.cost.estimated_api_equivalent_usd = Some(15);
        store.insert_event(&event).expect("insert event");

        let mut reportable = test_store_summary(&source, now, 100);
        reportable.summary_id = summary_id(&source.provider, &source.source_id, "reportable");
        reportable.source.source_kind = SourceKind::LocalAdapter;
        reportable.metadata.summary_format = "ccusage_daily".to_string();
        reportable.usage.requests = Some(3);
        reportable.cost.provider_reported_usd = Some(45);
        store
            .upsert_summary(&reportable)
            .expect("reportable summary");

        let mut requestless = test_store_summary(&source, now, 50);
        requestless.summary_id = summary_id(&source.provider, &source.source_id, "requestless");
        requestless.source.source_kind = SourceKind::LocalAdapter;
        requestless.metadata.summary_format = "ccusage_daily".to_string();
        requestless.cost.provider_reported_usd = Some(5);
        store
            .upsert_summary(&requestless)
            .expect("requestless summary");

        let mut local_summary = test_store_summary(&source, now, 1_000);
        local_summary.summary_id = summary_id(&source.provider, &source.source_id, "local");
        local_summary.metadata.summary_format = "claude_stats_cache".to_string();
        local_summary.cost.provider_reported_usd = Some(9_999);
        store.upsert_summary(&local_summary).expect("local summary");

        let totals = store
            .menu_usage_totals_by_provider()
            .expect("provider totals");
        let provider_totals = totals.get("codex").expect("codex totals");

        assert_eq!(
            *provider_totals,
            SourceUsageTotals {
                events: 5,
                tokens: 175,
                estimated_cost_cents: Some(65),
            }
        );
    }

    #[test]
    fn reportable_summary_period_stats_include_summary_only_usage() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "grok_build",
            "test",
            "0",
            Path::new("/tmp/grok-summary-period-stats"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();

        let mut recent = test_store_summary(&source, now, 70);
        recent.summary_id = summary_id(&source.provider, &source.source_id, "recent");
        recent.source.source_kind = SourceKind::LocalAdapter;
        recent.metadata.summary_format = "grok_build_session_summary".to_string();
        recent.period_start = Some(now);
        recent.period_end = Some(now);
        store.upsert_summary(&recent).expect("recent summary");

        let mut explicit_requests = test_store_summary(&source, now, 30);
        explicit_requests.summary_id =
            summary_id(&source.provider, &source.source_id, "explicit-requests");
        explicit_requests.source.source_kind = SourceKind::LocalAdapter;
        explicit_requests.metadata.summary_format = "grok_build_session_summary".to_string();
        explicit_requests.period_start = Some(now);
        explicit_requests.period_end = Some(now);
        explicit_requests.usage.requests = Some(4);
        store
            .upsert_summary(&explicit_requests)
            .expect("explicit request summary");

        let mut old = test_store_summary(&source, now - chrono::Duration::days(10), 1_000);
        old.summary_id = summary_id(&source.provider, &source.source_id, "old");
        old.source.source_kind = SourceKind::LocalAdapter;
        old.metadata.summary_format = "grok_build_session_summary".to_string();
        old.period_start = Some(now - chrono::Duration::days(10));
        old.period_end = Some(now - chrono::Duration::days(10));
        store.upsert_summary(&old).expect("old summary");

        let mut rollup = test_store_summary(&source, now, 2_000);
        rollup.summary_id = summary_id(&source.provider, &source.source_id, "rollup");
        rollup.source.source_kind = SourceKind::LocalAdapter;
        rollup.metadata.summary_format = "daily_rollup.v1".to_string();
        rollup.period_start = Some(now);
        rollup.period_end = Some(now);
        store.upsert_summary(&rollup).expect("rollup summary");

        let stats = store
            .reportable_summary_period_stats_since(now - chrono::Duration::hours(1))
            .expect("summary stats");
        assert_eq!(
            stats,
            RollupPeriodStats {
                tokens: 100,
                requests: 5,
            }
        );

        let day_stats = store
            .reportable_summary_period_stats_since_day(now.date_naive())
            .expect("summary day stats");
        assert_eq!(day_stats, stats);
    }

    #[test]
    fn pending_http_sync_summary_counts_include_summary_only_usage() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "grok_build",
            "test",
            "0",
            Path::new("/tmp/grok-pending-sync-summary"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let target = "https://api.example.com/api/sync/batches";

        let mut summary = test_store_summary(&source, now, 70);
        summary.summary_id = summary_id(&source.provider, &source.source_id, "pending-summary");
        summary.source.source_kind = SourceKind::LocalAdapter;
        summary.metadata.summary_format = "grok_build_session_summary".to_string();
        summary.period_start = Some(now);
        summary.period_end = Some(now);
        store.upsert_summary(&summary).expect("summary");

        let mut backfill = test_store_summary(&source, now, 500);
        backfill.summary_id = summary_id(&source.provider, &source.source_id, "manual-backfill");
        backfill.source.source_kind = SourceKind::Manual;
        backfill.metadata.summary_format = "manual_period_summary".to_string();
        backfill.period_start = Some(now - chrono::Duration::days(4));
        backfill.period_end = Some(now);
        store.upsert_summary(&backfill).expect("backfill summary");

        let counts = store
            .pending_http_sync_summary_counts(target, "device")
            .expect("pending counts");
        assert_eq!(
            counts,
            PendingSyncSummaryCounts {
                rollups: 0,
                passthrough_summaries: 2,
                retired_entities: 0,
                quota_cycle_contributions: 0,
                total: 2,
                days: 5,
            }
        );

        store
            .record_summaries_synced("http", target, &[summary, backfill])
            .expect("record synced");

        let counts = store
            .pending_http_sync_summary_counts(target, "device")
            .expect("pending counts after sync");
        assert_eq!(counts.total, 0);
    }

    #[test]
    fn pending_http_sync_summary_counts_include_edited_passthrough_summaries() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "grok_build",
            "test",
            "0",
            Path::new("/tmp/grok-edited-pending-sync-summary"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let target = "https://api.example.com/api/sync/batches";

        let mut summary = test_store_summary(&source, now, 70);
        summary.summary_id = summary_id(&source.provider, &source.source_id, "editable-summary");
        summary.source.source_kind = SourceKind::LocalAdapter;
        summary.metadata.summary_format = "grok_build_session_summary".to_string();
        summary.period_start = Some(now);
        summary.period_end = Some(now);
        store.upsert_summary(&summary).expect("summary");

        store
            .record_summaries_synced("http", target, &[summary.clone()])
            .expect("record synced");
        assert_eq!(
            store
                .pending_http_sync_summary_counts(target, "device")
                .expect("counts after sync")
                .total,
            0
        );

        let mut edited = summary.clone();
        edited.usage.total_tokens = Some(80);
        store.upsert_summary(&edited).expect("edited summary");

        let counts = store
            .pending_http_sync_summary_counts(target, "device")
            .expect("pending counts after edit");
        assert_eq!(
            counts,
            PendingSyncSummaryCounts {
                rollups: 0,
                passthrough_summaries: 1,
                retired_entities: 0,
                quota_cycle_contributions: 0,
                total: 1,
                days: 1,
            }
        );
    }

    #[test]
    fn pending_http_sync_summary_counts_include_retirement_only_reconciliation() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-retirement-only-pending-sync"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let target = "https://api.example.com/api/sync/batches";
        let event = test_store_event(&source, Utc::now(), "retired-event");
        store.insert_event(&event).expect("event");
        let rollups = store.all_sync_rollup_summaries().expect("initial rollups");
        assert_eq!(rollups.len(), 1);

        store
            .record_sources_synced("http", target, std::slice::from_ref(&source))
            .expect("record source synced");
        store
            .record_summaries_synced("http", target, &rollups)
            .expect("record rollup synced");
        assert_eq!(
            store
                .pending_http_sync_summary_counts(target, "device")
                .expect("settled counts")
                .total,
            0
        );

        store
            .delete_events_for_sources(std::slice::from_ref(&source.source_id))
            .expect("retire source events");
        let counts = store
            .pending_http_sync_summary_counts(target, "device")
            .expect("retirement counts");

        assert_eq!(counts.rollups, 0);
        assert_eq!(counts.passthrough_summaries, 0);
        assert_eq!(counts.retired_entities, 1);
        assert_eq!(
            counts.total, 1,
            "retirement-only reconciliation must surface as pending upload work"
        );
    }

    #[test]
    fn pending_http_sync_summary_counts_match_default_http_passthrough_payloads() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "grok_build",
            "test",
            "0",
            Path::new("/tmp/grok-project-pending-sync-summary"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let target = "https://api.example.com/api/sync/batches";

        let mut summary = test_store_summary(&source, now, 70);
        summary.summary_id = summary_id(&source.provider, &source.source_id, "project-summary");
        summary.source.source_kind = SourceKind::LocalAdapter;
        summary.metadata.summary_format = "grok_build_session_summary".to_string();
        summary.period_start = Some(now);
        summary.period_end = Some(now);
        summary.project = Some(ProjectInfo {
            project_id: "project-repo-backed".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/work/ai-stats".to_string()),
        });
        summary.privacy.contains_file_paths = true;
        store.upsert_summary(&summary).expect("summary");

        store
            .record_summaries_synced(
                "http",
                target,
                &[sanitize_summary_for_default_http_sync(summary.clone())],
            )
            .expect("record synced");

        let counts = store
            .pending_http_sync_summary_counts(target, "device")
            .expect("pending counts after sync");
        assert_eq!(counts.total, 0);
    }

    #[test]
    fn pending_http_sync_summary_counts_with_projects_detect_opt_in_backfill() {
        let store = Store::in_memory().expect("store");
        let source = statsai_core::SourceLocation::local_adapter(
            "grok_build",
            "test",
            "0",
            Path::new("/tmp/grok-project-opt-in-pending-sync-summary"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc::now();
        let target = "https://api.example.com/api/sync/batches";

        let mut summary = test_store_summary(&source, now, 70);
        summary.summary_id = summary_id(&source.provider, &source.source_id, "project-summary");
        summary.source.source_kind = SourceKind::LocalAdapter;
        summary.metadata.summary_format = "grok_build_session_summary".to_string();
        summary.period_start = Some(now);
        summary.period_end = Some(now);
        summary.project = Some(ProjectInfo {
            project_id: "project-repo-backed".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/work/ai-stats".to_string()),
        });
        summary.privacy.contains_file_paths = true;
        store.upsert_summary(&summary).expect("summary");

        store
            .record_summaries_synced(
                "http",
                target,
                &[sanitize_summary_for_default_http_sync(summary.clone())],
            )
            .expect("record synced");

        assert_eq!(
            store
                .pending_http_sync_summary_counts(target, "device")
                .expect("default payload counts")
                .total,
            0
        );
        assert_eq!(
            store
                .pending_http_sync_summary_counts_with_projects(target, "device", true)
                .expect("project payload counts")
                .total,
            1
        );
    }

    #[test]
    fn pending_http_sync_summary_counts_include_code_change_only_uploads() {
        let store = Store::in_memory().expect("store");
        let target = "https://api.example.com/api/sync/batches";
        let metric = CodeChangeMetric {
            schema_version: statsai_core::CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "pending-code-change".to_string(),
            device_id: "device".to_string(),
            day: Utc::now().date_naive(),
            project_id: Some("project".to_string()),
            repository_hash: Some("repository".to_string()),
            commit_hash: None,
            kind: statsai_core::CodeChangeMetricKind::AgentEdit,
            counts: statsai_core::CodeLineCounts::classified(
                statsai_core::CodeCategory::Source,
                3,
                1,
            ),
            attribution_confidence: None,
            trace_coverage: statsai_core::CoverageStatus::Complete,
            git_coverage: statsai_core::CoverageStatus::Complete,
        };
        store
            .ingest_code_change_metrics_inner(std::slice::from_ref(&metric))
            .expect("store metric");
        let mut peer_metric = metric.clone();
        peer_metric.metric_id = "peer-code-change".to_string();
        peer_metric.device_id = "peer-device".to_string();
        store
            .ingest_code_change_metrics_inner(std::slice::from_ref(&peer_metric))
            .expect("store peer metric");

        let counts = store
            .pending_http_sync_summary_counts_with_projects(target, "device", false)
            .expect("pending counts");

        assert_eq!(counts.total, 1);
        assert_eq!(counts.days, 1);

        let sanitized = sanitize_code_change_metric_for_sync(metric.clone(), false);
        store
            .record_code_change_metrics_synced("http", target, &[sanitized])
            .expect("record sanitized metric synced");
        assert_eq!(
            store
                .pending_http_sync_summary_counts_with_projects(target, "device", false)
                .expect("settled default counts")
                .total,
            0
        );
        assert_eq!(
            store
                .pending_http_sync_summary_counts_with_projects(target, "device", true)
                .expect("project backfill counts")
                .total,
            1
        );
    }

    #[test]
    fn pending_http_sync_summary_counts_include_account_plan_and_evidence_only_uploads() {
        let store = Store::in_memory().expect("store");
        let target = "https://api.example.com/api/sync/batches";
        let observed_at = Utc
            .with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
            .single()
            .expect("observed at");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-pending-account-evidence"),
            LocationOrigin::Configured,
        );
        let account_id = ProviderAccountId("pending-account".to_string());
        store.upsert_source(&source).expect("source");
        store
            .upsert_account_identity_observations(&[statsai_core::AccountIdentityObservationV1 {
                schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION
                    .to_string(),
                observation_id: "pending-identity".to_string(),
                provider: "codex".to_string(),
                source_id: source.source_id.clone(),
                provider_account_id: Some(account_id.clone()),
                provider_user_id_hash: Some("provider-id-hash".to_string()),
                email_hash: None,
                conversation_id_hash: None,
                turn_id_hash: None,
                observed_at,
                evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
                confidence: Confidence::High,
                auth_mode: Some("chatgpt".to_string()),
                application_version: None,
                parser_version: "test.v1".to_string(),
                artifact_kind: "auth_json".to_string(),
                artifact_path_hash: "path-hash".to_string(),
                record_fingerprint: "identity-fingerprint".to_string(),
            }])
            .expect("identity evidence");
        store
            .upsert_account_plan_observations(&[statsai_core::AccountPlanObservationV1 {
                schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
                observation_id: "pending-plan".to_string(),
                provider: "codex".to_string(),
                source_id: source.source_id,
                provider_account_id: Some(account_id),
                raw_plan_name: "plus".to_string(),
                plan_name: "Plus".to_string(),
                observed_at,
                active_from: None,
                active_until: None,
                is_current_snapshot: true,
                evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
                confidence: Confidence::High,
                parser_version: "test.v1".to_string(),
                artifact_path_hash: "path-hash".to_string(),
                record_fingerprint: "plan-fingerprint".to_string(),
            }])
            .expect("plan evidence");

        let plans = store
            .account_plan_projections("device")
            .expect("plan projections");
        let evidence = store
            .account_evidence_summaries("device")
            .expect("evidence summaries");
        assert_eq!(plans.len(), 1);
        assert_eq!(evidence.len(), 1);
        assert_eq!(
            store
                .pending_http_sync_summary_counts(target, "device")
                .expect("pending counts")
                .total,
            2
        );

        store
            .record_account_plan_projections_synced("http", target, &plans)
            .expect("record plan synced");
        store
            .record_account_evidence_summaries_synced("http", target, &evidence)
            .expect("record evidence synced");
        assert_eq!(
            store
                .pending_http_sync_summary_counts(target, "device")
                .expect("settled counts")
                .total,
            0
        );
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
    fn events_in_period_without_since_includes_pre_unix_history() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-pre-unix"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let pre_unix = Utc
            .with_ymd_and_hms(1969, 12, 31, 12, 0, 0)
            .single()
            .expect("pre-unix");
        let after = Utc
            .with_ymd_and_hms(1970, 1, 2, 12, 0, 0)
            .single()
            .expect("after");
        store
            .insert_events(&[
                test_store_event(&source, pre_unix, "pre-unix"),
                test_store_event(&source, after, "after"),
            ])
            .expect("insert events");
        let until = Utc
            .with_ymd_and_hms(1969, 12, 31, 23, 59, 59)
            .single()
            .expect("end of 1969");

        let unbounded = store
            .events_in_period(None, until)
            .expect("unbounded through 1969");
        assert_eq!(unbounded.len(), 1);
        assert_eq!(unbounded[0].session.started_at, pre_unix);

        let epoch_floor = store
            .events_in_period(Some(DateTime::<Utc>::UNIX_EPOCH), until)
            .expect("epoch floor");
        assert!(epoch_floor.is_empty());
    }

    #[test]
    fn conflicting_bindings_keep_a_manual_account_assignment() {
        let store = Store::in_memory().expect("store");
        let now = Utc::now();
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            std::path::Path::new("/tmp/binding-conflict"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let manual = ProviderAccountId("account-manual".to_string());

        // The same conversation is bound to two different accounts.
        for (index, account) in ["account-one", "account-two"].iter().enumerate() {
            store
                .upsert_conversation_account_bindings(&[
                    statsai_core::ConversationAccountBindingV1 {
                        schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION
                            .to_string(),
                        binding_id: format!("binding-{index}"),
                        provider: "codex".to_string(),
                        source_id: source.source_id.clone(),
                        provider_account_id: ProviderAccountId((*account).to_string()),
                        conversation_id_hash: "same-session".to_string(),
                        turn_id_hash: None,
                        observed_at: now,
                        evidence_kind: statsai_core::AccountEvidenceKind::TelemetryIdentity,
                        confidence: Confidence::High,
                    },
                ])
                .expect("binding");
        }

        let mut manual_event = test_store_event(&source, now, "manual-record");
        manual_event.provider_account_id = Some(manual.clone());
        manual_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "test".to_string(),
            source_file_path_hash: None,
            source_line_number: None,
            source_record_id: None,
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::UserConfigured,
        });
        let mut derived_event = test_store_event(&source, now, "derived-record");
        derived_event.provider_account_id = Some(ProviderAccountId("account-one".to_string()));
        derived_event.parse_evidence = Some(ParseEvidence {
            account_identity_source: IdentitySource::LocalAuth,
            ..manual_event
                .parse_evidence
                .clone()
                .expect("evidence template")
        });

        let mut events = vec![manual_event, derived_event];
        store
            .apply_conversation_account_bindings(&source.source_id, &mut events)
            .expect("apply bindings");

        assert_eq!(
            events[0].provider_account_id.as_ref(),
            Some(&manual),
            "a conflict between derived bindings must not discard a manual assignment"
        );
        assert_eq!(
            events[0]
                .parse_evidence
                .as_ref()
                .map(|evidence| &evidence.account_identity_source),
            Some(&IdentitySource::UserConfigured),
            "the recorded identity source must still describe the account on the event"
        );
        assert_eq!(
            events[1].provider_account_id, None,
            "a derived attribution is still cleared by a genuine conflict"
        );
    }

    #[test]
    fn reset_history_alone_does_not_truncate_a_source_assignment() {
        let store = Store::in_memory().expect("store");
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("started at");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            std::path::Path::new("/tmp/reset-history-truncation"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let bound = ProviderAccountId("account-bound".to_string());
        let assignment = statsai_core::SourceAccountAssignment {
            schema_version: statsai_core::SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: statsai_core::SourceAccountAssignmentId("assignment-1".to_string()),
            source_id: source.source_id.clone(),
            provider: "codex".to_string(),
            provider_account_id: bound.clone(),
            started_at,
            ended_at: None,
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(started_at),
            created_at: started_at,
            updated_at: started_at,
        };
        store
            .upsert_source_account_assignment(&assignment)
            .expect("assignment");

        // An auth snapshot corroborates the assignment, then a single
        // conversation-scoped reset-history entry names a different account.
        for (index, (kind, account, offset_days)) in [
            (
                statsai_core::AccountEvidenceKind::AuthSnapshot,
                "account-bound",
                1_i64,
            ),
            (
                statsai_core::AccountEvidenceKind::ResetHistory,
                "account-other",
                2,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            store
                .upsert_account_identity_observations(&[
                    statsai_core::AccountIdentityObservationV1 {
                        schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION
                            .to_string(),
                        observation_id: format!("identity-{index}"),
                        provider: "codex".to_string(),
                        source_id: source.source_id.clone(),
                        provider_account_id: Some(ProviderAccountId(account.to_string())),
                        provider_user_id_hash: None,
                        email_hash: None,
                        conversation_id_hash: Some("f".repeat(64)),
                        turn_id_hash: Some("a".repeat(64)),
                        observed_at: started_at + chrono::Duration::days(offset_days),
                        evidence_kind: kind,
                        confidence: Confidence::High,
                        auth_mode: None,
                        application_version: None,
                        parser_version: "test.v1".to_string(),
                        artifact_kind: "test".to_string(),
                        artifact_path_hash: "c".repeat(64),
                        record_fingerprint: format!("{index}").repeat(64),
                    },
                ])
                .expect("identity observation");
        }

        store
            .reconcile_source_account_evidence_assignments(&source.source_id)
            .expect("reconcile");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].provider_account_id, bound);
        assert_eq!(
            assignments[0].ended_at, None,
            "a per-conversation reset-history entry cannot end a source-wide assignment, \
             because nothing downstream is able to reopen one"
        );
    }

    #[test]
    fn reset_history_does_not_bound_a_reopened_auth_reload_interval() {
        let store = Store::in_memory().expect("store");
        let base = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("base");
        let source = statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            std::path::Path::new("/tmp/reload-interval-bounds"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let reloaded = ProviderAccountId("account-reloaded".to_string());

        // reload A, telemetry A, then a turn-scoped reset-history entry naming B.
        for (index, (kind, account, offset_days)) in [
            (
                statsai_core::AccountEvidenceKind::AuthReload,
                "account-reloaded",
                1_i64,
            ),
            (
                statsai_core::AccountEvidenceKind::TelemetryIdentity,
                "account-reloaded",
                2,
            ),
            (
                statsai_core::AccountEvidenceKind::ResetHistory,
                "account-other",
                3,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            store
                .upsert_account_identity_observations(&[
                    statsai_core::AccountIdentityObservationV1 {
                        schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION
                            .to_string(),
                        observation_id: format!("reload-identity-{index}"),
                        provider: "codex".to_string(),
                        source_id: source.source_id.clone(),
                        provider_account_id: Some(ProviderAccountId(account.to_string())),
                        provider_user_id_hash: None,
                        email_hash: None,
                        conversation_id_hash: Some("f".repeat(64)),
                        turn_id_hash: Some("a".repeat(64)),
                        observed_at: base + chrono::Duration::days(offset_days),
                        evidence_kind: kind,
                        confidence: Confidence::High,
                        auth_mode: None,
                        application_version: None,
                        parser_version: "test.v1".to_string(),
                        artifact_kind: "test".to_string(),
                        artifact_path_hash: "c".repeat(64),
                        record_fingerprint: format!("{index}").repeat(64),
                    },
                ])
                .expect("identity observation");
        }

        store
            .reconcile_source_account_evidence_assignments(&source.source_id)
            .expect("reconcile");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].provider_account_id, reloaded);
        assert_eq!(
            assignments[0].ended_at, None,
            "reset history must not close the interval an auth reload opened"
        );
    }

    #[test]
    fn refreshing_verified_at_does_not_rewrite_unchanged_source_records() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/verified-at-only-refresh"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let account_id = ProviderAccountId("account-verified".to_string());
        let authenticated_at = Utc::now() - chrono::Duration::days(1);
        upsert_verified_source_assignment(
            &store,
            &source,
            &account_id,
            authenticated_at,
            Some(authenticated_at),
            None,
            IdentitySource::LocalAuth,
        )
        .expect("initial verification");
        let mut event = test_store_event(&source, authenticated_at, "verified-event");
        event.provider_account_id = Some(account_id.clone());
        store.insert_events(&[event]).expect("event");
        store
            .conn
            .execute_batch(
                r#"
                CREATE TABLE event_update_audit (count INTEGER NOT NULL);
                INSERT INTO event_update_audit VALUES (0);
                CREATE TRIGGER count_event_updates
                AFTER UPDATE ON usage_events
                BEGIN
                  UPDATE event_update_audit SET count = count + 1;
                END;
                "#,
            )
            .expect("audit trigger");

        let refreshed_at = authenticated_at + chrono::Duration::hours(1);
        upsert_verified_source_assignment(
            &store,
            &source,
            &account_id,
            authenticated_at,
            Some(refreshed_at),
            None,
            IdentitySource::LocalAuth,
        )
        .expect("refresh verification");

        let update_count: i64 = store
            .conn
            .query_row("SELECT count FROM event_update_audit", [], |row| row.get(0))
            .expect("audit count");
        assert_eq!(update_count, 0);
        let assignment = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignment")
            .pop()
            .expect("verified assignment");
        assert_eq!(assignment.verified_at, Some(refreshed_at));
    }

    fn test_store_event(
        source: &statsai_core::SourceLocation,
        now: chrono::DateTime<Utc>,
        record_id: &str,
    ) -> UsageEvent {
        UsageEvent {
            schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
            event_id: event_id("codex", &source.source_id, record_id, None, now),
            device_id: "device".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: None,
            subscription_id: None,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalAdapter,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "jsonl".to_string(),
                source_path_hash: source.path_hash.clone(),
                source_record_id: Some(record_id.to_string()),
                parse_confidence: Confidence::High,
            },
            session: SessionInfo {
                session_id: "session".to_string(),
                local_session_id_hash: Some("same-session".to_string()),
                title: None,
                started_at: now,
                ended_at: None,
                duration_seconds: None,
            },
            model: None,
            usage: UsageCounts {
                input_tokens: Some(12),
                output_tokens: Some(3),
                total_tokens: Some(15),
                ..UsageCounts::default()
            },
            runtime: None,
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: None,
                provider_reported_usd: None,
                estimated_api_equivalent_micro_usd: None,
                provider_reported_micro_usd: None,
                pricing_source: Some("unknown".to_string()),
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
            created_at: now,
            imported_at: now,
        }
    }

    fn test_store_summary(
        source: &statsai_core::SourceLocation,
        now: chrono::DateTime<Utc>,
        total: u64,
    ) -> UsageSummary {
        UsageSummary {
            schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: summary_id(&source.provider, &source.source_id, "summary"),
            device_id: "device".to_string(),
            provider: source.provider.clone(),
            source_id: source.source_id.clone(),
            provider_account_id: None,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalSummary,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "stats-cache.json".to_string(),
                source_path_hash: source.path_hash.clone(),
                source_record_id: Some("summary".to_string()),
                parse_confidence: Confidence::Medium,
            },
            model: Some(ModelInfo {
                name: Some("claude-test".to_string()),
                normalized_name: Some("claude-test".to_string()),
                provider_model_id: Some("claude-test".to_string()),
                speed: None,
                reasoning_level: None,
                reasoning_level_raw: None,
            }),
            models: Vec::new(),
            usage: UsageCounts {
                input_tokens: Some(total),
                total_tokens: Some(total),
                ..UsageCounts::default()
            },
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: None,
                provider_reported_usd: None,
                estimated_api_equivalent_micro_usd: None,
                provider_reported_micro_usd: None,
                pricing_source: Some("unknown".to_string()),
                pricing_version: None,
                confidence: Confidence::Low,
            },
            parse_evidence: None,
            project: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            metrics: None,
            period_start: Some(now - chrono::Duration::days(1)),
            period_end: Some(now),
            observed_at: now,
            metadata: SummaryMetadata {
                summary_format: "test".to_string(),
                summary_version: Some("1".to_string()),
                total_sessions: Some(1),
                total_messages: Some(2),
                last_computed_at: Some(now),
            },
            imported_at: now,
        }
    }
}
