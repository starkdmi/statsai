//! Local SQLite storage for `statsai`.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use statsai_core::{
    hash_text, normalize_email, normalize_provider_user_id, periods_overlap, project_bucket_key,
    project_has_remote_identity, project_has_stable_identity, provider_account_id,
    provider_account_id_from_identity, semantic_event_fingerprint, source_account_assignment_id,
    subscription_id, summary_id, timestamp_in_period, BillingPeriod, Confidence, CostInfo,
    DailyRollup, EventSource, IdentitySource, LatencySource, MetricStats, ModelInfo, PrivacyInfo,
    PrivacyMode, ProviderAccount, ProviderAccountId, SemanticFingerprintInput,
    SourceAccountAssignment, SourceAccountAssignmentId, SourceId, SourceLocation,
    SourceVerificationMode, Subscription, SubscriptionId, SubscriptionStatus, SummaryId,
    SummaryMetadata, SummaryMetrics, SummaryModelUsage, UsageCounts, UsageEvent, UsageSummary,
    VerifiedSourceState, VerifiedSubscriptionState, PROVIDER_ACCOUNT_SCHEMA_VERSION,
    SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION, SUBSCRIPTION_SCHEMA_VERSION,
    USAGE_SUMMARY_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const SYNC_ROLLUP_SUMMARY_VERSION: &str = "8";

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
    pub failure_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFileStateEntry {
    pub cache_key: String,
    pub cache_signature: String,
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
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn in_memory() -> Result<Self> {
        let store = Self {
            conn: Connection::open_in_memory()?,
        };
        store.conn.busy_timeout(std::time::Duration::from_secs(5))?;
        store.migrate()?;
        Ok(store)
    }

    pub fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA busy_timeout = 5000;
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
        )?;
        Ok(())
    }

    pub fn pending_scan_file_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<Vec<ScanFileStateEntry>> {
        let mut pending = Vec::with_capacity(entries.len());
        let mut stmt = self.conn.prepare(
            "SELECT cache_signature FROM scan_file_state WHERE source_id = ?1 AND cache_key = ?2",
        )?;
        for entry in entries {
            let existing = stmt
                .query_row(params![&source_id.0, &entry.cache_key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?;
            if existing.as_deref() != Some(entry.cache_signature.as_str()) {
                pending.push(entry.clone());
            }
        }
        Ok(pending)
    }

    pub fn record_scan_file_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let synced_at = Utc::now().to_rfc3339();
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            let mut stmt = self.conn.prepare(
                r#"
                INSERT INTO scan_file_state (source_id, cache_key, cache_signature, synced_at)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(source_id, cache_key) DO UPDATE SET
                  cache_signature = excluded.cache_signature,
                  synced_at = excluded.synced_at
                "#,
            )?;
            for entry in entries {
                stmt.execute(params![
                    &source_id.0,
                    &entry.cache_key,
                    &entry.cache_signature,
                    &synced_at
                ])?;
            }
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

    pub fn delete_scan_file_entries_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            let mut deleted = 0u64;
            for source_id in source_ids {
                deleted += self.conn.execute(
                    "DELETE FROM scan_file_state WHERE source_id = ?1",
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            self.conn.execute(
                "DELETE FROM source_account_assignments WHERE source_id = ?1",
                params![&source_id.0],
            )?;
            Ok(self.conn.execute(
                "DELETE FROM sources WHERE source_id = ?1",
                params![&source_id.0],
            )? > 0)
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

    pub fn insert_event(&self, event: &UsageEvent) -> Result<bool> {
        let event = event_with_valid_project(event);
        let fingerprint = event_fingerprint(&event);
        if let Some(existing_id) = self.find_semantic_duplicate_event_id(&event, &fingerprint)? {
            let mut refreshed = event.clone();
            refreshed.event_id.0 = existing_id;
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
            let dirty_keys = self.update_event_payload(&event)?;
            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
        } else {
            self.refresh_sync_rollups_for_keys(&BTreeSet::from([sync_rollup_bucket_key(&event)]))?;
        }
        Ok(changed > 0)
    }

    pub fn insert_events(&self, events: &[UsageEvent]) -> Result<u64> {
        let events = events
            .iter()
            .map(event_with_valid_project)
            .collect::<Vec<_>>();
        let fingerprints: Vec<String> = events.iter().map(event_fingerprint).collect();
        let conflict_map = self.batch_load_conflicts(&fingerprints)?;

        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            let mut inserted = 0u64;
            let mut dirty_keys = BTreeSet::new();
            for (index, event) in events.iter().enumerate() {
                let fingerprint = &fingerprints[index];
                if let Some(existing_id) = conflict_map.get(fingerprint) {
                    let mut refreshed = event.clone();
                    refreshed.event_id.0 = existing_id.clone();
                    dirty_keys.extend(self.update_event_payload(&refreshed)?);
                    continue;
                }
                let outcome = self.insert_event_in_batch(event, fingerprint)?;
                if outcome.inserted {
                    inserted += 1;
                }
                dirty_keys.extend(outcome.dirty_keys);
            }
            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
            Ok(inserted)
        })();

        match result {
            Ok(inserted) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(inserted)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
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
        if let Some(existing_id) = self.find_semantic_duplicate_event_id(event, fingerprint)? {
            let mut refreshed = event.clone();
            refreshed.event_id.0 = existing_id;
            return Ok(EventInsertOutcome {
                inserted: false,
                dirty_keys: self.update_event_payload(&refreshed)?,
            });
        }

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
            return Ok(EventInsertOutcome {
                inserted: false,
                dirty_keys: self.update_event_payload(event)?,
            });
        }
        Ok(EventInsertOutcome {
            inserted: true,
            dirty_keys: BTreeSet::from([sync_rollup_bucket_key(event)]),
        })
    }

    fn batch_load_conflicts(
        &self,
        fingerprints: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut conflicts = std::collections::HashMap::new();
        if fingerprints.is_empty() {
            return Ok(conflicts);
        }

        const CHUNK_SIZE: usize = 500;
        for chunk in fingerprints.chunks(CHUNK_SIZE) {
            let placeholders: Vec<String> = chunk
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect();
            let sql = format!(
                "SELECT event_id, semantic_fingerprint FROM usage_events WHERE semantic_fingerprint IN ({})",
                placeholders.join(",")
            );

            let mut stmt = self.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::types::ToSql> = chunk
                .iter()
                .map(|fp| fp as &dyn rusqlite::types::ToSql)
                .collect();

            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            for row in rows {
                let (event_id, fingerprint) = row?;
                conflicts.insert(fingerprint, event_id);
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            let mut changed = 0u64;
            let mut dirty_keys = BTreeSet::new();
            for event in events {
                dirty_keys.extend(self.update_event_payload(event)?);
                changed += 1;
            }
            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
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

    pub fn delete_events_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            let mut deleted = 0u64;
            for source_id in source_ids {
                deleted += self.conn.execute(
                    "DELETE FROM usage_events WHERE source_id = ?1",
                    params![&source_id.0],
                )? as u64;
            }
            self.delete_sync_rollups_for_sources_in_tx(source_ids)?;
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
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
                       last_event_id, last_summary_observed_at, last_summary_id, failure_count
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
                   last_event_id, last_summary_observed_at, last_summary_id, failure_count
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            self.conn.execute("DELETE FROM entity_sync_state", [])?;
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            self.conn.execute(
                "DELETE FROM entity_sync_state WHERE sink = ?1 AND target = ?2",
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
        let now = Utc::now();

        self.conn.execute(
            r#"
            INSERT INTO sync_state (
              sink, target, last_success_at, last_batch_id, last_event_started_at,
              last_event_id, last_summary_observed_at, last_summary_id, failure_count
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
            ON CONFLICT(sink, target) DO UPDATE SET
              last_success_at = excluded.last_success_at,
              last_batch_id = excluded.last_batch_id,
              last_event_started_at = excluded.last_event_started_at,
              last_event_id = excluded.last_event_id,
              last_summary_observed_at = excluded.last_summary_observed_at,
              last_summary_id = excluded.last_summary_id,
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
            ],
        )?;
        Ok(())
    }

    pub fn record_sync_failure(&self, sink: &str, target: &str) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO sync_state (sink, target, last_success_at, last_batch_id, failure_count)
            VALUES (?1, ?2, ?3, '', 1)
            ON CONFLICT(sink, target) DO UPDATE SET
              failure_count = failure_count + 1
            "#,
            params![sink, target, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn delete_summaries_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            for summary_id in summary_ids {
                self.conn.execute(
                    "UPDATE sync_rollups SET dirty = 0 WHERE summary_id = ?1",
                    params![&summary_id.0],
                )?;
            }
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

        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            self.conn.execute("DELETE FROM sync_rollups", [])?;
            self.refresh_sync_rollups_for_keys(&keys)?;
            Ok(keys.len() as u64)
        })();

        match result {
            Ok(count) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(count)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
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
            let payload = serde_json::to_string(summary)?;
            if self.entity_requires_sync(
                sink,
                target,
                "summary",
                &summary.summary_id.0,
                &hash_text(&payload),
            )? {
                changed.push(summary.clone());
            }
        }
        Ok(changed)
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
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

    pub fn record_accounts_synced(
        &self,
        sink: &str,
        target: &str,
        accounts: &[ProviderAccount],
    ) -> Result<()> {
        if accounts.is_empty() {
            return Ok(());
        }
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
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

    pub fn record_source_account_assignments_synced(
        &self,
        sink: &str,
        target: &str,
        assignments: &[SourceAccountAssignment],
    ) -> Result<()> {
        if assignments.is_empty() {
            return Ok(());
        }
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
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

    pub fn record_subscriptions_synced(
        &self,
        sink: &str,
        target: &str,
        subscriptions: &[Subscription],
    ) -> Result<()> {
        if subscriptions.is_empty() {
            return Ok(());
        }
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
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

    pub fn record_summaries_synced(
        &self,
        sink: &str,
        target: &str,
        summaries: &[UsageSummary],
    ) -> Result<()> {
        if summaries.is_empty() {
            return Ok(());
        }
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            for summary in summaries {
                let payload = serde_json::to_string(summary)?;
                self.record_entity_synced(
                    sink,
                    target,
                    "summary",
                    &summary.summary_id.0,
                    &hash_text(&payload),
                )?;
            }
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
        let payload_hash = hash_text(&payload);
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

#[derive(Debug, Default)]
struct EventInsertOutcome {
    inserted: bool,
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
            .filter(|project| project_has_remote_identity(project))
            .cloned(),
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
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
        "{}|{}|{}",
        model.normalized_name.as_deref().unwrap_or(""),
        model.provider_model_id.as_deref().unwrap_or(""),
        model.name.as_deref().unwrap_or("")
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

fn sync_state_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncState> {
    let last_success_at: String = row.get(2)?;
    let last_event_started_at: Option<String> = row.get(4)?;
    let last_summary_observed_at: Option<String> = row.get(6)?;
    let failure_count: i64 = row.get(8)?;
    Ok(SyncState {
        sink: row.get(0)?,
        target: row.get(1)?,
        last_success_at: parse_rfc3339_for_row(&last_success_at, 2)?,
        last_batch_id: row.get(3)?,
        last_event_started_at: parse_optional_rfc3339_for_row(last_event_started_at, 4)?,
        last_event_id: row.get(5)?,
        last_summary_observed_at: parse_optional_rfc3339_for_row(last_summary_observed_at, 6)?,
        last_summary_id: row.get(7)?,
        failure_count: failure_count.max(0) as u64,
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
        && usage_counts_equivalent(&left.provider, &left.usage, &right.usage)
        && left.usage.computed_total() == right.usage.computed_total()
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
        ParseEvidence, PrivacyInfo, PrivacyMode, ProjectInfo, SessionInfo, SourceKind,
        SummaryMetadata, UsageCounts, UsageSummary, USAGE_EVENT_SCHEMA_VERSION,
        USAGE_SUMMARY_SCHEMA_VERSION,
    };
    use std::path::Path;

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
    fn sync_rollups_keep_path_only_buckets_without_exporting_project_metadata() {
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
        assert!(dirty.iter().all(|summary| summary.project.is_none()));
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
            repo_label: Some("starkdmi/statsai".to_string()),
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

        assert!(!store
            .insert_event(&new_event)
            .expect("refresh legacy projectless duplicate"));
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
            repo_label: Some("starkdmi/statsai".to_string()),
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
            repo_label: Some("starkdmi/statsai".to_string()),
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
            repo_label: Some("starkdmi/statsai".to_string()),
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
            repo_label: Some("starkdmi/statsai".to_string()),
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
            .record_sync_success("http", "http://localhost/sync", "batch_1", &[first], &[])
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
            )
            .expect("record success");
        store
            .record_sync_success(
                "http",
                "https://other.example.com/api/sync/batches",
                "batch_2",
                &[],
                &[],
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
            summary_id: summary_id("claude_code", &source.source_id, "summary"),
            device_id: "device".to_string(),
            provider: "claude_code".to_string(),
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
