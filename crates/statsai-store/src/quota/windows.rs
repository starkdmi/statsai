use super::*;

impl Store {
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

    pub(crate) fn materialize_quota_window(
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
}
