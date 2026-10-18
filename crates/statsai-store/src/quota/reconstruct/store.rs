use super::super::*;

impl Store {
    pub(crate) fn reconstruct_quota_windows(
        &self,
        query: &QuotaQuery,
    ) -> Result<Vec<ReconstructedQuotaWindow>> {
        let (windows, _) = self.reconstruct_quota_windows_counted(query)?;
        Ok(windows)
    }

    pub(crate) fn reconstruct_quota_windows_counted(
        &self,
        query: &QuotaQuery,
    ) -> Result<(Vec<ReconstructedQuotaWindow>, QuotaDiscardCounts)> {
        let records = self.quota_observations(&reconstruction_query(query), true)?;
        Ok(reconstruct_from_records(records, query))
    }

    /// The attributed weekly windows of [`Store::reconstruct_quota_windows`], and
    /// nothing else: the windows a cycle contribution can come from.
    ///
    /// Every scope holds one account and one window length and is reconstructed on
    /// its own, so leaving the other scopes unread changes none of these windows,
    /// and the final order is the same order filtered. What changes is the cost:
    /// [`Store::attributed_weekly_quota_observations`] reads indexes and columns
    /// rather than JSON, and the five-hour windows are never rebuilt only to be
    /// dropped.
    pub(crate) fn reconstruct_attributed_weekly_quota_windows(
        &self,
        query: &QuotaQuery,
    ) -> Result<Vec<ReconstructedQuotaWindow>> {
        let records = self.attributed_weekly_quota_observations(&reconstruction_query(query))?;
        Ok(reconstruct_from_records(records, query).0)
    }
}

/// Clusters always form over the whole history, whatever time range was asked
/// for; the range only selects among the finished clusters.
fn reconstruction_query(query: &QuotaQuery) -> QuotaQuery {
    QuotaQuery {
        from: None,
        to: None,
        ..query.clone()
    }
}

fn reconstruct_from_records(
    records: Vec<QuotaObservationRecordV1>,
    query: &QuotaQuery,
) -> (Vec<ReconstructedQuotaWindow>, QuotaDiscardCounts) {
    let mut discarded = QuotaDiscardCounts::default();
    let mut grouped = BTreeMap::<WindowScope, Vec<WindowPoint>>::new();
    for record in records {
        let observation = Rc::new(record.observation);
        for window in record.windows {
            if observation_postdates_reset(&observation, &window) {
                discarded.replayed_observations += 1;
                continue;
            }
            grouped
                .entry(WindowScope {
                    provider: observation.provider.clone(),
                    provider_account_id: observation.provider_account_id.clone(),
                    source_id: observation
                        .provider_account_id
                        .is_none()
                        .then(|| observation.source_id.clone()),
                    limit_id: window.limit_id.clone(),
                    window_minutes: window.window_minutes,
                })
                .or_default()
                .push(WindowPoint {
                    observation: Rc::clone(&observation),
                    window,
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
        for (index, (cluster, schedule)) in clusters.into_iter().zip(schedules.iter()).enumerate() {
            if !quota_cluster_matches_time_query(schedule, query) {
                continue;
            }
            let daily_envelopes = daily_envelopes_from_points(&cluster);
            let mut window = Store::materialize_quota_window(&scope, schedule, cluster);
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
    (windows, discarded)
}
