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

const RESET_CLUSTER_TOLERANCE_SECONDS: i64 = 5 * 60;
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
    pub assignment_overlap_warnings: Vec<String>,
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
            self.conn.execute(
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
            )?;
            self.conn.execute(
                "DELETE FROM quota_window_observations WHERE observation_id = ?1",
                [&record.observation.observation_id],
            )?;
            for window in &record.windows {
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
                        serde_json::to_string(window)?,
                    ],
                )?;
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
        let retained_observation_ids = records
            .iter()
            .filter(|record| {
                record.observation.source_id == *source_id
                    && source_file_path_hashes.contains(&record.observation.source_file_path_hash)
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

    pub fn quota_observations(
        &self,
        query: &QuotaQuery,
        collapse_duplicates: bool,
    ) -> Result<Vec<QuotaObservationRecordV1>> {
        let mut observations = BTreeMap::<String, QuotaObservationRecordV1>::new();
        let mut statement = self.conn.prepare(
            r#"
            SELECT q.payload, q.provider_account_id, q.usage_event_id, q.usage_link_kind,
                   p.payload
            FROM quota_observations q
            JOIN quota_payloads p ON p.payload_hash = q.payload_hash
            ORDER BY q.observed_at, q.observation_id
            "#,
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (payload, account_id, usage_event_id, usage_link_kind, raw_payload) = row?;
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
        drop(statement);

        let mut statement = self.conn.prepare(
            "SELECT observation_id, payload FROM quota_window_observations ORDER BY resets_at, window_observation_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (observation_id, payload) = row?;
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

    fn quota_observations_for_source(
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
                    .any(|window| window.window_minutes >= SYNC_ELIGIBLE_WINDOW_MINUTES)
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
        let mut reconstruction_query = query.clone();
        reconstruction_query.from = None;
        reconstruction_query.to = None;
        let records = self.quota_observations(&reconstruction_query, true)?;
        let mut grouped = BTreeMap::<WindowScope, Vec<WindowPoint>>::new();
        for record in records {
            for window in &record.windows {
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
            let mut clusters: Vec<Vec<WindowPoint>> = Vec::new();
            for point in points {
                if let Some(cluster) = clusters.last_mut() {
                    let min_reset = cluster
                        .first()
                        .map(|point| point.window.resets_at_epoch_seconds)
                        .unwrap_or(point.window.resets_at_epoch_seconds);
                    if point.window.resets_at_epoch_seconds - min_reset
                        <= RESET_CLUSTER_TOLERANCE_SECONDS
                    {
                        cluster.push(point);
                        continue;
                    }
                }
                clusters.push(vec![point]);
            }

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
        Ok(windows)
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
            .filter(|window| window.window_minutes >= SYNC_ELIGIBLE_WINDOW_MINUTES)
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct WindowScope {
    provider: String,
    provider_account_id: Option<ProviderAccountId>,
    source_id: Option<SourceId>,
    limit_id: Option<String>,
    window_minutes: u64,
}

#[derive(Debug, Clone)]
struct WindowPoint {
    observation: QuotaObservationV1,
    window: QuotaWindowObservationV1,
}

#[derive(Debug, Clone)]
struct QuotaClusterSchedule {
    inferred_start: DateTime<Utc>,
    representative_reset: DateTime<Utc>,
    representative_reset_epoch_seconds: i64,
    reset_min: DateTime<Utc>,
    reset_min_epoch_seconds: i64,
    reset_max: DateTime<Utc>,
    reset_max_epoch_seconds: i64,
    first_observed_at: DateTime<Utc>,
    last_observed_at: DateTime<Utc>,
}

impl QuotaClusterSchedule {
    fn from_points(scope: &WindowScope, points: &[WindowPoint]) -> Self {
        let first = points.first().expect("non-empty reset cluster");
        let last = points.last().expect("non-empty reset cluster");
        let representative = &points[points.len() / 2];
        let representative_reset = representative.window.resets_at;
        Self {
            inferred_start: representative_reset
                - Duration::minutes(i64::try_from(scope.window_minutes).unwrap_or(i64::MAX)),
            representative_reset,
            representative_reset_epoch_seconds: representative.window.resets_at_epoch_seconds,
            reset_min: first.window.resets_at,
            reset_min_epoch_seconds: first.window.resets_at_epoch_seconds,
            reset_max: last.window.resets_at,
            reset_max_epoch_seconds: last.window.resets_at_epoch_seconds,
            first_observed_at: points
                .iter()
                .map(|point| point.observation.observed_at)
                .min()
                .expect("non-empty reset cluster"),
            last_observed_at: points
                .iter()
                .map(|point| point.observation.observed_at)
                .max()
                .expect("non-empty reset cluster"),
        }
    }
}

#[derive(Debug, Clone)]
struct ReconstructedQuotaWindow {
    window: QuotaWindowV1,
    daily_envelopes: Vec<QuotaDailyEnvelopeV1>,
}

struct PendingQuotaCycleContribution {
    contribution_id: String,
    provider_account_id: ProviderAccountId,
    reconstructed: ReconstructedQuotaWindow,
}

#[derive(Debug, Clone, Copy)]
struct CycleBounds {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

struct BoundarySliceBuilders {
    provider: String,
    account_id: ProviderAccountId,
    slices: Vec<QuotaUsageSliceV1>,
}

fn daily_envelopes_from_points(points: &[WindowPoint]) -> Vec<QuotaDailyEnvelopeV1> {
    let mut grouped = BTreeMap::<String, Vec<&WindowPoint>>::new();
    for point in points {
        grouped
            .entry(point.observation.observed_at.date_naive().to_string())
            .or_default()
            .push(point);
    }
    grouped
        .into_iter()
        .map(|(day, mut day_points)| {
            day_points.sort_by_key(|point| point.observation.observed_at);
            let first = day_points.first().expect("non-empty utc day");
            let last = day_points.last().expect("non-empty utc day");
            QuotaDailyEnvelopeV1 {
                day,
                first_observed_at: first.observation.observed_at,
                first_used_percent: first.window.used_percent,
                last_observed_at: last.observation.observed_at,
                last_used_percent: last.window.used_percent,
                minimum_used_percent: day_points
                    .iter()
                    .map(|point| point.window.used_percent)
                    .fold(f64::INFINITY, f64::min),
                maximum_used_percent: day_points
                    .iter()
                    .map(|point| point.window.used_percent)
                    .fold(f64::NEG_INFINITY, f64::max),
            }
        })
        .collect()
}

fn utc_day_start(timestamp: DateTime<Utc>) -> DateTime<Utc> {
    timestamp
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc()
}

fn is_utc_midnight(timestamp: DateTime<Utc>) -> bool {
    timestamp.time().num_seconds_from_midnight() == 0 && timestamp.nanosecond() == 0
}

fn next_utc_day_start(timestamp: DateTime<Utc>) -> DateTime<Utc> {
    utc_day_start(timestamp) + Duration::days(1)
}

fn empty_usage_slice(period_start: DateTime<Utc>, period_end: DateTime<Utc>) -> QuotaUsageSliceV1 {
    QuotaUsageSliceV1 {
        period_start,
        period_end,
        ..QuotaUsageSliceV1::default()
    }
}

fn boundary_slice_builders(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    provider: &str,
    account_id: &ProviderAccountId,
) -> BoundarySliceBuilders {
    let mut slices = Vec::new();
    if start >= end {
        return BoundarySliceBuilders {
            provider: provider.to_string(),
            account_id: account_id.clone(),
            slices,
        };
    }
    if start.date_naive() == end.date_naive() {
        if !is_utc_midnight(start) || !is_utc_midnight(end) {
            slices.push(empty_usage_slice(start, end));
        }
        return BoundarySliceBuilders {
            provider: provider.to_string(),
            account_id: account_id.clone(),
            slices,
        };
    }
    if !is_utc_midnight(start) {
        slices.push(empty_usage_slice(start, next_utc_day_start(start)));
    }
    if !is_utc_midnight(end) {
        slices.push(empty_usage_slice(utc_day_start(end), end));
    }
    BoundarySliceBuilders {
        provider: provider.to_string(),
        account_id: account_id.clone(),
        slices,
    }
}

fn clamp_datetime(value: DateTime<Utc>, min: DateTime<Utc>, max: DateTime<Utc>) -> DateTime<Utc> {
    value.max(min).min(max)
}

fn effective_cycle_bounds(windows: Vec<&QuotaWindowV1>) -> HashMap<String, CycleBounds> {
    let mut grouped = BTreeMap::<(String, String, Option<String>, u64), Vec<&QuotaWindowV1>>::new();
    for window in windows {
        let Some(account_id) = window.provider_account_id.as_ref() else {
            continue;
        };
        grouped
            .entry((
                window.provider.clone(),
                account_id.0.clone(),
                window.limit_id.clone(),
                window.window_minutes,
            ))
            .or_default()
            .push(window);
    }
    let mut bounds = HashMap::new();
    for mut scope_windows in grouped.into_values() {
        scope_windows.sort_by_key(|window| {
            (
                window.inferred_start,
                window.representative_reset,
                window.window_id.clone(),
            )
        });
        for window in &scope_windows {
            bounds.insert(
                window.window_id.clone(),
                CycleBounds {
                    start: window.inferred_start,
                    end: window.representative_reset,
                },
            );
        }
        for index in 1..scope_windows.len() {
            let previous = scope_windows[index - 1];
            let current = scope_windows[index];
            let overlap_start = previous.inferred_start.max(current.inferred_start);
            let overlap_end = previous
                .representative_reset
                .min(current.representative_reset);
            if overlap_start >= overlap_end {
                continue;
            }
            let transition = clamp_datetime(current.first_observed_at, overlap_start, overlap_end);
            if let Some(previous_bounds) = bounds.get_mut(&previous.window_id) {
                previous_bounds.end = previous_bounds.end.min(transition);
            }
            if let Some(current_bounds) = bounds.get_mut(&current.window_id) {
                current_bounds.start = current_bounds.start.max(transition);
            }
        }
    }
    bounds
}

fn quota_point_fingerprint(scope: &WindowScope, point: &WindowPoint) -> String {
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
    hash_text(&format!(
        "quota_point.v1:{}:{}:{}:{}:{}:{}:{}",
        scope.provider,
        identity_scope,
        scope.limit_id.as_deref().unwrap_or("default"),
        scope.window_minutes,
        point.observation.observed_at.to_rfc3339(),
        point.window.used_percent,
        point.window.resets_at_epoch_seconds,
    ))
}

fn observation_matches_query(observation: &QuotaObservationV1, query: &QuotaQuery) -> bool {
    query
        .provider
        .as_deref()
        .is_none_or(|provider| observation.provider == provider)
        && query
            .provider_account_id
            .as_ref()
            .is_none_or(|account| observation.provider_account_id.as_ref() == Some(account))
        && query
            .source_id
            .as_ref()
            .is_none_or(|source_id| &observation.source_id == source_id)
        && query
            .from
            .is_none_or(|from| observation.observed_at >= from)
        && query.to.is_none_or(|to| observation.observed_at <= to)
}

fn assignment_matches_query(assignment: &SourceAccountAssignment, query: &QuotaQuery) -> bool {
    query
        .provider
        .as_deref()
        .is_none_or(|provider| assignment.provider == provider)
        && query
            .provider_account_id
            .as_ref()
            .is_none_or(|account| &assignment.provider_account_id == account)
        && query
            .source_id
            .as_ref()
            .is_none_or(|source_id| &assignment.source_id == source_id)
        && query
            .from
            .is_none_or(|from| assignment.ended_at.is_none_or(|ended_at| ended_at > from))
        && query.to.is_none_or(|to| assignment.started_at <= to)
}

fn quota_cluster_matches_time_query(schedule: &QuotaClusterSchedule, query: &QuotaQuery) -> bool {
    query
        .from
        .is_none_or(|from| schedule.last_observed_at >= from)
        && query.to.is_none_or(|to| schedule.first_observed_at <= to)
}

fn collapse_semantic_duplicates(
    records: Vec<QuotaObservationRecordV1>,
) -> Vec<QuotaObservationRecordV1> {
    let mut semantic_groups = HashMap::<String, Vec<QuotaObservationRecordV1>>::new();
    for record in records {
        semantic_groups
            .entry(record.observation.semantic_fingerprint.clone())
            .or_default()
            .push(record);
    }

    let mut collapsed = Vec::new();
    for group in semantic_groups.into_values() {
        let attributed_accounts = group
            .iter()
            .filter_map(|record| record.observation.provider_account_id.as_ref())
            .collect::<HashSet<_>>();
        if attributed_accounts.is_empty() {
            let mut source_scoped = HashMap::<SourceId, QuotaObservationRecordV1>::new();
            for record in group {
                let source_id = record.observation.source_id.clone();
                match source_scoped.get_mut(&source_id) {
                    Some(existing)
                        if observation_quality(&record) > observation_quality(existing) =>
                    {
                        *existing = record;
                    }
                    Some(_) => {}
                    None => {
                        source_scoped.insert(source_id, record);
                    }
                }
            }
            collapsed.extend(source_scoped.into_values());
            continue;
        }
        if attributed_accounts.len() == 1 {
            if let Some(best) = group.into_iter().max_by_key(observation_quality) {
                collapsed.push(best);
            }
            continue;
        }

        let mut account_scoped =
            HashMap::<Option<ProviderAccountId>, QuotaObservationRecordV1>::new();
        for record in group {
            let account_id = record.observation.provider_account_id.clone();
            if account_id.is_none() {
                continue;
            }
            match account_scoped.get_mut(&account_id) {
                Some(existing) if observation_quality(&record) > observation_quality(existing) => {
                    *existing = record;
                }
                Some(_) => {}
                None => {
                    account_scoped.insert(account_id, record);
                }
            }
        }
        collapsed.extend(account_scoped.into_values());
    }

    collapsed.sort_by_key(|record| {
        (
            record.observation.observed_at,
            record.observation.observation_id.clone(),
        )
    });
    collapsed
}

fn observation_quality(record: &QuotaObservationRecordV1) -> (bool, bool, usize) {
    (
        record.observation.provider_account_id.is_some(),
        record.observation.usage_event_id.is_some(),
        record.windows.len(),
    )
}

fn matching_positive_usage_sample(
    incoming: &QuotaObservationV1,
    existing: &QuotaObservationV1,
) -> bool {
    incoming.usage_sample.as_ref().is_some_and(|sample| {
        sample.computed_total() > 0 && existing.usage_sample.as_ref() == Some(sample)
    })
}

fn observation_range(records: &[&QuotaObservationRecordV1]) -> Option<QuotaDateRange> {
    Some(QuotaDateRange {
        first: records
            .iter()
            .map(|record| record.observation.observed_at)
            .min()?,
        last: records
            .iter()
            .map(|record| record.observation.observed_at)
            .max()?,
    })
}

fn usage_link_kind_label(kind: QuotaUsageLinkKind) -> &'static str {
    match kind {
        QuotaUsageLinkKind::RecordEvent => "record_event",
        QuotaUsageLinkKind::TurnEvent => "turn_event",
        QuotaUsageLinkKind::None => "none",
    }
}

fn parse_usage_link_kind(value: &str) -> QuotaUsageLinkKind {
    match value {
        "record_event" => QuotaUsageLinkKind::RecordEvent,
        "turn_event" => QuotaUsageLinkKind::TurnEvent,
        _ => QuotaUsageLinkKind::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use statsai_core::{
        event_id, Confidence, CostInfo, EventSource, IdentitySource, LocationOrigin, PrivacyInfo,
        PrivacyMode, QuotaCreditsV1, QuotaStatusV1, SessionInfo, SourceAccountAssignment,
        SourceAccountAssignmentId, SourceKind, SourceLocation, UsageCounts, UsageEvent,
        SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION, USAGE_EVENT_SCHEMA_VERSION,
    };
    use std::collections::HashSet;
    use std::path::Path;

    #[allow(clippy::too_many_arguments)]
    fn sample_record(
        source_id: SourceId,
        observation_id: &str,
        semantic_fingerprint: &str,
        observed_at: DateTime<Utc>,
        reset_epoch: i64,
        slot: &str,
        window_minutes: u64,
        used_percent: f64,
    ) -> QuotaObservationRecordV1 {
        let raw_rate_limits = serde_json::json!({
            slot: {
                "window_minutes": window_minutes,
                "used_percent": used_percent,
                "resets_at": reset_epoch
            },
            "credits": {"balance": "5.00"}
        });
        let payload_hash = hash_text(&serde_json::to_string(&raw_rate_limits).expect("payload"));
        QuotaObservationRecordV1 {
            observation: QuotaObservationV1 {
                schema_version: "quota_observation.v1".to_string(),
                observation_id: observation_id.to_string(),
                semantic_fingerprint: semantic_fingerprint.to_string(),
                provider: "codex".to_string(),
                source_id,
                provider_account_id: None,
                observed_at,
                source_file_path_hash: format!("file-{observation_id}"),
                source_record_id: format!("record-{observation_id}"),
                source_line_number: 1,
                payload_hash,
                usage_sample: None,
                usage_event_id: None,
                usage_link_kind: QuotaUsageLinkKind::None,
                status: QuotaStatusV1 {
                    plan_type: Some("pro".to_string()),
                    credits: QuotaCreditsV1 {
                        balance: Some("5".to_string()),
                        balance_raw: Some(serde_json::json!("5.00")),
                        ..QuotaCreditsV1::default()
                    },
                    ..QuotaStatusV1::default()
                },
            },
            windows: vec![QuotaWindowObservationV1 {
                schema_version: "quota_window_observation.v1".to_string(),
                window_observation_id: format!("window-{observation_id}"),
                observation_id: observation_id.to_string(),
                provider_slot: slot.to_string(),
                limit_id: Some("subscription".to_string()),
                window_minutes,
                used_percent,
                resets_at: DateTime::from_timestamp(reset_epoch, 0).expect("reset"),
                resets_at_epoch_seconds: reset_epoch,
            }],
            raw_rate_limits,
        }
    }

    fn assigned_source(store: &Store, started_at: DateTime<Utc>) -> (SourceId, ProviderAccountId) {
        let source = SourceLocation::local_adapter(
            "codex",
            "codex-local-jsonl",
            "test",
            Path::new("/tmp/quota-source"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let account_id = ProviderAccountId("account-codex".to_string());
        let now = Utc::now();
        store
            .upsert_source_account_assignment(&SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: SourceAccountAssignmentId("assignment-quota".to_string()),
                source_id: source.source_id.clone(),
                provider: "codex".to_string(),
                provider_account_id: account_id.clone(),
                started_at,
                ended_at: None,
                record_source: IdentitySource::UserConfigured,
                verified_at: Some(now),
                created_at: now,
                updated_at: now,
            })
            .expect("assignment");
        (source.source_id, account_id)
    }

    #[test]
    fn semantic_collapse_prefers_attributed_linked_evidence() {
        let now = Utc::now();
        let base = QuotaObservationV1 {
            schema_version: "quota_observation.v1".to_string(),
            observation_id: "one".to_string(),
            semantic_fingerprint: "same".to_string(),
            provider: "codex".to_string(),
            source_id: SourceId("source-a".to_string()),
            provider_account_id: None,
            observed_at: now,
            source_file_path_hash: "file".to_string(),
            source_record_id: "record".to_string(),
            source_line_number: 1,
            payload_hash: "payload".to_string(),
            usage_sample: None,
            usage_event_id: None,
            usage_link_kind: QuotaUsageLinkKind::None,
            status: statsai_core::QuotaStatusV1::default(),
        };
        let mut better = base.clone();
        better.observation_id = "two".to_string();
        better.provider_account_id = Some(ProviderAccountId("account".to_string()));
        better.usage_event_id = Some(EventId("event".to_string()));
        let records = collapse_semantic_duplicates(vec![
            QuotaObservationRecordV1 {
                observation: base,
                windows: Vec::new(),
                raw_rate_limits: serde_json::json!({}),
            },
            QuotaObservationRecordV1 {
                observation: better,
                windows: Vec::new(),
                raw_rate_limits: serde_json::json!({}),
            },
        ]);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].observation.observation_id, "two");
    }

    #[test]
    fn quota_status_scopes_assignment_overlap_warnings_to_query() {
        let store = Store::in_memory().expect("store");
        let started_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
        for (source, provider, account, prefix) in [
            ("source-a", "codex", "account-a", "a"),
            ("source-b", "codex", "account-b", "b"),
            ("source-c", "claude", "account-c", "c"),
        ] {
            for (suffix, offset) in [("one", 0), ("two", 10)] {
                store
                    .upsert_source_account_assignment(&SourceAccountAssignment {
                        schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                        assignment_id: SourceAccountAssignmentId(format!(
                            "assignment-{prefix}-{suffix}"
                        )),
                        source_id: SourceId(source.to_string()),
                        provider: provider.to_string(),
                        provider_account_id: ProviderAccountId(account.to_string()),
                        started_at: started_at + Duration::seconds(offset),
                        ended_at: Some(started_at + Duration::seconds(offset + 20)),
                        record_source: IdentitySource::UserConfigured,
                        verified_at: Some(started_at),
                        created_at: started_at,
                        updated_at: started_at,
                    })
                    .expect("assignment");
            }
        }

        let account_status = store
            .quota_status(&QuotaQuery {
                provider_account_id: Some(ProviderAccountId("account-a".to_string())),
                ..QuotaQuery::default()
            })
            .expect("account status");
        assert_eq!(
            account_status.assignment_overlap_warnings,
            ["source source-a assignments assignment-a-one and assignment-a-two overlap"]
        );

        let provider_status = store
            .quota_status(&QuotaQuery {
                provider: Some("claude".to_string()),
                ..QuotaQuery::default()
            })
            .expect("provider status");
        assert_eq!(
            provider_status.assignment_overlap_warnings,
            ["source source-c assignments assignment-c-one and assignment-c-two overlap"]
        );

        let source_status = store
            .quota_status(&QuotaQuery {
                source_id: Some(SourceId("source-b".to_string())),
                ..QuotaQuery::default()
            })
            .expect("source status");
        assert_eq!(
            source_status.assignment_overlap_warnings,
            ["source source-b assignments assignment-b-one and assignment-b-two overlap"]
        );

        let later_status = store
            .quota_status(&QuotaQuery {
                from: Some(started_at + Duration::seconds(31)),
                ..QuotaQuery::default()
            })
            .expect("later status");
        assert!(later_status.assignment_overlap_warnings.is_empty());

        let earlier_status = store
            .quota_status(&QuotaQuery {
                to: Some(started_at - Duration::seconds(1)),
                ..QuotaQuery::default()
            })
            .expect("earlier status");
        assert!(earlier_status.assignment_overlap_warnings.is_empty());
    }

    #[test]
    fn quota_windows_keep_identical_observations_for_distinct_accounts() {
        let store = Store::in_memory().expect("store");
        let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");

        for suffix in ["a", "b"] {
            let source = SourceLocation::local_adapter(
                "codex",
                "codex-local-jsonl",
                suffix,
                Path::new(&format!("/tmp/quota-source-{suffix}")),
                LocationOrigin::Configured,
            );
            store.upsert_source(&source).expect("source");
            store
                .upsert_source_account_assignment(&SourceAccountAssignment {
                    schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                    assignment_id: SourceAccountAssignmentId(format!("assignment-{suffix}")),
                    source_id: source.source_id.clone(),
                    provider: "codex".to_string(),
                    provider_account_id: ProviderAccountId(format!("account-{suffix}")),
                    started_at: observed_at - Duration::seconds(1),
                    ended_at: None,
                    record_source: IdentitySource::UserConfigured,
                    verified_at: Some(observed_at),
                    created_at: observed_at,
                    updated_at: observed_at,
                })
                .expect("assignment");
            let record = sample_record(
                source.source_id,
                &format!("observation-{suffix}"),
                "same-semantic-fingerprint",
                observed_at,
                1_787_500_000,
                "primary",
                10_080,
                20.0,
            );
            store
                .upsert_quota_observations(&[record])
                .expect("quota observation");
        }

        let windows = store
            .quota_windows(&QuotaQuery::default())
            .expect("quota windows");
        assert_eq!(windows.len(), 2);
        assert_eq!(
            windows
                .iter()
                .filter_map(|window| window.provider_account_id.as_ref())
                .collect::<HashSet<_>>()
                .len(),
            2
        );
    }

    #[test]
    fn quota_windows_keep_identical_unattributed_observations_in_distinct_source_scopes() {
        let store = Store::in_memory().expect("store");
        let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");

        for suffix in ["a", "b"] {
            let source = SourceLocation::local_adapter(
                "codex",
                "codex-local-jsonl",
                suffix,
                Path::new(&format!("/tmp/quota-unattributed-source-{suffix}")),
                LocationOrigin::Configured,
            );
            store.upsert_source(&source).expect("source");
            store
                .upsert_quota_observations(&[sample_record(
                    source.source_id,
                    &format!("unattributed-{suffix}"),
                    "same-unattributed-semantic-fingerprint",
                    observed_at,
                    1_787_500_000,
                    "primary",
                    10_080,
                    20.0,
                )])
                .expect("quota observation");
        }

        let windows = store
            .quota_windows(&QuotaQuery::default())
            .expect("quota windows");
        assert_eq!(windows.len(), 2);
        assert_eq!(
            windows
                .iter()
                .map(|window| window.window_id.as_str())
                .collect::<HashSet<_>>()
                .len(),
            2
        );
        assert_eq!(
            windows
                .iter()
                .filter_map(|window| window.source_id.as_ref())
                .collect::<HashSet<_>>()
                .len(),
            2
        );
        assert!(windows.iter().all(|window| window.sample_count == 1));
    }

    #[test]
    fn time_filtered_windows_do_not_query_usage_for_filtered_out_clusters() {
        let store = Store::in_memory().expect("store");
        let old_observed_at = DateTime::from_timestamp(1_735_689_600, 0).expect("old observed");
        let old_reset = DateTime::from_timestamp(1_736_294_400, 0).expect("old reset");
        let recent_observed_at =
            DateTime::from_timestamp(1_787_000_000, 0).expect("recent observed");
        let recent_reset = DateTime::from_timestamp(1_787_500_000, 0).expect("recent reset");
        let (source_id, account_id) =
            assigned_source(&store, old_observed_at - Duration::seconds(1));
        store
            .upsert_quota_observations(&[
                sample_record(
                    source_id.clone(),
                    "old-window",
                    "old-window",
                    old_observed_at,
                    old_reset.timestamp(),
                    "primary",
                    10_080,
                    10.0,
                ),
                sample_record(
                    source_id.clone(),
                    "recent-window",
                    "recent-window",
                    recent_observed_at,
                    recent_reset.timestamp(),
                    "primary",
                    10_080,
                    20.0,
                ),
            ])
            .expect("quota observations");
        store
            .conn
            .execute(
                r#"
                INSERT INTO usage_events (
                  event_id, provider, source_id, provider_account_id, started_at,
                  total_tokens, semantic_fingerprint, payload
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
                params![
                    "invalid-old-event",
                    "codex",
                    &source_id.0,
                    &account_id.0,
                    (old_reset - Duration::days(1)).to_rfc3339(),
                    0,
                    "invalid-old-event",
                    "not-json"
                ],
            )
            .expect("invalid legacy event");

        let windows = store
            .quota_windows(&QuotaQuery {
                from: Some(recent_observed_at),
                ..QuotaQuery::default()
            })
            .expect("recent windows");

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].first_observed_at, recent_observed_at);

        let mut limited = store
            .quota_windows_without_usage_totals(&QuotaQuery::default())
            .expect("unenriched windows");
        assert_eq!(limited.len(), 2);
        limited.truncate(1);
        store
            .enrich_quota_window_usage_totals(&mut limited)
            .expect("enrich only the caller-selected recent window");
        assert_eq!(limited[0].first_observed_at, recent_observed_at);
    }

    #[test]
    fn quota_window_identity_and_evidence_are_stable_across_time_filters() {
        let store = Store::in_memory().expect("store");
        let first_observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
        let second_observed_at = first_observed_at + Duration::minutes(5);
        let (source_id, _) = assigned_source(&store, first_observed_at - Duration::seconds(1));
        store
            .upsert_quota_observations(&[
                sample_record(
                    source_id.clone(),
                    "stable-first",
                    "stable-first",
                    first_observed_at,
                    1_787_500_000,
                    "primary",
                    10_080,
                    10.0,
                ),
                sample_record(
                    source_id,
                    "stable-second",
                    "stable-second",
                    second_observed_at,
                    1_787_500_000,
                    "secondary",
                    10_080,
                    20.0,
                ),
            ])
            .expect("observations");

        let full = store
            .quota_windows(&QuotaQuery::default())
            .expect("full windows");
        let filtered = store
            .quota_windows(&QuotaQuery {
                from: Some(second_observed_at),
                ..QuotaQuery::default()
            })
            .expect("filtered windows");

        assert_eq!(full.len(), 1);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].window_id, full[0].window_id);
        assert_eq!(filtered[0].sample_count, 2);
        assert_eq!(filtered[0].first_used_percent, 10.0);
        assert_eq!(filtered[0].change_points, full[0].change_points);
    }

    #[test]
    fn semantic_collapse_drops_ambiguous_unattributed_copies() {
        let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
        let mut unassigned = sample_record(
            SourceId("source-unassigned".to_string()),
            "ambiguous-unassigned",
            "ambiguous-semantic",
            observed_at,
            1_787_500_000,
            "primary",
            10_080,
            20.0,
        );
        let mut account_a = unassigned.clone();
        account_a.observation.observation_id = "ambiguous-a".to_string();
        account_a.observation.provider_account_id =
            Some(ProviderAccountId("account-a".to_string()));
        let mut account_b = unassigned.clone();
        account_b.observation.observation_id = "ambiguous-b".to_string();
        account_b.observation.provider_account_id =
            Some(ProviderAccountId("account-b".to_string()));
        unassigned.observation.provider_account_id = None;

        let collapsed = collapse_semantic_duplicates(vec![unassigned, account_a, account_b]);

        assert_eq!(collapsed.len(), 2);
        assert!(collapsed
            .iter()
            .all(|record| record.observation.provider_account_id.is_some()));
    }

    #[test]
    fn two_device_projection_fixture_has_one_merge_scope_and_shared_point() {
        let fixture =
            include_str!("../../../docs/fixtures/quota_window_projection.v1.two_devices.json");
        let raw: serde_json::Value = serde_json::from_str(fixture).expect("fixture JSON");
        let projections: Vec<QuotaWindowSyncProjectionV1> =
            serde_json::from_str(fixture).expect("projection fixture");

        assert_eq!(projections.len(), 2);
        assert_ne!(projections[0].device_id, projections[1].device_id);
        assert_ne!(projections[0].projection_id, projections[1].projection_id);
        assert_eq!(projections[0].provider, projections[1].provider);
        assert_eq!(
            projections[0].provider_account_id,
            projections[1].provider_account_id
        );
        assert_eq!(projections[0].limit_id, projections[1].limit_id);
        assert_eq!(projections[0].window_minutes, projections[1].window_minutes);
        assert!(
            (projections[0].representative_reset_epoch_seconds
                - projections[1].representative_reset_epoch_seconds)
                .abs()
                <= RESET_CLUSTER_TOLERANCE_SECONDS
        );
        assert_eq!(
            projections[0].change_points[0].point_fingerprint,
            projections[1].change_points[0].point_fingerprint
        );

        for contribution in raw.as_array().expect("fixture array") {
            for forbidden in [
                "source_id",
                "usage_totals",
                "total_tokens",
                "estimated_cost_micro_usd",
                "raw_rate_limits",
            ] {
                assert!(contribution.get(forbidden).is_none(), "found {forbidden}");
            }
        }
    }

    #[test]
    fn quota_store_reuses_payloads_and_rescans_idempotently() {
        let store = Store::in_memory().expect("store");
        let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
        let source_id = SourceId("source-a".to_string());
        let record = sample_record(
            source_id,
            "observation-a",
            "semantic-a",
            observed_at,
            1_787_500_000,
            "primary",
            10_080,
            20.0,
        );
        store
            .upsert_quota_observations(std::slice::from_ref(&record))
            .expect("first upsert");
        store
            .upsert_quota_observations(std::slice::from_ref(&record))
            .expect("rescan");

        assert_eq!(
            store
                .quota_observations(&QuotaQuery::default(), false)
                .expect("observations")
                .len(),
            1
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM quota_payloads", [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("payload count"),
            1
        );
    }

    #[test]
    fn quota_upsert_preserves_usage_link_only_for_matching_positive_sample() {
        let store = Store::in_memory().expect("store");
        let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
        let mut linked = sample_record(
            SourceId("source-link".to_string()),
            "stable-line",
            "semantic-original",
            observed_at,
            1_787_500_000,
            "primary",
            10_080,
            20.0,
        );
        linked.observation.usage_sample = Some(statsai_core::UsageCounts {
            input_tokens: Some(10),
            total_tokens: Some(10),
            ..statsai_core::UsageCounts::default()
        });
        linked.observation.usage_event_id = Some(EventId("event-original".to_string()));
        linked.observation.usage_link_kind = QuotaUsageLinkKind::RecordEvent;
        store
            .upsert_quota_observations(std::slice::from_ref(&linked))
            .expect("linked observation");

        let mut unchanged = linked.clone();
        unchanged.observation.usage_event_id = None;
        unchanged.observation.usage_link_kind = QuotaUsageLinkKind::None;
        store
            .upsert_quota_observations(std::slice::from_ref(&unchanged))
            .expect("unchanged archive observation");
        let stored = store
            .quota_observations(&QuotaQuery::default(), false)
            .expect("stored observation");
        assert_eq!(
            stored[0].observation.usage_event_id,
            Some(EventId("event-original".to_string()))
        );

        let mut changed = unchanged.clone();
        changed.observation.semantic_fingerprint = "semantic-changed".to_string();
        changed.observation.usage_sample = Some(statsai_core::UsageCounts {
            input_tokens: Some(11),
            total_tokens: Some(11),
            ..statsai_core::UsageCounts::default()
        });
        store
            .upsert_quota_observations(std::slice::from_ref(&changed))
            .expect("changed archive observation");
        let stored = store
            .quota_observations(&QuotaQuery::default(), false)
            .expect("changed observation");
        assert_eq!(stored[0].observation.usage_event_id, None);
        assert_eq!(
            stored[0].observation.usage_link_kind,
            QuotaUsageLinkKind::None
        );

        store
            .upsert_quota_observations(std::slice::from_ref(&linked))
            .expect("restore linked observation");
        let mut missing = linked;
        missing.observation.usage_sample = None;
        missing.observation.usage_event_id = None;
        missing.observation.usage_link_kind = QuotaUsageLinkKind::None;
        store
            .upsert_quota_observations(std::slice::from_ref(&missing))
            .expect("missing archive sample");
        let stored = store
            .quota_observations(&QuotaQuery::default(), false)
            .expect("missing observation");
        assert_eq!(stored[0].observation.usage_event_id, None);
    }

    #[test]
    fn quota_sync_projection_skips_local_usage_enrichment() {
        let store = Store::in_memory().expect("store");
        let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("observed");
        let reset = DateTime::from_timestamp(1_787_500_000, 0).expect("reset");
        let (source_id, account_id) = assigned_source(&store, observed_at - Duration::days(8));
        store
            .upsert_quota_observations(&[sample_record(
                source_id.clone(),
                "projection-window",
                "projection-window",
                observed_at,
                reset.timestamp(),
                "primary",
                10_080,
                20.0,
            )])
            .expect("quota observation");
        store
            .conn
            .execute(
                r#"
                INSERT INTO usage_events (
                  event_id, provider, source_id, provider_account_id, started_at,
                  total_tokens, semantic_fingerprint, payload
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
                params![
                    "invalid-projection-event",
                    "codex",
                    &source_id.0,
                    &account_id.0,
                    (reset - Duration::days(1)).to_rfc3339(),
                    0,
                    "invalid-projection-event",
                    "not-json"
                ],
            )
            .expect("invalid legacy event");

        let projections = store
            .quota_sync_projections(&QuotaQuery::default(), "device-a")
            .expect("projection without local usage enrichment");

        assert_eq!(projections.len(), 1);
    }

    #[test]
    fn quota_store_handles_ten_thousand_observations_with_one_repeated_payload() {
        let store = Store::in_memory().expect("store");
        let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
        let base = sample_record(
            SourceId("source-scale".to_string()),
            "scale-0",
            "scale-0",
            observed_at,
            1_787_500_000,
            "primary",
            10_080,
            20.0,
        );
        let mut records = Vec::with_capacity(10_000);
        for index in 0..10_000 {
            let mut record = base.clone();
            record.observation.observation_id = format!("scale-{index}");
            record.observation.semantic_fingerprint = format!("semantic-scale-{index}");
            record.observation.source_record_id = format!("record-scale-{index}");
            record.observation.source_line_number = index as u64 + 1;
            record.observation.observed_at = observed_at + Duration::seconds(index as i64);
            record.windows[0].observation_id = record.observation.observation_id.clone();
            record.windows[0].window_observation_id = format!("window-scale-{index}");
            records.push(record);
        }
        store
            .upsert_quota_observations(&records)
            .expect("bulk observations");
        assert_eq!(
            store
                .quota_status(&QuotaQuery::default())
                .expect("status")
                .total_observations,
            10_000
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM quota_payloads", [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("payload count"),
            1
        );
    }

    #[test]
    fn quota_attribution_uses_exact_interval_boundary_and_reattributes_history() {
        let store = Store::in_memory().expect("store");
        let boundary = DateTime::from_timestamp(1_787_000_000, 0).expect("boundary");
        let (source_id, account_id) = assigned_source(&store, boundary);
        let before = sample_record(
            source_id.clone(),
            "before",
            "before",
            boundary - Duration::seconds(1),
            1_787_500_000,
            "primary",
            10_080,
            10.0,
        );
        let at = sample_record(
            source_id.clone(),
            "at",
            "at",
            boundary,
            1_787_500_100,
            "secondary",
            10_080,
            11.0,
        );
        store
            .upsert_quota_observations(&[before, at])
            .expect("upsert");
        let observations = store
            .quota_observations(&QuotaQuery::default(), false)
            .expect("observations");
        assert!(observations[0].observation.provider_account_id.is_none());
        assert_eq!(
            observations[1].observation.provider_account_id,
            Some(account_id.clone())
        );
        let mut assignment = store
            .list_source_account_assignments_for_source(&source_id)
            .expect("assignments")
            .remove(0);
        assignment.started_at = boundary - Duration::minutes(1);
        assignment.updated_at = Utc::now();
        store
            .upsert_source_account_assignment(&assignment)
            .expect("backdate assignment");
        store
            .reattribute_quota_observations(&source_id)
            .expect("reattribute");
        assert!(store
            .quota_observations(&QuotaQuery::default(), false)
            .expect("reattributed observations")
            .iter()
            .all(|record| record.observation.provider_account_id == Some(account_id.clone())));
    }

    #[test]
    fn reconstruction_clusters_reset_drift_ignores_slot_and_projects_weekly_windows() {
        let store = Store::in_memory().expect("store");
        let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
        let (source_id, account_id) = assigned_source(&store, observed_at - Duration::hours(1));
        let records = vec![
            sample_record(
                source_id.clone(),
                "one",
                "one",
                observed_at,
                1_787_500_000,
                "primary",
                10_080,
                10.0,
            ),
            sample_record(
                source_id,
                "two",
                "two",
                observed_at + Duration::minutes(1),
                1_787_500_240,
                "secondary",
                10_080,
                12.0,
            ),
        ];
        store.upsert_quota_observations(&records).expect("upsert");

        let windows = store
            .quota_windows(&QuotaQuery::default())
            .expect("windows");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].sample_count, 2);
        assert_eq!(windows[0].reset_min_epoch_seconds, 1_787_500_000);
        assert_eq!(windows[0].reset_max_epoch_seconds, 1_787_500_240);
        let projections = store
            .quota_sync_projections(&QuotaQuery::default(), "device-a")
            .expect("projections");
        let peer_projections = store
            .quota_sync_projections(&QuotaQuery::default(), "device-b")
            .expect("peer projections");
        assert_eq!(projections.len(), 1);
        assert_eq!(peer_projections.len(), 1);
        assert_eq!(projections[0].provider_account_id, account_id);
        assert_ne!(
            projections[0].projection_id,
            peer_projections[0].projection_id
        );
        assert_eq!(
            projections[0].change_points[0].point_fingerprint,
            peer_projections[0].change_points[0].point_fingerprint
        );
        let projection_json = serde_json::to_value(&projections[0]).expect("json");
        assert!(projection_json.get("source_id").is_none());
        assert!(projection_json.get("total_tokens").is_none());
        assert!(projection_json.get("estimated_cost").is_none());
    }

    #[allow(clippy::too_many_arguments)]
    fn sample_usage_event(
        source_id: &SourceId,
        account_id: &ProviderAccountId,
        started_at: DateTime<Utc>,
        record_id: &str,
        input_tokens: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
        estimated_cost_micro_usd: i64,
    ) -> UsageEvent {
        UsageEvent {
            schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
            event_id: event_id("codex", source_id, record_id, None, started_at),
            device_id: "device".to_string(),
            provider: "codex".to_string(),
            source_id: source_id.clone(),
            provider_account_id: Some(account_id.clone()),
            subscription_id: None,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalAdapter,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "jsonl".to_string(),
                source_path_hash: Some("quota-event".to_string()),
                source_record_id: Some(record_id.to_string()),
                parse_confidence: Confidence::High,
            },
            session: SessionInfo {
                session_id: record_id.to_string(),
                local_session_id_hash: Some(record_id.to_string()),
                title: None,
                started_at,
                ended_at: None,
                duration_seconds: None,
            },
            model: None,
            usage: UsageCounts {
                input_tokens: Some(input_tokens),
                output_tokens: Some(output_tokens),
                cache_read_tokens: Some(cache_read_tokens),
                reasoning_tokens: Some(reasoning_tokens),
                total_tokens: Some(
                    input_tokens + cache_read_tokens + output_tokens + reasoning_tokens,
                ),
                ..UsageCounts::default()
            },
            runtime: None,
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: None,
                provider_reported_usd: None,
                estimated_api_equivalent_micro_usd: Some(estimated_cost_micro_usd),
                provider_reported_micro_usd: None,
                pricing_source: Some("test".to_string()),
                pricing_version: None,
                confidence: Confidence::High,
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
            created_at: started_at,
            imported_at: started_at,
        }
    }

    #[test]
    fn quota_cycle_contributions_select_weekly_attributed_cycles_only() {
        let store = Store::in_memory().expect("store");
        let observed_at = DateTime::from_timestamp(1_787_011_200, 0).expect("observed");
        let reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
        let (source_id, _) = assigned_source(&store, observed_at - Duration::days(8));
        store
            .upsert_quota_observations(&[
                sample_record(
                    source_id.clone(),
                    "weekly",
                    "weekly",
                    observed_at,
                    reset.timestamp(),
                    "secondary",
                    10_080,
                    20.0,
                ),
                sample_record(
                    source_id.clone(),
                    "five-hour",
                    "five-hour",
                    observed_at,
                    (reset - Duration::hours(4)).timestamp(),
                    "primary",
                    300,
                    40.0,
                ),
                sample_record(
                    source_id,
                    "monthly",
                    "monthly",
                    observed_at,
                    (reset + Duration::days(20)).timestamp(),
                    "monthly",
                    43_200,
                    8.0,
                ),
            ])
            .expect("observations");

        let contributions = store
            .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
            .expect("contributions");
        assert_eq!(contributions.len(), 1);
        assert_eq!(contributions[0].window_minutes, 10_080);
        assert_eq!(
            contributions[0].schema_version,
            "quota_cycle_contribution.v1"
        );
        let json = serde_json::to_value(&contributions[0]).expect("json");
        assert!(json.get("source_id").is_none());
        assert!(json.get("device_id").is_none());
        assert!(json.get("change_points").is_none());
        assert!(json.get("sample_count").is_none());
        assert!(json.get("latest_status").is_none());
        assert_eq!(json.get("has_schedule_overlap"), Some(&json!(false)));
    }

    #[test]
    fn quota_cycle_contributions_report_locally_reconstructed_schedule_overlaps() {
        let store = Store::in_memory().expect("store");
        // An early reset restarts the weekly schedule three days in, so the two
        // reconstructed cycles overlap for the remaining four days.
        let first_reset = DateTime::from_timestamp(1_787_616_000, 0).expect("first reset");
        let second_reset = first_reset + Duration::days(3);
        let first_start = first_reset - Duration::days(7);
        let (source_id, _) = assigned_source(&store, first_start - Duration::days(1));
        store
            .upsert_quota_observations(&[
                sample_record(
                    source_id.clone(),
                    "weekly-first",
                    "weekly-first",
                    first_start + Duration::hours(1),
                    first_reset.timestamp(),
                    "secondary",
                    10_080,
                    62.0,
                ),
                sample_record(
                    source_id,
                    "weekly-second",
                    "weekly-second",
                    second_reset - Duration::days(7) + Duration::hours(1),
                    second_reset.timestamp(),
                    "secondary",
                    10_080,
                    4.0,
                ),
            ])
            .expect("observations");

        let contributions = store
            .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
            .expect("contributions");
        assert_eq!(contributions.len(), 2);
        assert!(
            contributions
                .iter()
                .all(|contribution| contribution.has_schedule_overlap),
            "both sides of a locally reconstructed overlap carry the flag"
        );
    }

    #[test]
    fn quota_cycle_contributions_default_schedule_overlap_when_absent_on_the_wire() {
        let wire = json!({
            "schema_version": "quota_cycle_contribution.v1",
            "contribution_id": "quota_cycle_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "provider": "codex",
            "provider_account_id": "acct_aaaaaaaaaaaaaaaaaaaaaaaa",
            "limit_id": "codex",
            "window_minutes": 10_080,
            "representative_reset": "2026-08-25T15:00:00Z",
            "representative_reset_epoch_seconds": 1_787_670_000i64,
        });
        let contribution: QuotaCycleContributionV1 =
            serde_json::from_value(wire).expect("legacy payload deserializes");
        assert!(!contribution.has_schedule_overlap);
    }

    #[test]
    fn quota_cycle_contributions_exclude_unattributed_cycles() {
        let store = Store::in_memory().expect("store");
        let observed_at = DateTime::from_timestamp(1_787_011_200, 0).expect("observed");
        let source = SourceLocation::local_adapter(
            "codex",
            "codex-local-jsonl",
            "unattributed",
            Path::new("/tmp/quota-unattributed-cycle"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        store
            .upsert_quota_observations(&[sample_record(
                source.source_id,
                "unattributed-weekly",
                "unattributed-weekly",
                observed_at,
                1_787_616_000,
                "secondary",
                10_080,
                15.0,
            )])
            .expect("observation");

        let contributions = store
            .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
            .expect("contributions");
        assert!(contributions.is_empty());
    }

    #[test]
    fn quota_cycle_contributions_build_daily_envelopes_without_carry_forward() {
        let store = Store::in_memory().expect("store");
        let day_one = DateTime::from_timestamp(1_787_011_200, 0).expect("day one");
        let day_three = day_one + Duration::days(2) + Duration::hours(3);
        let reset = day_one + Duration::days(7);
        let (source_id, _) = assigned_source(&store, day_one - Duration::days(1));
        store
            .upsert_quota_observations(&[
                sample_record(
                    source_id.clone(),
                    "day-one-first",
                    "day-one-first",
                    day_one + Duration::hours(1),
                    reset.timestamp(),
                    "secondary",
                    10_080,
                    10.0,
                ),
                sample_record(
                    source_id.clone(),
                    "day-one-last",
                    "day-one-last",
                    day_one + Duration::hours(8),
                    reset.timestamp(),
                    "secondary",
                    10_080,
                    25.0,
                ),
                sample_record(
                    source_id,
                    "day-three",
                    "day-three",
                    day_three,
                    reset.timestamp(),
                    "secondary",
                    10_080,
                    40.0,
                ),
            ])
            .expect("observations");

        let contributions = store
            .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
            .expect("contributions");
        assert_eq!(contributions.len(), 1);
        let days = contributions[0]
            .daily_envelopes
            .iter()
            .map(|envelope| envelope.day.as_str())
            .collect::<Vec<_>>();
        assert_eq!(days, ["2026-08-18", "2026-08-20"]);
        assert_eq!(contributions[0].daily_envelopes[0].first_used_percent, 10.0);
        assert_eq!(contributions[0].daily_envelopes[0].last_used_percent, 25.0);
        assert_eq!(
            contributions[0].daily_envelopes[0].minimum_used_percent,
            10.0
        );
        assert_eq!(
            contributions[0].daily_envelopes[0].maximum_used_percent,
            25.0
        );
        assert_eq!(contributions[0].daily_envelopes[1].first_used_percent, 40.0);
        assert_eq!(contributions[0].daily_envelopes[1].last_used_percent, 40.0);
    }

    #[test]
    fn quota_cycle_contributions_emit_exact_boundary_slices() {
        let store = Store::in_memory().expect("store");
        // Use a mid-day reset so start and end fall on partial UTC days.
        let reset = DateTime::from_timestamp(1_787_670_000, 0).expect("reset");
        let inferred_start = reset - Duration::minutes(10_080);
        let observed_at = inferred_start + Duration::hours(2);
        let (source_id, account_id) = assigned_source(&store, inferred_start - Duration::days(1));
        store
            .upsert_quota_observations(&[sample_record(
                source_id.clone(),
                "weekly-boundary",
                "weekly-boundary",
                observed_at,
                reset.timestamp(),
                "secondary",
                10_080,
                33.0,
            )])
            .expect("observation");
        store
            .insert_event(&sample_usage_event(
                &source_id,
                &account_id,
                inferred_start + Duration::minutes(30),
                "start-boundary",
                100,
                20,
                10,
                5,
                1_500,
            ))
            .expect("start event");
        store
            .insert_event(&sample_usage_event(
                &source_id,
                &account_id,
                utc_day_start(reset) + Duration::hours(1),
                "end-boundary",
                40,
                0,
                8,
                2,
                700,
            ))
            .expect("end event");
        store
            .insert_event(&sample_usage_event(
                &source_id,
                &account_id,
                inferred_start + Duration::days(1) + Duration::hours(2),
                "interior-day",
                9_999,
                0,
                0,
                0,
                99_000,
            ))
            .expect("interior event");

        let contributions = store
            .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
            .expect("contributions");
        assert_eq!(contributions.len(), 1);
        assert_eq!(contributions[0].boundary_slices.len(), 2);
        assert_eq!(
            contributions[0].boundary_slices[0].period_start,
            inferred_start
        );
        assert_eq!(
            contributions[0].boundary_slices[0].period_end,
            next_utc_day_start(inferred_start)
        );
        assert_eq!(contributions[0].boundary_slices[0].input_tokens, 100);
        assert_eq!(contributions[0].boundary_slices[0].cache_read_tokens, 20);
        assert_eq!(contributions[0].boundary_slices[0].output_tokens, 10);
        assert_eq!(contributions[0].boundary_slices[0].reasoning_tokens, 5);
        assert_eq!(contributions[0].boundary_slices[0].total_tokens, 135);
        assert_eq!(
            contributions[0].boundary_slices[0].estimated_cost_micro_usd,
            1_500
        );
        assert_eq!(
            contributions[0].boundary_slices[1].period_start,
            utc_day_start(reset)
        );
        assert_eq!(contributions[0].boundary_slices[1].period_end, reset);
        assert_eq!(contributions[0].boundary_slices[1].input_tokens, 40);
        assert_eq!(
            contributions[0].boundary_slices[1].estimated_cost_micro_usd,
            700
        );
        assert!(
            contributions[0]
                .boundary_slices
                .iter()
                .all(|slice| slice.input_tokens != 9_999),
            "complete utc days stay out of boundary slices"
        );
    }

    #[test]
    fn quota_cycle_contribution_ids_are_stable_for_the_same_device_anchor() {
        let store = Store::in_memory().expect("store");
        let observed_at = DateTime::from_timestamp(1_787_011_200, 0).expect("observed");
        let reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
        let (source_id, _) = assigned_source(&store, observed_at - Duration::days(8));
        store
            .upsert_quota_observations(&[sample_record(
                source_id,
                "stable-id",
                "stable-id",
                observed_at,
                reset.timestamp(),
                "secondary",
                10_080,
                18.0,
            )])
            .expect("observation");

        let first = store
            .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
            .expect("first");
        let second = store
            .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
            .expect("second");
        let peer = store
            .quota_cycle_contributions(&QuotaQuery::default(), "device-b")
            .expect("peer");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].contribution_id, second[0].contribution_id);
        assert_ne!(first[0].contribution_id, peer[0].contribution_id);
        assert!(first[0].contribution_id.starts_with("quota_cycle_"));
        assert_eq!(first[0].contribution_id.len(), "quota_cycle_".len() + 32);
    }
}
