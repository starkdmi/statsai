//! Local SQLite storage for `statsai`.

mod migrations;
mod tasks;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use statsai_core::{
    hash_text, normalize_email, normalize_provider_user_id, periods_overlap, project_bucket_key,
    project_contains_file_paths, project_has_stable_identity, provider_account_id,
    provider_account_id_from_identity, sanitize_summary_for_sync, semantic_event_fingerprint,
    source_account_assignment_id, subscription_id, summary_id, timestamp_in_period, BillingPeriod,
    Confidence, CostInfo, DailyRollup, EventId, EventSource, IdentitySource, LatencySource,
    MetricStats, ModelInfo, PrivacyInfo, PrivacyMode, ProviderAccount, ProviderAccountId,
    SemanticFingerprintInput, SourceAccountAssignment, SourceAccountAssignmentId, SourceId,
    SourceKind, SourceLocation, SourceVerificationMode, Subscription, SubscriptionId,
    SubscriptionStatus, SummaryId, SummaryMetadata, SummaryMetrics, SummaryModelUsage, SyncBatch,
    TaskVerificationCursor, TaskVerificationId, UsageCounts, UsageEvent, UsageSummary,
    VerifiedSourceState, VerifiedSubscriptionState, PROVIDER_ACCOUNT_SCHEMA_VERSION,
    SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION, SUBSCRIPTION_SCHEMA_VERSION,
    USAGE_SUMMARY_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;

pub use tasks::{
    derive_task_work_items, NamedTaskBenchmark, TaskBenchmarkMetrics, TaskBenchmarkReport,
    TaskDeletionImpact, TaskRebuildReport, TaskRebuildTimings, TaskStats,
};

const SYNC_ROLLUP_SUMMARY_VERSION: &str = "10";
const SYNC_INCLUDE_PROJECTS_METADATA_KEY: &str = "sync.include_projects";
const SYNC_INCLUDE_TASKS_METADATA_KEY: &str = "sync.include_tasks";
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

fn summary_sync_payload_hash(summary: &UsageSummary) -> Result<String> {
    let payload = serde_json::to_string(&sanitize_summary_for_sync(summary.clone()))?;
    Ok(hash_text(&payload))
}

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

#[derive(Debug, Deserialize)]
struct SubscriptionCompat {
    #[serde(default = "default_subscription_schema_version")]
    schema_version: String,
    subscription_id: SubscriptionId,
    provider: String,
    provider_account_id: Option<ProviderAccountId>,
    plan_name: String,
    price: f64,
    currency: String,
    billing_period: BillingPeriod,
    paid_at: Option<DateTime<Utc>>,
    renewal_day: Option<u8>,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
    current_period_ends_at: Option<DateTime<Utc>>,
    #[serde(default = "default_subscription_status_active")]
    status: SubscriptionStatus,
    #[serde(default = "default_identity_source_unknown")]
    record_source: IdentitySource,
    verified_at: Option<DateTime<Utc>>,
    notes: Option<String>,
}

fn default_subscription_schema_version() -> String {
    SUBSCRIPTION_SCHEMA_VERSION.to_string()
}

fn default_subscription_status_active() -> SubscriptionStatus {
    SubscriptionStatus::Active
}

fn default_identity_source_unknown() -> IdentitySource {
    IdentitySource::Unknown
}

fn deserialize_subscription_payload(
    payload: &str,
    provider_account_id_column: Option<&str>,
) -> Result<Subscription> {
    if let Ok(subscription) = serde_json::from_str(payload) {
        return Ok(subscription);
    }

    let compat: SubscriptionCompat =
        serde_json::from_str(payload).context("deserialize legacy subscription payload")?;
    let provider_account_id = compat
        .provider_account_id
        .or_else(|| {
            provider_account_id_column
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| ProviderAccountId(value.to_string()))
        })
        .unwrap_or_else(|| {
            provider_account_id(
                &compat.provider,
                &format!("legacy_subscription:{}", compat.subscription_id.0),
            )
        });
    let started_at = compat
        .started_at
        .or(compat.paid_at)
        .or(compat.current_period_ends_at)
        .or(compat.ended_at)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);

    Ok(Subscription {
        schema_version: compat.schema_version,
        subscription_id: compat.subscription_id,
        provider: compat.provider,
        provider_account_id,
        plan_name: compat.plan_name,
        price: (compat.price * 100.0).round() as i64,
        currency: compat.currency,
        billing_period: compat.billing_period,
        paid_at: compat.paid_at,
        renewal_day: compat.renewal_day,
        started_at,
        ended_at: compat.ended_at,
        current_period_ends_at: compat.current_period_ends_at,
        status: compat.status,
        record_source: compat.record_source,
        verified_at: compat.verified_at,
        notes: compat.notes,
    })
}

fn restrict_dir_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn restrict_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn merge_source_totals(existing: &mut SourceUsageTotals, incoming: SourceUsageTotals) {
    if incoming.tokens > existing.tokens {
        *existing = incoming;
        return;
    }
    if incoming.tokens == existing.tokens {
        existing.events = existing.events.max(incoming.events);
        existing.estimated_cost_cents =
            max_optional_i64(existing.estimated_cost_cents, incoming.estimated_cost_cents);
    }
}

fn merge_additive_source_totals(existing: &mut SourceUsageTotals, incoming: SourceUsageTotals) {
    existing.events = existing.events.saturating_add(incoming.events);
    existing.tokens = existing.tokens.saturating_add(incoming.tokens);
    existing.estimated_cost_cents =
        match (existing.estimated_cost_cents, incoming.estimated_cost_cents) {
            (Some(existing), Some(incoming)) => Some(existing.saturating_add(incoming)),
            (Some(existing), None) => Some(existing),
            (None, Some(incoming)) => Some(incoming),
            (None, None) => None,
        };
}

fn sanitize_summary_for_default_http_sync(summary: UsageSummary) -> UsageSummary {
    sanitize_summary_for_http_sync(summary, false)
}

fn is_daily_rollup_summary(summary: &UsageSummary) -> bool {
    summary.metadata.summary_format == "daily_rollup.v1"
}

fn sanitize_summary_for_http_sync(summary: UsageSummary, include_projects: bool) -> UsageSummary {
    let mut summary = sanitize_summary_for_sync(summary);
    if !include_projects {
        summary.project = None;
    }
    summary
}

fn parse_bool_metadata_value(key: &str, value: &str) -> Result<bool> {
    match value.trim() {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        other => bail!("invalid boolean metadata value for {key}: {other}"),
    }
}

fn summary_sync_day(summary: &UsageSummary) -> NaiveDate {
    summary
        .period_start
        .map(|start| start.date_naive())
        .unwrap_or_else(|| summary.observed_at.date_naive())
}

fn summary_period_bounds(summary: &UsageSummary) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = summary
        .period_start
        .or(summary.period_end)
        .unwrap_or(summary.observed_at);
    let end = summary
        .period_end
        .or(summary.period_start)
        .unwrap_or(summary.observed_at);
    if end < start {
        (end, start)
    } else {
        (start, end)
    }
}

fn summary_spans_single_day(summary: &UsageSummary) -> bool {
    let (start, end) = summary_period_bounds(summary);
    start.date_naive() == end.date_naive()
}

fn summary_fits_single_daily_report_day(summary: &UsageSummary) -> bool {
    let (start, end) = summary_period_bounds(summary);
    if start.date_naive() == end.date_naive() {
        return true;
    }
    let duration = end - start;
    duration >= chrono::Duration::zero() && duration <= chrono::Duration::hours(25)
}

fn is_exact_daily_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "external_daily" | "manual_daily" | "custom_daily" | "ccusage_daily"
    )
}

fn is_exact_period_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "manual_period_summary" | "custom_period_summary"
    )
}

