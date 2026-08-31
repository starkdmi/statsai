use super::{assignment_for_timestamp, Store};
use anyhow::Result;
use chrono::{DateTime, Duration, Timelike, Utc};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use statsai_core::{
    hash_text, periods_overlap, EventId, ProviderAccountId, QuotaChangePointV1,
    QuotaCycleContributionV1, QuotaDailyEnvelopeV1, QuotaObservationRecordV1, QuotaObservationV1,
    QuotaProjectionStatusV1, QuotaTransitionKind, QuotaUsageLinkKind, QuotaUsageSliceV1,
    QuotaUsageTotalsV1, QuotaWindowObservationV1, QuotaWindowSyncProjectionV1, QuotaWindowV1,
    SourceAccountAssignment, SourceId, QUOTA_CYCLE_CONTRIBUTION_SCHEMA_VERSION,
    QUOTA_WEEKLY_WINDOW_MINUTES, QUOTA_WINDOW_SCHEMA_VERSION,
    QUOTA_WINDOW_SYNC_PROJECTION_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, HashMap, HashSet};

mod cycles;
mod reconstruct;

pub(crate) use cycles::*;
pub(crate) use reconstruct::*;

const RESET_CLUSTER_TOLERANCE_SECONDS: i64 = 5 * 60;
/// A window cannot be observed once it has reset: the provider issues a fresh
/// `resets_at` from that moment on. An observation that post-dates its own reset
/// by more than provider recomputation lag is therefore a replay of historical
/// evidence recorded at import time, and must not extend the closed window.
///
/// The bound is an hour because the provider has been seen serving an elapsed
/// window for 37 continuous minutes: roughly 180 separate requests, seconds
/// apart, every one still carrying the expired `resets_at` at a steady 96%.
/// Discarding those loses the closing figure of a cycle spent to exhaustion.
/// Genuine lag has never been observed to outlast that; the stale records past
/// it sit 14 hours to 3 days late, which is import time, not lag.
const STALE_OBSERVATION_TOLERANCE_SECONDS: i64 = 60 * 60;
pub const SYNC_ELIGIBLE_WINDOW_MINUTES: u64 = QUOTA_WEEKLY_WINDOW_MINUTES;

#[derive(Debug, Clone, Default)]
pub struct QuotaQuery {
    pub provider: Option<String>,
    pub provider_account_id: Option<ProviderAccountId>,
    pub source_id: Option<SourceId>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuotaDateRange {
    pub first: DateTime<Utc>,
    pub last: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaStatus {
    pub schema_version: String,
    pub total_observations: u64,
    pub distinct_observations: u64,
    pub duplicate_observations: u64,
    pub attributed_observations: u64,
    pub unattributed_observations: u64,
    pub attributed_range: Option<QuotaDateRange>,
    pub unattributed_range: Option<QuotaDateRange>,
    pub weekly_observations: u64,
    pub weekly_sync_eligible_observations: u64,
    pub weekly_sync_eligible_coverage_percent: f64,
    pub discarded: QuotaDiscardCounts,
    pub assignment_overlap_warnings: Vec<String>,
}

/// What reconstruction threw away, and why.
///
/// Each rule discards evidence that cannot describe a real cycle, and each is
/// silent by design. Counting the discards is what tells a provider change
/// apart from a quiet week: if these numbers climb, the rules have started
/// firing on data they were never meant to judge.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct QuotaDiscardCounts {
    /// Observations recorded more than an hour after the window they describe
    /// had already reset.
    pub replayed_observations: u64,
    /// Windows that reported zero for their whole life and were superseded.
    pub unused_windows: u64,
    /// Schedules another schedule was reported both before and after.
    pub bracketed_schedules: u64,
}

impl Store {
    pub fn clear_orphaned_quota_usage_links(&self) -> Result<u64> {
        Ok(self.conn.execute(
            r#"
            UPDATE quota_observations
            SET usage_event_id = NULL,
                usage_link_kind = 'none',
                payload = json_set(
                  payload,
                  '$.usage_event_id', NULL,
                  '$.usage_link_kind', 'none'
                )
            WHERE usage_event_id IS NOT NULL
              AND NOT EXISTS (
                SELECT 1 FROM usage_events e
                WHERE e.event_id = quota_observations.usage_event_id
              )
            "#,
            [],
        )? as u64)
    }

    pub fn upsert_quota_observations(&self, records: &[QuotaObservationRecordV1]) -> Result<u64> {
        self.with_immediate_transaction(|| self.upsert_quota_observations_inner(records))
    }

