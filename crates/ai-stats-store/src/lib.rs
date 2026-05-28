//! Local SQLite storage for `ai-stats`.

use ai_stats_core::{
    semantic_event_fingerprint, DailyRollup, ProviderAccount, SemanticFingerprintInput, SourceId,
    SourceLocation, Subscription, SummaryId, UsageEvent, UsageSummary,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

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
            CREATE TABLE IF NOT EXISTS subscriptions (
              subscription_id TEXT PRIMARY KEY,
              provider TEXT NOT NULL,
              provider_account_id TEXT,
              payload TEXT NOT NULL
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
                subscription
                    .provider_account_id
                    .as_ref()
                    .map(|id| id.0.as_str()),
                &payload
            ],
        )?;
        Ok(())
    }

    pub fn list_subscriptions(&self) -> Result<Vec<Subscription>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM subscriptions ORDER BY provider, subscription_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut subscriptions = Vec::new();
        for row in rows {
            subscriptions.push(serde_json::from_str(&row?)?);
        }
        Ok(subscriptions)
    }

    pub fn insert_event(&self, event: &UsageEvent) -> Result<bool> {
        let fingerprint = event_fingerprint(event);
        if let Some(existing_id) = self.find_semantic_duplicate_event_id(event, &fingerprint)? {
            let mut refreshed = event.clone();
            refreshed.event_id.0 = existing_id;
            self.update_event_payload(&refreshed)?;
            return Ok(false);
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
                &fingerprint,
                &payload
            ],
        )?;
        if changed == 0 {
            self.update_event_payload(event)?;
        }
        Ok(changed > 0)
    }

    pub fn insert_events(&self, events: &[UsageEvent]) -> Result<u64> {
        let fingerprints: Vec<String> = events.iter().map(event_fingerprint).collect();
        let conflict_map = self.batch_load_conflicts(&fingerprints)?;

        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            let mut inserted = 0u64;
            for (index, event) in events.iter().enumerate() {
                let fingerprint = &fingerprints[index];
                if let Some(existing_id) = conflict_map.get(fingerprint) {
                    let mut refreshed = event.clone();
                    refreshed.event_id.0 = existing_id.clone();
                    self.update_event_payload(&refreshed)?;
                    continue;
                }
                if self.insert_event_in_batch(event, fingerprint)? {
                    inserted += 1;
                }
            }
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

    fn update_event_payload(&self, event: &UsageEvent) -> Result<()> {
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
        Ok(())
    }

    fn insert_event_in_batch(&self, event: &UsageEvent, fingerprint: &str) -> Result<bool> {
        if let Some(existing_id) = self.find_semantic_duplicate_event_id(event, fingerprint)? {
            let mut refreshed = event.clone();
            refreshed.event_id.0 = existing_id;
            self.update_event_payload(&refreshed)?;
            return Ok(false);
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
            self.update_event_payload(event)?;
        }
        Ok(changed > 0)
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
        let mut events = Vec::new();
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
        let mut estimated_cost = None::<f64>;
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
                estimated_cost = Some(estimated_cost.unwrap_or(0.0) + cost);
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
                .unwrap_or_else(|| "unmapped".to_string());
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
            schema_version: ai_stats_core::DAILY_ROLLUP_SCHEMA_VERSION.to_string(),
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

    pub fn checkpoint_wal(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }
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
    semantic_event_fingerprint(&SemanticFingerprintInput {
        provider: &event.provider,
        source_id: &event.source_id,
        started_at: event.session.started_at,
        session_hash: event.session.local_session_id_hash.as_deref(),
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
    left.provider == right.provider
        && left.source_id == right.source_id
        && left.session.started_at == right.session.started_at
        && left.session.local_session_id_hash == right.session.local_session_id_hash
        && model_key(left) == model_key(right)
        && left.usage.input_tokens == right.usage.input_tokens
        && left.usage.cache_read_tokens == right.usage.cache_read_tokens
        && left.usage.cache_creation_tokens == right.usage.cache_creation_tokens
        && left.usage.output_tokens == right.usage.output_tokens
        && left.usage.reasoning_tokens == right.usage.reasoning_tokens
        && left.usage.computed_total() == right.usage.computed_total()
}

fn model_key(event: &UsageEvent) -> Option<&str> {
    event
        .model
        .as_ref()
        .and_then(|model| model.normalized_name.as_deref().or(model.name.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_stats_core::{
        event_id, summary_id, Confidence, CostInfo, EventSource, LocationOrigin, ModelInfo,
        PrivacyInfo, PrivacyMode, SessionInfo, SourceKind, SummaryMetadata, UsageCounts,
        UsageSummary, USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
    };
    use chrono::Utc;
    use std::path::Path;

    #[test]
    fn inserts_events_idempotently() {
        let store = Store::in_memory().expect("store");
        let source = ai_stats_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex"),
            LocationOrigin::Configured,
            None,
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
        event.cost.estimated_api_equivalent_usd = Some(0.001);

        assert!(!store.insert_event(&event).expect("refresh duplicate"));
        assert_eq!(store.event_count().expect("count after refresh"), 1);
        assert_eq!(store.token_total().expect("tokens after refresh"), 15);

        let events = store.events().expect("events");
        assert_eq!(events[0].usage.input_tokens, Some(12));
        assert_eq!(events[0].cost.estimated_api_equivalent_usd, Some(0.001));
    }

    #[test]
    fn refreshes_semantic_duplicate_with_new_event_id_without_double_counting() {
        let store = Store::in_memory().expect("store");
        let source = ai_stats_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-semantic"),
            LocationOrigin::Configured,
            None,
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
    fn insert_events_batches_in_one_transaction() {
        let store = Store::in_memory().expect("store");
        let source = ai_stats_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-batch"),
            LocationOrigin::Configured,
            None,
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
        let source = ai_stats_core::SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-summary"),
            LocationOrigin::Configured,
            None,
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
    fn sync_state_tracks_success_and_filters_after_cursor() {
        let store = Store::in_memory().expect("store");
        let source = ai_stats_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-sync-state"),
            LocationOrigin::Configured,
            None,
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

    fn test_store_event(
        source: &ai_stats_core::SourceLocation,
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
        source: &ai_stats_core::SourceLocation,
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
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
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
