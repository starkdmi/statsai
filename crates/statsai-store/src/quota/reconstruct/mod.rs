use super::*;

mod store;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WindowScope {
    pub(crate) provider: String,
    pub(crate) provider_account_id: Option<ProviderAccountId>,
    pub(crate) source_id: Option<SourceId>,
    pub(crate) limit_id: Option<String>,
    pub(crate) window_minutes: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct WindowPoint {
    pub(crate) observation: QuotaObservationV1,
    pub(crate) window: QuotaWindowObservationV1,
}

/// The span over which a cluster was observed.
pub(crate) struct ClusterLife {
    pub(crate) first_observed_at: DateTime<Utc>,
    pub(crate) last_observed_at: DateTime<Utc>,
}

pub(crate) fn cluster_life(cluster: &[WindowPoint]) -> Option<ClusterLife> {
    Some(ClusterLife {
        first_observed_at: cluster.iter().map(|p| p.observation.observed_at).min()?,
        last_observed_at: cluster.iter().map(|p| p.observation.observed_at).max()?,
    })
}

/// True when `live` was reported before `suspect` appeared and again after it
/// vanished.
pub(crate) fn brackets(live: &[WindowPoint], suspect: &ClusterLife) -> bool {
    live.iter()
        .any(|p| p.observation.observed_at < suspect.first_observed_at)
        && live
            .iter()
            .any(|p| p.observation.observed_at > suspect.last_observed_at)
}

/// Clusters that describe a schedule the provider never actually switched to.
///
/// A reset is a handover: once a window resets, the provider reports the new
/// schedule and never returns to the old one. So a cluster that another cluster
/// brackets in observation time — reported before this one appeared and again
/// after it vanished — cannot be a cycle in its own right, however many
/// observations back it.
///
/// During the July 2026 tier migration Codex answered a scattering of turns
/// with a blank snapshot: near-zero usage and a fresh weekly reset, interleaved
/// with the live schedule for as long as 34 hours. Each run became a cycle that
/// took days away from the cycle actually running, which is what put two rising
/// lines over the same hours for a single account.
///
/// Bracketing alone settles it, whatever the two schedules claim was spent.
/// Once a window resets, its `resets_at` moves forward and that value can never
/// be reported again, so a schedule cannot resume after another has taken over
/// from it. A genuine early reset — banked or granted — is therefore never
/// bracketed: the schedule it replaced simply stops.
///
/// This deliberately drops well-evidenced clusters. One such ran to 99% over
/// 19 hours across 623 observations while the schedule either side of it sat
/// near 54%, which is not a cycle that reset early but a second counter
/// reported under the same slot for the same account.
pub(crate) fn phantom_cluster_indices(clusters: &[Vec<WindowPoint>]) -> HashSet<usize> {
    let lives = clusters.iter().map(|c| cluster_life(c)).collect::<Vec<_>>();
    let mut phantoms = HashSet::new();
    for (index, life) in lives.iter().enumerate() {
        let Some(life) = life else { continue };
        let bracketed = clusters
            .iter()
            .enumerate()
            .any(|(other, live)| other != index && brackets(live, life));
        if bracketed {
            phantoms.insert(index);
        }
    }
    phantoms
}

#[derive(Debug, Clone)]
pub(crate) struct QuotaClusterSchedule {
    pub(crate) inferred_start: DateTime<Utc>,
    pub(crate) representative_reset: DateTime<Utc>,
    pub(crate) representative_reset_epoch_seconds: i64,
    pub(crate) reset_min: DateTime<Utc>,
    pub(crate) reset_min_epoch_seconds: i64,
    pub(crate) reset_max: DateTime<Utc>,
    pub(crate) reset_max_epoch_seconds: i64,
    pub(crate) first_observed_at: DateTime<Utc>,
    pub(crate) last_observed_at: DateTime<Utc>,
}

impl QuotaClusterSchedule {
    pub(crate) fn from_points(scope: &WindowScope, points: &[WindowPoint]) -> Self {
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
pub(crate) struct ReconstructedQuotaWindow {
    pub(crate) window: QuotaWindowV1,
    pub(crate) daily_envelopes: Vec<QuotaDailyEnvelopeV1>,
}

/// True when the evidence was recorded after the window it describes had already
/// reset, which only happens when historical records are re-imported and stamped
/// with the import time. Such a point would otherwise be clustered into the long
/// closed cycle by its `resets_at` and then bucketed into a daily envelope by its
/// `observed_at`, giving that cycle an observation days or weeks past its end.
pub(crate) fn observation_postdates_reset(
    observation: &QuotaObservationV1,
    window: &QuotaWindowObservationV1,
) -> bool {
    (observation.observed_at - window.resets_at).num_seconds() > STALE_OBSERVATION_TOLERANCE_SECONDS
}

pub(crate) fn daily_envelopes_from_points(points: &[WindowPoint]) -> Vec<QuotaDailyEnvelopeV1> {
    let mut grouped = BTreeMap::<String, Vec<&WindowPoint>>::new();
    for point in points {
        grouped
            .entry(point.observation.observed_at.date_naive().to_string())
            .or_default()
            .push(point);
    }
    let mut envelopes = grouped
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
        .collect::<Vec<_>>();

    // Consumption cannot fall inside a window, so a reading below one already
    // seen is stale rather than a refund. Concurrent sessions produce them
    // routinely: two poll at once and the slower one still holds the earlier
    // figure, sometimes within the same second. Left alone the daily closing
    // walks backwards. The observed extremes stay untouched, since they are
    // the evidence that the jitter happened.
    let mut consumed = f64::NEG_INFINITY;
    for envelope in &mut envelopes {
        if consumed.is_finite() {
            envelope.first_used_percent = envelope.first_used_percent.max(consumed);
        }
        consumed = consumed.max(envelope.maximum_used_percent);
        envelope.last_used_percent = envelope.last_used_percent.max(consumed);
    }
    envelopes
}

pub(crate) fn utc_day_start(timestamp: DateTime<Utc>) -> DateTime<Utc> {
    timestamp
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc()
}

pub(crate) fn is_utc_midnight(timestamp: DateTime<Utc>) -> bool {
    timestamp.time().num_seconds_from_midnight() == 0 && timestamp.nanosecond() == 0
}

pub(crate) fn next_utc_day_start(timestamp: DateTime<Utc>) -> DateTime<Utc> {
    utc_day_start(timestamp) + Duration::days(1)
}

pub(crate) fn quota_point_fingerprint(scope: &WindowScope, point: &WindowPoint) -> String {
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

pub(crate) fn observation_matches_query(
    observation: &QuotaObservationV1,
    query: &QuotaQuery,
) -> bool {
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

pub(crate) fn assignment_matches_query(
    assignment: &SourceAccountAssignment,
    query: &QuotaQuery,
) -> bool {
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

pub(crate) fn quota_cluster_matches_time_query(
    schedule: &QuotaClusterSchedule,
    query: &QuotaQuery,
) -> bool {
    query
        .from
        .is_none_or(|from| schedule.last_observed_at >= from)
        && query.to.is_none_or(|to| schedule.first_observed_at <= to)
}

pub(crate) fn collapse_semantic_duplicates(
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

pub(crate) fn observation_quality(record: &QuotaObservationRecordV1) -> (bool, bool, usize) {
    (
        record.observation.provider_account_id.is_some(),
        record.observation.usage_event_id.is_some(),
        record.windows.len(),
    )
}

pub(crate) fn matching_positive_usage_sample(
    incoming: &QuotaObservationV1,
    existing: &QuotaObservationV1,
) -> bool {
    incoming.usage_sample.as_ref().is_some_and(|sample| {
        sample.computed_total() > 0 && existing.usage_sample.as_ref() == Some(sample)
    })
}

pub(crate) fn observation_range(records: &[&QuotaObservationRecordV1]) -> Option<QuotaDateRange> {
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

pub(crate) fn usage_link_kind_label(kind: QuotaUsageLinkKind) -> &'static str {
    match kind {
        QuotaUsageLinkKind::RecordEvent => "record_event",
        QuotaUsageLinkKind::TurnEvent => "turn_event",
        QuotaUsageLinkKind::None => "none",
    }
}

pub(crate) fn parse_usage_link_kind(value: &str) -> QuotaUsageLinkKind {
    match value {
        "record_event" => QuotaUsageLinkKind::RecordEvent,
        "turn_event" => QuotaUsageLinkKind::TurnEvent,
        _ => QuotaUsageLinkKind::None,
    }
}
