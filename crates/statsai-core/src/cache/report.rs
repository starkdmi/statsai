use super::{
    cache_gap_bin_label, cache_hit_ratio, provider_has_cache_diagnostics, AnalyzedCacheCall,
    CacheBoundaryCounts, CacheCallClass, CacheHealthV1, CACHE_GAP_BIN_UPPER_MINUTES,
    CACHE_HEALTH_VERSION,
};
use crate::types::{UsageCounts, UsageEvent, UsageSummary};
use chrono::{DateTime, Duration, Offset, TimeZone, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const CACHE_REPORT_SCHEMA_VERSION: &str = "cache_report.v1";

/// Width of a `--timeline` bucket.
pub const CACHE_TIMELINE_BUCKET_MINUTES: i64 = 10;

/// The prompt-cache report for a period, as `statsai report cache --json` and
/// the local API print it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CacheReport {
    pub schema_version: String,
    pub detector_version: u32,
    pub label: String,
    pub since: Option<DateTime<Utc>>,
    pub until: DateTime<Utc>,
    pub filters: CacheReportFilters,
    /// Token totals across every provider in scope.
    pub totals: CacheTokenTotals,
    /// Detector counts across the providers it covers.
    pub diagnostics: CacheHealthV1,
    pub boundaries: CacheBoundaryCounts,
    pub gap_histogram: Vec<CacheGapRow>,
    pub providers: Vec<CacheProviderRow>,
    /// UTC days, matching the daily summaries that sync.
    pub days: Vec<CacheDayRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<Vec<CacheTimelineRow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<AnalyzedCacheCall>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheReportFilters {
    pub provider: Option<String>,
    pub account: Option<String>,
    pub session: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CacheTokenTotals {
    pub requests: u64,
    pub input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    /// Ordinary input, cache writes, and cache reads: the context processed.
    pub logical_input_tokens: u64,
    /// Cache reads over logical input.
    pub cache_hit_ratio: Option<f64>,
    pub avg_logical_input_per_request: Option<f64>,
}

impl CacheTokenTotals {
    pub fn add_event(&mut self, event: &UsageEvent) {
        self.add_usage(&event.usage, 1);
    }

    /// Adds a summary that has no events behind it, such as a provider's own
    /// session totals. It counts toward token totals, never diagnostics.
    pub fn add_summary(&mut self, summary: &UsageSummary) {
        self.add_usage(&summary.usage, 0);
    }

    fn add_usage(&mut self, usage: &UsageCounts, default_requests: u64) {
        self.requests = self
            .requests
            .saturating_add(usage.requests.unwrap_or(default_requests));
        self.input_tokens = self
            .input_tokens
            .saturating_add(usage.input_tokens.unwrap_or(0));
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(usage.cache_creation_tokens.unwrap_or(0));
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens.unwrap_or(0));
        self.finish();
    }

    fn finish(&mut self) {
        self.logical_input_tokens = self
            .input_tokens
            .saturating_add(self.cache_creation_tokens)
            .saturating_add(self.cache_read_tokens);
        self.cache_hit_ratio = cache_hit_ratio(
            self.input_tokens,
            self.cache_creation_tokens,
            self.cache_read_tokens,
        );
        self.avg_logical_input_per_request =
            (self.requests > 0).then(|| self.logical_input_tokens as f64 / self.requests as f64);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheGapRow {
    pub label: String,
    pub min_minutes: u64,
    /// Exclusive upper bound; `None` for the open last bin.
    pub max_minutes: Option<u64>,
    pub comparable: u64,
    pub losses: u64,
    /// Reusable context the bin's suspected losses did not read from the cache.
    pub missed_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CacheProviderRow {
    pub provider: String,
    pub totals: CacheTokenTotals,
    /// `None` for providers the detector does not cover.
    pub diagnostics: Option<CacheHealthV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CacheDayRow {
    pub day: String,
    pub totals: CacheTokenTotals,
    pub diagnostics: Option<CacheHealthV1>,
}

/// One local-time bucket of model calls, keyed by when each call completed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CacheTimelineRow {
    /// Bucket start in local time, RFC 3339 with offset.
    pub start: String,
    pub requests: u64,
    pub logical_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_hit_ratio: Option<f64>,
    pub first_observed: u64,
    pub boundaries: u64,
    pub losses: u64,
    pub missed_tokens: u64,
}

/// Inputs for [`build_cache_report`]. `events` are the events in scope;
/// `calls` are their analyzed calls, judged against whole sessions.
/// `summaries` are single-day summaries in scope with no events behind them,
/// which sync with the daily summaries and count toward the same totals.
pub struct CacheReportInput<'a> {
    pub label: String,
    pub since: Option<DateTime<Utc>>,
    pub until: DateTime<Utc>,
    pub filters: CacheReportFilters,
    pub events: &'a [UsageEvent],
    pub summaries: &'a [UsageSummary],
    pub calls: Vec<AnalyzedCacheCall>,
    pub include_details: bool,
}

#[must_use]
pub fn build_cache_report<Tz: TimeZone>(
    input: CacheReportInput<'_>,
    timeline: Option<&Tz>,
) -> CacheReport
where
    Tz::Offset: std::fmt::Display,
{
    let mut totals = CacheTokenTotals::default();
    let mut diagnostics = CacheHealthV1::default();
    let mut boundaries = CacheBoundaryCounts::default();
    let mut providers: BTreeMap<String, (CacheTokenTotals, Option<CacheHealthV1>)> =
        BTreeMap::new();
    let mut days: BTreeMap<String, (CacheTokenTotals, Option<CacheHealthV1>)> = BTreeMap::new();
    let mut event_days: BTreeMap<&str, String> = BTreeMap::new();

    for event in input.events {
        let supported = provider_has_cache_diagnostics(&event.provider);
        let day = event.session.started_at.date_naive().to_string();
        event_days.insert(event.event_id.0.as_str(), day.clone());
        totals.add_event(event);
        for (totals, health) in [
            providers.entry(event.provider.clone()).or_default(),
            days.entry(day).or_default(),
        ] {
            totals.add_event(event);
            if supported {
                health
                    .get_or_insert_with(CacheHealthV1::default)
                    .add_event_input(event);
            }
        }
        if supported {
            diagnostics.add_event_input(event);
        }
    }
    for summary in input.summaries {
        let day = summary
            .period_start
            .unwrap_or(summary.observed_at)
            .date_naive()
            .to_string();
        totals.add_summary(summary);
        providers
            .entry(summary.provider.clone())
            .or_default()
            .0
            .add_summary(summary);
        days.entry(day).or_default().0.add_summary(summary);
    }
    for call in &input.calls {
        diagnostics.add_call(call);
        boundaries.add_call(call);
        if let Some((_, health)) = providers.get_mut(&call.provider) {
            health
                .get_or_insert_with(CacheHealthV1::default)
                .add_call(call);
        }
        if let Some((_, health)) = event_days
            .get(call.event_id.0.as_str())
            .and_then(|day| days.get_mut(day))
        {
            health
                .get_or_insert_with(CacheHealthV1::default)
                .add_call(call);
        }
    }

    CacheReport {
        schema_version: CACHE_REPORT_SCHEMA_VERSION.to_string(),
        detector_version: CACHE_HEALTH_VERSION,
        label: input.label,
        since: input.since,
        until: input.until,
        filters: input.filters,
        totals,
        gap_histogram: gap_histogram(&diagnostics),
        diagnostics,
        boundaries,
        providers: providers
            .into_iter()
            .map(|(provider, (totals, diagnostics))| CacheProviderRow {
                provider,
                totals,
                diagnostics,
            })
            .collect(),
        days: days
            .into_iter()
            .map(|(day, (totals, diagnostics))| CacheDayRow {
                day,
                totals,
                diagnostics,
            })
            .collect(),
        timeline: timeline.map(|tz| cache_timeline(&input.calls, tz)),
        details: input.include_details.then_some(input.calls),
    }
}

#[must_use]
pub fn gap_histogram(health: &CacheHealthV1) -> Vec<CacheGapRow> {
    health
        .gap_comparable
        .iter()
        .zip(&health.gap_losses)
        .zip(&health.gap_missed_tokens)
        .enumerate()
        .map(
            |(index, ((comparable, losses), missed_tokens))| CacheGapRow {
                label: cache_gap_bin_label(index),
                min_minutes: index
                    .checked_sub(1)
                    .and_then(|previous| CACHE_GAP_BIN_UPPER_MINUTES.get(previous))
                    .copied()
                    .unwrap_or(0),
                max_minutes: CACHE_GAP_BIN_UPPER_MINUTES.get(index).copied(),
                comparable: *comparable,
                losses: *losses,
                missed_tokens: *missed_tokens,
            },
        )
        .collect()
}

/// Groups calls into [`CACHE_TIMELINE_BUCKET_MINUTES`] buckets of local time.
/// Buckets without calls are left out.
#[must_use]
pub fn cache_timeline<Tz: TimeZone>(calls: &[AnalyzedCacheCall], tz: &Tz) -> Vec<CacheTimelineRow>
where
    Tz::Offset: std::fmt::Display,
{
    let width = Duration::minutes(CACHE_TIMELINE_BUCKET_MINUTES).num_seconds();
    let mut buckets: BTreeMap<DateTime<Utc>, CacheTimelineRow> = BTreeMap::new();
    for call in calls {
        // Truncating local wall-clock seconds keeps buckets on local ten-minute
        // marks in zones whose offset is not a whole number of hours.
        let offset = i64::from(
            call.completed_at
                .with_timezone(tz)
                .offset()
                .fix()
                .local_minus_utc(),
        );
        let local_seconds = call.completed_at.timestamp() + offset;
        let start_seconds = local_seconds - local_seconds.rem_euclid(width) - offset;
        let Some(start) = DateTime::<Utc>::from_timestamp(start_seconds, 0) else {
            continue;
        };
        let row = buckets.entry(start).or_insert_with(|| CacheTimelineRow {
            start: start.with_timezone(tz).to_rfc3339(),
            requests: 0,
            logical_input_tokens: 0,
            cache_read_tokens: 0,
            cache_hit_ratio: None,
            first_observed: 0,
            boundaries: 0,
            losses: 0,
            missed_tokens: 0,
        });
        row.requests = row.requests.saturating_add(call.requests);
        row.logical_input_tokens = row
            .logical_input_tokens
            .saturating_add(call.logical_input_tokens());
        row.cache_read_tokens = row.cache_read_tokens.saturating_add(call.cache_read_tokens);
        match call.verdict.class {
            CacheCallClass::FirstObserved => row.first_observed += 1,
            CacheCallClass::Boundary => row.boundaries += 1,
            _ => {}
        }
        if call.verdict.loss.is_some() {
            row.losses += 1;
        }
        row.missed_tokens = row
            .missed_tokens
            .saturating_add(call.verdict.missed_tokens.unwrap_or(0));
    }
    buckets
        .into_values()
        .map(|mut row| {
            row.cache_hit_ratio = (row.logical_input_tokens > 0)
                .then(|| row.cache_read_tokens as f64 / row.logical_input_tokens as f64);
            row
        })
        .collect()
}