fn is_http_rollup_passthrough_summary(summary: &UsageSummary) -> bool {
    if summary.metadata.summary_format == "daily_rollup.v1" {
        return false;
    }
    if summary.metadata.summary_format == "claude_stats_cache" {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalSummary {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalAdapter {
        return true;
    }
    (is_exact_daily_passthrough_summary(summary) && summary_fits_single_daily_report_day(summary))
        || (is_exact_period_passthrough_summary(summary) && !summary_spans_single_day(summary))
}

fn collect_pending_summary_days<'a>(
    summaries: impl IntoIterator<Item = &'a UsageSummary>,
) -> BTreeSet<NaiveDate> {
    let mut days = BTreeSet::new();
    for summary in summaries {
        if summary.metadata.summary_format == "daily_rollup.v1"
            || (is_exact_daily_passthrough_summary(summary)
                && summary_fits_single_daily_report_day(summary))
        {
            days.insert(summary_sync_day(summary));
            continue;
        }

        let (start, end) = summary_period_bounds(summary);
        let mut day = start.date_naive();
        let end_day = end.date_naive();
        loop {
            days.insert(day);
            if day >= end_day {
                break;
            }
            let Some(next_day) = day.succ_opt() else {
                break;
            };
            day = next_day;
        }
    }
    days
}

fn max_optional_i64(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

pub struct Store {
    conn: Connection,
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
        store.conn.execute_batch("PRAGMA optimize=0x10002;")?;
        Ok(store)
    }

    pub fn in_memory() -> Result<Self> {
        let store = Self {
            conn: Connection::open_in_memory()?,
        };
        store.conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        store.migrate()?;
        store.conn.execute_batch("PRAGMA optimize=0x10002;")?;
        Ok(store)
    }

    fn with_immediate_transaction<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        if !self.conn.is_autocommit() {
            return operation();
        }
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = operation();
        match result {
            Ok(value) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn migrate(&self) -> Result<()> {
        migrations::migrate(&self.conn)
    }

    pub fn schema_version(&self) -> Result<i64> {
        migrations::schema_version(&self.conn)
    }

    pub fn pending_scan_file_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<Vec<ScanFileStateEntry>> {
        let compatible_signatures = HashMap::new();
        Ok(self
            .select_scan_file_state_entries_with_task_requirement_and_compatibility(
                source_id,
                entries,
                false,
                &compatible_signatures,
            )?
            .pending_entries)
    }

    pub fn pending_scan_file_entries_with_compatibility(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
        compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    ) -> Result<Vec<ScanFileStateEntry>> {
        Ok(self
            .select_scan_file_state_entries_with_task_requirement_and_compatibility(
                source_id,
                entries,
                false,
                compatible_signatures_by_key,
            )?
            .pending_entries)
    }

    pub fn pending_scan_file_entries_with_task_requirement(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
        require_tasks_collected: bool,
    ) -> Result<Vec<ScanFileStateEntry>> {
        let compatible_signatures = HashMap::new();
        Ok(self
            .select_scan_file_state_entries_with_task_requirement_and_compatibility(
                source_id,
                entries,
                require_tasks_collected,
                &compatible_signatures,
            )?
            .pending_entries)
    }

    pub fn pending_scan_file_entries_with_task_requirement_and_compatibility(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
        require_tasks_collected: bool,
        compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    ) -> Result<Vec<ScanFileStateEntry>> {
        Ok(self
            .select_scan_file_state_entries_with_task_requirement_and_compatibility(
                source_id,
                entries,
                require_tasks_collected,
                compatible_signatures_by_key,
            )?
            .pending_entries)
    }

    pub fn select_scan_file_state_entries_with_task_requirement_and_compatibility(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
        require_tasks_collected: bool,
        compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    ) -> Result<ScanFileStateSelection> {
        let mut selection = ScanFileStateSelection::default();
        selection.pending_entries.reserve(entries.len());
        let mut stmt = self.conn.prepare(
            "SELECT cache_signature, tasks_collected FROM scan_file_state WHERE source_id = ?1 AND cache_key = ?2",
        )?;
        for entry in entries {
            let existing = stmt
                .query_row(params![&source_id.0, &entry.cache_key], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
                })
                .optional()?;
            let Some((signature, tasks_collected)) = existing.as_ref() else {
                selection.pending_entries.push(entry.clone());
                continue;
            };
            let tasks_satisfied = !require_tasks_collected || *tasks_collected;
            if signature == &entry.cache_signature {
                if !tasks_satisfied {
                    selection.pending_entries.push(entry.clone());
                }
                continue;
            }
            let compatible_match = compatible_signatures_by_key
                .get(&entry.cache_key)
                .is_some_and(|compatible| {
                    compatible
                        .iter()
                        .any(|candidate_signature| candidate_signature == signature)
                });
            if compatible_match && tasks_satisfied {
                selection.compatible_entries_to_upgrade.push(entry.clone());
            } else {
                selection.pending_entries.push(entry.clone());
            }
        }
        Ok(selection)
    }

    pub fn record_scan_file_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<()> {
        self.record_scan_file_entries_with_tasks_collected(source_id, entries, false)
    }

    pub fn record_scan_file_entries_with_tasks_collected(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
        tasks_collected: bool,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let synced_at = Utc::now().to_rfc3339();
        let tasks_collected = i64::from(tasks_collected);
        self.with_immediate_transaction(|| {
            let mut stmt = self.conn.prepare(
                r#"
                INSERT INTO scan_file_state
                  (source_id, cache_key, cache_signature, synced_at, tasks_collected)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(source_id, cache_key) DO UPDATE SET
                  cache_signature = excluded.cache_signature,
                  synced_at = excluded.synced_at,
                  tasks_collected = CASE
                    WHEN scan_file_state.cache_signature = excluded.cache_signature
                    THEN MAX(scan_file_state.tasks_collected, excluded.tasks_collected)
                    ELSE excluded.tasks_collected
                  END
                "#,
            )?;
            for entry in entries {
                stmt.execute(params![
                    &source_id.0,
                    &entry.cache_key,
                    &entry.cache_signature,
                    &synced_at,
                    tasks_collected,
                ])?;
            }
            Ok(())
        })
    }

    pub fn upgrade_scan_file_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let synced_at = Utc::now().to_rfc3339();
        self.with_immediate_transaction(|| {
            let mut stmt = self.conn.prepare(
                r#"
                UPDATE scan_file_state
                   SET cache_signature = ?3,
                       synced_at = ?4
                 WHERE source_id = ?1
                   AND cache_key = ?2
                "#,
            )?;
            for entry in entries {
                stmt.execute(params![
                    &source_id.0,
                    &entry.cache_key,
                    &entry.cache_signature,
                    &synced_at,
                ])?;
            }
            Ok(())
        })
    }

    pub fn scan_file_entries(&self, source_id: &SourceId) -> Result<Vec<ScanFileStateEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT cache_key, cache_signature
             FROM scan_file_state
             WHERE source_id = ?1
             ORDER BY cache_key",
        )?;
        let rows = stmt.query_map(params![&source_id.0], |row| {
            Ok(ScanFileStateEntry {
                cache_key: row.get(0)?,
                cache_signature: row.get(1)?,
            })
        })?;
        rows.collect::<Result<_, _>>().map_err(Into::into)
    }

    pub fn delete_scan_file_entries(
        &self,
        source_id: &SourceId,
        cache_keys: &[String],
    ) -> Result<u64> {
        if cache_keys.is_empty() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            let mut stmt = self
                .conn
                .prepare("DELETE FROM scan_file_state WHERE source_id = ?1 AND cache_key = ?2")?;
            for cache_key in cache_keys {
                deleted += stmt.execute(params![&source_id.0, cache_key])? as u64;
            }
            Ok(deleted)
        })
    }

    pub fn delete_scan_file_entries_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for source_id in source_ids {
                deleted += self.conn.execute(
                    "DELETE FROM scan_file_state WHERE source_id = ?1",
                    params![&source_id.0],
                )? as u64;
            }
            Ok(deleted)
        })
    }

    pub fn replace_scan_file_records(
        &self,
        replacement: ScanFileReplacement<'_>,
    ) -> Result<ScanFileReplacementResult> {
        self.with_immediate_transaction(|| {
            self.delete_events_for_source_file_hashes(
                replacement.source_id,
                replacement.reconciled_file_hashes,
            )?;
            self.delete_summaries_for_source_file_hashes(
                replacement.source_id,
                replacement.reconciled_file_hashes,
            )?;
            let inserted_events = self.insert_events(replacement.events)?;
            let written_summaries = self.upsert_summaries(replacement.summaries)?;
            self.record_scan_file_entries(replacement.source_id, replacement.pending_entries)?;
            self.upgrade_scan_file_entries(
                replacement.source_id,
                replacement.compatible_entries_to_upgrade,
            )?;
            self.delete_scan_file_entries(replacement.source_id, replacement.removed_cache_keys)?;
            Ok(ScanFileReplacementResult {
                inserted_events,
                written_summaries,
            })
        })
    }

    pub fn source_records_missing_scan_file_hashes(&self, source_id: &SourceId) -> Result<bool> {
        let event_missing: i64 = self.conn.query_row(
            r#"
            SELECT COUNT(*)
            FROM usage_events
            WHERE source_id = ?1
              AND COALESCE(json_extract(payload, '$.parse_evidence.source_file_path_hash'), '') = ''
            "#,
            params![&source_id.0],
            |row| row.get(0),
        )?;
        if event_missing > 0 {
            return Ok(true);
        }

        let summary_missing: i64 = self.conn.query_row(
            r#"
            SELECT COUNT(*)
            FROM usage_summaries
            WHERE source_id = ?1
              AND COALESCE(json_extract(payload, '$.parse_evidence.source_file_path_hash'), '') = ''
            "#,
            params![&source_id.0],
            |row| row.get(0),
        )?;
        Ok(summary_missing > 0)
    }

    pub fn upsert_source(&self, source: &SourceLocation) -> Result<()> {
        let payload = serde_json::to_string(source)?;
        self.conn.execute(
            r#"
            INSERT INTO sources (source_id, provider, source_kind, location_origin, payload, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(source_id) DO UPDATE SET
              provider = excluded.provider,
              source_kind = excluded.source_kind,
              location_origin = excluded.location_origin,
              payload = excluded.payload,
              updated_at = excluded.updated_at
            "#,
            params![
                &source.source_id.0,
                &source.provider,
                format!("{:?}", source.source_kind),
                format!("{:?}", source.location_origin),
                &payload,
                source.updated_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn list_sources(&self) -> Result<Vec<SourceLocation>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM sources ORDER BY provider, source_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut sources = Vec::new();
        for row in rows {
            sources.push(serde_json::from_str(&row?)?);
        }
        Ok(sources)
    }

    pub fn event_counts_by_source(&self) -> Result<HashMap<String, u64>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT source_id, COUNT(*)
            FROM usage_events
            GROUP BY source_id
            "#,
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut counts = HashMap::new();
        for row in rows {
            let (source_id, count) = row?;
            counts.insert(source_id, count.max(0) as u64);
        }
        Ok(counts)
    }

    pub fn usage_totals_by_source(&self) -> Result<HashMap<String, SourceUsageTotals>> {
        let mut totals = HashMap::new();
        let mut rollup_stmt = self.conn.prepare(
            r#"
            SELECT
              source_id,
              COALESCE(SUM(CAST(json_extract(payload, '$.usage.requests') AS INTEGER)), 0),
              COALESCE(SUM(CAST(json_extract(payload, '$.usage.total_tokens') AS INTEGER)), 0),
              SUM(COALESCE(
                CAST(json_extract(payload, '$.cost.provider_reported_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_usd') AS INTEGER)
              ))
            FROM sync_rollups
            GROUP BY source_id
            "#,
        )?;
        let rollup_rows = rollup_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SourceUsageTotals {
                    events: row.get::<_, i64>(1)?.max(0) as u64,
                    tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    estimated_cost_cents: row.get::<_, Option<i64>>(3)?,
                },
            ))
        })?;
        for row in rollup_rows {
            let (source_id, source_totals) = row?;
            totals.insert(source_id, source_totals);
        }

        let mut summary_stmt = self.conn.prepare(
            r#"
            SELECT
              source_id,
              COALESCE(SUM(COALESCE(CAST(json_extract(payload, '$.usage.requests') AS INTEGER), 1)), 0),
              COALESCE(SUM(total_tokens), 0),
              SUM(COALESCE(
                CAST(json_extract(payload, '$.cost.provider_reported_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_usd') AS INTEGER)
              ))
            FROM usage_summaries
            GROUP BY source_id
            "#,
        )?;
        let summary_rows = summary_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SourceUsageTotals {
                    events: row.get::<_, i64>(1)?.max(0) as u64,
                    tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    estimated_cost_cents: row.get::<_, Option<i64>>(3)?,
                },
            ))
        })?;
        for row in summary_rows {
            let (source_id, summary_totals) = row?;
            match totals.get_mut(&source_id) {
                Some(existing) => merge_source_totals(existing, summary_totals),
                None => {
                    totals.insert(source_id, summary_totals);
                }
            }
        }
        Ok(totals)
    }

    pub fn menu_usage_totals_by_provider(&self) -> Result<HashMap<String, SourceUsageTotals>> {
        let mut totals = HashMap::new();
        let mut rollup_stmt = self.conn.prepare(
            r#"
            SELECT
              provider,
              COALESCE(SUM(CAST(json_extract(payload, '$.usage.requests') AS INTEGER)), 0),
              COALESCE(SUM(CAST(json_extract(payload, '$.usage.total_tokens') AS INTEGER)), 0),
              SUM(COALESCE(
                CAST(json_extract(payload, '$.cost.provider_reported_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_usd') AS INTEGER)
              ))
            FROM sync_rollups
            GROUP BY provider
            "#,
        )?;
        let rollup_rows = rollup_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SourceUsageTotals {
                    events: row.get::<_, i64>(1)?.max(0) as u64,
                    tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    estimated_cost_cents: row.get::<_, Option<i64>>(3)?,
                },
            ))
        })?;
        for row in rollup_rows {
            let (provider, provider_totals) = row?;
            totals.insert(provider, provider_totals);
        }

        let mut summary_stmt = self.conn.prepare(
            r#"
            SELECT
              provider,
              COALESCE(SUM(COALESCE(CAST(json_extract(payload, '$.usage.requests') AS INTEGER), 1)), 0),
              COALESCE(SUM(total_tokens), 0),
              SUM(COALESCE(
                CAST(json_extract(payload, '$.cost.provider_reported_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_usd') AS INTEGER)
              ))
            FROM usage_summaries
            WHERE COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'daily_rollup.v1'
              AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'claude_stats_cache'
              AND COALESCE(json_extract(payload, '$.source.source_kind'), '') != 'local_summary'
            GROUP BY provider
            "#,
        )?;
        let summary_rows = summary_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SourceUsageTotals {
                    events: row.get::<_, i64>(1)?.max(0) as u64,
                    tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    estimated_cost_cents: row.get::<_, Option<i64>>(3)?,
                },
            ))
        })?;
        for row in summary_rows {
            let (provider, summary_totals) = row?;
            let entry = totals.entry(provider).or_default();
            merge_additive_source_totals(entry, summary_totals);
        }
        Ok(totals)
    }

    pub fn source(&self, source_id: &SourceId) -> Result<Option<SourceLocation>> {
        self.conn
            .query_row(
                "SELECT payload FROM sources WHERE source_id = ?1",
                params![&source_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| Ok(serde_json::from_str(&payload)?))
            .transpose()
    }

    pub fn set_source_enabled(
        &self,
        source_id: &SourceId,
        enabled: bool,
    ) -> Result<Option<SourceLocation>> {
        let Some(mut source) = self.source(source_id)? else {
            return Ok(None);
        };
        source.enabled = enabled;
        source.updated_at = Utc::now();
        self.upsert_source(&source)?;
        Ok(Some(source))
    }

    pub fn delete_source(&self, source_id: &SourceId) -> Result<bool> {
        self.with_immediate_transaction(|| {
            self.conn.execute(
                "DELETE FROM source_account_assignments WHERE source_id = ?1",
                params![&source_id.0],
            )?;
            Ok(self.conn.execute(
                "DELETE FROM sources WHERE source_id = ?1",
                params![&source_id.0],
            )? > 0)
        })
    }

    pub fn upsert_account(&self, account: &ProviderAccount) -> Result<()> {
        let payload = serde_json::to_string(account)?;
        self.conn.execute(
            r#"
            INSERT INTO provider_accounts (provider_account_id, provider, payload, updated_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(provider_account_id) DO UPDATE SET
              provider = excluded.provider,
              payload = excluded.payload,
              updated_at = excluded.updated_at
            "#,
            params![
                &account.provider_account_id.0,
                &account.provider,
                &payload,
                account.updated_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn account(
        &self,
        provider_account_id: &ProviderAccountId,
    ) -> Result<Option<ProviderAccount>> {
        Ok(self
            .conn
            .query_row(
                "SELECT payload FROM provider_accounts WHERE provider_account_id = ?1",
                params![&provider_account_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| serde_json::from_str(&payload))
            .transpose()?)
    }

    pub fn list_accounts(&self) -> Result<Vec<ProviderAccount>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload FROM provider_accounts ORDER BY provider, provider_account_id",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut accounts = Vec::new();
        for row in rows {
            accounts.push(serde_json::from_str(&row?)?);
        }
        Ok(accounts)
    }

    pub fn delete_account(&self, provider_account_id: &ProviderAccountId) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM provider_accounts WHERE provider_account_id = ?1",
            params![&provider_account_id.0],
        )? > 0)
    }

    pub fn upsert_source_account_assignment(
        &self,
        assignment: &SourceAccountAssignment,
    ) -> Result<()> {
        let payload = serde_json::to_string(assignment)?;
        self.conn.execute(
            r#"
            INSERT INTO source_account_assignments (
              assignment_id, source_id, provider, provider_account_id,
              started_at, ended_at, payload, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(assignment_id) DO UPDATE SET
              source_id = excluded.source_id,
              provider = excluded.provider,
              provider_account_id = excluded.provider_account_id,
              started_at = excluded.started_at,
              ended_at = excluded.ended_at,
              payload = excluded.payload,
              updated_at = excluded.updated_at
            "#,
            params![
                &assignment.assignment_id.0,
                &assignment.source_id.0,
                &assignment.provider,
                &assignment.provider_account_id.0,
                assignment.started_at.to_rfc3339(),
                assignment.ended_at.map(|date| date.to_rfc3339()),
                &payload,
                assignment.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn source_account_assignment(
        &self,
        assignment_id: &SourceAccountAssignmentId,
    ) -> Result<Option<SourceAccountAssignment>> {
        Ok(self
            .conn
            .query_row(
                "SELECT payload FROM source_account_assignments WHERE assignment_id = ?1",
                params![&assignment_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| serde_json::from_str(&payload))
            .transpose()?)
    }

    pub fn list_source_account_assignments(&self) -> Result<Vec<SourceAccountAssignment>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT payload
            FROM source_account_assignments
            ORDER BY provider, source_id, started_at, assignment_id
            "#,
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut assignments = Vec::new();
        for row in rows {
            assignments.push(serde_json::from_str(&row?)?);
        }
        Ok(assignments)
    }

    pub fn list_source_account_assignments_for_source(
        &self,
        source_id: &SourceId,
    ) -> Result<Vec<SourceAccountAssignment>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT payload
            FROM source_account_assignments
            WHERE source_id = ?1
            ORDER BY started_at, assignment_id
            "#,
        )?;
        let rows = stmt.query_map(params![&source_id.0], |row| row.get::<_, String>(0))?;
        let mut assignments = Vec::new();
        for row in rows {
            assignments.push(serde_json::from_str(&row?)?);
        }
        Ok(assignments)
    }

    pub fn delete_source_account_assignment(
        &self,
        assignment_id: &SourceAccountAssignmentId,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM source_account_assignments WHERE assignment_id = ?1",
            params![&assignment_id.0],
        )? > 0)
    }

    pub fn upsert_subscription(&self, subscription: &Subscription) -> Result<()> {
        let payload = serde_json::to_string(subscription)?;
        self.conn.execute(
            r#"
            INSERT INTO subscriptions (subscription_id, provider, provider_account_id, payload)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(subscription_id) DO UPDATE SET
              provider = excluded.provider,
              provider_account_id = excluded.provider_account_id,
              payload = excluded.payload
            "#,
            params![
                &subscription.subscription_id.0,
                &subscription.provider,
                subscription.provider_account_id.0.as_str(),
                &payload
            ],
        )?;
        Ok(())
    }

    pub fn subscription(&self, subscription_id: &SubscriptionId) -> Result<Option<Subscription>> {
        let row = self
            .conn
            .query_row(
                "SELECT payload, provider_account_id FROM subscriptions WHERE subscription_id = ?1",
                params![&subscription_id.0],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        match row {
            Some((payload, provider_account_id)) => Ok(Some(deserialize_subscription_payload(
                &payload,
                provider_account_id.as_deref(),
            )?)),
            None => Ok(None),
        }
    }

    pub fn list_subscriptions(&self) -> Result<Vec<Subscription>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT payload, provider_account_id FROM subscriptions ORDER BY provider, subscription_id",
            )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut subscriptions = Vec::new();
        for row in rows {
            let (payload, provider_account_id) = row?;
            subscriptions.push(deserialize_subscription_payload(
                &payload,
                provider_account_id.as_deref(),
            )?);
        }
        Ok(subscriptions)
    }

    pub fn delete_subscription(&self, subscription_id: &SubscriptionId) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM subscriptions WHERE subscription_id = ?1",
            params![&subscription_id.0],
        )? > 0)
    }

    pub fn metadata_value(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM local_metadata WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_metadata_value(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO local_metadata (key, value, updated_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(key) DO UPDATE SET
              value = excluded.value,
              updated_at = excluded.updated_at
            "#,
            params![key, value, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn sync_preferences(&self) -> Result<SyncPreferences> {
        let include_projects = self
            .metadata_value(SYNC_INCLUDE_PROJECTS_METADATA_KEY)?
            .as_deref()
            .map(|value| parse_bool_metadata_value(SYNC_INCLUDE_PROJECTS_METADATA_KEY, value))
            .transpose()?
            .unwrap_or(false);
        let include_tasks = self
            .metadata_value(SYNC_INCLUDE_TASKS_METADATA_KEY)?
            .as_deref()
            .map(|value| parse_bool_metadata_value(SYNC_INCLUDE_TASKS_METADATA_KEY, value))
            .transpose()?
            .unwrap_or(false);
        Ok(SyncPreferences {
            include_projects,
            include_tasks,
        }
        .normalized())
    }

    pub fn set_sync_preferences(&self, preferences: SyncPreferences) -> Result<()> {
        let preferences = preferences.normalized();
        self.set_metadata_value(
            SYNC_INCLUDE_PROJECTS_METADATA_KEY,
            if preferences.include_projects {
                "1"
            } else {
                "0"
            },
        )?;
        self.set_metadata_value(
            SYNC_INCLUDE_TASKS_METADATA_KEY,
            if preferences.include_tasks { "1" } else { "0" },
        )?;
        Ok(())
    }

    pub fn insert_event(&self, event: &UsageEvent) -> Result<bool> {
        let event = event_with_valid_project(event);
        let fingerprint = event_fingerprint(&event);
        if let Some(existing_id) = self.find_semantic_duplicate_event_id(&event, &fingerprint)? {
            let existing = self.event_by_id(&existing_id)?;
            let refreshed =
                refreshed_duplicate_event(existing.as_ref(), &event, existing_id.as_str());
            let dirty_keys = self.update_event_payload(&refreshed)?;
            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
            return Ok(false);
        }

        let payload = serde_json::to_string(&event)?;
        let changed = self.conn.execute(
            r#"
            INSERT OR IGNORE INTO usage_events (
              event_id, provider, source_id, provider_account_id, started_at, total_tokens,
              semantic_fingerprint, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                &event.event_id.0,
                &event.provider,
                &event.source_id.0,
                event.provider_account_id.as_ref().map(|id| id.0.as_str()),
                event.session.started_at.to_rfc3339(),
                safe_u64_to_i64(event.usage.computed_total()),
                &fingerprint,
                &payload
            ],
        )?;
        if changed == 0 {
            let existing = self.event_by_id(&event.event_id.0)?;
            let refreshed =
                refreshed_duplicate_event(existing.as_ref(), &event, event.event_id.0.as_str());
            let dirty_keys = self.update_event_payload(&refreshed)?;
            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
        } else {
            self.refresh_sync_rollups_for_keys(&BTreeSet::from([sync_rollup_bucket_key(&event)]))?;
        }
        Ok(changed > 0)
    }

    pub fn insert_events(&self, events: &[UsageEvent]) -> Result<u64> {
        Ok(self.insert_events_with_resolution(events)?.inserted)
    }

    pub fn insert_events_with_resolution(
        &self,
        events: &[UsageEvent],
    ) -> Result<EventInsertBatchResult> {
        let events = events
            .iter()
            .map(event_with_valid_project)
            .collect::<Vec<_>>();
        let fingerprints: Vec<String> = events.iter().map(event_fingerprint).collect();
        let conflict_keys: Vec<ConflictLookupKey> = events
            .iter()
            .zip(fingerprints.iter())
            .map(|(event, fingerprint)| conflict_lookup_key(event, fingerprint))
            .collect();
        self.with_immediate_transaction(|| {
            let mut conflict_map = self.batch_load_conflicts(&conflict_keys)?;
            let mut inserted = 0u64;
            let mut canonical_event_ids = HashMap::with_capacity(events.len());
            let mut dirty_keys = BTreeSet::new();
            for (index, event) in events.iter().enumerate() {
                let incoming_event_id = event.event_id.clone();
                let matched_event =
                    conflict_map
                        .get(&conflict_keys[index])
                        .and_then(|candidates| {
                            exact_or_semantic_conflict(Some(candidates.as_slice()), event).map(
                                |candidate| (candidate.event_id.clone(), candidate.event.clone()),
                            )
                        });
                let matched_event = if matched_event.is_some() {
                    matched_event
                } else if let Some(existing_id) =
                    self.find_codex_fallback_duplicate_event_id(event)?
                {
                    self.event_by_id(&existing_id)?
                        .map(|existing| (existing_id, existing))
                } else {
                    None
                };
                if let Some((existing_id, existing)) = matched_event {
                    let refreshed =
                        refreshed_duplicate_event(Some(&existing), event, existing_id.as_str());
                    dirty_keys.extend(self.update_event_payload(&refreshed)?);
                    canonical_event_ids.insert(incoming_event_id, EventId(existing_id.clone()));
                    let candidates = conflict_map
                        .entry(conflict_keys[index].clone())
                        .or_default();
                    if let Some(candidate) = candidates
                        .iter_mut()
                        .find(|candidate| candidate.event_id == existing_id)
                    {
                        candidate.event = refreshed;
                    } else {
                        candidates.push(ConflictCandidate {
                            event_id: existing_id,
                            event: refreshed,
                        });
                    }
                    continue;
                }
                let fingerprint = &fingerprints[index];
                let outcome = self.insert_event_in_batch(event, fingerprint)?;
                if outcome.inserted {
                    inserted += 1;
                }
                canonical_event_ids.insert(incoming_event_id, outcome.canonical_event_id.clone());
                dirty_keys.extend(outcome.dirty_keys);
                conflict_map
                    .entry(conflict_keys[index].clone())
                    .or_default()
                    .push(ConflictCandidate {
                        event_id: outcome.canonical_event_id.0,
                        event: event.clone(),
                    });
            }
            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
            Ok(EventInsertBatchResult {
                inserted,
                canonical_event_ids,
            })
        })
    }

    fn update_event_payload(&self, event: &UsageEvent) -> Result<BTreeSet<SyncRollupBucketKey>> {
        let existing_bucket = self
            .event_by_id(&event.event_id.0)?
            .map(|existing| sync_rollup_bucket_key(&existing));
        let payload = serde_json::to_string(event)?;
        let fingerprint = event_fingerprint(event);
        self.conn.execute(
            r#"
            UPDATE usage_events
            SET provider = ?2,
                source_id = ?3,
                provider_account_id = ?4,
                started_at = ?5,
                total_tokens = ?6,
                semantic_fingerprint = ?7,
                payload = ?8
            WHERE event_id = ?1
            "#,
            params![
                &event.event_id.0,
                &event.provider,
                &event.source_id.0,
                event.provider_account_id.as_ref().map(|id| id.0.as_str()),
                event.session.started_at.to_rfc3339(),
                safe_u64_to_i64(event.usage.computed_total()),
                &fingerprint,
                &payload
            ],
        )?;
        let mut dirty_keys = BTreeSet::new();
        if let Some(existing_bucket) = existing_bucket {
            dirty_keys.insert(existing_bucket);
        }
        dirty_keys.insert(sync_rollup_bucket_key(event));
        Ok(dirty_keys)
    }

    fn insert_event_in_batch(
        &self,
        event: &UsageEvent,
        fingerprint: &str,
    ) -> Result<EventInsertOutcome> {
        let payload = serde_json::to_string(event)?;
        let changed = self.conn.execute(
            r#"
            INSERT OR IGNORE INTO usage_events (
              event_id, provider, source_id, provider_account_id, started_at, total_tokens,
              semantic_fingerprint, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                &event.event_id.0,
                &event.provider,
                &event.source_id.0,
                event.provider_account_id.as_ref().map(|id| id.0.as_str()),
                event.session.started_at.to_rfc3339(),
                safe_u64_to_i64(event.usage.computed_total()),
                fingerprint,
                &payload
            ],
        )?;
        if changed == 0 {
            let existing = self.event_by_id(&event.event_id.0)?;
            let refreshed =
                refreshed_duplicate_event(existing.as_ref(), event, event.event_id.0.as_str());
            return Ok(EventInsertOutcome {
                inserted: false,
                canonical_event_id: event.event_id.clone(),
                dirty_keys: self.update_event_payload(&refreshed)?,
            });
        }
        Ok(EventInsertOutcome {
            inserted: true,
            canonical_event_id: event.event_id.clone(),
            dirty_keys: BTreeSet::from([sync_rollup_bucket_key(event)]),
        })
    }

    fn batch_load_conflicts(
        &self,
        keys: &[ConflictLookupKey],
    ) -> Result<std::collections::HashMap<ConflictLookupKey, Vec<ConflictCandidate>>> {
        let mut conflicts = std::collections::HashMap::new();
        if keys.is_empty() {
            return Ok(conflicts);
        }

        // Keep the query within SQLite's common 999-parameter limit:
        // each lookup uses provider, source_id, and semantic_fingerprint.
        const CHUNK_SIZE: usize = 300;
        for chunk in keys.chunks(CHUNK_SIZE) {
            let placeholders: Vec<String> = chunk
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let base = (i * 3) + 1;
                    format!(
                        "(provider = ?{base} AND source_id = ?{} AND semantic_fingerprint = ?{})",
                        base + 1,
                        base + 2
                    )
                })
                .collect();
            let sql = format!(
                "SELECT provider, source_id, event_id, semantic_fingerprint, payload \
                 FROM usage_events WHERE {}",
                placeholders.join(" OR ")
            );

            let mut stmt = self.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::types::ToSql> = chunk
                .iter()
                .flat_map(|key| {
                    [
                        &key.provider as &dyn rusqlite::types::ToSql,
                        &key.source_id as &dyn rusqlite::types::ToSql,
                        &key.fingerprint as &dyn rusqlite::types::ToSql,
                    ]
                })
                .collect();

            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;

            for row in rows {
                let (provider, source_id, event_id, fingerprint, payload) = row?;
                let event: UsageEvent = serde_json::from_str(&payload)?;
                conflicts
                    .entry(ConflictLookupKey {
                        provider,
                        source_id,
                        fingerprint,
                    })
                    .or_insert_with(Vec::new)
                    .push(ConflictCandidate { event_id, event });
            }
        }
        Ok(conflicts)
    }

    fn find_semantic_duplicate_event_id(
        &self,
        event: &UsageEvent,
        fingerprint: &str,
    ) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT event_id, payload
            FROM usage_events
            WHERE provider = ?1
              AND source_id = ?2
              AND semantic_fingerprint = ?3
            "#,
        )?;
        let rows = stmt.query_map(
            params![&event.provider, &event.source_id.0, fingerprint,],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        for row in rows {
            let (event_id, payload) = row?;
            if event_id == event.event_id.0 {
                return Ok(None);
            }
            let candidate: UsageEvent = serde_json::from_str(&payload)?;
            if semantically_same_event(&candidate, event) {
                return Ok(Some(event_id));
            }
        }
        self.find_codex_fallback_duplicate_event_id(event)
    }

    fn find_codex_fallback_duplicate_event_id(&self, event: &UsageEvent) -> Result<Option<String>> {
        if event.provider != "codex" {
            return Ok(None);
        }
        let mut fallback = self.conn.prepare(
            r#"
            SELECT event_id, payload
            FROM usage_events
            WHERE provider = ?1
              AND source_id = ?2
              AND started_at = ?3
              AND total_tokens = ?4
            "#,
        )?;
        let fallback_rows = fallback.query_map(
            params![
                &event.provider,
                &event.source_id.0,
                event.session.started_at.to_rfc3339(),
                safe_u64_to_i64(event.usage.computed_total()),
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        for row in fallback_rows {
            let (event_id, payload) = row?;
            if event_id == event.event_id.0 {
                return Ok(None);
            }
            let candidate: UsageEvent = serde_json::from_str(&payload)?;
            if semantically_same_event(&candidate, event) {
                return Ok(Some(event_id));
            }
        }
        Ok(None)
    }

    pub fn event_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    pub fn token_total(&self) -> Result<u64> {
        let count: Option<i64> =
            self.conn
                .query_row("SELECT SUM(total_tokens) FROM usage_events", [], |row| {
                    row.get(0)
                })?;
        Ok(count.unwrap_or(0) as u64)
    }

    pub fn usage_period_stats(&self, since: DateTime<Utc>) -> Result<UsagePeriodStats> {
        let since = since.to_rfc3339();
        self.conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0) FROM usage_events WHERE started_at >= ?1",
                params![since],
                |row| {
                    Ok(UsagePeriodStats {
                        events: row.get::<_, i64>(0)? as u64,
                        tokens: row.get::<_, i64>(1)? as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn unsynced_event_count(&self, cursor: Option<(&DateTime<Utc>, &str)>) -> Result<u64> {
        let count: i64 = if let Some((started_at, event_id)) = cursor {
            self.conn.query_row(
                r#"
                SELECT COUNT(*) FROM usage_events
                WHERE started_at > ?1 OR (started_at = ?1 AND event_id > ?2)
                "#,
                params![started_at.to_rfc3339(), event_id],
                |row| row.get(0),
            )?
        } else {
            self.conn
                .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))?
        };
        Ok(count as u64)
    }

    pub fn events(&self) -> Result<Vec<UsageEvent>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM usage_events ORDER BY started_at, event_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut events: Vec<UsageEvent> = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        Ok(events)
    }

    pub fn events_in_period(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<UsageEvent>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT payload FROM usage_events
            WHERE started_at >= ?1 AND started_at <= ?2
            ORDER BY started_at, event_id
            "#,
        )?;
        let rows = stmt.query_map(params![since.to_rfc3339(), until.to_rfc3339()], |row| {
            row.get::<_, String>(0)
        })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        Ok(events)
    }

    pub fn events_for_source(&self, source_id: &SourceId) -> Result<Vec<UsageEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload FROM usage_events WHERE source_id = ?1 ORDER BY started_at, event_id",
        )?;
        let rows = stmt.query_map(params![&source_id.0], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        Ok(events)
    }

    pub fn events_after(&self, cursor: Option<(&DateTime<Utc>, &str)>) -> Result<Vec<UsageEvent>> {
        let sql = if cursor.is_some() {
            r#"
            SELECT payload FROM usage_events
            WHERE started_at > ?1 OR (started_at = ?1 AND event_id > ?2)
            ORDER BY started_at, event_id
            "#
        } else {
            "SELECT payload FROM usage_events ORDER BY started_at, event_id"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut events: Vec<UsageEvent> = Vec::new();
        if let Some((started_at, event_id)) = cursor {
            let rows = stmt.query_map(params![started_at.to_rfc3339(), event_id], |row| {
                row.get::<_, String>(0)
            })?;
            for row in rows {
                events.push(serde_json::from_str(&row?)?);
            }
        } else {
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                events.push(serde_json::from_str(&row?)?);
            }
        }
        Ok(events)
    }

    pub fn rewrite_events(&self, events: &[UsageEvent]) -> Result<u64> {
        if events.is_empty() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| {
            let mut changed = 0u64;
            let mut dirty_keys = BTreeSet::new();
            for event in events {
                dirty_keys.extend(self.update_event_payload(event)?);
                changed += 1;
            }
            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
            Ok(changed)
        })
    }

    pub fn delete_events_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for source_id in source_ids {
                deleted += self.conn.execute(
                    "DELETE FROM usage_events WHERE source_id = ?1",
                    params![&source_id.0],
                )? as u64;
            }
            self.delete_sync_rollups_for_sources_in_tx(source_ids)?;
            Ok(deleted)
        })
    }

    pub fn delete_events_for_source_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
    ) -> Result<u64> {
        if file_hashes.is_empty() {
            return Ok(0);
        }

        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            let mut dirty_keys = BTreeSet::new();

            for file_hash in file_hashes {
                let payloads: Vec<String>;
                {
                    let mut stmt = self.conn.prepare(
                        r#"
                        SELECT payload
                        FROM usage_events
                        WHERE source_id = ?1
                          AND json_extract(payload, '$.parse_evidence.source_file_path_hash') = ?2
                        "#,
                    )?;
                    let rows =
                        stmt.query_map(params![&source_id.0, file_hash], |row| row.get(0))?;
                    payloads = rows.collect::<Result<Vec<_>, _>>()?;
                }

                for payload in payloads {
                    let event: UsageEvent = serde_json::from_str(&payload)?;
                    dirty_keys.insert(sync_rollup_bucket_key(&event));
                }

                deleted += self.conn.execute(
                    r#"
                    DELETE FROM usage_events
                    WHERE source_id = ?1
                      AND json_extract(payload, '$.parse_evidence.source_file_path_hash') = ?2
                    "#,
                    params![&source_id.0, file_hash],
                )? as u64;
            }

            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
            Ok(deleted)
        })
    }

    pub fn upsert_summary(&self, summary: &UsageSummary) -> Result<bool> {
        let payload = serde_json::to_string(summary)?;
        let changed = self.conn.execute(
            r#"
            INSERT INTO usage_summaries (
              summary_id, provider, source_id, provider_account_id, period_start, period_end,
              observed_at, total_tokens, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(summary_id) DO UPDATE SET
              provider = excluded.provider,
              source_id = excluded.source_id,
              provider_account_id = excluded.provider_account_id,
              period_start = excluded.period_start,
              period_end = excluded.period_end,
              observed_at = excluded.observed_at,
              total_tokens = excluded.total_tokens,
              payload = excluded.payload
            "#,
            params![
                &summary.summary_id.0,
                &summary.provider,
                &summary.source_id.0,
                summary.provider_account_id.as_ref().map(|id| id.0.as_str()),
                summary.period_start.map(|date| date.to_rfc3339()),
                summary.period_end.map(|date| date.to_rfc3339()),
                summary.observed_at.to_rfc3339(),
                safe_u64_to_i64(summary.usage.computed_total()),
                &payload,
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn upsert_summaries(&self, summaries: &[UsageSummary]) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut changed = 0u64;
            for summary in summaries {
                if self.upsert_summary(summary)? {
                    changed += 1;
                }
            }
            Ok(changed)
        })
    }

    pub fn delete_summaries_for_source_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
    ) -> Result<u64> {
        if file_hashes.is_empty() {
            return Ok(0);
        }

        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for file_hash in file_hashes {
                deleted += self.conn.execute(
                    r#"
                    DELETE FROM usage_summaries
                    WHERE source_id = ?1
                      AND json_extract(payload, '$.parse_evidence.source_file_path_hash') = ?2
                    "#,
                    params![&source_id.0, file_hash],
                )? as u64;
            }
            Ok(deleted)
        })
    }

    pub fn summaries(&self) -> Result<Vec<UsageSummary>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM usage_summaries ORDER BY observed_at, summary_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(serde_json::from_str(&row?)?);
        }
        Ok(summaries)
    }

    pub fn summaries_for_source(&self, source_id: &SourceId) -> Result<Vec<UsageSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload FROM usage_summaries WHERE source_id = ?1 ORDER BY observed_at, summary_id",
        )?;
        let rows = stmt.query_map(params![&source_id.0], |row| row.get::<_, String>(0))?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(serde_json::from_str(&row?)?);
        }
        Ok(summaries)
    }

    pub fn rewrite_summaries(&self, summaries: &[UsageSummary]) -> Result<u64> {
        if summaries.is_empty() {
            return Ok(0);
        }
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            let mut changed = 0u64;
            for summary in summaries {
                if self.upsert_summary(summary)? {
                    changed += 1;
                }
            }
            Ok(changed)
        })();

        match result {
            Ok(changed) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(changed)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn summaries_after(
        &self,
        cursor: Option<(&DateTime<Utc>, &str)>,
    ) -> Result<Vec<UsageSummary>> {
        let sql = if cursor.is_some() {
            r#"
            SELECT payload FROM usage_summaries
            WHERE observed_at > ?1 OR (observed_at = ?1 AND summary_id > ?2)
            ORDER BY observed_at, summary_id
            "#
        } else {
            "SELECT payload FROM usage_summaries ORDER BY observed_at, summary_id"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut summaries = Vec::new();
        if let Some((observed_at, summary_id)) = cursor {
            let rows = stmt.query_map(params![observed_at.to_rfc3339(), summary_id], |row| {
                row.get::<_, String>(0)
            })?;
            for row in rows {
                summaries.push(serde_json::from_str(&row?)?);
            }
        } else {
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                summaries.push(serde_json::from_str(&row?)?);
            }
        }
        Ok(summaries)
    }

    pub fn sync_state(&self, sink: &str, target: &str) -> Result<Option<SyncState>> {
        self.conn
            .query_row(
                r#"
                SELECT sink, target, last_success_at, last_batch_id, last_event_started_at,
                       last_event_id, last_summary_observed_at, last_summary_id,
                       last_task_verification_updated_at, last_task_verification_id,
                       failure_count, pending_resume_batch_id
                FROM sync_state
                WHERE sink = ?1 AND target = ?2
                "#,
                params![sink, target],
                sync_state_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_sync_states(&self) -> Result<Vec<SyncState>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT sink, target, last_success_at, last_batch_id, last_event_started_at,
                   last_event_id, last_summary_observed_at, last_summary_id,
                   last_task_verification_updated_at, last_task_verification_id,
                   failure_count, pending_resume_batch_id
            FROM sync_state
            ORDER BY sink, target
            "#,
        )?;
        let rows = stmt.query_map([], sync_state_from_row)?;
        let mut states = Vec::new();
        for row in rows {
            states.push(row?);
        }
        Ok(states)
    }

    pub fn clear_sync_tracking(&self) -> Result<()> {
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            self.conn.execute("DELETE FROM entity_sync_state", [])?;
            self.conn
                .execute("DELETE FROM task_bucket_sync_state", [])?;
            self.conn.execute("DELETE FROM sync_state", [])?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn clear_sync_tracking_for_target(&self, sink: &str, target: &str) -> Result<()> {
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            self.conn.execute(
                "DELETE FROM entity_sync_state WHERE sink = ?1 AND target = ?2",
                params![sink, target],
            )?;
            self.conn.execute(
                "DELETE FROM task_bucket_sync_state WHERE sink = ?1 AND target = ?2",
                params![sink, target],
            )?;
            self.conn.execute(
                "DELETE FROM sync_state WHERE sink = ?1 AND target = ?2",
                params![sink, target],
            )?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn record_sync_success(
        &self,
        sink: &str,
        target: &str,
        batch_id: &str,
        events: &[UsageEvent],
        summaries: &[UsageSummary],
        task_verification_cursor: Option<&TaskVerificationCursor>,
    ) -> Result<()> {
        let event_cursor = events
            .iter()
            .max_by(|left, right| {
                left.session
                    .started_at
                    .cmp(&right.session.started_at)
                    .then_with(|| left.event_id.0.cmp(&right.event_id.0))
            })
            .map(|event| (event.session.started_at, event.event_id.0.as_str()));
        let summary_cursor = summaries
            .iter()
            .max_by(|left, right| {
                left.observed_at
                    .cmp(&right.observed_at)
                    .then_with(|| left.summary_id.0.cmp(&right.summary_id.0))
            })
            .map(|summary| (summary.observed_at, summary.summary_id.0.as_str()));
        let existing = self.sync_state(sink, target)?;
        let event_started_at = event_cursor.map(|(date, _)| date).or_else(|| {
            existing
                .as_ref()
                .and_then(|state| state.last_event_started_at)
        });
        let event_id = event_cursor.map(|(_, id)| id.to_string()).or_else(|| {
            existing
                .as_ref()
                .and_then(|state| state.last_event_id.clone())
        });
        let summary_observed_at = summary_cursor.map(|(date, _)| date).or_else(|| {
            existing
                .as_ref()
                .and_then(|state| state.last_summary_observed_at)
        });
        let summary_id = summary_cursor.map(|(_, id)| id.to_string()).or_else(|| {
            existing
                .as_ref()
                .and_then(|state| state.last_summary_id.clone())
        });
        let task_verification_updated_at = task_verification_cursor
            .map(|cursor| cursor.updated_at)
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|state| state.last_task_verification_updated_at)
            });
        let task_verification_id = task_verification_cursor
            .map(|cursor| cursor.verification_id.0.clone())
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|state| state.last_task_verification_id.clone())
            });
        let now = Utc::now();

        self.conn.execute(
            r#"
            INSERT INTO sync_state (
              sink, target, last_success_at, last_batch_id, last_event_started_at,
              last_event_id, last_summary_observed_at, last_summary_id,
              last_task_verification_updated_at, last_task_verification_id, failure_count
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0)
            ON CONFLICT(sink, target) DO UPDATE SET
              last_success_at = excluded.last_success_at,
              last_batch_id = excluded.last_batch_id,
              last_event_started_at = excluded.last_event_started_at,
              last_event_id = excluded.last_event_id,
              last_summary_observed_at = excluded.last_summary_observed_at,
              last_summary_id = excluded.last_summary_id,
              last_task_verification_updated_at = excluded.last_task_verification_updated_at,
              last_task_verification_id = excluded.last_task_verification_id,
              failure_count = 0
            "#,
            params![
                sink,
                target,
                now.to_rfc3339(),
                batch_id,
                event_started_at.map(|date| date.to_rfc3339()),
                event_id,
                summary_observed_at.map(|date| date.to_rfc3339()),
                summary_id,
                task_verification_updated_at.map(|date| date.to_rfc3339()),
                task_verification_id,
            ],
        )?;
        Ok(())
    }

    fn sync_batch_task_verification_cursor(batch: &SyncBatch) -> Option<TaskVerificationCursor> {
        batch
            .task_buckets
            .iter()
            .filter_map(|bucket| bucket.applied_verification_cursor.clone())
            .max_by(|left, right| {
                left.updated_at
                    .cmp(&right.updated_at)
                    .then_with(|| left.verification_id.0.cmp(&right.verification_id.0))
            })
    }

    pub fn record_rollup_chunk_sync_success(
        &self,
        sink: &str,
        target: &str,
        logical_batch_id: &str,
        batch: &SyncBatch,
    ) -> Result<()> {
        self.ensure_current_sync_rollup_versions()?;
        let passthrough_summaries: Vec<_> = batch
            .summaries
            .iter()
            .filter(|summary| !is_daily_rollup_summary(summary))
            .cloned()
            .collect();
        let rollup_summary_ids: Vec<_> = batch
            .summaries
            .iter()
            .filter(|summary| is_daily_rollup_summary(summary))
            .map(|summary| summary.summary_id.clone())
            .collect();
        let rollup_summaries = self.all_sync_rollup_summaries()?;
        let task_verification_cursor = Self::sync_batch_task_verification_cursor(batch);

        self.with_immediate_transaction(|| {
            self.record_sync_success(
                sink,
                target,
                logical_batch_id,
                &batch.events,
                &passthrough_summaries,
                task_verification_cursor.as_ref(),
            )?;
            self.mark_pending_sync_resume(sink, target, logical_batch_id)?;
            self.mark_sync_rollups_synced_in_transaction(&rollup_summary_ids)?;
            self.reconcile_sync_rollup_dirty_flags_in_transaction(sink, target, &rollup_summaries)?;
            self.record_summaries_synced_in_transaction(sink, target, &batch.summaries)?;
            self.record_sources_synced_in_transaction(sink, target, &batch.sources)?;
            self.record_accounts_synced_in_transaction(sink, target, &batch.accounts)?;
            self.record_source_account_assignments_synced_in_transaction(
                sink,
                target,
                &batch.source_account_assignments,
            )?;
            self.record_subscriptions_synced_in_transaction(sink, target, &batch.subscriptions)?;
            self.record_task_bucket_snapshots_synced_in_transaction(
                sink,
                target,
                &batch.device_id,
                &batch.task_buckets,
            )?;
            self.record_task_verifications_synced_in_transaction(
                sink,
                target,
                &batch.task_verifications,
            )?;
            Ok(())
        })
    }

    pub fn sync_task_verification_cursor(
        &self,
        sink: &str,
        target: &str,
    ) -> Result<Option<TaskVerificationCursor>> {
        let Some(state) = self.sync_state(sink, target)? else {
            return Ok(None);
        };
        let Some(updated_at) = state.last_task_verification_updated_at else {
            return Ok(None);
        };
        let Some(verification_id) = state.last_task_verification_id else {
            return Ok(None);
        };
        Ok(Some(TaskVerificationCursor {
            updated_at,
            verification_id: TaskVerificationId(verification_id),
        }))
    }

    pub fn task_bucket_sync_status(
        &self,
        sink: &str,
        target: &str,
        device_id: &str,
    ) -> Result<TaskBucketSyncStatus> {
        let local_buckets = self.task_project_buckets()?;
        let mut statement = self.conn.prepare(
            r#"
            SELECT project_bucket, dirty
            FROM task_bucket_sync_state
            WHERE sink = ?1 AND target = ?2 AND device_id = ?3
            "#,
        )?;
        let rows = statement.query_map(params![sink, target, device_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut tracked = HashMap::<String, i64>::new();
        for row in rows {
            let (project_bucket, dirty) = row?;
            tracked.insert(project_bucket, dirty);
        }
        let tracked_total = tracked.len() as u64;
        let tracked_dirty = tracked.values().filter(|dirty| **dirty == 1).count() as u64;
        let missing_local = local_buckets
            .iter()
            .filter(|project_bucket| !tracked.contains_key(project_bucket.as_str()))
            .count() as u64;
        Ok(TaskBucketSyncStatus {
            total: tracked_total.saturating_add(missing_local),
            dirty: tracked_dirty.saturating_add(missing_local),
        })
    }

    pub fn record_sync_failure(&self, sink: &str, target: &str) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO sync_state (
              sink, target, last_success_at, last_batch_id, failure_count, pending_resume_batch_id
            )
            VALUES (?1, ?2, ?3, '', 1, NULL)
            ON CONFLICT(sink, target) DO UPDATE SET
              failure_count = failure_count + 1
            "#,
            params![sink, target, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn mark_pending_sync_resume(&self, sink: &str, target: &str, batch_id: &str) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO sync_state (
              sink, target, last_success_at, last_batch_id, failure_count, pending_resume_batch_id
            )
            VALUES (?1, ?2, ?3, '', 0, ?4)
            ON CONFLICT(sink, target) DO UPDATE SET
              pending_resume_batch_id = excluded.pending_resume_batch_id
            "#,
            params![sink, target, Utc::now().to_rfc3339(), batch_id],
        )?;
        Ok(())
    }

    pub fn clear_pending_sync_resume(&self, sink: &str, target: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sync_state SET pending_resume_batch_id = NULL WHERE sink = ?1 AND target = ?2",
            params![sink, target],
        )?;
        Ok(())
    }

    pub fn delete_summaries_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            let mut deleted = 0u64;
            for source_id in source_ids {
                deleted += self.conn.execute(
                    "DELETE FROM usage_summaries WHERE source_id = ?1",
                    params![&source_id.0],
                )? as u64;
            }
            Ok(deleted)
        })();

        match result {
            Ok(deleted) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(deleted)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn delete_summaries(&self, summary_ids: &[SummaryId]) -> Result<u64> {
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            let mut deleted = 0u64;
            for summary_id in summary_ids {
                deleted += self.conn.execute(
                    "DELETE FROM usage_summaries WHERE summary_id = ?1",
                    params![&summary_id.0],
                )? as u64;
            }
            Ok(deleted)
        })();

        match result {
            Ok(deleted) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(deleted)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn summary_count(&self) -> Result<u64> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM usage_summaries", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    pub fn sync_rollup_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sync_rollups", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    pub fn dirty_sync_rollup_summaries(&self) -> Result<Vec<UsageSummary>> {
        self.ensure_current_sync_rollup_versions()?;
        self.sync_rollup_summaries_by_sql(
            "SELECT payload FROM sync_rollups WHERE dirty = 1 ORDER BY updated_at, summary_id",
        )
    }

    pub fn all_sync_rollup_summaries(&self) -> Result<Vec<UsageSummary>> {
        self.ensure_current_sync_rollup_versions()?;
        self.sync_rollup_summaries_by_sql(
            "SELECT payload FROM sync_rollups ORDER BY updated_at, summary_id",
        )
    }

    pub fn mark_sync_rollups_synced(&self, summary_ids: &[SummaryId]) -> Result<()> {
        if summary_ids.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.mark_sync_rollups_synced_in_transaction(summary_ids)
        })
    }

    fn mark_sync_rollups_synced_in_transaction(&self, summary_ids: &[SummaryId]) -> Result<()> {
        for summary_id in summary_ids {
            self.conn.execute(
                "UPDATE sync_rollups SET dirty = 0 WHERE summary_id = ?1",
                params![&summary_id.0],
            )?;
        }
        Ok(())
    }

    pub fn mark_all_sync_rollups_dirty(&self) -> Result<u64> {
        let updated = self.conn.execute(
            "UPDATE sync_rollups SET dirty = 1, updated_at = ?1",
            params![Utc::now().to_rfc3339()],
        )? as u64;
        Ok(updated)
    }

    pub fn rebuild_sync_rollups(&self) -> Result<u64> {
        let events = self.events()?;
        let keys: BTreeSet<_> = events.iter().map(sync_rollup_bucket_key).collect();

        self.with_immediate_transaction(|| {
            self.conn.execute("DELETE FROM sync_rollups", [])?;
            self.refresh_sync_rollups_for_keys(&keys)?;
            Ok(keys.len() as u64)
        })
    }

    fn ensure_current_sync_rollup_versions(&self) -> Result<()> {
        let stale_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sync_rollups
             WHERE json_extract(payload, '$.metadata.summary_format') = 'daily_rollup.v1'
               AND COALESCE(json_extract(payload, '$.metadata.summary_version'), '') != ?1",
            params![SYNC_ROLLUP_SUMMARY_VERSION],
            |row| row.get(0),
        )?;
        if stale_count > 0 {
            self.rebuild_sync_rollups()?;
        }
        Ok(())
    }

    pub fn pending_sources_for_sync(
        &self,
        sink: &str,
        target: &str,
        sources: &[SourceLocation],
    ) -> Result<Vec<SourceLocation>> {
        let mut changed = Vec::new();
        for source in sources {
            let payload = serde_json::to_string(source)?;
            if self.entity_requires_sync(
                sink,
                target,
                "source",
                &source.source_id.0,
                &hash_text(&payload),
            )? {
                changed.push(source.clone());
            }
        }
        Ok(changed)
    }

    pub fn pending_accounts_for_sync(
        &self,
        sink: &str,
        target: &str,
        accounts: &[ProviderAccount],
    ) -> Result<Vec<ProviderAccount>> {
        let mut changed = Vec::new();
        for account in accounts {
            let payload = serde_json::to_string(account)?;
            if self.entity_requires_sync(
                sink,
                target,
                "account",
                &account.provider_account_id.0,
                &hash_text(&payload),
            )? {
                changed.push(account.clone());
            }
        }
        Ok(changed)
    }

    pub fn pending_source_account_assignments_for_sync(
        &self,
        sink: &str,
        target: &str,
        assignments: &[SourceAccountAssignment],
    ) -> Result<Vec<SourceAccountAssignment>> {
        let mut changed = Vec::new();
        for assignment in assignments {
            let payload = serde_json::to_string(assignment)?;
            if self.entity_requires_sync(
                sink,
                target,
                "source_account_assignment",
                &assignment.assignment_id.0,
                &hash_text(&payload),
            )? {
                changed.push(assignment.clone());
            }
        }
        Ok(changed)
    }

    pub fn pending_subscriptions_for_sync(
        &self,
        sink: &str,
        target: &str,
        subscriptions: &[Subscription],
    ) -> Result<Vec<Subscription>> {
        let mut changed = Vec::new();
        for subscription in subscriptions {
            let payload = serde_json::to_string(subscription)?;
            if self.entity_requires_sync(
                sink,
                target,
                "subscription",
                &subscription.subscription_id.0,
                &hash_text(&payload),
            )? {
                changed.push(subscription.clone());
            }
        }
        Ok(changed)
    }

    pub fn pending_summaries_for_sync(
        &self,
        sink: &str,
        target: &str,
        summaries: &[UsageSummary],
    ) -> Result<Vec<UsageSummary>> {
        let mut changed = Vec::new();
        for summary in summaries {
            let payload_hash = summary_sync_payload_hash(summary)?;
            if self.entity_requires_sync(
                sink,
                target,
                "summary",
                &summary.summary_id.0,
                &payload_hash,
            )? {
                changed.push(summary.clone());
            }
        }
        Ok(changed)
    }

    pub fn pending_http_sync_rollup_summaries(&self, target: &str) -> Result<Vec<UsageSummary>> {
        self.pending_http_sync_rollup_summaries_with_projects(target, false)
    }

    pub fn pending_http_sync_rollup_summaries_with_projects(
        &self,
        target: &str,
        include_projects: bool,
    ) -> Result<Vec<UsageSummary>> {
        let rollups = self
            .all_sync_rollup_summaries()?
            .into_iter()
            .map(|summary| sanitize_summary_for_http_sync(summary, include_projects))
            .collect::<Vec<_>>();
        self.pending_summaries_for_sync("http", target, &rollups)
    }

    pub fn pending_http_sync_summary_counts(
        &self,
        target: &str,
    ) -> Result<PendingSyncSummaryCounts> {
        self.pending_http_sync_summary_counts_with_projects(target, false)
    }

    pub fn pending_http_sync_summary_counts_with_projects(
        &self,
        target: &str,
        include_projects: bool,
    ) -> Result<PendingSyncSummaryCounts> {
        let rollups =
            self.pending_http_sync_rollup_summaries_with_projects(target, include_projects)?;
        let passthrough_summaries =
            self.pending_http_passthrough_summaries_with_projects(target, include_projects)?;
        let mut days = collect_pending_summary_days(rollups.iter());
        days.extend(collect_pending_summary_days(passthrough_summaries.iter()));
        Ok(PendingSyncSummaryCounts {
            rollups: rollups.len() as u64,
            passthrough_summaries: passthrough_summaries.len() as u64,
            total: rollups.len().saturating_add(passthrough_summaries.len()) as u64,
            days: days.len() as u64,
        })
    }

    fn pending_http_passthrough_summaries_with_projects(
        &self,
        target: &str,
        include_projects: bool,
    ) -> Result<Vec<UsageSummary>> {
        let summaries = self
            .summaries()?
            .into_iter()
            .filter(is_http_rollup_passthrough_summary)
            .map(|summary| sanitize_summary_for_http_sync(summary, include_projects))
            .collect::<Vec<_>>();
        self.pending_summaries_for_sync("http", target, &summaries)
    }

    pub fn sync_rollup_period_stats(&self, cutoff_day: NaiveDate) -> Result<RollupPeriodStats> {
        let mut tokens = 0u64;
        let mut requests = 0u64;
        for summary in self.all_sync_rollup_summaries()? {
            let day = summary_sync_day(&summary);
            if day < cutoff_day {
                continue;
            }
            tokens = tokens.saturating_add(summary.usage.computed_total());
            requests = requests.saturating_add(summary.usage.requests.unwrap_or(0));
        }
        Ok(RollupPeriodStats { tokens, requests })
    }

    pub fn usage_event_period_stats_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<RollupPeriodStats> {
        Ok(self.conn.query_row(
            r#"
            SELECT
              COALESCE(SUM(total_tokens), 0),
              COUNT(*)
            FROM usage_events
            WHERE started_at >= ?1
            "#,
            params![since.to_rfc3339()],
            |row| {
                Ok(RollupPeriodStats {
                    tokens: row.get::<_, i64>(0)?.max(0) as u64,
                    requests: row.get::<_, i64>(1)?.max(0) as u64,
                })
            },
        )?)
    }

    pub fn reportable_summary_period_stats_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<RollupPeriodStats> {
        self.conn
            .query_row(
                r#"
                SELECT
                  COALESCE(SUM(total_tokens), 0),
                  COALESCE(SUM(COALESCE(CAST(json_extract(payload, '$.usage.requests') AS INTEGER), 1)), 0)
                FROM usage_summaries
                WHERE datetime(COALESCE(period_start, observed_at)) >= datetime(?1)
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'daily_rollup.v1'
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'claude_stats_cache'
                  AND COALESCE(json_extract(payload, '$.source.source_kind'), '') != 'local_summary'
                "#,
                params![since.to_rfc3339()],
                |row| {
                    Ok(RollupPeriodStats {
                        tokens: row.get::<_, i64>(0)?.max(0) as u64,
                        requests: row.get::<_, i64>(1)?.max(0) as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn reportable_summary_period_stats_since_day(
        &self,
        cutoff_day: NaiveDate,
    ) -> Result<RollupPeriodStats> {
        let cutoff_day = cutoff_day.format("%Y-%m-%d").to_string();
        self.conn
            .query_row(
                r#"
                SELECT
                  COALESCE(SUM(total_tokens), 0),
                  COALESCE(SUM(COALESCE(CAST(json_extract(payload, '$.usage.requests') AS INTEGER), 1)), 0)
                FROM usage_summaries
                WHERE substr(COALESCE(period_start, observed_at), 1, 10) >= ?1
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'daily_rollup.v1'
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'claude_stats_cache'
                  AND COALESCE(json_extract(payload, '$.source.source_kind'), '') != 'local_summary'
                "#,
                params![cutoff_day],
                |row| {
                    Ok(RollupPeriodStats {
                        tokens: row.get::<_, i64>(0)?.max(0) as u64,
                        requests: row.get::<_, i64>(1)?.max(0) as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn snapshot_rollup_view(
        &self,
        sink: &str,
        target: &str,
        week_cutoff: NaiveDate,
        today_cutoff: NaiveDate,
    ) -> Result<SnapshotRollupView> {
        let week_cutoff = week_cutoff.format("%Y-%m-%d").to_string();
        let today_cutoff = today_cutoff.format("%Y-%m-%d").to_string();
        let week = self.sync_rollup_stats_since_day(&week_cutoff)?;
        let today = self.sync_rollup_stats_since_day(&today_cutoff)?;
        let (pending_count, pending_days) = self.pending_sync_rollup_counts(sink, target)?;
        Ok(SnapshotRollupView {
            pending_count,
            pending_days,
            today,
            week,
        })
    }

    fn sync_rollup_stats_since_day(&self, cutoff_day: &str) -> Result<RollupPeriodStats> {
        self.conn
            .query_row(
                r#"
                SELECT
                  COALESCE(SUM(CAST(json_extract(payload, '$.usage.total_tokens') AS INTEGER)), 0),
                  COALESCE(SUM(CAST(json_extract(payload, '$.usage.requests') AS INTEGER)), 0)
                FROM sync_rollups
                WHERE day_key >= ?1
                "#,
                params![cutoff_day],
                |row| {
                    Ok(RollupPeriodStats {
                        tokens: row.get::<_, i64>(0)? as u64,
                        requests: row.get::<_, i64>(1)? as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    const SYNC_ROLLUP_HASH_RECONCILE_KEY: &str = "sync_rollup_sync_hashes_reconciled_v1";

    pub fn reconcile_sync_rollup_sync_hashes_if_needed(&self) -> Result<u64> {
        if self
            .metadata_value(Self::SYNC_ROLLUP_HASH_RECONCILE_KEY)?
            .as_deref()
            == Some("1")
        {
            return Ok(0);
        }
        let updated = self.reconcile_sync_rollup_sync_hashes()?;
        self.set_metadata_value(Self::SYNC_ROLLUP_HASH_RECONCILE_KEY, "1")?;
        Ok(updated)
    }

    pub fn reconcile_sync_rollup_sync_hashes(&self) -> Result<u64> {
        let mut stmt = self
            .conn
            .prepare("SELECT summary_id, payload FROM sync_rollups")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;

        self.with_immediate_transaction(|| {
            let mut updated = 0u64;
            for (summary_id, payload) in &rows {
                let summary: UsageSummary = serde_json::from_str(payload)?;
                let payload_hash = summary_sync_payload_hash(&summary)?;
                updated += self.conn.execute(
                    "UPDATE sync_rollups SET payload_hash = ?1 WHERE summary_id = ?2 AND payload_hash != ?1",
                    params![payload_hash, summary_id],
                )? as u64;
            }
            Ok(updated)
        })
    }

    fn pending_sync_rollup_counts(&self, sink: &str, target: &str) -> Result<(u64, u64)> {
        let rollups = self
            .all_sync_rollup_summaries()?
            .into_iter()
            .map(sanitize_summary_for_default_http_sync)
            .collect::<Vec<_>>();
        let pending = self.pending_summaries_for_sync(sink, target, &rollups)?;
        let days = collect_pending_summary_days(pending.iter());
        Ok((pending.len() as u64, days.len() as u64))
    }

    pub fn reconcile_sync_rollup_dirty_flags(&self, sink: &str, target: &str) -> Result<u64> {
        self.ensure_current_sync_rollup_versions()?;
        let summaries = self.all_sync_rollup_summaries()?;
        self.with_immediate_transaction(|| {
            self.reconcile_sync_rollup_dirty_flags_in_transaction(sink, target, &summaries)
        })
    }

    fn reconcile_sync_rollup_dirty_flags_in_transaction(
        &self,
        sink: &str,
        target: &str,
        summaries: &[UsageSummary],
    ) -> Result<u64> {
        let mut cleared = 0u64;
        for summary in summaries {
            let payload_hash = summary_sync_payload_hash(summary)?;
            if self.entity_requires_sync(
                sink,
                target,
                "summary",
                &summary.summary_id.0,
                &payload_hash,
            )? {
                continue;
            }
            cleared += self.conn.execute(
                "UPDATE sync_rollups SET dirty = 0 WHERE summary_id = ?1 AND dirty = 1",
                params![&summary.summary_id.0],
            )? as u64;
        }
        Ok(cleared)
    }

    pub fn record_sources_synced(
        &self,
        sink: &str,
        target: &str,
        sources: &[SourceLocation],
    ) -> Result<()> {
        if sources.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_sources_synced_in_transaction(sink, target, sources)
        })
    }

    fn record_sources_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        sources: &[SourceLocation],
    ) -> Result<()> {
        for source in sources {
            let payload = serde_json::to_string(source)?;
            self.record_entity_synced(
                sink,
                target,
                "source",
                &source.source_id.0,
                &hash_text(&payload),
            )?;
        }
        Ok(())
    }

    pub fn record_accounts_synced(
        &self,
        sink: &str,
        target: &str,
        accounts: &[ProviderAccount],
    ) -> Result<()> {
        if accounts.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_accounts_synced_in_transaction(sink, target, accounts)
        })
    }

    fn record_accounts_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        accounts: &[ProviderAccount],
    ) -> Result<()> {
        for account in accounts {
            let payload = serde_json::to_string(account)?;
            self.record_entity_synced(
                sink,
                target,
                "account",
                &account.provider_account_id.0,
                &hash_text(&payload),
            )?;
        }
        Ok(())
    }

    pub fn record_source_account_assignments_synced(
        &self,
        sink: &str,
        target: &str,
        assignments: &[SourceAccountAssignment],
    ) -> Result<()> {
        if assignments.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_source_account_assignments_synced_in_transaction(sink, target, assignments)
        })
    }

    fn record_source_account_assignments_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        assignments: &[SourceAccountAssignment],
    ) -> Result<()> {
        for assignment in assignments {
            let payload = serde_json::to_string(assignment)?;
            self.record_entity_synced(
                sink,
                target,
                "source_account_assignment",
                &assignment.assignment_id.0,
                &hash_text(&payload),
            )?;
        }
        Ok(())
    }

    pub fn record_subscriptions_synced(
        &self,
        sink: &str,
        target: &str,
        subscriptions: &[Subscription],
    ) -> Result<()> {
        if subscriptions.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_subscriptions_synced_in_transaction(sink, target, subscriptions)
        })
    }

    fn record_subscriptions_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        subscriptions: &[Subscription],
    ) -> Result<()> {
        for subscription in subscriptions {
            let payload = serde_json::to_string(subscription)?;
            self.record_entity_synced(
                sink,
                target,
                "subscription",
                &subscription.subscription_id.0,
                &hash_text(&payload),
            )?;
        }
        Ok(())
    }

    pub fn record_summaries_synced(
        &self,
        sink: &str,
        target: &str,
        summaries: &[UsageSummary],
    ) -> Result<()> {
        if summaries.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_summaries_synced_in_transaction(sink, target, summaries)
        })
    }

    fn record_summaries_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        summaries: &[UsageSummary],
    ) -> Result<()> {
        for summary in summaries {
            let payload_hash = summary_sync_payload_hash(summary)?;
            self.record_entity_synced(
                sink,
                target,
                "summary",
                &summary.summary_id.0,
                &payload_hash,
            )?;
        }
        Ok(())
    }

    pub fn compute_daily_rollup(&self, date: &str, device_id: &str) -> Result<DailyRollup> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT payload FROM usage_events
            WHERE started_at >= ?1 AND started_at < ?2
            "#,
        )?;
        let end_date = {
            let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
            (parsed + chrono::Duration::days(1))
                .format("%Y-%m-%d")
                .to_string()
        };
        let rows = stmt.query_map(params![date, &end_date], |row| row.get::<_, String>(0))?;

        let mut total_input = 0u64;
        let mut total_cache_create = 0u64;
        let mut total_cache_read = 0u64;
        let mut total_output = 0u64;
        let mut total_reasoning = 0u64;
        let mut total_tokens = 0u64;
        let mut total_events = 0u64;
        let mut sessions = std::collections::BTreeSet::new();
        let mut estimated_cost = None::<i64>; // cents USD
        let mut by_provider: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();
        let mut by_account: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();

        for row in rows {
            let event: UsageEvent = serde_json::from_str(&row?)?;
            total_input += event.usage.input_tokens.unwrap_or(0);
            total_cache_create += event.usage.cache_creation_tokens.unwrap_or(0);
            total_cache_read += event.usage.cache_read_tokens.unwrap_or(0);
            total_output += event.usage.output_tokens.unwrap_or(0);
            total_reasoning += event.usage.reasoning_tokens.unwrap_or(0);
            total_tokens += event.usage.computed_total();
            total_events += 1;
            sessions.insert(event.session.session_id.clone());

            if let Some(cost) = event.cost.estimated_api_equivalent_usd {
                estimated_cost = Some(estimated_cost.unwrap_or(0) + cost);
            }

            let provider_entry = by_provider
                .entry(event.provider.clone())
                .or_insert_with(|| serde_json::json!({"tokens": 0, "events": 0}));
            provider_entry["tokens"] = serde_json::json!(
                provider_entry["tokens"].as_u64().unwrap_or(0) + event.usage.computed_total()
            );
            provider_entry["events"] =
                serde_json::json!(provider_entry["events"].as_u64().unwrap_or(0) + 1);

            let account_key = event
                .provider_account_id
                .as_ref()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| "unassigned".to_string());
            let account_entry = by_account.entry(account_key).or_insert_with(
                || serde_json::json!({"tokens": 0, "events": 0, "provider": event.provider}),
            );
            account_entry["tokens"] = serde_json::json!(
                account_entry["tokens"].as_u64().unwrap_or(0) + event.usage.computed_total()
            );
            account_entry["events"] =
                serde_json::json!(account_entry["events"].as_u64().unwrap_or(0) + 1);
        }

        Ok(DailyRollup {
            schema_version: statsai_core::DAILY_ROLLUP_SCHEMA_VERSION.to_string(),
            date: date.to_string(),
            device_id: device_id.to_string(),
            total_input_tokens: total_input,
            total_cache_creation_tokens: total_cache_create,
            total_cache_read_tokens: total_cache_read,
            total_output_tokens: total_output,
            total_reasoning_tokens: total_reasoning,
            total_tokens,
            total_events,
            total_sessions: sessions.len() as u64,
            estimated_cost_usd: estimated_cost,
            by_provider: Some(serde_json::to_string(&by_provider)?),
            by_account: Some(serde_json::to_string(&by_account)?),
            updated_at: chrono::Utc::now(),
        })
    }

    pub fn upsert_daily_rollup(&self, rollup: &DailyRollup) -> Result<()> {
        let payload = serde_json::to_string(rollup)?;
        self.conn.execute(
            r#"
            INSERT INTO daily_rollups (
              date, device_id, total_tokens, total_events, total_sessions,
              estimated_cost_usd, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(date, device_id) DO UPDATE SET
              total_tokens = excluded.total_tokens,
              total_events = excluded.total_events,
              total_sessions = excluded.total_sessions,
              estimated_cost_usd = excluded.estimated_cost_usd,
              payload = excluded.payload
            "#,
            params![
                &rollup.date,
                &rollup.device_id,
                safe_u64_to_i64(rollup.total_tokens),
                safe_u64_to_i64(rollup.total_events),
                safe_u64_to_i64(rollup.total_sessions),
                rollup.estimated_cost_usd,
                &payload,
            ],
        )?;
        Ok(())
    }

    pub fn daily_rollups_between(
        &self,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<DailyRollup>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload FROM daily_rollups WHERE date >= ?1 AND date <= ?2 ORDER BY date",
        )?;
        let rows = stmt.query_map(params![start_date, end_date], |row| row.get::<_, String>(0))?;
        let mut rollups = Vec::new();
        for row in rows {
            rollups.push(serde_json::from_str(&row?)?);
        }
        Ok(rollups)
    }

    pub fn delete_rollups_for_device(&self, device_id: &str) -> Result<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM daily_rollups WHERE device_id = ?1",
            params![device_id],
        )? as u64;
        Ok(deleted)
    }

    fn event_by_id(&self, event_id: &str) -> Result<Option<UsageEvent>> {
        self.conn
            .query_row(
                "SELECT payload FROM usage_events WHERE event_id = ?1",
                params![event_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }

    fn sync_rollup_summaries_by_sql(&self, sql: &str) -> Result<Vec<UsageSummary>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(serde_json::from_str(&row?)?);
        }
        Ok(summaries)
    }

    fn refresh_sync_rollups_for_keys(&self, keys: &BTreeSet<SyncRollupBucketKey>) -> Result<()> {
        for key in keys {
            self.refresh_sync_rollup_for_key(key)?;
        }
        Ok(())
    }

    fn refresh_sync_rollup_for_key(&self, key: &SyncRollupBucketKey) -> Result<()> {
        let events = self.sync_rollup_events(key)?;
        if events.is_empty() {
            self.conn.execute(
                "DELETE FROM sync_rollups WHERE summary_id = ?1",
                params![sync_rollup_summary_id(key).0],
            )?;
            return Ok(());
        }

        let summary = build_sync_rollup_summary(&events);
        let payload = serde_json::to_string(&summary)?;
        let payload_hash = summary_sync_payload_hash(&summary)?;
        let existing: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT payload_hash, dirty FROM sync_rollups WHERE summary_id = ?1",
                params![&summary.summary_id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if existing
            .as_ref()
            .is_some_and(|(existing_hash, _)| existing_hash == &payload_hash)
        {
            return Ok(());
        }

        let dirty = existing.as_ref().map_or(1, |(_, dirty)| (*dirty).max(1));
        self.conn.execute(
            r#"
            INSERT INTO sync_rollups (
              summary_id, provider, source_id, provider_account_id, day_key,
              observed_at, updated_at, payload_hash, dirty, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(summary_id) DO UPDATE SET
              provider = excluded.provider,
              source_id = excluded.source_id,
              provider_account_id = excluded.provider_account_id,
              day_key = excluded.day_key,
              observed_at = excluded.observed_at,
              updated_at = excluded.updated_at,
              payload_hash = excluded.payload_hash,
              dirty = excluded.dirty,
              payload = excluded.payload
            "#,
            params![
                &summary.summary_id.0,
                &summary.provider,
                &summary.source_id.0,
                summary.provider_account_id.as_ref().map(|id| id.0.as_str()),
                &key.day_key,
                summary.observed_at.to_rfc3339(),
                Utc::now().to_rfc3339(),
                &payload_hash,
                dirty,
                &payload,
            ],
        )?;
        Ok(())
    }

    fn sync_rollup_events(&self, key: &SyncRollupBucketKey) -> Result<Vec<UsageEvent>> {
        let start = format!("{}T00:00:00+00:00", key.day_key);
        let end = {
            let day = NaiveDate::parse_from_str(&key.day_key, "%Y-%m-%d")?;
            format!(
                "{}T00:00:00+00:00",
                (day + chrono::Duration::days(1)).format("%Y-%m-%d")
            )
        };
        let sql = if key.provider_account_id.is_some() {
            r#"
            SELECT payload FROM usage_events
            WHERE provider = ?1
              AND source_id = ?2
              AND provider_account_id = ?3
              AND started_at >= ?4
              AND started_at < ?5
            ORDER BY started_at, event_id
            "#
        } else {
            r#"
            SELECT payload FROM usage_events
            WHERE provider = ?1
              AND source_id = ?2
              AND provider_account_id IS NULL
              AND started_at >= ?3
              AND started_at < ?4
            ORDER BY started_at, event_id
            "#
        };

        let mut stmt = self.conn.prepare(sql)?;
        let mut events: Vec<UsageEvent> = Vec::new();
        if let Some(provider_account_id) = key.provider_account_id.as_deref() {
            let rows = stmt.query_map(
                params![
                    &key.provider,
                    &key.source_id,
                    provider_account_id,
                    &start,
                    &end
                ],
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                events.push(serde_json::from_str(&row?)?);
            }
        } else {
            let rows = stmt.query_map(
                params![&key.provider, &key.source_id, &start, &end],
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                events.push(serde_json::from_str(&row?)?);
            }
        }
        events.retain(|event| sync_rollup_project_key(event.project.as_ref()) == key.project_key);
        Ok(events)
    }

    fn delete_sync_rollups_for_sources_in_tx(&self, source_ids: &[SourceId]) -> Result<u64> {
        let mut deleted = 0u64;
        for source_id in source_ids {
            deleted += self.conn.execute(
                "DELETE FROM sync_rollups WHERE source_id = ?1",
                params![&source_id.0],
            )? as u64;
        }
        Ok(deleted)
    }

    fn entity_requires_sync(
        &self,
        sink: &str,
        target: &str,
        entity_kind: &str,
        entity_id: &str,
        payload_hash: &str,
    ) -> Result<bool> {
        let existing: Option<String> = self
            .conn
            .query_row(
                r#"
                SELECT payload_hash
                FROM entity_sync_state
                WHERE sink = ?1 AND target = ?2 AND entity_kind = ?3 AND entity_id = ?4
                "#,
                params![sink, target, entity_kind, entity_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(existing.as_deref() != Some(payload_hash))
    }

    fn record_entity_synced(
        &self,
        sink: &str,
        target: &str,
        entity_kind: &str,
        entity_id: &str,
        payload_hash: &str,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO entity_sync_state (
              sink, target, entity_kind, entity_id, payload_hash, synced_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(sink, target, entity_kind, entity_id) DO UPDATE SET
              payload_hash = excluded.payload_hash,
              synced_at = excluded.synced_at
            "#,
            params![
                sink,
                target,
                entity_kind,
                entity_id,
                payload_hash,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn checkpoint_wal(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }
}

pub fn verified_source_state_hash(
    verified_state: Option<&VerifiedSourceState>,
) -> Result<Option<String>> {
    verified_state
        .map(|verified_state| serde_json::to_string(verified_state).map(|json| hash_text(&json)))
        .transpose()
        .map_err(Into::into)
}

pub fn find_existing_provider_account(
    store: &Store,
    provider: &str,
    provider_user_id: Option<&str>,
    email: Option<&str>,
) -> Result<Option<ProviderAccount>> {
    let normalized_provider_user_id = provider_user_id
        .map(normalize_provider_user_id)
        .filter(|provider_user_id| !provider_user_id.is_empty());
    let normalized_email = email.map(normalize_email).filter(|email| !email.is_empty());
    let accounts = store.list_accounts()?;
    let mut matches: Vec<(&'static str, ProviderAccount)> = Vec::new();

    if let Some(email) = normalized_email.as_deref() {
        if let Some(account) = accounts.iter().find(|account| {
            account.provider == provider
                && account.email.as_deref().map(normalize_email).as_deref() == Some(email)
        }) {
            matches.push(("email", account.clone()));
        }
    }
    if let Some(provider_user_id) = normalized_provider_user_id.as_deref() {
        if let Some(account) = accounts.iter().find(|account| {
            account.provider == provider
                && account
                    .provider_user_id
                    .as_deref()
                    .map(normalize_provider_user_id)
                    .as_deref()
                    == Some(provider_user_id)
        }) {
            matches.push(("provider_user_id", account.clone()));
        }
    }
    if let Some(provider_account_id) = provider_account_id_from_identity(
        provider,
        normalized_provider_user_id.as_deref(),
        normalized_email.as_deref(),
    ) {
        if let Some(account) = store.account(&provider_account_id)? {
            matches.push(("provider_account_id", account));
        }
    }

    let mut unique_matches: Vec<(&'static str, ProviderAccount)> = Vec::new();
    for (match_kind, account) in matches {
        if !unique_matches
            .iter()
            .any(|(_, existing)| existing.provider_account_id == account.provider_account_id)
        {
            unique_matches.push((match_kind, account));
        }
    }

    if unique_matches.len() > 1 {
        let details = unique_matches
            .iter()
            .map(|(match_kind, account)| {
                format!("{match_kind} matched {}", account.provider_account_id.0)
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!("conflicting provider account identifiers for {provider}: {details}");
    }

    Ok(unique_matches
        .into_iter()
        .next()
        .map(|(_, account)| account))
}

#[derive(Debug, Clone)]
pub struct UpsertProviderAccountInput<'a> {
    pub provider: &'a str,
    pub provider_user_id: Option<&'a str>,
    pub email: Option<&'a str>,
    pub label: Option<String>,
    pub plan_name: Option<String>,
    pub identity_source: Option<IdentitySource>,
    pub verified_at: Option<DateTime<Utc>>,
}

pub fn upsert_provider_account(
    store: &Store,
    input: UpsertProviderAccountInput<'_>,
) -> Result<ProviderAccount> {
    let UpsertProviderAccountInput {
        provider,
        provider_user_id,
        email,
        label,
        plan_name,
        identity_source,
        verified_at,
    } = input;
    let normalized_provider_user_id = provider_user_id
        .map(normalize_provider_user_id)
        .filter(|provider_user_id| !provider_user_id.is_empty());
    let normalized_email = email.map(normalize_email).filter(|email| !email.is_empty());
    let existing = find_existing_provider_account(
        store,
        provider,
        normalized_provider_user_id.as_deref(),
        normalized_email.as_deref(),
    )?;
    let provider_account_id = existing
        .as_ref()
        .map(|account| account.provider_account_id.clone())
        .or_else(|| {
            provider_account_id_from_identity(
                provider,
                normalized_provider_user_id.as_deref(),
                normalized_email.as_deref(),
            )
        })
        .with_context(|| format!("missing canonical account identity for {provider}"))?;
    let now = Utc::now();
    let mut account = existing.unwrap_or(ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: provider_account_id.clone(),
        provider: provider.to_string(),
        identity_source: identity_source
            .clone()
            .unwrap_or(IdentitySource::UserConfigured),
        provider_user_id: None,
        email: None,
        provider_user_id_hash: None,
        email_hash: None,
        org_id_hash: None,
        account_label: None,
        plan_name: None,
        confidence: Confidence::High,
        verified_at: None,
        created_at: now,
        updated_at: now,
    });
    if let Some(provider_user_id) = normalized_provider_user_id.as_deref() {
        account.provider_user_id = Some(provider_user_id.to_string());
        account.provider_user_id_hash = Some(hash_text(provider_user_id));
    }
    if let Some(email) = normalized_email.as_deref() {
        account.email_hash = Some(hash_text(email));
        account.email = Some(email.to_string());
    }
    if let Some(label) = label {
        account.account_label = Some(label);
    }
    if let Some(plan_name) = plan_name.filter(|plan_name| !plan_name.trim().is_empty()) {
        account.plan_name = Some(plan_name);
    }
    if let Some(identity_source) = identity_source {
        account.identity_source = merge_identity_source(&account.identity_source, identity_source);
    }
    account.verified_at = max_datetime(account.verified_at, verified_at);
    account.provider = provider.to_string();
    account.confidence = if account.provider_user_id.is_some() || account.email.is_some() {
        Confidence::High
    } else {
        account.confidence
    };
    account.updated_at = now;
    store.upsert_account(&account)?;
    Ok(account)
}

pub fn apply_verified_source_state(
    store: &Store,
    source: &SourceLocation,
    verified_state: Option<&VerifiedSourceState>,
) -> Result<()> {
    let Some(verified_state) = verified_state else {
        return Ok(());
    };
    let account = upsert_provider_account(
        store,
        UpsertProviderAccountInput {
            provider: &source.provider,
            provider_user_id: verified_state.provider_user_id.as_deref(),
            email: verified_state.email.as_deref(),
            label: verified_state.account_label.clone(),
            plan_name: verified_state.plan_name.clone(),
            identity_source: Some(IdentitySource::LocalAuth),
            verified_at: verified_state.verified_at,
        },
    )?;
    let assignment_started_at = verified_state.authenticated_at.or_else(|| {
        verified_state
            .subscription
            .as_ref()
            .map(|subscription| subscription.started_at)
    });
    if let Some(started_at) = assignment_started_at {
        upsert_verified_source_assignment(
            store,
            source,
            &account.provider_account_id,
            started_at,
            verified_state.verified_at,
        )?;
    }
    if let Some(subscription) = verified_state.subscription.as_ref() {
        upsert_verified_subscription(
            store,
            &source.provider,
            &account.provider_account_id,
            subscription,
        )?;
    }
    Ok(())
}

pub fn reconcile_verified_source_state(
    store: &Store,
    source: &mut SourceLocation,
    verified_state: Option<&VerifiedSourceState>,
    next_verified_state_hash: Option<String>,
) -> Result<()> {
    if !matches!(source.verification_mode, SourceVerificationMode::Auto) {
        return Ok(());
    }
    let has_legacy_verified_assignment = next_verified_state_hash.is_none()
        && source.verified_state_hash.is_none()
        && has_active_verified_source_assignment(store, &source.source_id)?;
    if source.verified_state_hash == next_verified_state_hash && !has_legacy_verified_assignment {
        return Ok(());
    }

    match verified_state {
        Some(verified_state) => apply_verified_source_state(store, source, Some(verified_state))?,
        None => close_active_verified_source_linkages(store, &source.source_id, Utc::now())?,
    }
    source.verified_state_hash = next_verified_state_hash;
    source.updated_at = Utc::now();
    Ok(())
}

pub fn has_active_verified_source_assignment(store: &Store, source_id: &SourceId) -> Result<bool> {
    Ok(store
        .list_source_account_assignments_for_source(source_id)?
        .into_iter()
        .any(|assignment| {
            assignment.ended_at.is_none()
                && matches!(
                    assignment.record_source,
                    IdentitySource::LocalAuth
                        | IdentitySource::ProviderAuth
                        | IdentitySource::ProviderApi
                        | IdentitySource::CookieOauth
                        | IdentitySource::CliProbe
                )
        }))
}

pub fn effective_verified_source_state_is_missing(
    verified_state: &Option<VerifiedSourceState>,
) -> bool {
    verified_state.is_none()
}

pub fn apply_source_account_resolution(
    store: &Store,
    source: &SourceLocation,
    events: &mut [UsageEvent],
    summaries: &mut [UsageSummary],
) -> Result<()> {
    let assignments = store.list_source_account_assignments_for_source(&source.source_id)?;
    for event in events {
        apply_account_resolution_to_event(&assignments, event);
    }
    for summary in summaries {
        apply_account_resolution_to_summary(&assignments, summary);
    }
    Ok(())
}

fn upsert_verified_source_assignment(
    store: &Store,
    source: &SourceLocation,
    provider_account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    verified_at: Option<DateTime<Utc>>,
) -> Result<()> {
    let assignments = store.list_source_account_assignments_for_source(&source.source_id)?;
    let overlaps: Vec<_> = assignments
        .iter()
        .filter(|assignment| {
            periods_overlap(started_at, None, assignment.started_at, assignment.ended_at)
        })
        .cloned()
        .collect();

    if let Some(existing) = overlaps
        .iter()
        .find(|assignment| assignment.provider_account_id == *provider_account_id)
    {
        let merged_started_at = existing.started_at.min(started_at);
        let merged = SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: source_account_assignment_id(
                &source.source_id,
                provider_account_id,
                merged_started_at,
            ),
            source_id: source.source_id.clone(),
            provider: source.provider.clone(),
            provider_account_id: provider_account_id.clone(),
            started_at: merged_started_at,
            ended_at: None,
            record_source: merge_identity_source(
                &existing.record_source,
                IdentitySource::LocalAuth,
            ),
            verified_at: max_datetime(existing.verified_at, verified_at),
            created_at: existing.created_at,
            updated_at: Utc::now(),
        };
        if merged.assignment_id != existing.assignment_id {
            store.delete_source_account_assignment(&existing.assignment_id)?;
        }
        store.upsert_source_account_assignment(&merged)?;
        reattribute_source_records(store, &source.source_id)?;
        return Ok(());
    }

    if overlaps
        .iter()
        .any(|assignment| matches!(assignment.record_source, IdentitySource::UserConfigured))
    {
        return Ok(());
    }

    for existing in overlaps
        .iter()
        .filter(|assignment| assignment.provider_account_id != *provider_account_id)
    {
        if existing.started_at <= started_at {
            let mut closed = existing.clone();
            closed.ended_at = Some(started_at);
            closed.updated_at = Utc::now();
            store.upsert_source_account_assignment(&closed)?;
        } else {
            store.delete_source_account_assignment(&existing.assignment_id)?;
        }
    }

    let now = Utc::now();
    let assignment = SourceAccountAssignment {
        schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
        assignment_id: source_account_assignment_id(
            &source.source_id,
            provider_account_id,
            started_at,
        ),
        source_id: source.source_id.clone(),
        provider: source.provider.clone(),
        provider_account_id: provider_account_id.clone(),
        started_at,
        ended_at: None,
        record_source: IdentitySource::LocalAuth,
        verified_at,
        created_at: now,
        updated_at: now,
    };
    validate_source_assignment_overlap(
        store,
        &source.source_id,
        provider_account_id,
        assignment.started_at,
        assignment.ended_at,
        None,
    )?;
    store.upsert_source_account_assignment(&assignment)?;
    reattribute_source_records(store, &source.source_id)?;
    Ok(())
}

pub fn close_active_verified_source_linkages(
    store: &Store,
    source_id: &SourceId,
    ended_at: DateTime<Utc>,
) -> Result<()> {
    let mut changed = false;
    let mut closed_account_ids = Vec::new();
    for mut assignment in store.list_source_account_assignments_for_source(source_id)? {
        if assignment.ended_at.is_some()
            || !matches!(
                assignment.record_source,
                IdentitySource::LocalAuth
                    | IdentitySource::ProviderAuth
                    | IdentitySource::ProviderApi
                    | IdentitySource::CookieOauth
                    | IdentitySource::CliProbe
            )
            || !timestamp_in_period(ended_at, assignment.started_at, assignment.ended_at)
        {
            continue;
        }
        validate_time_window(assignment.started_at, Some(ended_at), "source connection")?;
        assignment.ended_at = Some(ended_at);
        assignment.updated_at = Utc::now();
        store.upsert_source_account_assignment(&assignment)?;
        closed_account_ids.push(assignment.provider_account_id.clone());
        changed = true;
    }
    for mut subscription in store.list_subscriptions()? {
        if !closed_account_ids.contains(&subscription.provider_account_id)
            || subscription.ended_at.is_some()
            || !is_verified_subscription_source(&subscription.record_source)
            || !timestamp_in_period(ended_at, subscription.started_at, subscription.ended_at)
        {
            continue;
        }
        validate_time_window(subscription.started_at, Some(ended_at), "subscription")?;
        subscription.ended_at = Some(ended_at);
        store.upsert_subscription(&subscription)?;
        changed = true;
    }
    if changed {
        reattribute_source_records(store, source_id)?;
    }
    Ok(())
}

fn upsert_verified_subscription(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    verified: &VerifiedSubscriptionState,
) -> Result<()> {
    validate_time_window(verified.started_at, None, "subscription")?;
    let subscriptions: Vec<_> = store
        .list_subscriptions()?
        .into_iter()
        .filter(|subscription| {
            subscription.provider == provider
                && subscription.provider_account_id == *provider_account_id
        })
        .collect();

    if let Some(existing) = subscriptions.iter().find(|subscription| {
        subscription
            .plan_name
            .eq_ignore_ascii_case(&verified.plan_name)
            && periods_overlap(
                verified.started_at,
                None,
                subscription.started_at,
                subscription.ended_at,
            )
    }) {
        let merged = merge_verified_subscription(existing, verified);
        store.upsert_subscription(&merged)?;
        return Ok(());
    }

    if subscriptions.iter().any(|subscription| {
        subscription.record_source == IdentitySource::UserConfigured
            && periods_overlap(
                verified.started_at,
                None,
                subscription.started_at,
                subscription.ended_at,
            )
            && !subscription
                .plan_name
                .eq_ignore_ascii_case(&verified.plan_name)
    }) {
        return Ok(());
    }

    for mut subscription in subscriptions
        .iter()
        .filter(|subscription| {
            is_verified_subscription_source(&subscription.record_source)
                && periods_overlap(
                    verified.started_at,
                    None,
                    subscription.started_at,
                    subscription.ended_at,
                )
                && !subscription
                    .plan_name
                    .eq_ignore_ascii_case(&verified.plan_name)
        })
        .cloned()
    {
        if subscription.started_at < verified.started_at {
            subscription.ended_at = Some(verified.started_at);
            store.upsert_subscription(&subscription)?;
        } else {
            store.delete_subscription(&subscription.subscription_id)?;
        }
    }

    let current_subscriptions: Vec<_> = store
        .list_subscriptions()?
        .into_iter()
        .filter(|subscription| {
            subscription.provider == provider
                && subscription.provider_account_id == *provider_account_id
        })
        .collect();

    if current_subscriptions.iter().any(|subscription| {
        periods_overlap(
            verified.started_at,
            None,
            subscription.started_at,
            subscription.ended_at,
        )
    }) {
        return Ok(());
    }

    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id(
            provider,
            provider_account_id,
            &verified.plan_name,
            verified.started_at,
        ),
        provider: provider.to_string(),
        provider_account_id: provider_account_id.clone(),
        plan_name: verified.plan_name.clone(),
        price: verified.price,
        currency: verified.currency.clone(),
        billing_period: verified.billing_period.clone(),
        paid_at: verified.paid_at.or(Some(verified.started_at)),
        renewal_day: verified
            .current_period_ends_at
            .and_then(subscription_renewal_day),
        started_at: verified.started_at,
        ended_at: None,
        current_period_ends_at: verified.current_period_ends_at,
        status: verified.status.clone(),
        record_source: IdentitySource::LocalAuth,
        verified_at: verified.verified_at,
        notes: None,
    };
    validate_subscription_overlap(
        store,
        provider,
        provider_account_id,
        subscription.started_at,
        subscription.ended_at,
        None,
    )?;
    store.upsert_subscription(&subscription)?;
    Ok(())
}

fn merge_verified_subscription(
    existing: &Subscription,
    verified: &VerifiedSubscriptionState,
) -> Subscription {
    let mut merged = existing.clone();
    if merged.price <= 0 {
        merged.price = verified.price;
    }
    if merged.currency.trim().is_empty() {
        merged.currency = verified.currency.clone();
    }
    merged.billing_period = verified.billing_period.clone();
    merged.paid_at = max_datetime(
        merged.paid_at,
        verified.paid_at.or(Some(verified.started_at)),
    );
    merged.renewal_day = verified
        .current_period_ends_at
        .and_then(subscription_renewal_day)
        .or(merged.renewal_day);
    merged.current_period_ends_at = max_datetime(
        merged.current_period_ends_at,
        verified.current_period_ends_at,
    );
    merged.status = verified.status.clone();
    merged.record_source = merge_identity_source(&merged.record_source, IdentitySource::LocalAuth);
    merged.verified_at = max_datetime(merged.verified_at, verified.verified_at);
    merged
}

fn merge_identity_source(existing: &IdentitySource, incoming: IdentitySource) -> IdentitySource {
    if identity_source_rank(&incoming) >= identity_source_rank(existing) {
        incoming
    } else {
        existing.clone()
    }
}

fn identity_source_rank(source: &IdentitySource) -> u8 {
    match source {
        IdentitySource::UserConfigured => 100,
        IdentitySource::ProviderApi => 90,
        IdentitySource::ProviderAuth => 80,
        IdentitySource::LocalAuth => 70,
        IdentitySource::CliProbe => 60,
        IdentitySource::CookieOauth => 50,
        IdentitySource::SourceConfig => 40,
        IdentitySource::ManualHint => 30,
        IdentitySource::Unresolved => 10,
        IdentitySource::Unknown => 0,
    }
}

fn is_verified_subscription_source(source: &IdentitySource) -> bool {
    matches!(
        source,
        IdentitySource::LocalAuth
            | IdentitySource::ProviderAuth
            | IdentitySource::ProviderApi
            | IdentitySource::CookieOauth
            | IdentitySource::CliProbe
    )
}

fn max_datetime(
    left: Option<DateTime<Utc>>,
    right: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn subscription_renewal_day(timestamp: DateTime<Utc>) -> Option<u8> {
    u8::try_from(timestamp.day()).ok()
}

fn validate_time_window(
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    label: &str,
) -> Result<()> {
    if ended_at.is_some_and(|ended_at| ended_at <= started_at) {
        bail!("{label} ended_at must be after started_at");
    }
    Ok(())
}

fn validate_source_assignment_overlap(
    store: &Store,
    source_id: &SourceId,
    _provider_account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    ignore_assignment_id: Option<&SourceAccountAssignmentId>,
) -> Result<()> {
    for assignment in store.list_source_account_assignments_for_source(source_id)? {
        if ignore_assignment_id == Some(&assignment.assignment_id) {
            continue;
        }
        if periods_overlap(
            started_at,
            ended_at,
            assignment.started_at,
            assignment.ended_at,
        ) {
            bail!(
                "source connection overlaps an existing connection for source {}",
                source_id.0
            );
        }
    }
    Ok(())
}

fn validate_subscription_overlap(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    ignore_subscription_id: Option<&SubscriptionId>,
) -> Result<()> {
    for subscription in store.list_subscriptions()? {
        if ignore_subscription_id == Some(&subscription.subscription_id) {
            continue;
        }
        if subscription.provider != provider {
            continue;
        }
        if &subscription.provider_account_id != provider_account_id {
            continue;
        }
        if periods_overlap(
            started_at,
            ended_at,
            subscription.started_at,
            subscription.ended_at,
        ) {
            bail!(
                "subscription overlaps existing subscription {} for account {}",
                subscription.subscription_id.0,
                provider_account_id.0
            );
        }
    }
    Ok(())
}

fn reattribute_source_records(store: &Store, source_id: &SourceId) -> Result<()> {
    if store.source(source_id)?.is_none() {
        return Ok(());
    }
    let assignments = store.list_source_account_assignments_for_source(source_id)?;
    let mut events = store.events_for_source(source_id)?;
    let mut summaries = store.summaries_for_source(source_id)?;
    for event in &mut events {
        apply_account_resolution_to_event(&assignments, event);
    }
    for summary in &mut summaries {
        apply_account_resolution_to_summary(&assignments, summary);
    }
    store.rewrite_events(&events)?;
    store.rewrite_summaries(&summaries)?;
    Ok(())
}

fn apply_account_resolution_to_event(
    assignments: &[SourceAccountAssignment],
    event: &mut UsageEvent,
) {
    if keep_detected_account_identity(
        event.provider_account_id.as_ref(),
        event
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        return;
    }
    let assignment = assignment_for_timestamp(assignments, event.session.started_at);
    if let Some(assignment) = assignment {
        event.provider_account_id = Some(assignment.provider_account_id.clone());
        if let Some(evidence) = event.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::SourceConfig;
        }
    } else if should_clear_resolved_account(
        event.provider_account_id.as_ref(),
        event
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        event.provider_account_id = None;
        if let Some(evidence) = event.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::Unresolved;
        }
    }
}

fn apply_account_resolution_to_summary(
    assignments: &[SourceAccountAssignment],
    summary: &mut UsageSummary,
) {
    if keep_detected_account_identity(
        summary.provider_account_id.as_ref(),
        summary
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        return;
    }
    let timestamp = summary.period_start.unwrap_or(summary.observed_at);
    let assignment = assignment_for_timestamp(assignments, timestamp);
    if let Some(assignment) = assignment {
        summary.provider_account_id = Some(assignment.provider_account_id.clone());
        if let Some(evidence) = summary.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::SourceConfig;
        }
    } else if should_clear_resolved_account(
        summary.provider_account_id.as_ref(),
        summary
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        summary.provider_account_id = None;
        if let Some(evidence) = summary.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::Unresolved;
        }
    }
}

fn keep_detected_account_identity(
    provider_account_id: Option<&ProviderAccountId>,
    identity_source: Option<&IdentitySource>,
) -> bool {
    let Some(provider_account_id) = provider_account_id else {
        return false;
    };
    if provider_account_id.0.trim().is_empty() {
        return false;
    }
    let Some(identity_source) = identity_source else {
        return false;
    };
    !matches!(
        identity_source,
        IdentitySource::SourceConfig
            | IdentitySource::UserConfigured
            | IdentitySource::ManualHint
            | IdentitySource::Unknown
            | IdentitySource::Unresolved
    )
}

fn should_clear_resolved_account(
    provider_account_id: Option<&ProviderAccountId>,
    identity_source: Option<&IdentitySource>,
) -> bool {
    let Some(provider_account_id) = provider_account_id else {
        return false;
    };
    if provider_account_id.0.trim().is_empty() {
        return false;
    }
    matches!(
        identity_source,
        None | Some(
            IdentitySource::SourceConfig
                | IdentitySource::UserConfigured
                | IdentitySource::ManualHint
                | IdentitySource::Unknown
                | IdentitySource::Unresolved
        )
    )
}

fn assignment_for_timestamp(
    assignments: &[SourceAccountAssignment],
    timestamp: DateTime<Utc>,
) -> Option<&SourceAccountAssignment> {
    assignments
        .iter()
        .filter(|assignment| {
            timestamp_in_period(timestamp, assignment.started_at, assignment.ended_at)
        })
        .max_by(|left, right| left.started_at.cmp(&right.started_at))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SyncRollupBucketKey {
    provider: String,
    source_id: String,
    provider_account_id: Option<String>,
    day_key: String,
    project_key: String,
}

#[derive(Debug)]
struct EventInsertOutcome {
    inserted: bool,
    canonical_event_id: EventId,
    dirty_keys: BTreeSet<SyncRollupBucketKey>,
}

fn sync_rollup_bucket_key(event: &UsageEvent) -> SyncRollupBucketKey {
    SyncRollupBucketKey {
        provider: event.provider.clone(),
        source_id: event.source_id.0.clone(),
        provider_account_id: event.provider_account_id.as_ref().map(|id| id.0.clone()),
        day_key: event.session.started_at.date_naive().to_string(),
        project_key: sync_rollup_project_key(event.project.as_ref()),
    }
}

fn sync_rollup_summary_id(key: &SyncRollupBucketKey) -> SummaryId {
    summary_id(
        &key.provider,
        &SourceId(key.source_id.clone()),
        &format!(
            "daily_stats:{}:{}:{}",
            key.day_key,
            key.provider_account_id.as_deref().unwrap_or("unlinked"),
            hash_text(&key.project_key),
        ),
    )
}

fn sync_rollup_project_key(project: Option<&statsai_core::ProjectInfo>) -> String {
    project_bucket_key(project)
}

fn event_with_valid_project(event: &UsageEvent) -> UsageEvent {
    let mut event = event.clone();
    if event
        .project
        .as_ref()
        .is_some_and(|project| !project_has_stable_identity(project))
    {
        event.project = None;
    }
    event
}

fn build_sync_rollup_summary(events: &[UsageEvent]) -> UsageSummary {
    let first = events.first().expect("rollup bucket must contain events");
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_cache_creation = 0u64;
    let mut total_cache_read = 0u64;
    let mut total_reasoning = 0u64;
    let mut total_tokens = 0u64;
    let mut total_events = 0u64;
    let mut estimated_cost_usd = 0i64; // cents
    let mut provider_reported_usd = 0i64; // cents
    let mut has_provider_reported_usd = false;
    let mut observed_at = first.created_at;
    let mut model_buckets: BTreeMap<String, (ModelInfo, SyncRollupModelTotals)> = BTreeMap::new();
    let mut session_ids = BTreeSet::new();
    let mut active_seconds = 0.0_f64;
    let mut latency_values = Vec::new();
    let mut ttft_values = Vec::new();
    let mut generated_tps_values = Vec::new();
    let mut visible_tps_values = Vec::new();
    let mut cache_hit_ratio_values = Vec::new();
    let mut reasoning_share_values = Vec::new();
    let mut total_messages = 0u64;
    let mut user_messages = 0u64;
    let mut assistant_messages = 0u64;
    let mut developer_messages = 0u64;
    let mut tracked_requests = 0u64;
    let mut tracked_output_tokens = 0u64;
    let mut tracked_reasoning_tokens = 0u64;

    for event in events {
        total_input = total_input.saturating_add(event.usage.input_tokens.unwrap_or(0));
        total_output = total_output.saturating_add(event.usage.output_tokens.unwrap_or(0));
        total_cache_creation =
            total_cache_creation.saturating_add(event.usage.cache_creation_tokens.unwrap_or(0));
        total_cache_read =
            total_cache_read.saturating_add(event.usage.cache_read_tokens.unwrap_or(0));
        total_reasoning = total_reasoning.saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
        total_tokens = total_tokens.saturating_add(event.usage.computed_total());
        total_events = total_events.saturating_add(1);
        estimated_cost_usd += event.cost.estimated_api_equivalent_usd.unwrap_or(0);
        if let Some(cost) = event.cost.provider_reported_usd {
            provider_reported_usd += cost;
            has_provider_reported_usd = true;
        }
        if event.created_at > observed_at {
            observed_at = event.created_at;
        }
        session_ids.insert(
            event
                .session
                .local_session_id_hash
                .clone()
                .unwrap_or_else(|| event.session.session_id.clone()),
        );
        let is_tracked_turn = event
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.latency_ms)
            .is_some();
        if let Some(runtime) = event.runtime.as_ref() {
            let derived_total_messages = runtime.total_messages.or_else(|| {
                let derived = runtime.user_messages.unwrap_or(0)
                    + runtime.assistant_messages.unwrap_or(0)
                    + runtime.developer_messages.unwrap_or(0);
                (derived > 0).then_some(derived)
            });
            total_messages = total_messages.saturating_add(derived_total_messages.unwrap_or(0));
            user_messages = user_messages.saturating_add(runtime.user_messages.unwrap_or(0));
            assistant_messages =
                assistant_messages.saturating_add(runtime.assistant_messages.unwrap_or(0));
            developer_messages =
                developer_messages.saturating_add(runtime.developer_messages.unwrap_or(0));

            if let Some(latency_ms) = runtime.latency_ms {
                let latency_ms_f64 = latency_ms as f64;
                active_seconds += latency_ms_f64 / 1000.0;

                if runtime_latency_supports_distribution_metrics(runtime) {
                    latency_values.push(latency_ms_f64);
                }

                if latency_ms > 0 && runtime_latency_supports_distribution_metrics(runtime) {
                    let duration_seconds = latency_ms_f64 / 1000.0;
                    let generated_tokens = event.usage.output_tokens.unwrap_or(0)
                        + event.usage.reasoning_tokens.unwrap_or(0);
                    generated_tps_values.push(generated_tokens as f64 / duration_seconds);
                    visible_tps_values
                        .push(event.usage.output_tokens.unwrap_or(0) as f64 / duration_seconds);
                }
            }

            if let Some(ttft_ms) = runtime.time_to_first_token_ms {
                ttft_values.push(ttft_ms as f64);
            }
        }
        if is_tracked_turn {
            tracked_requests = tracked_requests.saturating_add(1);
            tracked_output_tokens =
                tracked_output_tokens.saturating_add(event.usage.output_tokens.unwrap_or(0));
            tracked_reasoning_tokens =
                tracked_reasoning_tokens.saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
        }

        let prompt_tokens = event
            .usage
            .input_tokens
            .unwrap_or(0)
            .saturating_add(event.usage.cache_read_tokens.unwrap_or(0));
        if prompt_tokens > 0 {
            cache_hit_ratio_values
                .push(event.usage.cache_read_tokens.unwrap_or(0) as f64 / prompt_tokens as f64);
        }
        let generated_tokens = event
            .usage
            .output_tokens
            .unwrap_or(0)
            .saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
        if generated_tokens > 0 {
            reasoning_share_values
                .push(event.usage.reasoning_tokens.unwrap_or(0) as f64 / generated_tokens as f64);
        }

        let model = event.model.clone().unwrap_or_default();
        let entry = model_buckets
            .entry(sync_rollup_model_key(&model))
            .or_insert_with(|| (model.clone(), SyncRollupModelTotals::default()));
        entry.1.input_tokens = entry
            .1
            .input_tokens
            .saturating_add(event.usage.input_tokens.unwrap_or(0));
        entry.1.output_tokens = entry
            .1
            .output_tokens
            .saturating_add(event.usage.output_tokens.unwrap_or(0));
        entry.1.cache_creation_tokens = entry
            .1
            .cache_creation_tokens
            .saturating_add(event.usage.cache_creation_tokens.unwrap_or(0));
        entry.1.cache_read_tokens = entry
            .1
            .cache_read_tokens
            .saturating_add(event.usage.cache_read_tokens.unwrap_or(0));
        entry.1.reasoning_tokens = entry
            .1
            .reasoning_tokens
            .saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
        entry.1.total_tokens = entry
            .1
            .total_tokens
            .saturating_add(event.usage.computed_total());
        entry.1.requests = entry.1.requests.saturating_add(1);
        entry.1.estimated_cost_usd += event.cost.estimated_api_equivalent_usd.unwrap_or(0);
        if let Some(cost) = event.cost.provider_reported_usd {
            entry.1.provider_reported_usd += cost;
            entry.1.has_provider_reported_usd = true;
        }
    }

    let day = first.session.started_at.date_naive();
    let period_start = day
        .and_hms_opt(0, 0, 0)
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
        .unwrap_or(first.session.started_at);
    let period_end = period_start + chrono::Duration::days(1);
    let bucket_key = sync_rollup_bucket_key(first);
    let models = model_buckets
        .into_values()
        .map(|(model, totals)| SummaryModelUsage {
            model,
            usage: UsageCounts {
                input_tokens: Some(totals.input_tokens),
                output_tokens: Some(totals.output_tokens),
                cache_creation_tokens: Some(totals.cache_creation_tokens),
                cache_read_tokens: Some(totals.cache_read_tokens),
                reasoning_tokens: Some(totals.reasoning_tokens),
                total_tokens: Some(totals.total_tokens),
                requests: Some(totals.requests),
                local_prompt_eval_tokens: None,
                local_eval_tokens: None,
            },
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: Some(totals.estimated_cost_usd),
                provider_reported_usd: totals
                    .has_provider_reported_usd
                    .then_some(totals.provider_reported_usd),
                pricing_source: Some("local_rollup".to_string()),
                pricing_version: None,
                confidence: Confidence::Medium,
            },
        })
        .collect();
    let summary_metrics = summary_metrics_or_none(SummaryMetrics {
        active_seconds: (active_seconds > 0.0).then_some(active_seconds),
        tracked_requests: (tracked_requests > 0).then_some(tracked_requests),
        tracked_output_tokens: (tracked_output_tokens > 0).then_some(tracked_output_tokens),
        tracked_reasoning_tokens: (tracked_reasoning_tokens > 0)
            .then_some(tracked_reasoning_tokens),
        latency_ms: finalize_metric_stats(latency_values),
        time_to_first_token_ms: finalize_metric_stats(ttft_values),
        generated_tps: finalize_metric_stats(generated_tps_values),
        visible_tps: finalize_metric_stats(visible_tps_values),
        overall_generated_tps: (active_seconds > 0.0)
            .then_some((tracked_output_tokens + tracked_reasoning_tokens) as f64 / active_seconds),
        overall_visible_tps: (active_seconds > 0.0)
            .then_some(tracked_output_tokens as f64 / active_seconds),
        cache_hit_ratio: finalize_metric_stats(cache_hit_ratio_values),
        reasoning_share: finalize_metric_stats(reasoning_share_values),
        total_messages: (total_messages > 0).then_some(total_messages),
        user_messages: (user_messages > 0).then_some(user_messages),
        assistant_messages: (assistant_messages > 0).then_some(assistant_messages),
        developer_messages: (developer_messages > 0).then_some(developer_messages),
    });
    let total_sessions = (!session_ids.is_empty()).then_some(session_ids.len() as u64);
    let total_messages_metadata = summary_metrics
        .as_ref()
        .and_then(|metrics| metrics.total_messages);

    UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: sync_rollup_summary_id(&bucket_key),
        device_id: first.device_id.clone(),
        provider: first.provider.clone(),
        source_id: first.source_id.clone(),
        provider_account_id: first.provider_account_id.clone(),
        source: EventSource {
            source_record_id: None,
            ..first.source.clone()
        },
        model: None,
        models,
        usage: UsageCounts {
            input_tokens: Some(total_input),
            output_tokens: Some(total_output),
            cache_creation_tokens: Some(total_cache_creation),
            cache_read_tokens: Some(total_cache_read),
            reasoning_tokens: Some(total_reasoning),
            total_tokens: Some(total_tokens),
            requests: Some(total_events),
            local_prompt_eval_tokens: None,
            local_eval_tokens: None,
        },
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: Some(estimated_cost_usd),
            provider_reported_usd: has_provider_reported_usd.then_some(provider_reported_usd),
            pricing_source: Some("local_rollup".to_string()),
            pricing_version: None,
            confidence: Confidence::Medium,
        },
        parse_evidence: None,
        project: first
            .project
            .as_ref()
            .filter(|project| project_has_stable_identity(project))
            .cloned(),
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: project_contains_file_paths(first.project.as_ref()),
        },
        metrics: summary_metrics,
        period_start: Some(period_start),
        period_end: Some(period_end),
        observed_at,
        metadata: SummaryMetadata {
            summary_format: "daily_rollup.v1".to_string(),
            summary_version: Some(SYNC_ROLLUP_SUMMARY_VERSION.to_string()),
            total_sessions,
            total_messages: total_messages_metadata,
            last_computed_at: Some(observed_at),
        },
        imported_at: observed_at,
    }
}

