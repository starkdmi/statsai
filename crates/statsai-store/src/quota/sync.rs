use super::*;

impl Store {
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
