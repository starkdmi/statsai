use super::*;

pub(crate) fn summary_sync_payload_hash(summary: &UsageSummary) -> Result<String> {
    let payload = serde_json::to_string(&sanitize_summary_for_sync(summary.clone()))?;
    Ok(hash_text(&payload))
}

pub(crate) fn summary_sync_day(summary: &UsageSummary) -> NaiveDate {
    summary
        .period_start
        .map(|start| start.date_naive())
        .unwrap_or_else(|| summary.observed_at.date_naive())
}

pub(crate) fn summary_period_bounds(summary: &UsageSummary) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = summary
        .period_start
        .or(summary.period_end)
        .unwrap_or(summary.observed_at);
    let end = summary
        .period_end
        .or(summary.period_start)
        .unwrap_or(summary.observed_at);
    if end < start {
        (end, start)
    } else {
        (start, end)
    }
}

fn summary_spans_single_day(summary: &UsageSummary) -> bool {
    let (start, end) = summary_period_bounds(summary);
    start.date_naive() == end.date_naive()
}

fn summary_fits_single_daily_report_day(summary: &UsageSummary) -> bool {
    let (start, end) = summary_period_bounds(summary);
    if start.date_naive() == end.date_naive() {
        return true;
    }
    let duration = end - start;
    duration >= chrono::Duration::zero() && duration <= chrono::Duration::hours(25)
}

fn is_exact_daily_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "external_daily" | "manual_daily" | "custom_daily" | "ccusage_daily"
    )
}

fn is_exact_period_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "manual_period_summary" | "custom_period_summary"
    )
}

/// The cache report's and the pending counts' copy of
/// [`crate::sync_payload::is_http_rollup_passthrough_summary`], which the batch
/// uses. It differs in one case: it swaps bounds that run backwards, so a daily
/// report whose end precedes its start by up to 25 hours across two dates
/// counts here but is not sent.
pub(crate) fn is_http_rollup_passthrough_summary(summary: &UsageSummary) -> bool {
    if summary.metadata.summary_format == "daily_rollup.v1" {
        return false;
    }
    if summary.metadata.summary_format == "claude_stats_cache" {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalSummary {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalAdapter {
        return true;
    }
    (is_exact_daily_passthrough_summary(summary) && summary_fits_single_daily_report_day(summary))
        || (is_exact_period_passthrough_summary(summary) && !summary_spans_single_day(summary))
}

pub(crate) fn collect_pending_summary_days<'a>(
    summaries: impl IntoIterator<Item = &'a UsageSummary>,
) -> BTreeSet<NaiveDate> {
    let mut days = BTreeSet::new();
    for summary in summaries {
        if summary.metadata.summary_format == "daily_rollup.v1"
            || (is_exact_daily_passthrough_summary(summary)
                && summary_fits_single_daily_report_day(summary))
        {
            days.insert(summary_sync_day(summary));
            continue;
        }

        let (start, end) = summary_period_bounds(summary);
        let mut day = start.date_naive();
        let end_day = end.date_naive();
        loop {
            days.insert(day);
            if day >= end_day {
                break;
            }
            let Some(next_day) = day.succ_opt() else {
                break;
            };
            day = next_day;
        }
    }
    days
}