fn finalize_metric_stats(mut values: Vec<f64>) -> Option<MetricStats> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let samples = values.len() as u64;
    let sum = values.iter().copied().sum::<f64>();
    Some(MetricStats {
        samples,
        avg: Some(sum / samples as f64),
        min: values.first().copied(),
        max: values.last().copied(),
        p50: percentile_nearest_rank(&values, 0.50),
        p95: percentile_nearest_rank(&values, 0.95),
        sum: Some(sum),
    })
}

fn runtime_latency_supports_distribution_metrics(runtime: &statsai_core::RuntimeInfo) -> bool {
    !matches!(runtime.latency_source, Some(LatencySource::Inferred))
}

fn percentile_nearest_rank(values: &[f64], percentile: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let rank = ((values.len() as f64) * percentile).ceil() as usize;
    values
        .get(rank.saturating_sub(1).min(values.len().saturating_sub(1)))
        .copied()
}

fn summary_metrics_or_none(metrics: SummaryMetrics) -> Option<SummaryMetrics> {
    let has_metrics = metrics.active_seconds.is_some()
        || metrics.tracked_requests.is_some()
        || metrics.tracked_output_tokens.is_some()
        || metrics.tracked_reasoning_tokens.is_some()
        || metrics.latency_ms.is_some()
        || metrics.time_to_first_token_ms.is_some()
        || metrics.generated_tps.is_some()
        || metrics.visible_tps.is_some()
        || metrics.overall_generated_tps.is_some()
        || metrics.overall_visible_tps.is_some()
        || metrics.cache_hit_ratio.is_some()
        || metrics.reasoning_share.is_some()
        || metrics.total_messages.is_some()
        || metrics.user_messages.is_some()
        || metrics.assistant_messages.is_some()
        || metrics.developer_messages.is_some();
    has_metrics.then_some(metrics)
}

