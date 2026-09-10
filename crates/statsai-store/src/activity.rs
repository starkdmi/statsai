//! Local activity invocations, daily rollups, coverage, and OpenCode scan cursors.

use super::*;
use rusqlite::{params, OptionalExtension};
use statsai_core::{
    activity_account_key, activity_day_key, activity_duration_bucket_index, activity_entity_key,
    activity_rollup_id, hash_text, sanitize_activity_invocation, ActivityCoverageV1,
    ActivityFamily, ActivityInvocationV1, ActivityKind, ActivityOutcome, ActivityRollupV1,
    ProviderAccountId, ACTIVITY_DURATION_BUCKET_COUNT, ACTIVITY_ROLLUP_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivityPersistMode {
    /// Delete rows for the reconciled file hashes, then insert.
    #[default]
    ReplaceFiles,
    /// Upsert by invocation id (OpenCode incremental cursor).
    Upsert,
    /// Delete every invocation for the source, then insert.
    ReplaceSource,
}

impl ActivityPersistMode {
    #[must_use]
    pub fn for_scan(provider: &str, full_source_replace: bool) -> Self {
        if full_source_replace {
            Self::ReplaceSource
        } else if provider == "opencode" {
            Self::Upsert
        } else {
            Self::ReplaceFiles
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityScanCursor {
    pub last_time_updated: i64,
    pub parser_revision: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ActivityPersistResult {
    pub written_invocations: u64,
    pub written_coverage: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActivityStatus {
    pub tool_calls: u64,
    pub mcp_calls: u64,
    pub skill_loads: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub unknown: u64,
    pub first_seen: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
    pub top_tools: Vec<ActivityStatusRow>,
    pub top_mcp: Vec<ActivityStatusRow>,
    pub top_skills: Vec<ActivityStatusRow>,
    pub coverage: Vec<ActivityCoverageV1>,
    pub include_activity: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityStatusRow {
    pub kind: ActivityKind,
    pub display_name: String,
    pub family: ActivityFamily,
    pub provider: String,
    pub calls: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub unknown: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ActivityRollupBucketKey {
    source_id: String,
    account_key: String,
    day: String,
}

impl Store {
    pub fn activity_scan_cursor(&self, source_id: &SourceId) -> Result<Option<ActivityScanCursor>> {
        self.conn
            .query_row(
                "SELECT last_time_updated, parser_revision FROM activity_scan_cursors WHERE source_id = ?1",
                params![&source_id.0],
                |row| {
                    Ok(ActivityScanCursor {
                        last_time_updated: row.get(0)?,
                        parser_revision: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_activity_scan_cursor(
        &self,
        source_id: &SourceId,
        cursor: &ActivityScanCursor,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO activity_scan_cursors (source_id, last_time_updated, parser_revision)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(source_id) DO UPDATE SET
              last_time_updated = excluded.last_time_updated,
              parser_revision = excluded.parser_revision
            "#,
            params![
                &source_id.0,
                cursor.last_time_updated,
                &cursor.parser_revision
            ],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn persist_activity_scan(
        &self,
        device_id: &str,
        source_id: &SourceId,
        invocations: &[ActivityInvocationV1],
        coverage: &[ActivityCoverageV1],
        reconciled_file_hashes: &[String],
        mode: ActivityPersistMode,
        cursor: Option<ActivityScanCursor>,
    ) -> Result<ActivityPersistResult> {
        self.with_immediate_transaction(|| {
            self.persist_activity_scan_inner(
                device_id,
                source_id,
                invocations,
                coverage,
                reconciled_file_hashes,
                mode,
                cursor,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn persist_activity_scan_inner(
        &self,
        device_id: &str,
        source_id: &SourceId,
        invocations: &[ActivityInvocationV1],
        coverage: &[ActivityCoverageV1],
        reconciled_file_hashes: &[String],
        mode: ActivityPersistMode,
        cursor: Option<ActivityScanCursor>,
    ) -> Result<ActivityPersistResult> {
        let mut affected = BTreeSet::new();
        match mode {
            ActivityPersistMode::ReplaceSource => {
                self.collect_activity_buckets_for_source(source_id, &mut affected)?;
                self.conn.execute(
                    "DELETE FROM activity_invocations WHERE source_id = ?1",
                    params![&source_id.0],
                )?;
            }
            ActivityPersistMode::ReplaceFiles => {
                self.collect_activity_buckets_for_file_hashes(
                    source_id,
                    reconciled_file_hashes,
                    &mut affected,
                )?;
                self.delete_activity_invocations_for_file_hashes(
                    source_id,
                    reconciled_file_hashes,
                )?;
            }
            ActivityPersistMode::Upsert => {}
        }

        let mut attributed = Vec::with_capacity(invocations.len());
        let mut assignments_by_source = HashMap::new();
        for invocation in invocations {
            let Some(mut invocation) = sanitize_activity_invocation(invocation.clone()) else {
                continue;
            };
            if !assignments_by_source.contains_key(&invocation.source_id) {
                assignments_by_source.insert(
                    invocation.source_id.clone(),
                    self.list_source_account_assignments_for_source(&invocation.source_id)?,
                );
            }
            let assignments = assignments_by_source
                .get(&invocation.source_id)
                .expect("assignments inserted");
            invocation.provider_account_id =
                assignment_for_timestamp(assignments, invocation.observed_at)
                    .map(|assignment| assignment.provider_account_id.clone());
            affected.insert(rollup_bucket_key(&invocation));
            attributed.push(invocation);
        }

        let written_invocations = self.upsert_activity_invocations_inner(&attributed)?;
        let written_coverage = self.upsert_activity_coverage_inner(coverage)?;
        self.rebuild_activity_rollups_for_keys(device_id, &affected)?;
        if matches!(mode, ActivityPersistMode::ReplaceSource) {
            self.drop_orphaned_activity_coverage(source_id, coverage)?;
        }
        if let Some(cursor) = cursor {
            self.set_activity_scan_cursor(source_id, &cursor)?;
        }
        Ok(ActivityPersistResult {
            written_invocations,
            written_coverage,
        })
    }

    fn upsert_activity_invocations_inner(
        &self,
        invocations: &[ActivityInvocationV1],
    ) -> Result<u64> {
        let mut written = 0u64;
        let mut statement = self.conn.prepare(
            r#"
            INSERT INTO activity_invocations (
              invocation_id, provider, source_id, provider_account_id, source_file_path_hash,
              observed_at, kind, display_name, family, mcp_server, mcp_tool, plugin,
              skill_catalog, outcome, duration_ms, duration_kind, evidence, parser_revision,
              payload
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
            ON CONFLICT(invocation_id) DO UPDATE SET
              provider = excluded.provider,
              source_id = excluded.source_id,
              provider_account_id = excluded.provider_account_id,
              source_file_path_hash = excluded.source_file_path_hash,
              observed_at = excluded.observed_at,
              kind = excluded.kind,
              display_name = excluded.display_name,
              family = excluded.family,
              mcp_server = excluded.mcp_server,
              mcp_tool = excluded.mcp_tool,
              plugin = excluded.plugin,
              skill_catalog = excluded.skill_catalog,
              outcome = excluded.outcome,
              duration_ms = excluded.duration_ms,
              duration_kind = excluded.duration_kind,
              evidence = excluded.evidence,
              parser_revision = excluded.parser_revision,
              payload = excluded.payload
            "#,
        )?;
        for invocation in invocations {
            let payload = serde_json::to_string(invocation)?;
            written += statement.execute(params![
                &invocation.invocation_id,
                &invocation.provider,
                &invocation.source_id.0,
                invocation
                    .provider_account_id
                    .as_ref()
                    .map(|id| id.0.as_str()),
                &invocation.source_file_path_hash,
                invocation.observed_at.to_rfc3339(),
                invocation.kind.as_str(),
                &invocation.display_name,
                invocation.family.as_str(),
                invocation.mcp_server.as_deref(),
                invocation.mcp_tool.as_deref(),
                invocation.plugin.as_deref(),
                invocation.skill_catalog.map(|catalog| catalog.as_str()),
                invocation.outcome.as_str(),
                invocation.duration_ms.map(safe_u64_to_i64),
                invocation.duration_kind.map(|kind| kind.as_str()),
                &invocation.evidence,
                &invocation.parser_revision,
                payload,
            ])? as u64;
        }
        Ok(written)
    }

    fn upsert_activity_coverage_inner(&self, coverage: &[ActivityCoverageV1]) -> Result<u64> {
        let mut written = 0u64;
        let mut statement = self.conn.prepare(
            r#"
            INSERT INTO activity_coverage (
              coverage_id, device_id, source_id, provider, day, kind, dirty, payload
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)
            ON CONFLICT(coverage_id) DO UPDATE SET
              device_id = excluded.device_id,
              source_id = excluded.source_id,
              provider = excluded.provider,
              day = excluded.day,
              kind = excluded.kind,
              dirty = CASE
                WHEN activity_coverage.payload = excluded.payload THEN activity_coverage.dirty
                ELSE 1
              END,
              payload = excluded.payload
            "#,
        )?;
        for row in coverage {
            let payload = serde_json::to_string(row)?;
            written += statement.execute(params![
                &row.coverage_id,
                &row.device_id,
                &row.source_id.0,
                &row.provider,
                &row.day,
                row.kind.as_str(),
                payload,
            ])? as u64;
        }
        Ok(written)
    }

    fn delete_activity_invocations_for_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
    ) -> Result<()> {
        if file_hashes.is_empty() {
            return Ok(());
        }
        let placeholders = sqlite_in_clause_placeholders(file_hashes.len());
        let sql = format!(
            "DELETE FROM activity_invocations WHERE source_id = ?1 AND source_file_path_hash IN ({placeholders})"
        );
        let mut params: Vec<&dyn rusqlite::types::ToSql> = vec![&source_id.0];
        for hash in file_hashes {
            params.push(hash);
        }
        self.conn.execute(&sql, params.as_slice())?;
        Ok(())
    }

    fn collect_activity_buckets_for_source(
        &self,
        source_id: &SourceId,
        affected: &mut BTreeSet<ActivityRollupBucketKey>,
    ) -> Result<()> {
        let mut statement = self.conn.prepare(
            "SELECT provider_account_id, observed_at FROM activity_invocations WHERE source_id = ?1",
        )?;
        let rows = statement.query_map(params![&source_id.0], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (account, observed_at) = row?;
            let observed = parse_rfc3339_for_row(&observed_at, 1)?;
            affected.insert(ActivityRollupBucketKey {
                source_id: source_id.0.clone(),
                account_key: account.unwrap_or_else(|| "unlinked".to_string()),
                day: activity_day_key(observed),
            });
        }
        Ok(())
    }

    fn collect_activity_buckets_for_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
        affected: &mut BTreeSet<ActivityRollupBucketKey>,
    ) -> Result<()> {
        if file_hashes.is_empty() {
            return Ok(());
        }
        let placeholders = sqlite_in_clause_placeholders(file_hashes.len());
        let sql = format!(
            "SELECT provider_account_id, observed_at FROM activity_invocations
             WHERE source_id = ?1 AND source_file_path_hash IN ({placeholders})"
        );
        let mut params: Vec<&dyn rusqlite::types::ToSql> = vec![&source_id.0];
        for hash in file_hashes {
            params.push(hash);
        }
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params.as_slice(), |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (account, observed_at) = row?;
            let observed = parse_rfc3339_for_row(&observed_at, 1)?;
            affected.insert(ActivityRollupBucketKey {
                source_id: source_id.0.clone(),
                account_key: account.unwrap_or_else(|| "unlinked".to_string()),
                day: activity_day_key(observed),
            });
        }
        Ok(())
    }

    fn rebuild_activity_rollups_for_keys(
        &self,
        device_id: &str,
        keys: &BTreeSet<ActivityRollupBucketKey>,
    ) -> Result<()> {
        for key in keys {
            self.rebuild_activity_rollups_for_key(device_id, key)?;
        }
        Ok(())
    }

    fn rebuild_activity_rollups_for_key(
        &self,
        device_id: &str,
        key: &ActivityRollupBucketKey,
    ) -> Result<()> {
        let invocations = self.activity_invocations_for_bucket(key)?;
        let previous_ids = self.activity_rollup_ids_for_bucket(key)?;
        let mut next_ids = BTreeSet::new();
        let mut grouped: BTreeMap<(ActivityKind, String), Vec<&ActivityInvocationV1>> =
            BTreeMap::new();
        for invocation in &invocations {
            let entity_key = activity_entity_key(
                invocation.kind,
                &invocation.display_name,
                invocation.mcp_server.as_deref(),
                invocation.mcp_tool.as_deref(),
                invocation.plugin.as_deref(),
            );
            grouped
                .entry((invocation.kind, entity_key))
                .or_default()
                .push(invocation);
        }
        for ((kind, entity_key), rows) in grouped {
            let rollup = build_activity_rollup(device_id, key, kind, &entity_key, &rows);
            next_ids.insert(rollup.rollup_id.clone());
            self.upsert_activity_rollup(&rollup)?;
        }
        for rollup_id in previous_ids {
            if !next_ids.contains(&rollup_id) {
                self.conn.execute(
                    "DELETE FROM activity_rollups WHERE rollup_id = ?1",
                    params![&rollup_id],
                )?;
            }
        }
        Ok(())
    }

    fn activity_invocations_for_bucket(
        &self,
        key: &ActivityRollupBucketKey,
    ) -> Result<Vec<ActivityInvocationV1>> {
        let account_filter = if key.account_key == "unlinked" {
            "provider_account_id IS NULL"
        } else {
            "provider_account_id = ?3"
        };
        let sql = format!(
            "SELECT payload FROM activity_invocations
             WHERE source_id = ?1 AND substr(observed_at, 1, 10) = ?2 AND {account_filter}"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = if key.account_key == "unlinked" {
            statement
                .query_map(params![&key.source_id, &key.day], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            statement
                .query_map(params![&key.source_id, &key.day, &key.account_key], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        rows.into_iter()
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .collect()
    }

    fn activity_rollup_ids_for_bucket(&self, key: &ActivityRollupBucketKey) -> Result<Vec<String>> {
        let account_filter = if key.account_key == "unlinked" {
            "provider_account_id IS NULL"
        } else {
            "provider_account_id = ?3"
        };
        let sql = format!(
            "SELECT rollup_id FROM activity_rollups
             WHERE source_id = ?1 AND day = ?2 AND {account_filter}"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = if key.account_key == "unlinked" {
            statement
                .query_map(params![&key.source_id, &key.day], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            statement
                .query_map(params![&key.source_id, &key.day, &key.account_key], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    fn upsert_activity_rollup(&self, rollup: &ActivityRollupV1) -> Result<()> {
        let payload = serde_json::to_string(rollup)?;
        let payload_hash = hash_text(&payload);
        let existing_hash: Option<String> = self
            .conn
            .query_row(
                "SELECT payload_hash FROM activity_rollups WHERE rollup_id = ?1",
                params![&rollup.rollup_id],
                |row| row.get(0),
            )
            .optional()?;
        let dirty = match existing_hash {
            Some(hash) if hash == payload_hash => {
                // Content unchanged: preserve the current dirty flag.
                None
            }
            _ => Some(1i64),
        };
        if let Some(dirty) = dirty {
            self.conn.execute(
                r#"
                INSERT INTO activity_rollups (
                  rollup_id, device_id, source_id, provider, provider_account_id, day, kind,
                  entity_key, dirty, payload_hash, payload
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ON CONFLICT(rollup_id) DO UPDATE SET
                  device_id = excluded.device_id,
                  source_id = excluded.source_id,
                  provider = excluded.provider,
                  provider_account_id = excluded.provider_account_id,
                  day = excluded.day,
                  kind = excluded.kind,
                  entity_key = excluded.entity_key,
                  dirty = excluded.dirty,
                  payload_hash = excluded.payload_hash,
                  payload = excluded.payload
                "#,
                params![
                    &rollup.rollup_id,
                    &rollup.device_id,
                    &rollup.source_id.0,
                    &rollup.provider,
                    rollup.provider_account_id.as_ref().map(|id| id.0.as_str()),
                    &rollup.day,
                    rollup.kind.as_str(),
                    &rollup.entity_key,
                    dirty,
                    payload_hash,
                    payload,
                ],
            )?;
        }
        Ok(())
    }

    fn drop_orphaned_activity_coverage(
        &self,
        source_id: &SourceId,
        incoming: &[ActivityCoverageV1],
    ) -> Result<()> {
        let keep: BTreeSet<&str> = incoming
            .iter()
            .map(|row| row.coverage_id.as_str())
            .collect();
        let mut statement = self
            .conn
            .prepare("SELECT coverage_id, day, kind FROM activity_coverage WHERE source_id = ?1")?;
        let rows = statement
            .query_map(params![&source_id.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (coverage_id, day, kind) in rows {
            if keep.contains(coverage_id.as_str()) {
                continue;
            }
            let parsed_kind = ActivityKind::parse(&kind);
            let superseded = parsed_kind.is_some_and(|kind| {
                incoming.iter().any(|row| {
                    row.kind == kind
                        && row.day.as_str() <= day.as_str()
                        && row.effective_day_end() >= day.as_str()
                })
            });
            let remaining: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM activity_invocations
                 WHERE source_id = ?1 AND substr(observed_at, 1, 10) = ?2 AND kind = ?3",
                params![&source_id.0, &day, &kind],
                |row| row.get(0),
            )?;
            if remaining == 0 || superseded {
                self.conn.execute(
                    "DELETE FROM activity_coverage WHERE coverage_id = ?1",
                    params![coverage_id],
                )?;
            }
        }
        Ok(())
    }

    pub fn mark_all_activity_rollups_dirty(&self) -> Result<u64> {
        Ok(self
            .conn
            .execute("UPDATE activity_rollups SET dirty = 1", [])? as u64)
    }

    pub fn mark_all_activity_coverage_dirty(&self) -> Result<u64> {
        Ok(self
            .conn
            .execute("UPDATE activity_coverage SET dirty = 1", [])? as u64)
    }

    pub fn mark_activity_rollups_synced(&self, rollup_ids: &[String]) -> Result<()> {
        if rollup_ids.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.mark_activity_rollups_synced_in_transaction(rollup_ids)
        })
    }

    pub(crate) fn mark_activity_rollups_synced_in_transaction(
        &self,
        rollup_ids: &[String],
    ) -> Result<()> {
        for rollup_id in rollup_ids {
            self.conn.execute(
                "UPDATE activity_rollups SET dirty = 0 WHERE rollup_id = ?1",
                params![rollup_id],
            )?;
        }
        Ok(())
    }

    pub fn mark_activity_coverage_synced(&self, coverage_ids: &[String]) -> Result<()> {
        if coverage_ids.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.mark_activity_coverage_synced_in_transaction(coverage_ids)
        })
    }

    pub(crate) fn mark_activity_coverage_synced_in_transaction(
        &self,
        coverage_ids: &[String],
    ) -> Result<()> {
        for coverage_id in coverage_ids {
            self.conn.execute(
                "UPDATE activity_coverage SET dirty = 0 WHERE coverage_id = ?1",
                params![coverage_id],
            )?;
        }
        Ok(())
    }

    pub fn all_activity_rollups(&self) -> Result<Vec<ActivityRollupV1>> {
        self.activity_rollups_by_sql("SELECT payload FROM activity_rollups ORDER BY day, rollup_id")
    }

    pub fn dirty_activity_rollups(&self) -> Result<Vec<ActivityRollupV1>> {
        self.activity_rollups_by_sql(
            "SELECT payload FROM activity_rollups WHERE dirty = 1 ORDER BY day, rollup_id",
        )
    }

    pub fn dirty_activity_coverage(&self) -> Result<Vec<ActivityCoverageV1>> {
        self.activity_coverage_by_sql(
            "SELECT payload FROM activity_coverage WHERE dirty = 1 ORDER BY day, coverage_id",
        )
    }

    pub fn all_activity_coverage(&self) -> Result<Vec<ActivityCoverageV1>> {
        self.activity_coverage_by_sql(
            "SELECT payload FROM activity_coverage ORDER BY day, coverage_id",
        )
    }

    fn activity_coverage_by_sql(&self, sql: &str) -> Result<Vec<ActivityCoverageV1>> {
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .collect()
    }

    fn activity_rollups_by_sql(&self, sql: &str) -> Result<Vec<ActivityRollupV1>> {
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut rollups = Vec::with_capacity(rows.len());
        for payload in rows {
            let mut rollup: ActivityRollupV1 = serde_json::from_str(&payload)?;
            if rollup.entity_key.is_empty() {
                rollup.entity_key = activity_entity_key(
                    rollup.kind,
                    &rollup.display_name,
                    rollup.mcp_server.as_deref(),
                    rollup.mcp_tool.as_deref(),
                    rollup.plugin.as_deref(),
                );
            }
            rollups.push(rollup);
        }
        Ok(rollups)
    }

    pub fn activity_invocation_count(&self) -> Result<u64> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM activity_invocations", [], |row| {
                    row.get(0)
                })?;
        Ok(count as u64)
    }

    pub fn activity_status(
        &self,
        provider: Option<&str>,
        kind: Option<ActivityKind>,
        include_activity: bool,
    ) -> Result<ActivityStatus> {
        let mut status = ActivityStatus {
            include_activity,
            ..ActivityStatus::default()
        };
        let mut sql = String::from(
            "SELECT kind, display_name, family, provider, outcome, observed_at
             FROM activity_invocations WHERE 1=1",
        );
        let mut bind: Vec<String> = Vec::new();
        if let Some(provider) = provider {
            sql.push_str(" AND provider = ?");
            bind.push(provider.to_string());
        }
        if let Some(kind) = kind {
            sql.push_str(" AND kind = ?");
            bind.push(kind.as_str().to_string());
        }
        let mut statement = self.conn.prepare(&sql)?;
        let params = sqlite_string_params(&bind);
        let rows = statement.query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut grouped: BTreeMap<(String, String, String, String), ActivityStatusRow> =
            BTreeMap::new();
        for row in rows {
            let (kind_raw, display_name, family_raw, provider, outcome_raw, observed_at) = row?;
            let Some(parsed_kind) = ActivityKind::parse(&kind_raw) else {
                continue;
            };
            let Some(family) = ActivityFamily::parse(&family_raw) else {
                continue;
            };
            match parsed_kind {
                ActivityKind::Tool => status.tool_calls += 1,
                ActivityKind::Mcp => status.mcp_calls += 1,
                ActivityKind::Skill => status.skill_loads += 1,
                ActivityKind::Command => {}
            }
            match outcome_raw.as_str() {
                "succeeded" => status.succeeded += 1,
                "failed" => status.failed += 1,
                _ => status.unknown += 1,
            }
            let observed = parse_rfc3339_for_row(&observed_at, 5)?;
            status.first_seen = Some(match status.first_seen {
                Some(existing) => existing.min(observed),
                None => observed,
            });
            status.last_seen = Some(match status.last_seen {
                Some(existing) => existing.max(observed),
                None => observed,
            });
            let entry = grouped
                .entry((
                    kind_raw.clone(),
                    display_name.clone(),
                    family_raw.clone(),
                    provider.clone(),
                ))
                .or_insert(ActivityStatusRow {
                    kind: parsed_kind,
                    display_name,
                    family,
                    provider,
                    calls: 0,
                    succeeded: 0,
                    failed: 0,
                    unknown: 0,
                });
            entry.calls += 1;
            match outcome_raw.as_str() {
                "succeeded" => entry.succeeded += 1,
                "failed" => entry.failed += 1,
                _ => entry.unknown += 1,
            }
        }
        let mut tools = Vec::new();
        let mut mcp = Vec::new();
        let mut skills = Vec::new();
        for row in grouped.into_values() {
            match row.kind {
                ActivityKind::Tool => tools.push(row),
                ActivityKind::Mcp => mcp.push(row),
                ActivityKind::Skill => skills.push(row),
                ActivityKind::Command => {}
            }
        }
        for list in [&mut tools, &mut mcp, &mut skills] {
            list.sort_by(|a, b| {
                b.calls
                    .cmp(&a.calls)
                    .then(a.display_name.cmp(&b.display_name))
            });
            list.truncate(10);
        }
        status.top_tools = tools;
        status.top_mcp = mcp;
        status.top_skills = skills;
        let mut coverage = self.all_activity_coverage()?;
        if let Some(provider) = provider {
            coverage.retain(|row| row.provider == provider);
        }
        if let Some(kind) = kind {
            coverage.retain(|row| row.kind == kind);
        }
        status.coverage = coverage;
        Ok(status)
    }

    pub fn reattribute_activity_invocations(&self, source_id: &SourceId) -> Result<u64> {
        let assignments = self.list_source_account_assignments_for_source(source_id)?;
        let mut statement = self.conn.prepare(
            "SELECT invocation_id, observed_at, provider_account_id, payload
             FROM activity_invocations WHERE source_id = ?1",
        )?;
        let rows = statement
            .query_map(params![&source_id.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut changed = 0u64;
        let mut affected = BTreeSet::new();
        let mut device_id = String::new();
        for (invocation_id, observed_at, previous_account, payload) in rows {
            let observed = parse_rfc3339_for_row(&observed_at, 1)?;
            let next_account = assignment_for_timestamp(&assignments, observed)
                .map(|assignment| assignment.provider_account_id.clone());
            let next_key = next_account
                .as_ref()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| "unlinked".to_string());
            let previous_key = previous_account
                .clone()
                .unwrap_or_else(|| "unlinked".to_string());
            if next_key == previous_key {
                continue;
            }
            let mut invocation: ActivityInvocationV1 = serde_json::from_str(&payload)?;
            if device_id.is_empty() {
                device_id = self
                    .all_activity_rollups()?
                    .into_iter()
                    .find(|rollup| rollup.source_id == *source_id)
                    .map(|rollup| rollup.device_id)
                    .unwrap_or_else(|| "device".to_string());
            }
            invocation.provider_account_id = next_account.clone();
            let next_payload = serde_json::to_string(&invocation)?;
            self.conn.execute(
                "UPDATE activity_invocations SET provider_account_id = ?2, payload = ?3 WHERE invocation_id = ?1",
                params![
                    invocation_id,
                    next_account.as_ref().map(|id| id.0.as_str()),
                    next_payload
                ],
            )?;
            affected.insert(ActivityRollupBucketKey {
                source_id: source_id.0.clone(),
                account_key: previous_key,
                day: activity_day_key(observed),
            });
            affected.insert(ActivityRollupBucketKey {
                source_id: source_id.0.clone(),
                account_key: next_key,
                day: activity_day_key(observed),
            });
            changed += 1;
        }
        if !affected.is_empty() {
            self.rebuild_activity_rollups_for_keys(&device_id, &affected)?;
        }
        Ok(changed)
    }
}

fn rollup_bucket_key(invocation: &ActivityInvocationV1) -> ActivityRollupBucketKey {
    ActivityRollupBucketKey {
        source_id: invocation.source_id.0.clone(),
        account_key: activity_account_key(invocation.provider_account_id.as_ref()).to_string(),
        day: activity_day_key(invocation.observed_at),
    }
}

fn pick_rollup_representative<'a>(rows: &[&'a ActivityInvocationV1]) -> &'a ActivityInvocationV1 {
    #[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct Key<'a> {
        family: &'a str,
        display_name: &'a str,
        mcp_server: &'a str,
        mcp_tool: &'a str,
        plugin: &'a str,
        skill_catalog: &'a str,
        evidence: &'a str,
    }
    let mut counts: BTreeMap<Key<'_>, u64> = BTreeMap::new();
    for row in rows {
        let key = Key {
            family: row.family.as_str(),
            display_name: row.display_name.as_str(),
            mcp_server: row.mcp_server.as_deref().unwrap_or(""),
            mcp_tool: row.mcp_tool.as_deref().unwrap_or(""),
            plugin: row.plugin.as_deref().unwrap_or(""),
            skill_catalog: row
                .skill_catalog
                .map(|catalog| catalog.as_str())
                .unwrap_or(""),
            evidence: row.evidence.as_str(),
        };
        *counts.entry(key).or_insert(0) += 1;
    }
    let (best, _) = counts
        .into_iter()
        .max_by_key(|(key, count)| (*count, std::cmp::Reverse(key.clone())))
        .expect("rollup group is non-empty");
    rows.iter()
        .copied()
        .find(|row| {
            row.family.as_str() == best.family
                && row.display_name == best.display_name
                && row.mcp_server.as_deref().unwrap_or("") == best.mcp_server
                && row.mcp_tool.as_deref().unwrap_or("") == best.mcp_tool
                && row.plugin.as_deref().unwrap_or("") == best.plugin
                && row
                    .skill_catalog
                    .map(|catalog| catalog.as_str())
                    .unwrap_or("")
                    == best.skill_catalog
                && row.evidence == best.evidence
        })
        .unwrap_or(rows[0])
}

fn build_activity_rollup(
    device_id: &str,
    key: &ActivityRollupBucketKey,
    kind: ActivityKind,
    entity_key: &str,
    rows: &[&ActivityInvocationV1],
) -> ActivityRollupV1 {
    let first = pick_rollup_representative(rows);
    let mut calls = 0u64;
    let mut succeeded = 0u64;
    let mut failed = 0u64;
    let mut unknown = 0u64;
    let mut duration_samples = 0u64;
    let mut duration_sum_ms = 0u64;
    let mut duration_max_ms: Option<u64> = None;
    let mut duration_buckets = [0u64; ACTIVITY_DURATION_BUCKET_COUNT];
    let mut duration_kind = None;
    let mut mixed_duration_kind = false;
    let mut first_seen = first.observed_at;
    let mut last_seen = first.observed_at;
    for invocation in rows {
        calls += 1;
        match invocation.outcome {
            ActivityOutcome::Succeeded => succeeded += 1,
            ActivityOutcome::Failed => failed += 1,
            ActivityOutcome::Unknown => unknown += 1,
        }
        if invocation.observed_at < first_seen {
            first_seen = invocation.observed_at;
        }
        if invocation.observed_at > last_seen {
            last_seen = invocation.observed_at;
        }
        if let Some(duration_ms) = invocation.duration_ms {
            duration_samples += 1;
            duration_sum_ms = duration_sum_ms.saturating_add(duration_ms);
            duration_max_ms = Some(match duration_max_ms {
                Some(max) => max.max(duration_ms),
                None => duration_ms,
            });
            duration_buckets[activity_duration_bucket_index(duration_ms)] += 1;
            match (duration_kind, invocation.duration_kind) {
                (None, Some(kind)) if !mixed_duration_kind => duration_kind = Some(kind),
                (Some(existing), Some(kind)) if existing != kind => {
                    mixed_duration_kind = true;
                    duration_kind = None;
                }
                _ => {}
            }
        }
    }
    let account_id = if key.account_key == "unlinked" {
        None
    } else {
        Some(ProviderAccountId(key.account_key.clone()))
    };
    ActivityRollupV1 {
        schema_version: ACTIVITY_ROLLUP_SCHEMA_VERSION.to_string(),
        rollup_id: activity_rollup_id(
            device_id,
            &key.source_id,
            &key.account_key,
            &key.day,
            kind,
            entity_key,
        ),
        device_id: device_id.to_string(),
        source_id: SourceId(key.source_id.clone()),
        provider: first.provider.clone(),
        provider_account_id: account_id,
        day: key.day.clone(),
        kind,
        entity_key: entity_key.to_string(),
        display_name: first.display_name.clone(),
        family: first.family,
        mcp_server: first.mcp_server.clone(),
        mcp_tool: first.mcp_tool.clone(),
        plugin: first.plugin.clone(),
        skill_catalog: first.skill_catalog,
        calls,
        succeeded,
        failed,
        unknown,
        duration_samples,
        duration_sum_ms,
        duration_max_ms,
        duration_buckets,
        duration_kind,
        first_seen,
        last_seen,
        evidence: first.evidence.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use statsai_core::{
        ActivityCoverageLevel, ActivityDurationKind, ActivityFamily, ActivityKind, ActivityOutcome,
        LocationOrigin, ACTIVITY_COVERAGE_SCHEMA_VERSION, ACTIVITY_INVOCATION_SCHEMA_VERSION,
        ACTIVITY_PARSER_REVISION,
    };

    fn test_source() -> statsai_core::SourceLocation {
        statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-activity"),
            LocationOrigin::Configured,
        )
    }

    #[test]
    fn persist_mode_for_scan_uses_replace_files_unless_full_or_opencode() {
        assert_eq!(
            ActivityPersistMode::for_scan("codex", false),
            ActivityPersistMode::ReplaceFiles
        );
        assert_eq!(
            ActivityPersistMode::for_scan("codex", true),
            ActivityPersistMode::ReplaceSource
        );
        assert_eq!(
            ActivityPersistMode::for_scan("opencode", false),
            ActivityPersistMode::Upsert
        );
        assert_eq!(
            ActivityPersistMode::for_scan("opencode", true),
            ActivityPersistMode::ReplaceSource
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn invocation(
        id: &str,
        source: &statsai_core::SourceLocation,
        day_hour: u32,
        kind: ActivityKind,
        name: &str,
        family: ActivityFamily,
        outcome: ActivityOutcome,
        duration_ms: Option<u64>,
        file_hash: &str,
    ) -> ActivityInvocationV1 {
        ActivityInvocationV1 {
            schema_version: ACTIVITY_INVOCATION_SCHEMA_VERSION.to_string(),
            invocation_id: id.to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: None,
            source_file_path_hash: file_hash.to_string(),
            observed_at: Utc
                .with_ymd_and_hms(2026, 1, 1, day_hour, 0, 0)
                .single()
                .expect("ts"),
            kind,
            display_name: name.to_string(),
            family,
            mcp_server: None,
            mcp_tool: None,
            plugin: None,
            skill_catalog: None,
            outcome,
            duration_ms,
            duration_kind: duration_ms.map(|_| ActivityDurationKind::Reported),
            evidence: "codex-native-items".to_string(),
            parser_revision: ACTIVITY_PARSER_REVISION.to_string(),
        }
    }

    fn coverage(
        source: &statsai_core::SourceLocation,
        kind: ActivityKind,
        day: &str,
    ) -> ActivityCoverageV1 {
        ActivityCoverageV1 {
            schema_version: ACTIVITY_COVERAGE_SCHEMA_VERSION.to_string(),
            coverage_id: format!("coverage-{kind}-{day}", kind = kind.as_str()),
            device_id: "device".to_string(),
            source_id: source.source_id.clone(),
            provider: "codex".to_string(),
            day: day.to_string(),
            day_end: day.to_string(),
            kind,
            level: ActivityCoverageLevel::Complete,
            evidence: "codex-native-items".to_string(),
            parser_revision: ACTIVITY_PARSER_REVISION.to_string(),
        }
    }

    #[test]
    fn persist_is_idempotent_and_conserves_counts() {
        let store = Store::in_memory().expect("store");
        let source = test_source();
        store.upsert_source(&source).expect("source");
        let rows = vec![
            invocation(
                "a",
                &source,
                1,
                ActivityKind::Tool,
                "exec",
                ActivityFamily::Shell,
                ActivityOutcome::Succeeded,
                Some(12),
                "file-a",
            ),
            invocation(
                "b",
                &source,
                2,
                ActivityKind::Tool,
                "exec",
                ActivityFamily::Shell,
                ActivityOutcome::Failed,
                Some(40),
                "file-a",
            ),
        ];
        let cover = vec![coverage(&source, ActivityKind::Tool, "2026-01-01")];
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &rows,
                &cover,
                &["file-a".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("first");
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &rows,
                &cover,
                &["file-a".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("second");
        assert_eq!(store.activity_invocation_count().expect("count"), 2);
        let rollups = store.all_activity_rollups().expect("rollups");
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].calls, 2);
        assert_eq!(rollups[0].succeeded, 1);
        assert_eq!(rollups[0].failed, 1);
        assert_eq!(
            rollups[0].duration_buckets.iter().sum::<u64>(),
            rollups[0].duration_samples
        );
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &rows,
                &cover,
                &["file-a".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("third");
        assert_eq!(store.activity_invocation_count().expect("count"), 2);
        let rollups = store.all_activity_rollups().expect("rollups");
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].calls, 2);
        assert_eq!(
            rollups.iter().map(|row| row.calls).sum::<u64>(),
            store.activity_invocation_count().expect("count")
        );
    }

    #[test]
    fn rollup_picks_majority_family_when_one_entity_disagrees() {
        let store = Store::in_memory().expect("store");
        let source = test_source();
        store.upsert_source(&source).expect("source");
        let mut rows = Vec::new();
        for (id, family) in [
            ("a", ActivityFamily::Shell),
            ("b", ActivityFamily::Shell),
            ("c", ActivityFamily::Shell),
            ("d", ActivityFamily::FileRead),
        ] {
            rows.push(invocation(
                id,
                &source,
                1,
                ActivityKind::Tool,
                "exec",
                family,
                ActivityOutcome::Succeeded,
                None,
                "file-a",
            ));
        }
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &rows,
                &[],
                &["file-a".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("persist");
        let rollups = store.all_activity_rollups().expect("rollups");
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].family, ActivityFamily::Shell);
        assert_eq!(rollups[0].calls, 4);
    }

    #[test]
    fn replace_files_keeps_invocations_from_untouched_files() {
        let store = Store::in_memory().expect("store");
        let source = test_source();
        store.upsert_source(&source).expect("source");
        let day1 = invocation(
            "a",
            &source,
            1,
            ActivityKind::Tool,
            "exec",
            ActivityFamily::Shell,
            ActivityOutcome::Succeeded,
            Some(5),
            "file-a",
        );
        let mut day2 = invocation(
            "b",
            &source,
            2,
            ActivityKind::Tool,
            "exec",
            ActivityFamily::Shell,
            ActivityOutcome::Succeeded,
            Some(8),
            "file-b",
        );
        day2.observed_at = Utc
            .with_ymd_and_hms(2026, 1, 2, 0, 0, 0)
            .single()
            .expect("ts");
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[day1.clone(), day2.clone()],
                &[
                    coverage(&source, ActivityKind::Tool, "2026-01-01"),
                    coverage(&source, ActivityKind::Tool, "2026-01-02"),
                ],
                &["file-a".to_string(), "file-b".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("first");
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[day2],
                &[coverage(&source, ActivityKind::Tool, "2026-01-02")],
                &["file-b".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("incremental");
        assert_eq!(store.activity_invocation_count().expect("count"), 2);
        let rollups = store.all_activity_rollups().expect("rollups");
        assert_eq!(rollups.len(), 2);
    }

    #[test]
    fn incremental_replace_files_keeps_zero_call_coverage() {
        let store = Store::in_memory().expect("store");
        let source = test_source();
        store.upsert_source(&source).expect("source");
        let tool = invocation(
            "a",
            &source,
            1,
            ActivityKind::Tool,
            "exec",
            ActivityFamily::Shell,
            ActivityOutcome::Succeeded,
            Some(5),
            "file-a",
        );
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                std::slice::from_ref(&tool),
                &[
                    coverage(&source, ActivityKind::Tool, "2026-01-01"),
                    coverage(&source, ActivityKind::Mcp, "2026-01-01"),
                    coverage(&source, ActivityKind::Skill, "2026-01-01"),
                ],
                &["file-a".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("day1");
        let mut day2 = tool.clone();
        day2.invocation_id = "b".to_string();
        day2.source_file_path_hash = "file-b".to_string();
        day2.observed_at = Utc
            .with_ymd_and_hms(2026, 1, 2, 0, 0, 0)
            .single()
            .expect("ts");
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[day2],
                &[coverage(&source, ActivityKind::Tool, "2026-01-02")],
                &["file-b".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("day2");
        let coverage_rows = store.all_activity_coverage().expect("coverage");
        assert_eq!(coverage_rows.len(), 4);
        assert!(coverage_rows
            .iter()
            .any(|row| { row.day == "2026-01-01" && row.kind == ActivityKind::Mcp }));
        assert!(coverage_rows
            .iter()
            .any(|row| { row.day == "2026-01-01" && row.kind == ActivityKind::Skill }));
    }

    #[test]
    fn persist_drops_incomplete_mcp_invocations() {
        let store = Store::in_memory().expect("store");
        let source = test_source();
        store.upsert_source(&source).expect("source");
        let mut incomplete = invocation(
            "mcp-missing",
            &source,
            1,
            ActivityKind::Mcp,
            "mcp",
            ActivityFamily::Mcp,
            ActivityOutcome::Unknown,
            None,
            "file-a",
        );
        incomplete.mcp_server = Some("server".to_string());
        incomplete.mcp_tool = None;
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[incomplete],
                &[],
                &["file-a".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("persist");
        assert_eq!(store.activity_invocation_count().expect("count"), 0);
    }

    #[test]
    fn file_removal_drops_invocations_and_rollups() {
        let store = Store::in_memory().expect("store");
        let source = test_source();
        store.upsert_source(&source).expect("source");
        let keep = invocation(
            "keep",
            &source,
            1,
            ActivityKind::Tool,
            "exec",
            ActivityFamily::Shell,
            ActivityOutcome::Succeeded,
            Some(5),
            "keep",
        );
        let gone = invocation(
            "gone",
            &source,
            1,
            ActivityKind::Tool,
            "web_search",
            ActivityFamily::Web,
            ActivityOutcome::Unknown,
            None,
            "gone",
        );
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[keep.clone(), gone],
                &[coverage(&source, ActivityKind::Tool, "2026-01-01")],
                &["keep".to_string(), "gone".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("insert");
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[keep],
                &[coverage(&source, ActivityKind::Tool, "2026-01-01")],
                &["gone".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("remove");
        assert_eq!(store.activity_invocation_count().expect("count"), 1);
        let rollups = store.all_activity_rollups().expect("rollups");
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].display_name, "exec");
        assert_eq!(rollups[0].calls, 1);
    }

    #[test]
    fn upsert_heals_running_to_completed() {
        let store = Store::in_memory().expect("store");
        let source = test_source();
        store.upsert_source(&source).expect("source");
        let mut running = invocation(
            "part-1",
            &source,
            1,
            ActivityKind::Tool,
            "bash",
            ActivityFamily::Shell,
            ActivityOutcome::Unknown,
            None,
            "db",
        );
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[running.clone()],
                &[],
                &[],
                ActivityPersistMode::Upsert,
                Some(ActivityScanCursor {
                    last_time_updated: 10,
                    parser_revision: ACTIVITY_PARSER_REVISION.to_string(),
                }),
            )
            .expect("running");
        running.outcome = ActivityOutcome::Succeeded;
        running.duration_ms = Some(9);
        running.duration_kind = Some(ActivityDurationKind::Reported);
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[running],
                &[],
                &[],
                ActivityPersistMode::Upsert,
                Some(ActivityScanCursor {
                    last_time_updated: 20,
                    parser_revision: ACTIVITY_PARSER_REVISION.to_string(),
                }),
            )
            .expect("completed");
        assert_eq!(store.activity_invocation_count().expect("count"), 1);
        let rollups = store.all_activity_rollups().expect("rollups");
        assert_eq!(rollups[0].calls, 1);
        assert_eq!(rollups[0].succeeded, 1);
        assert_eq!(rollups[0].unknown, 0);
        let cursor = store
            .activity_scan_cursor(&source.source_id)
            .expect("cursor")
            .expect("present");
        assert_eq!(cursor.last_time_updated, 20);
    }

    #[test]
    fn replace_source_rebuilds_from_full_set() {
        let store = Store::in_memory().expect("store");
        let source = test_source();
        store.upsert_source(&source).expect("source");
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[invocation(
                    "old",
                    &source,
                    1,
                    ActivityKind::Tool,
                    "old",
                    ActivityFamily::Other,
                    ActivityOutcome::Unknown,
                    None,
                    "db",
                )],
                &[],
                &[],
                ActivityPersistMode::ReplaceSource,
                None,
            )
            .expect("old");
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[invocation(
                    "new",
                    &source,
                    1,
                    ActivityKind::Tool,
                    "read",
                    ActivityFamily::FileRead,
                    ActivityOutcome::Succeeded,
                    Some(1),
                    "db",
                )],
                &[],
                &[],
                ActivityPersistMode::ReplaceSource,
                None,
            )
            .expect("new");
        assert_eq!(store.activity_invocation_count().expect("count"), 1);
        assert_eq!(
            store.all_activity_rollups().expect("rollups")[0].display_name,
            "read"
        );
    }

    #[test]
    fn replace_source_drops_per_day_coverage_inside_a_coalesced_range() {
        let store = Store::in_memory().expect("store");
        let source = test_source();
        store.upsert_source(&source).expect("source");
        let day1 = invocation(
            "a",
            &source,
            1,
            ActivityKind::Tool,
            "exec",
            ActivityFamily::Shell,
            ActivityOutcome::Succeeded,
            Some(1),
            "file-a",
        );
        let mut day2 = day1.clone();
        day2.invocation_id = "b".to_string();
        day2.observed_at = Utc
            .with_ymd_and_hms(2026, 1, 2, 1, 0, 0)
            .single()
            .expect("ts");
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[day1.clone(), day2.clone()],
                &[
                    coverage(&source, ActivityKind::Tool, "2026-01-01"),
                    coverage(&source, ActivityKind::Tool, "2026-01-02"),
                ],
                &["file-a".to_string()],
                ActivityPersistMode::ReplaceSource,
                None,
            )
            .expect("per-day");
        assert_eq!(store.all_activity_coverage().expect("before").len(), 2);

        let mut range = coverage(&source, ActivityKind::Tool, "2026-01-01");
        range.day_end = "2026-01-02".to_string();
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[day1, day2],
                &[range],
                &["file-a".to_string()],
                ActivityPersistMode::ReplaceSource,
                None,
            )
            .expect("range");
        let rows = store.all_activity_coverage().expect("after");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].day, "2026-01-01");
        assert_eq!(rows[0].day_end, "2026-01-02");
    }

    #[test]
    fn migrates_from_schema_24() {
        let conn = rusqlite::Connection::open_in_memory().expect("conn");
        crate::migrations::migrate(&conn).expect("migrate");
        assert!(conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'activity_invocations'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count")
            > 0);
    }

    #[test]
    fn mark_all_dirty_then_clear_on_sync() {
        let store = Store::in_memory().expect("store");
        let source = test_source();
        store.upsert_source(&source).expect("source");
        store
            .persist_activity_scan(
                "device",
                &source.source_id,
                &[invocation(
                    "a",
                    &source,
                    1,
                    ActivityKind::Tool,
                    "exec",
                    ActivityFamily::Shell,
                    ActivityOutcome::Succeeded,
                    Some(3),
                    "file",
                )],
                &[],
                &["file".to_string()],
                ActivityPersistMode::ReplaceFiles,
                None,
            )
            .expect("insert");
        let marked = store.mark_all_activity_rollups_dirty().expect("dirty");
        assert_eq!(marked, 1);
        let dirty = store.dirty_activity_rollups().expect("list");
        assert_eq!(dirty.len(), 1);
        store
            .mark_activity_rollups_synced(&[dirty[0].rollup_id.clone()])
            .expect("synced");
        assert!(store.dirty_activity_rollups().expect("after").is_empty());
    }
}
