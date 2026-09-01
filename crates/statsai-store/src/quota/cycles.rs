use super::*;

pub(crate) struct PendingQuotaCycleContribution {
    pub(crate) contribution_id: String,
    pub(crate) provider_account_id: ProviderAccountId,
    pub(crate) reconstructed: ReconstructedQuotaWindow,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CycleBounds {
    pub(crate) start: DateTime<Utc>,
    pub(crate) end: DateTime<Utc>,
}

pub(crate) struct BoundarySliceBuilders {
    pub(crate) provider: String,
    pub(crate) account_id: ProviderAccountId,
    pub(crate) slices: Vec<QuotaUsageSliceV1>,
}

pub(crate) fn empty_usage_slice(
    period_start: DateTime<Utc>,
    period_end: DateTime<Utc>,
) -> QuotaUsageSliceV1 {
    QuotaUsageSliceV1 {
        period_start,
        period_end,
        ..QuotaUsageSliceV1::default()
    }
}

pub(crate) fn boundary_slice_builders(
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

pub(crate) fn clamp_datetime(
    value: DateTime<Utc>,
    min: DateTime<Utc>,
    max: DateTime<Utc>,
) -> DateTime<Utc> {
    value.max(min).min(max)
}

pub(crate) fn effective_cycle_bounds(windows: Vec<&QuotaWindowV1>) -> HashMap<String, CycleBounds> {
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