#[derive(Debug, Default)]
struct SyncRollupModelTotals {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
    requests: u64,
    estimated_cost_usd: i64,    // cents
    provider_reported_usd: i64, // cents
    has_provider_reported_usd: bool,
}

fn sync_rollup_model_key(model: &ModelInfo) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        model.normalized_name.as_deref().unwrap_or(""),
        model.provider_model_id.as_deref().unwrap_or(""),
        model.name.as_deref().unwrap_or(""),
        model
            .reasoning_level
            .as_ref()
            .map(|level| level.as_str())
            .unwrap_or(""),
        model.reasoning_level_raw.as_deref().unwrap_or("")
    )
}

fn safe_u64_to_i64(value: u64) -> i64 {
    if value > i64::MAX as u64 {
        i64::MAX
    } else {
        value as i64
    }
}

fn rollback(conn: &Connection) {
    if let Err(e) = conn.execute_batch("ROLLBACK") {
        eprintln!("store: ROLLBACK failed: {e}");
    }
}

fn begin_immediate_transaction_with_retry(conn: &Connection) -> Result<()> {
    let mut last_busy_error = None;
    for attempt in 0..=SQLITE_BUSY_RETRY_ATTEMPTS {
        match conn.execute_batch("BEGIN IMMEDIATE TRANSACTION") {
            Ok(()) => return Ok(()),
            Err(error)
                if is_sqlite_busy_or_locked(&error) && attempt < SQLITE_BUSY_RETRY_ATTEMPTS =>
            {
                last_busy_error = Some(error);
                std::thread::sleep(SQLITE_BUSY_RETRY_DELAY);
            }
            Err(error) => return Err(error.into()),
        }
    }

    match last_busy_error {
        Some(error) => Err(error.into()),
        None => bail!("failed to begin immediate SQLite transaction"),
    }
}

