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

pub(crate) use accounts::deserialize_subscription_payload;
pub(crate) use rollups::{
    collect_pending_summary_days, event_with_valid_project, is_daily_rollup_summary,
    is_http_rollup_passthrough_summary, sanitize_summary_for_http_sync, summary_period_bounds,
    summary_sync_payload_hash, sync_rollup_bucket_key, sync_rollup_project_key,
    SyncRollupBucketKey,
};
pub(crate) use sql::{
    begin_immediate_transaction_with_retry, commit_transaction, restrict_dir_permissions,
    restrict_file_permissions, rollback, safe_u64_to_i64, sqlite_in_clause_placeholders,
    sqlite_string_params, sync_state_from_row,
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

/// Everything tracked for one sync target, so a caller that must clear it for an
/// operation can put it back if that operation does not establish new progress.
#[derive(Debug, Clone)]
pub struct SyncTrackingSnapshot {
    pub(crate) sink: String,
    pub(crate) target: String,
    pub(crate) state: Option<SyncState>,
    pub(crate) entities: Vec<(String, String, String, String)>,
    pub(crate) buckets: Vec<(String, String, i64, Option<String>, String)>,
}

impl SyncTrackingSnapshot {
    /// Whether there was anything to preserve.
    pub fn is_empty(&self) -> bool {
        self.state.is_none() && self.entities.is_empty() && self.buckets.is_empty()
    }

    /// The sync target this was captured for, for diagnostics.
    pub fn target(&self) -> &str {
        &self.target
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
mod tests;