    pub(crate) fn upsert_quota_observations_inner(
        &self,
        records: &[QuotaObservationRecordV1],
    ) -> Result<u64> {
        let mut written = 0u64;
        let mut assignments_by_source = HashMap::new();
        for record in records {
            let source_id = &record.observation.source_id;
            if !assignments_by_source.contains_key(source_id) {
                assignments_by_source.insert(
                    source_id.clone(),
                    self.list_source_account_assignments_for_source(source_id)?,
                );
            }
            let mut record = record.clone();
            let assignments = assignments_by_source
                .get(source_id)
                .expect("source assignments inserted above");
            record.observation.provider_account_id =
                assignment_for_timestamp(assignments, record.observation.observed_at)
                    .map(|assignment| assignment.provider_account_id.clone());

            if record.observation.usage_event_id.is_none() {
                let existing = self
                        .conn
                        .query_row(
                            "SELECT usage_event_id, usage_link_kind, payload FROM quota_observations WHERE observation_id = ?1",
                            [&record.observation.observation_id],
                            |row| {
                                Ok((
                                    row.get::<_, Option<String>>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, String>(2)?,
                                ))
                            },
                        )
                        .optional()?;
                if let Some((Some(event_id), link_kind, payload)) = existing {
                    let existing_observation: QuotaObservationV1 = serde_json::from_str(&payload)?;
                    if matching_positive_usage_sample(&record.observation, &existing_observation) {
                        record.observation.usage_event_id = Some(EventId(event_id));
                        record.observation.usage_link_kind = parse_usage_link_kind(&link_kind);
                    }
                }
            }

            self.conn.execute(
                r#"
                    INSERT OR IGNORE INTO quota_payloads
                      (payload_hash, provider, payload, created_at)
                    VALUES (?1, ?2, ?3, ?4)
                    "#,
                params![
                    &record.observation.payload_hash,
                    &record.observation.provider,
                    serde_json::to_string(&record.raw_rate_limits)?,
                    Utc::now().to_rfc3339(),
                ],
            )?;
            let observation_payload = serde_json::to_string(&record.observation)?;
            let observation_changed = self.conn.execute(
                r#"
                    INSERT INTO quota_observations (
                      observation_id, semantic_fingerprint, provider, source_id,
                      provider_account_id, observed_at, source_file_path_hash,
                      source_record_id, usage_event_id, usage_link_kind, payload_hash, payload
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                    ON CONFLICT(observation_id) DO UPDATE SET
                      semantic_fingerprint = excluded.semantic_fingerprint,
                      provider = excluded.provider,
                      source_id = excluded.source_id,
                      provider_account_id = excluded.provider_account_id,
                      observed_at = excluded.observed_at,
                      source_file_path_hash = excluded.source_file_path_hash,
                      source_record_id = excluded.source_record_id,
                      usage_event_id = excluded.usage_event_id,
                      usage_link_kind = excluded.usage_link_kind,
                      payload_hash = excluded.payload_hash,
                      payload = excluded.payload
                    WHERE quota_observations.semantic_fingerprint IS NOT excluded.semantic_fingerprint
                       OR quota_observations.provider IS NOT excluded.provider
                       OR quota_observations.source_id IS NOT excluded.source_id
                       OR quota_observations.provider_account_id IS NOT excluded.provider_account_id
                       OR quota_observations.observed_at IS NOT excluded.observed_at
                       OR quota_observations.source_file_path_hash IS NOT excluded.source_file_path_hash
                       OR quota_observations.source_record_id IS NOT excluded.source_record_id
                       OR quota_observations.usage_event_id IS NOT excluded.usage_event_id
                       OR quota_observations.usage_link_kind IS NOT excluded.usage_link_kind
                       OR quota_observations.payload_hash IS NOT excluded.payload_hash
                       OR quota_observations.payload IS NOT excluded.payload
                    "#,
                params![
                    &record.observation.observation_id,
                    &record.observation.semantic_fingerprint,
                    &record.observation.provider,
                    &record.observation.source_id.0,
                    record
                        .observation
                        .provider_account_id
                        .as_ref()
                        .map(|id| id.0.as_str()),
                    record.observation.observed_at.to_rfc3339(),
                    &record.observation.source_file_path_hash,
                    &record.observation.source_record_id,
                    record
                        .observation
                        .usage_event_id
                        .as_ref()
                        .map(|id| id.0.as_str()),
                    usage_link_kind_label(record.observation.usage_link_kind),
                    &record.observation.payload_hash,
                    observation_payload,
                ],
            )? > 0;
            let mut incoming_windows = record
                .windows
                .iter()
                .map(|window| Ok((window, serde_json::to_string(window)?)))
                .collect::<Result<Vec<_>>>()?;
            incoming_windows.sort_by_key(|(window, _)| window.window_observation_id.as_str());
            let stored_windows = if observation_changed {
                Vec::new()
            } else {
                let mut statement = self.conn.prepare(
                    "SELECT window_observation_id, payload
                     FROM quota_window_observations
                     WHERE observation_id = ?1
                     ORDER BY window_observation_id",
                )?;
                let windows = statement
                    .query_map([&record.observation.observation_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                windows
            };
            let windows_changed = observation_changed
                || incoming_windows.len() != stored_windows.len()
                || incoming_windows.iter().zip(&stored_windows).any(
                    |((incoming_window, incoming_payload), (stored_id, stored_payload))| {
                        incoming_window.window_observation_id != *stored_id
                            || incoming_payload != stored_payload
                    },
                );
            if windows_changed {
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id = ?1",
                    [&record.observation.observation_id],
                )?;
                for (window, payload) in &incoming_windows {
                    self.conn.execute(
                        r#"
                        INSERT INTO quota_window_observations (
                          window_observation_id, observation_id, provider_slot, limit_id,
                          window_minutes, used_percent, resets_at, payload
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                        "#,
                        params![
                            &window.window_observation_id,
                            &window.observation_id,
                            &window.provider_slot,
                            window.limit_id.as_deref(),
                            i64::try_from(window.window_minutes).unwrap_or(i64::MAX),
                            window.used_percent,
                            window.resets_at_epoch_seconds,
                            payload,
                        ],
                    )?;
                }
            }
            written = written.saturating_add(1);
        }
        Ok(written)
    }

    pub(crate) fn replace_quota_observations_for_source_files_inner(
        &self,
        source_id: &SourceId,
        source_file_path_hashes: &[String],
        records: &[QuotaObservationRecordV1],
    ) -> Result<u64> {
        let source_file_path_hashes = source_file_path_hashes
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let retained_observation_ids = records
            .iter()
            .filter(|record| {
                record.observation.source_id == *source_id
                    && source_file_path_hashes
                        .contains(record.observation.source_file_path_hash.as_str())
            })
            .map(|record| record.observation.observation_id.as_str())
            .collect::<HashSet<_>>();

        for source_file_path_hash in source_file_path_hashes {
            let mut statement = self.conn.prepare(
                "SELECT observation_id FROM quota_observations
                 WHERE source_id = ?1 AND source_file_path_hash = ?2",
            )?;
            let stored_observation_ids = statement
                .query_map(params![&source_id.0, source_file_path_hash], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);

            for observation_id in stored_observation_ids {
                if retained_observation_ids.contains(observation_id.as_str()) {
                    continue;
                }
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id = ?1",
                    [&observation_id],
                )?;
                self.conn.execute(
                    "DELETE FROM quota_observations WHERE observation_id = ?1",
                    [&observation_id],
                )?;
            }
        }

        let written = self.upsert_quota_observations_inner(records)?;
        self.delete_unreferenced_quota_payloads()?;
        Ok(written)
    }

    pub fn replace_quota_observations_for_source_files(
        &self,
        source_id: &SourceId,
        source_file_path_hashes: &[String],
        records: &[QuotaObservationRecordV1],
    ) -> Result<u64> {
        self.with_immediate_transaction(|| {
            self.replace_quota_observations_for_source_files_inner(
                source_id,
                source_file_path_hashes,
                records,
            )
        })
    }

    pub fn quota_observations(
        &self,
        query: &QuotaQuery,
        collapse_duplicates: bool,
    ) -> Result<Vec<QuotaObservationRecordV1>> {
        let mut observations = BTreeMap::<String, QuotaObservationRecordV1>::new();
        let observation_sql = if query.source_id.is_some() {
            r#"
            SELECT q.payload, q.provider_account_id, q.usage_event_id, q.usage_link_kind,
                   p.payload
            FROM quota_observations q
            JOIN quota_payloads p ON p.payload_hash = q.payload_hash
            WHERE q.source_id = ?1
            ORDER BY q.observed_at, q.observation_id
            "#
        } else {
            r#"
            SELECT q.payload, q.provider_account_id, q.usage_event_id, q.usage_link_kind,
                   p.payload
            FROM quota_observations q
            JOIN quota_payloads p ON p.payload_hash = q.payload_hash
            ORDER BY q.observed_at, q.observation_id
            "#
        };
        let source_bindings = query
            .source_id
            .as_ref()
            .map(|source_id| source_id.0.as_str())
            .into_iter();
        let mut statement = self.conn.prepare(observation_sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(source_bindings))?;
        while let Some(row) = rows.next()? {
            let payload = row.get::<_, String>(0)?;
            let account_id = row.get::<_, Option<String>>(1)?;
            let usage_event_id = row.get::<_, Option<String>>(2)?;
            let usage_link_kind = row.get::<_, String>(3)?;
            let raw_payload = row.get::<_, String>(4)?;
            let mut observation: QuotaObservationV1 = serde_json::from_str(&payload)?;
            observation.provider_account_id = account_id.map(ProviderAccountId);
            observation.usage_event_id = usage_event_id.map(EventId);
            observation.usage_link_kind = parse_usage_link_kind(&usage_link_kind);
            if !observation_matches_query(&observation, query) {
                continue;
            }
            observations.insert(
                observation.observation_id.clone(),
                QuotaObservationRecordV1 {
                    observation,
                    windows: Vec::new(),
                    raw_rate_limits: serde_json::from_str(&raw_payload)?,
                },
            );
        }
        drop(rows);
        drop(statement);

        let window_sql = if query.source_id.is_some() {
            r#"
            SELECT window.observation_id, window.payload
            FROM quota_observations observation INDEXED BY quota_observations_source_idx
            CROSS JOIN quota_window_observations window
              ON window.observation_id = observation.observation_id
            WHERE observation.source_id = ?1
            ORDER BY window.resets_at, window.window_observation_id
            "#
        } else {
            "SELECT observation_id, payload FROM quota_window_observations ORDER BY resets_at, window_observation_id"
        };
        let source_bindings = query
            .source_id
            .as_ref()
            .map(|source_id| source_id.0.as_str())
            .into_iter();
        let mut statement = self.conn.prepare(window_sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(source_bindings))?;
        while let Some(row) = rows.next()? {
            let observation_id = row.get::<_, String>(0)?;
            let payload = row.get::<_, String>(1)?;
            let Some(record) = observations.get_mut(&observation_id) else {
                continue;
            };
            let window: QuotaWindowObservationV1 = serde_json::from_str(&payload)?;
            if query
                .limit_id
                .as_deref()
                .is_some_and(|limit| window.limit_id.as_deref() != Some(limit))
            {
                continue;
            }
            record.windows.push(window);
        }
        let mut records = observations.into_values().collect::<Vec<_>>();
        records.sort_by_key(|record| {
            (
                record.observation.observed_at,
                record.observation.observation_id.clone(),
            )
        });
        Ok(if collapse_duplicates {
            collapse_semantic_duplicates(records)
        } else {
            records
        })
    }

    pub fn delete_quota_observations_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for source_id in source_ids {
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id IN (SELECT observation_id FROM quota_observations WHERE source_id = ?1)",
                    [&source_id.0],
                )?;
                deleted = deleted.saturating_add(self.conn.execute(
                    "DELETE FROM quota_observations WHERE source_id = ?1",
                    [&source_id.0],
                )? as u64);
            }
            self.delete_unreferenced_quota_payloads()?;
            Ok(deleted)
        })
    }

    pub fn delete_quota_observations_for_source_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
    ) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for file_hash in file_hashes {
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id IN (SELECT observation_id FROM quota_observations WHERE source_id = ?1 AND source_file_path_hash = ?2)",
                    params![&source_id.0, file_hash],
                )?;
                deleted = deleted.saturating_add(self.conn.execute(
                    "DELETE FROM quota_observations WHERE source_id = ?1 AND source_file_path_hash = ?2",
                    params![&source_id.0, file_hash],
                )? as u64);
            }
            self.delete_unreferenced_quota_payloads()?;
            Ok(deleted)
        })
    }

    /// Deletes this source's quota rows that no file in `source_file_path_hashes` accounts for.
    ///
    /// A full rescan replaces every file it can see, but `replace_quota_observations_for_source_files`
    /// only reconciles the hashes handed to it, so rows written for a file that has since disappeared
    /// -- and legacy rows stored before file hashes were recorded -- outlive the scan that should have
    /// retired them. Deleting the whole source instead would retire them, and that is what `--no-cache`
    /// used to do: on a store with six figures of observations it walked every row, every window, and
    /// the payload table, which is the stall this replaces. The unmatched set is normally empty, and the
    /// source index covers the lookup, so the common case reads an index range and writes nothing.
    pub fn delete_quota_observations_for_source_outside_file_hashes(
        &self,
        source_id: &SourceId,
        source_file_path_hashes: &[String],
    ) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let retained = source_file_path_hashes
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>();
            let mut statement = self.conn.prepare(
                "SELECT observation_id, source_file_path_hash FROM quota_observations
                 WHERE source_id = ?1",
            )?;
            let orphaned = statement
                .query_map([&source_id.0], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(_, file_hash)| !retained.contains(file_hash.as_str()))
                .map(|(observation_id, _)| observation_id)
                .collect::<Vec<_>>();
            drop(statement);
            if orphaned.is_empty() {
                return Ok(0);
            }
            let mut deleted = 0u64;
            for observation_id in orphaned {
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id = ?1",
                    [&observation_id],
                )?;
                deleted = deleted.saturating_add(self.conn.execute(
                    "DELETE FROM quota_observations WHERE observation_id = ?1",
                    [&observation_id],
                )? as u64);
            }
            self.delete_unreferenced_quota_payloads()?;
            Ok(deleted)
        })
    }

    fn delete_unreferenced_quota_payloads(&self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM quota_payloads WHERE payload_hash NOT IN (SELECT payload_hash FROM quota_observations)",
            [],
        )?;
        Ok(())
    }

    pub fn reattribute_quota_observations(&self, source_id: &SourceId) -> Result<u64> {
        let assignments = self.list_source_account_assignments_for_source(source_id)?;
        let records = self.quota_observations_for_source(source_id)?;
        self.with_immediate_transaction(|| {
            let mut changed = 0u64;
            for mut record in records {
                let account_id = assignment_for_timestamp(
                    &assignments,
                    record.observation.observed_at,
                )
                .map(|assignment| assignment.provider_account_id.clone());
                if record.observation.provider_account_id == account_id {
                    continue;
                }
                record.observation.provider_account_id = account_id;
                changed = changed.saturating_add(self.conn.execute(
                    "UPDATE quota_observations SET provider_account_id = ?2, payload = ?3 WHERE observation_id = ?1",
                    params![
                        &record.observation.observation_id,
                        record.observation.provider_account_id.as_ref().map(|id| id.0.as_str()),
                        serde_json::to_string(&record.observation)?,
                    ],
                )? as u64);
            }
            Ok(changed)
        })
    }

    pub(crate) fn quota_observations_for_source(
        &self,
        source_id: &SourceId,
    ) -> Result<Vec<QuotaObservationRecordV1>> {
        self.quota_observations(
            &QuotaQuery {
                source_id: Some(source_id.clone()),
                ..QuotaQuery::default()
            },
            false,
        )
    }

    pub fn quota_status(&self, query: &QuotaQuery) -> Result<QuotaStatus> {
        let (_, discarded) = self.reconstruct_quota_windows_counted(query)?;
        let raw = self.quota_observations(query, false)?;
        let total_observations = raw.len();
        let distinct = collapse_semantic_duplicates(raw);
        let attributed = distinct
            .iter()
            .filter(|record| record.observation.provider_account_id.is_some())
            .collect::<Vec<_>>();
        let unattributed = distinct
            .iter()
            .filter(|record| record.observation.provider_account_id.is_none())
            .collect::<Vec<_>>();
        let weekly = distinct
            .iter()
            .filter(|record| {
                record
                    .windows
                    .iter()
                    .any(|window| window.window_minutes == SYNC_ELIGIBLE_WINDOW_MINUTES)
            })
            .collect::<Vec<_>>();
        let weekly_eligible = weekly
            .iter()
            .filter(|record| record.observation.provider_account_id.is_some())
            .count() as u64;
        let assignments = self
            .list_source_account_assignments()?
            .into_iter()
            .filter(|assignment| assignment_matches_query(assignment, query))
            .collect::<Vec<_>>();
        let mut warnings = Vec::new();
        for (index, left) in assignments.iter().enumerate() {
            for right in assignments.iter().skip(index + 1) {
                if left.source_id == right.source_id
                    && periods_overlap(
                        left.started_at,
                        left.ended_at,
                        right.started_at,
                        right.ended_at,
                    )
                {
                    warnings.push(format!(
                        "source {} assignments {} and {} overlap",
                        left.source_id.0, left.assignment_id.0, right.assignment_id.0
                    ));
                }
            }
        }
        Ok(QuotaStatus {
            schema_version: "quota_status.v1".to_string(),
            total_observations: total_observations as u64,
            distinct_observations: distinct.len() as u64,
            duplicate_observations: total_observations.saturating_sub(distinct.len()) as u64,
            attributed_observations: attributed.len() as u64,
            unattributed_observations: unattributed.len() as u64,
            attributed_range: observation_range(&attributed),
            unattributed_range: observation_range(&unattributed),
            weekly_observations: weekly.len() as u64,
            weekly_sync_eligible_observations: weekly_eligible,
            weekly_sync_eligible_coverage_percent: if weekly.is_empty() {
                100.0
            } else {
                weekly_eligible as f64 * 100.0 / weekly.len() as f64
            },
            discarded,
            assignment_overlap_warnings: warnings,
        })
    }

    pub fn quota_windows(&self, query: &QuotaQuery) -> Result<Vec<QuotaWindowV1>> {
        let mut windows = self.quota_windows_without_usage_totals(query)?;
        self.enrich_quota_window_usage_totals(&mut windows)?;
        Ok(windows)
    }

    pub fn quota_windows_without_usage_totals(
        &self,
        query: &QuotaQuery,
    ) -> Result<Vec<QuotaWindowV1>> {
        Ok(self
            .reconstruct_quota_windows(query)?
            .into_iter()
            .map(|reconstructed| reconstructed.window)
            .collect())
    }

    fn reconstruct_quota_windows(
        &self,
        query: &QuotaQuery,
    ) -> Result<Vec<ReconstructedQuotaWindow>> {
        let (windows, _) = self.reconstruct_quota_windows_counted(query)?;
        Ok(windows)
    }

    fn reconstruct_quota_windows_counted(
        &self,
        query: &QuotaQuery,
    ) -> Result<(Vec<ReconstructedQuotaWindow>, QuotaDiscardCounts)> {
        let mut discarded = QuotaDiscardCounts::default();
        let mut reconstruction_query = query.clone();
        reconstruction_query.from = None;
        reconstruction_query.to = None;
        let records = self.quota_observations(&reconstruction_query, true)?;
        let mut grouped = BTreeMap::<WindowScope, Vec<WindowPoint>>::new();
        for record in records {
            for window in &record.windows {
                if observation_postdates_reset(&record.observation, window) {
                    discarded.replayed_observations += 1;
                    continue;
                }
                grouped
                    .entry(WindowScope {
                        provider: record.observation.provider.clone(),
                        provider_account_id: record.observation.provider_account_id.clone(),
                        source_id: record
                            .observation
                            .provider_account_id
                            .is_none()
                            .then(|| record.observation.source_id.clone()),
                        limit_id: window.limit_id.clone(),
                        window_minutes: window.window_minutes,
                    })
                    .or_default()
                    .push(WindowPoint {
                        observation: record.observation.clone(),
                        window: window.clone(),
                    });
            }
        }

        let mut windows = Vec::new();
        for (scope, mut points) in grouped {
            points.sort_by_key(|point| {
                (
                    point.window.resets_at_epoch_seconds,
                    point.observation.observed_at,
                )
            });
            // The tolerance is a gap between neighbouring resets, not a width
            // the whole cycle must fit inside. A cycle's reported reset drifts
            // a second or two at a time and has been seen wandering nine
            // minutes over its life; measured against the cluster's first
            // point that cycle is cut in two the moment it passes five, and
            // the remainder is drawn as a second cycle running beside the
            // first over the same days. Consecutive resets sit hours apart at
            // the very least, so chaining cannot reach across one.
            let mut clusters: Vec<Vec<WindowPoint>> = Vec::new();
            for point in points {
                if let Some(cluster) = clusters.last_mut() {
                    let previous_reset = cluster
                        .last()
                        .map(|point| point.window.resets_at_epoch_seconds)
                        .unwrap_or(point.window.resets_at_epoch_seconds);
                    if point.window.resets_at_epoch_seconds - previous_reset
                        <= RESET_CLUSTER_TOLERANCE_SECONDS
                    {
                        cluster.push(point);
                        continue;
                    }
                }
                clusters.push(vec![point]);
            }

            // A window that reported zero usage for its whole life and was then
            // replaced never functioned as a cycle: nothing was ever attributed
            // to it, so it cannot be told apart from a reschedule of the window
            // that followed. Provider-side resets during the July 2026 tier
            // migration left runs of these minutes apart, each one drawn as a
            // separate reset that never happened. The final cluster is exempt,
            // because a cycle that has only just begun legitimately reads zero.
            let cluster_count = clusters.len();
            let clusters = clusters
                .into_iter()
                .enumerate()
                .filter(|(index, cluster)| {
                    *index + 1 == cluster_count
                        || cluster.iter().any(|point| point.window.used_percent > 0.0)
                })
                .map(|(_, cluster)| cluster)
                .collect::<Vec<_>>();
            discarded.unused_windows += (cluster_count - clusters.len()) as u64;

            let phantoms = phantom_cluster_indices(&clusters);
            discarded.bracketed_schedules += phantoms.len() as u64;
            let clusters = clusters
                .into_iter()
                .enumerate()
                .filter(|(index, _)| !phantoms.contains(index))
                .map(|(_, cluster)| cluster)
                .collect::<Vec<_>>();

            let schedules = clusters
                .iter()
                .map(|cluster| QuotaClusterSchedule::from_points(&scope, cluster))
                .collect::<Vec<_>>();
            let mut scope_windows = Vec::with_capacity(clusters.len());
            for (index, (cluster, schedule)) in
                clusters.into_iter().zip(schedules.iter()).enumerate()
            {
                if !quota_cluster_matches_time_query(schedule, query) {
                    continue;
                }
                let daily_envelopes = daily_envelopes_from_points(&cluster);
                let mut window = Self::materialize_quota_window(&scope, schedule, cluster);
                window.transition = if index == 0 {
                    QuotaTransitionKind::Initial
                } else if schedule.first_observed_at < schedules[index - 1].representative_reset {
                    QuotaTransitionKind::Early
                } else {
                    QuotaTransitionKind::OnOrAfterPreviousSchedule
                };
                window.has_schedule_overlap = schedules.iter().enumerate().any(|(other, peer)| {
                    other != index
                        && schedule.inferred_start < peer.representative_reset
                        && schedule.representative_reset > peer.inferred_start
                });
                scope_windows.push(ReconstructedQuotaWindow {
                    window,
                    daily_envelopes,
                });
            }
            windows.extend(scope_windows);
        }
        windows.sort_by(|left, right| {
            right
                .window
                .representative_reset
                .cmp(&left.window.representative_reset)
                .then_with(|| right.window.window_minutes.cmp(&left.window.window_minutes))
                .then_with(|| left.window.window_id.cmp(&right.window.window_id))
        });
        Ok((windows, discarded))
    }

    fn materialize_quota_window(
        scope: &WindowScope,
        schedule: &QuotaClusterSchedule,
        mut points: Vec<WindowPoint>,
    ) -> QuotaWindowV1 {
        points.sort_by_key(|point| point.observation.observed_at);
        let mut change_points = Vec::new();
        for point in &points {
            let changed = change_points
                .last()
                .is_none_or(|previous: &QuotaChangePointV1| {
                    previous.used_percent != point.window.used_percent
                        || previous.resets_at_epoch_seconds != point.window.resets_at_epoch_seconds
                });
            if !changed {
                continue;
            }
            let point_fingerprint = quota_point_fingerprint(scope, point);
            change_points.push(QuotaChangePointV1 {
                observed_at: point.observation.observed_at,
                used_percent: point.window.used_percent,
                resets_at: point.window.resets_at,
                resets_at_epoch_seconds: point.window.resets_at_epoch_seconds,
                point_fingerprint,
                provider_slot: point.window.provider_slot.clone(),
            });
        }
        let first = points.first().expect("non-empty reset cluster");
        let latest = points.last().expect("non-empty reset cluster");
        let anchor = change_points
            .first()
            .expect("non-empty reset cluster has a change point");
        let identity_scope = scope.provider_account_id.as_ref().map_or_else(
            || {
                format!(
                    "unassigned:{}",
                    scope
                        .source_id
                        .as_ref()
                        .map(|id| id.0.as_str())
                        .unwrap_or("unknown")
                )
            },
            |id| id.0.clone(),
        );
        let window_id = format!(
            "quota_window_{}",
            &hash_text(&format!(
                "quota_window.v1:{}:{identity_scope}:{}:{}:{}",
                scope.provider,
                scope.limit_id.as_deref().unwrap_or("default"),
                scope.window_minutes,
                anchor.point_fingerprint
            ))[..32]
        );
        QuotaWindowV1 {
            schema_version: QUOTA_WINDOW_SCHEMA_VERSION.to_string(),
            window_id,
            provider: scope.provider.clone(),
            provider_account_id: scope.provider_account_id.clone(),
            source_id: scope.source_id.clone(),
            limit_id: scope.limit_id.clone(),
            window_minutes: scope.window_minutes,
            inferred_start: schedule.inferred_start,
            representative_reset: schedule.representative_reset,
            representative_reset_epoch_seconds: schedule.representative_reset_epoch_seconds,
            reset_min: schedule.reset_min,
            reset_min_epoch_seconds: schedule.reset_min_epoch_seconds,
            reset_max: schedule.reset_max,
            reset_max_epoch_seconds: schedule.reset_max_epoch_seconds,
            first_observed_at: first.observation.observed_at,
            last_observed_at: latest.observation.observed_at,
            sample_count: points.len() as u64,
            first_used_percent: first.window.used_percent,
            latest_used_percent: latest.window.used_percent,
            minimum_used_percent: points
                .iter()
                .map(|point| point.window.used_percent)
                .fold(f64::INFINITY, f64::min),
            maximum_used_percent: points
                .iter()
                .map(|point| point.window.used_percent)
                .fold(f64::NEG_INFINITY, f64::max),
            transition: QuotaTransitionKind::Initial,
            has_schedule_overlap: false,
            change_points,
            latest_status: latest.observation.status.clone(),
            usage_totals: None,
        }
    }

    pub fn enrich_quota_window_usage_totals(&self, windows: &mut [QuotaWindowV1]) -> Result<()> {
        let mut windows_by_account =
            HashMap::<String, HashMap<ProviderAccountId, Vec<usize>>>::new();
        for (index, window) in windows.iter_mut().enumerate() {
            let Some(account_id) = window.provider_account_id.clone() else {
                window.usage_totals = None;
                continue;
            };
            window.usage_totals = Some(QuotaUsageTotalsV1::default());
            windows_by_account
                .entry(window.provider.clone())
                .or_default()
                .entry(account_id)
                .or_default()
                .push(index);
        }
        if windows_by_account.is_empty() {
            return Ok(());
        }

        let first_start = windows
            .iter()
            .filter(|window| window.provider_account_id.is_some())
            .map(|window| window.inferred_start)
            .min()
            .expect("attributed window index is non-empty");
        let last_reset = windows
            .iter()
            .filter(|window| window.provider_account_id.is_some())
            .map(|window| window.representative_reset)
            .max()
            .expect("attributed window index is non-empty");

        for event in self.events_in_period(Some(first_start), last_reset)? {
            let Some(account_id) = event.provider_account_id.as_ref() else {
                continue;
            };
            let Some(indexes) = windows_by_account
                .get(event.provider.as_str())
                .and_then(|accounts| accounts.get(account_id))
            else {
                continue;
            };
            for &index in indexes {
                let window = &mut windows[index];
                if event.session.started_at < window.inferred_start
                    || event.session.started_at >= window.representative_reset
                {
                    continue;
                }
                let totals = window
                    .usage_totals
                    .as_mut()
                    .expect("attributed windows receive totals above");
                totals.event_count = totals.event_count.saturating_add(1);
                totals.total_tokens = totals
                    .total_tokens
                    .saturating_add(event.usage.computed_total());
                if let Some(value) = event
                    .cost
                    .provider_reported_micro_usd_value()
                    .or_else(|| event.cost.estimated_micro_usd())
                {
                    totals.estimated_cost_micro_usd = Some(
                        totals
                            .estimated_cost_micro_usd
                            .unwrap_or_default()
                            .saturating_add(value),
                    );
                }
            }
        }
        Ok(())
    }

    pub fn quota_sync_projections(
        &self,
        query: &QuotaQuery,
        device_id: &str,
    ) -> Result<Vec<QuotaWindowSyncProjectionV1>> {
        Ok(self
            .quota_windows_without_usage_totals(query)?
            .into_iter()
            .filter(|window| window.window_minutes == SYNC_ELIGIBLE_WINDOW_MINUTES)
            .filter_map(|window| {
                let provider_account_id = window.provider_account_id.clone()?;
                let anchor = window.change_points.first()?;
                let projection_id = format!(
                    "quota_projection_{}",
                    &hash_text(&format!(
                        "quota_projection.v1:{device_id}:{}:{}:{}:{}:{}",
                        window.provider,
                        provider_account_id.0,
                        window.limit_id.as_deref().unwrap_or("default"),
                        window.window_minutes,
                        anchor.point_fingerprint
                    ))[..32]
                );
                Some(QuotaWindowSyncProjectionV1 {
                    schema_version: QUOTA_WINDOW_SYNC_PROJECTION_SCHEMA_VERSION.to_string(),
                    projection_id,
                    device_id: device_id.to_string(),
                    provider: window.provider,
                    provider_account_id,
                    limit_id: window.limit_id,
                    window_minutes: window.window_minutes,
                    inferred_start: window.inferred_start,
                    representative_reset: window.representative_reset,
                    representative_reset_epoch_seconds: window.representative_reset_epoch_seconds,
                    reset_min: window.reset_min,
                    reset_min_epoch_seconds: window.reset_min_epoch_seconds,
                    reset_max: window.reset_max,
                    reset_max_epoch_seconds: window.reset_max_epoch_seconds,
                    first_observed_at: window.first_observed_at,
                    last_observed_at: window.last_observed_at,
                    sample_count: window.sample_count,
                    first_used_percent: window.first_used_percent,
                    latest_used_percent: window.latest_used_percent,
                    minimum_used_percent: window.minimum_used_percent,
                    maximum_used_percent: window.maximum_used_percent,
                    change_points: window.change_points,
                    latest_status: QuotaProjectionStatusV1::from(&window.latest_status),
                })
            })
            .collect())
    }

    pub fn quota_cycle_contributions(
        &self,
        query: &QuotaQuery,
        device_id: &str,
    ) -> Result<Vec<QuotaCycleContributionV1>> {
        let reconstructed = self.reconstruct_quota_windows(query)?;
        let mut contributions = Vec::new();
        let mut pending = Vec::new();
        for item in reconstructed {
            if item.window.window_minutes != QUOTA_WEEKLY_WINDOW_MINUTES {
                continue;
            }
            let Some(provider_account_id) = item.window.provider_account_id.clone() else {
                continue;
            };
            let Some(anchor) = item.window.change_points.first() else {
                continue;
            };
            let contribution_id = format!(
                "quota_cycle_{}",
                &hash_text(&format!(
                    "quota_cycle_contribution.v1:{device_id}:{}:{}:{}:{}:{}",
                    item.window.provider,
                    provider_account_id.0,
                    item.window.limit_id.as_deref().unwrap_or("default"),
                    item.window.window_minutes,
                    anchor.point_fingerprint
                ))[..32]
            );
            pending.push(PendingQuotaCycleContribution {
                contribution_id,
                provider_account_id,
                reconstructed: item,
            });
        }

        let effective_bounds = effective_cycle_bounds(
            pending
                .iter()
                .map(|item| &item.reconstructed.window)
                .collect::<Vec<_>>(),
        );
        let mut slices_by_window = HashMap::<String, Vec<QuotaUsageSliceV1>>::new();
        if !pending.is_empty() {
            let first_start = pending
                .iter()
                .map(|item| {
                    effective_bounds
                        .get(&item.reconstructed.window.window_id)
                        .map(|bounds| bounds.start)
                        .unwrap_or(item.reconstructed.window.inferred_start)
                })
                .min()
                .expect("pending contributions are non-empty");
            let last_end = pending
                .iter()
                .map(|item| {
                    effective_bounds
                        .get(&item.reconstructed.window.window_id)
                        .map(|bounds| bounds.end)
                        .unwrap_or(item.reconstructed.window.representative_reset)
                })
                .max()
                .expect("pending contributions are non-empty");
            let mut slice_builders = pending
                .iter()
                .map(|item| {
                    let bounds = effective_bounds
                        .get(&item.reconstructed.window.window_id)
                        .copied()
                        .unwrap_or(CycleBounds {
                            start: item.reconstructed.window.inferred_start,
                            end: item.reconstructed.window.representative_reset,
                        });
                    (
                        item.reconstructed.window.window_id.clone(),
                        boundary_slice_builders(
                            bounds.start,
                            bounds.end,
                            &item.reconstructed.window.provider,
                            item.reconstructed
                                .window
                                .provider_account_id
                                .as_ref()
                                .expect("attributed cycle"),
                        ),
                    )
                })
                .collect::<Vec<_>>();

            for event in self.events_in_period(Some(first_start), last_end)? {
                let Some(account_id) = event.provider_account_id.as_ref() else {
                    continue;
                };
                let estimated_cost = event.cost.estimated_micro_usd();
                for (_, builders) in &mut slice_builders {
                    if event.provider != builders.provider || account_id != &builders.account_id {
                        continue;
                    }
                    for slice in builders.slices.iter_mut() {
                        if event.session.started_at < slice.period_start
                            || event.session.started_at >= slice.period_end
                        {
                            continue;
                        }
                        slice.add_usage(&event.usage, estimated_cost);
                    }
                }
            }

            for (window_id, builders) in slice_builders {
                slices_by_window.insert(window_id, builders.slices);
            }
        }

        for item in pending {
            let boundary_slices = slices_by_window
                .remove(&item.reconstructed.window.window_id)
                .unwrap_or_default();
            contributions.push(QuotaCycleContributionV1 {
                schema_version: QUOTA_CYCLE_CONTRIBUTION_SCHEMA_VERSION.to_string(),
                contribution_id: item.contribution_id,
                provider: item.reconstructed.window.provider,
                provider_account_id: item.provider_account_id,
                limit_id: item.reconstructed.window.limit_id,
                window_minutes: item.reconstructed.window.window_minutes,
                representative_reset: item.reconstructed.window.representative_reset,
                representative_reset_epoch_seconds: item
                    .reconstructed
                    .window
                    .representative_reset_epoch_seconds,
                has_schedule_overlap: item.reconstructed.window.has_schedule_overlap,
                daily_envelopes: item.reconstructed.daily_envelopes,
                boundary_slices,
            });
        }
        Ok(contributions)
    }

    pub fn pending_quota_cycle_contributions_for_sync(
        &self,
        sink: &str,
        target: &str,
        contributions: &[QuotaCycleContributionV1],
    ) -> Result<Vec<QuotaCycleContributionV1>> {
        let mut pending = Vec::new();
        for contribution in contributions {
            let payload = serde_json::to_string(contribution)?;
            if self.entity_requires_sync(
                sink,
                target,
                "quota_cycle_contribution",
                &contribution.contribution_id,
                &hash_text(&payload),
            )? {
                pending.push(contribution.clone());
            }
        }
        Ok(pending)
    }
}

#[cfg(test)]
mod tests;