fn is_sqlite_busy_or_locked(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn sync_state_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncState> {
    let last_success_at: String = row.get(2)?;
    let last_event_started_at: Option<String> = row.get(4)?;
    let last_summary_observed_at: Option<String> = row.get(6)?;
    let last_task_verification_updated_at: Option<String> = row.get(8)?;
    let failure_count: i64 = row.get(10)?;
    Ok(SyncState {
        sink: row.get(0)?,
        target: row.get(1)?,
        last_success_at: parse_rfc3339_for_row(&last_success_at, 2)?,
        last_batch_id: row.get(3)?,
        last_event_started_at: parse_optional_rfc3339_for_row(last_event_started_at, 4)?,
        last_event_id: row.get(5)?,
        last_summary_observed_at: parse_optional_rfc3339_for_row(last_summary_observed_at, 6)?,
        last_summary_id: row.get(7)?,
        last_task_verification_updated_at: parse_optional_rfc3339_for_row(
            last_task_verification_updated_at,
            8,
        )?,
        last_task_verification_id: row.get(9)?,
        failure_count: failure_count.max(0) as u64,
        pending_resume_batch_id: row.get(11)?,
    })
}

fn parse_optional_rfc3339_for_row(
    value: Option<String>,
    index: usize,
) -> rusqlite::Result<Option<DateTime<Utc>>> {
    value
        .as_deref()
        .map(|value| parse_rfc3339_for_row(value, index))
        .transpose()
}

fn parse_rfc3339_for_row(value: &str, index: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

fn event_fingerprint(event: &UsageEvent) -> String {
    let project_key = path_independent_project_key(event);
    semantic_event_fingerprint(&SemanticFingerprintInput {
        provider: &event.provider,
        source_id: &event.source_id,
        started_at: event.session.started_at,
        session_hash: session_hash_for_fingerprint(event),
        project_key: project_key.as_deref(),
        model_name: event
            .model
            .as_ref()
            .and_then(|model| model.normalized_name.as_deref().or(model.name.as_deref())),
        input_tokens: event.usage.input_tokens,
        cache_read_tokens: event.usage.cache_read_tokens,
        cache_creation_tokens: event.usage.cache_creation_tokens,
        output_tokens: event.usage.output_tokens,
        reasoning_tokens: event.usage.reasoning_tokens,
        total_tokens: event.usage.computed_total(),
    })
}

fn semantically_same_event(left: &UsageEvent, right: &UsageEvent) -> bool {
    let uses_path_independent =
        uses_path_independent_codex_dedupe(left) && uses_path_independent_codex_dedupe(right);
    let session_matches = if uses_path_independent {
        true
    } else {
        left.session.local_session_id_hash == right.session.local_session_id_hash
    };
    let project_matches = if uses_path_independent {
        path_independent_projects_match(left, right)
    } else {
        true
    };
    left.provider == right.provider
        && left.source_id == right.source_id
        && left.session.started_at == right.session.started_at
        && session_matches
        && project_matches
        && model_key(left) == model_key(right)
        && reasoning_matches_for_dedupe(left.model.as_ref(), right.model.as_ref())
        && usage_counts_equivalent(&left.provider, &left.usage, &right.usage)
        && left.usage.computed_total() == right.usage.computed_total()
}

fn reasoning_matches_for_dedupe(left: Option<&ModelInfo>, right: Option<&ModelInfo>) -> bool {
    optional_value_matches(
        left.and_then(|model| model.reasoning_level),
        right.and_then(|model| model.reasoning_level),
    ) && optional_value_matches(
        left.and_then(|model| model.reasoning_level_raw.as_deref()),
        right.and_then(|model| model.reasoning_level_raw.as_deref()),
    )
}

fn optional_value_matches<T: PartialEq>(left: Option<T>, right: Option<T>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn path_independent_projects_match(left: &UsageEvent, right: &UsageEvent) -> bool {
    let left_key = path_independent_project_key(left);
    let right_key = path_independent_project_key(right);
    left_key == right_key
        || legacy_opaque_path_independent_project_match(left, right_key.as_deref())
        || legacy_opaque_path_independent_project_match(right, left_key.as_deref())
}

fn legacy_opaque_path_independent_project_match(
    legacy_candidate: &UsageEvent,
    other_project_key: Option<&str>,
) -> bool {
    other_project_key.is_some_and(|project_key| {
        project_key != "none"
            && legacy_candidate
                .parse_evidence
                .as_ref()
                .map(|evidence| evidence.event_key_version.as_str())
                != Some("semantic_usage_event.v4")
            && match legacy_candidate.project.as_ref() {
                None => true,
                Some(project) => {
                    project.repo_remote_hash.is_none()
                        && project.path_hash.is_none()
                        && project.branch_hash.is_none()
                }
            }
    })
}

fn usage_counts_equivalent(provider: &str, left: &UsageCounts, right: &UsageCounts) -> bool {
    if left.input_tokens == right.input_tokens
        && left.cache_read_tokens == right.cache_read_tokens
        && left.cache_creation_tokens == right.cache_creation_tokens
        && left.output_tokens == right.output_tokens
        && left.reasoning_tokens == right.reasoning_tokens
    {
        return true;
    }
    if provider != "codex" || left.cache_creation_tokens != right.cache_creation_tokens {
        return false;
    }

    let left_matches_right_legacy = left.input_tokens
        == right
            .input_tokens
            .map(|value| value.saturating_add(right.cache_read_tokens.unwrap_or(0)))
        && left.output_tokens
            == right
                .output_tokens
                .map(|value| value.saturating_add(right.reasoning_tokens.unwrap_or(0)))
        && left.cache_read_tokens == right.cache_read_tokens
        && left.reasoning_tokens == right.reasoning_tokens;
    let right_matches_left_legacy = right.input_tokens
        == left
            .input_tokens
            .map(|value| value.saturating_add(left.cache_read_tokens.unwrap_or(0)))
        && right.output_tokens
            == left
                .output_tokens
                .map(|value| value.saturating_add(left.reasoning_tokens.unwrap_or(0)))
        && right.cache_read_tokens == left.cache_read_tokens
        && right.reasoning_tokens == left.reasoning_tokens;

    left_matches_right_legacy || right_matches_left_legacy
}

fn model_key(event: &UsageEvent) -> Option<&str> {
    event
        .model
        .as_ref()
        .and_then(|model| model.normalized_name.as_deref().or(model.name.as_deref()))
}

#[derive(Debug)]
struct ConflictCandidate {
    event_id: String,
    event: UsageEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ConflictLookupKey {
    provider: String,
    source_id: String,
    fingerprint: String,
}

fn conflict_lookup_key(event: &UsageEvent, fingerprint: &str) -> ConflictLookupKey {
    ConflictLookupKey {
        provider: event.provider.clone(),
        source_id: event.source_id.0.clone(),
        fingerprint: fingerprint.to_string(),
    }
}

fn exact_or_semantic_conflict<'a>(
    candidates: Option<&'a [ConflictCandidate]>,
    event: &UsageEvent,
) -> Option<&'a ConflictCandidate> {
    let candidates = candidates?;
    candidates
        .iter()
        .find(|candidate| candidate.event_id == event.event_id.0)
        .or_else(|| {
            candidates
                .iter()
                .find(|candidate| semantically_same_event(&candidate.event, event))
        })
}

fn refreshed_duplicate_event(
    existing: Option<&UsageEvent>,
    incoming: &UsageEvent,
    existing_id: &str,
) -> UsageEvent {
    let mut refreshed = incoming.clone();
    refreshed.event_id.0 = existing_id.to_string();

    let Some(existing_model) = existing.and_then(|event| event.model.as_ref()) else {
        return refreshed;
    };
    let Some(refreshed_model) = refreshed.model.as_mut() else {
        return refreshed;
    };

    if refreshed_model.reasoning_level.is_none() {
        refreshed_model.reasoning_level = existing_model.reasoning_level;
    }
    if refreshed_model.reasoning_level_raw.is_none() {
        refreshed_model.reasoning_level_raw = existing_model.reasoning_level_raw.clone();
    }

    refreshed
}

fn session_hash_for_fingerprint(event: &UsageEvent) -> Option<&str> {
    if uses_path_independent_codex_dedupe(event) {
        None
    } else {
        event.session.local_session_id_hash.as_deref()
    }
}

fn path_independent_project_key(event: &UsageEvent) -> Option<String> {
    uses_path_independent_codex_dedupe(event)
        .then(|| sync_rollup_project_key(event.project.as_ref()))
}

fn uses_path_independent_codex_dedupe(event: &UsageEvent) -> bool {
    event.provider == "codex"
        && event
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_record_id.as_deref())
            .is_some_and(|record_id| {
                record_id.contains(":codex_token_count:")
                    || record_id.contains(":codex_turn_usage:")
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use statsai_core::{
        event_id, summary_id, Confidence, CostInfo, EventSource, LocationOrigin, ModelInfo,
        ParseEvidence, PrivacyInfo, PrivacyMode, ProjectInfo, ReasoningLevel, SessionInfo,
        SourceKind, SummaryMetadata, UsageCounts, UsageSummary, SYNC_BATCH_SCHEMA_VERSION,
        USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
    };
    use std::path::Path;

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
            reasoning_level: Some(ReasoningLevel::Medium),
            reasoning_level_raw: Some("medium".to_string()),
        });

        let mut less_enriched = enriched.clone();
        less_enriched.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
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
            events: Vec::new(),
            summaries: summaries.clone(),
            task_buckets: Vec::new(),
            task_verifications: Vec::new(),
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
            reasoning_level: Some(ReasoningLevel::Low),
            reasoning_level_raw: Some("low".to_string()),
        });
        low.usage.total_tokens = Some(15);

        let mut high = test_store_event(&source, day + chrono::Duration::hours(1), "record-high");
        high.model = Some(ModelInfo {
            name: Some("gpt-5.5".to_string()),
            normalized_name: Some("gpt-5.5".to_string()),
            provider_model_id: Some("gpt-5.5".to_string()),
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
            .pending_http_sync_summary_counts(target)
            .expect("pending counts");
        assert_eq!(
            counts,
            PendingSyncSummaryCounts {
                rollups: 0,
                passthrough_summaries: 2,
                total: 2,
                days: 5,
            }
        );

        store
            .record_summaries_synced("http", target, &[summary, backfill])
            .expect("record synced");

        let counts = store
            .pending_http_sync_summary_counts(target)
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
                .pending_http_sync_summary_counts(target)
                .expect("counts after sync")
                .total,
            0
        );

        let mut edited = summary.clone();
        edited.usage.total_tokens = Some(80);
        store.upsert_summary(&edited).expect("edited summary");

        let counts = store
            .pending_http_sync_summary_counts(target)
            .expect("pending counts after edit");
        assert_eq!(
            counts,
            PendingSyncSummaryCounts {
                rollups: 0,
                passthrough_summaries: 1,
                total: 1,
                days: 1,
            }
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
            .pending_http_sync_summary_counts(target)
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
                .pending_http_sync_summary_counts(target)
                .expect("default payload counts")
                .total,
            0
        );
        assert_eq!(
            store
                .pending_http_sync_summary_counts_with_projects(target, true)
                .expect("project payload counts")
                .total,
            1
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
