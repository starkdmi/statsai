use crate::{
    display_account_identity, home_dir, micro_usd_to_cents_rounded, BillingPeriod, CostAccumulator,
    IdentitySource, ProviderAccount, ProviderAccountId, SourceLocation, Subscription,
    SubscriptionId, SubscriptionStatus, UsageEvent, UsageSummary,
};
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeMap, BTreeSet};

// ── Report building ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportBound {
    pub timestamp: DateTime<Utc>,
    pub date_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportPeriod {
    LastDays(i64),
    AllTime,
    Range {
        since: Option<ReportBound>,
        until: ReportBound,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReportRangeError {
    #[error("invalid report date '{value}': expected YYYY-MM-DD or RFC3339")]
    InvalidDate { value: String },
    #[error("date is out of range")]
    DateOutOfRange,
    #[error("range start ({since}) must be earlier than or equal to range end ({until})")]
    InvertedRange {
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    },
    #[error("provide a range start and/or end")]
    MissingBound,
}

/// Parse a report bound from `YYYY-MM-DD` or RFC3339.
///
/// Date-only values start at `00:00:00` UTC. When `end_of_calendar_day` is
/// true, a date-only value includes the whole UTC day.
pub fn parse_report_date_bound(
    value: &str,
    end_of_calendar_day: bool,
) -> Result<DateTime<Utc>, ReportRangeError> {
    if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        return Ok(date.with_timezone(&Utc));
    }
    let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
        ReportRangeError::InvalidDate {
            value: value.to_string(),
        }
    })?;
    if end_of_calendar_day {
        let next_midnight = date
            .succ_opt()
            .and_then(|next| next.and_hms_opt(0, 0, 0))
            .ok_or(ReportRangeError::DateOutOfRange)?;
        return Ok(next_midnight.and_utc() - Duration::nanoseconds(1));
    }
    let datetime = date
        .and_hms_opt(0, 0, 0)
        .ok_or(ReportRangeError::DateOutOfRange)?;
    Ok(datetime.and_utc())
}

/// Build a custom report window from optional start / end strings.
///
/// A missing end bound uses `now`. A missing start bound means the beginning
/// of stored history. Invert is only checked when the caller supplied both
/// bounds. A start after `now` (with or without a future end) stays valid;
/// [`ReportPeriod::window`] clamps the end to `now` and the report is empty.
pub fn report_period_from_range(
    from: Option<&str>,
    to: Option<&str>,
    now: DateTime<Utc>,
) -> Result<ReportPeriod, ReportRangeError> {
    if from.is_none() && to.is_none() {
        return Err(ReportRangeError::MissingBound);
    }
    let since = from
        .map(|value| parse_report_bound(value, false))
        .transpose()?;
    let until = match to {
        Some(value) => parse_report_bound(value, true)?,
        None => ReportBound {
            timestamp: now,
            date_only: false,
        },
    };
    if let (Some(since), Some(_)) = (since, to) {
        if since.timestamp > until.timestamp {
            return Err(ReportRangeError::InvertedRange {
                since: since.timestamp,
                until: until.timestamp,
            });
        }
    }
    Ok(ReportPeriod::Range { since, until })
}

fn parse_report_bound(
    value: &str,
    end_of_calendar_day: bool,
) -> Result<ReportBound, ReportRangeError> {
    Ok(ReportBound {
        timestamp: parse_report_date_bound(value, end_of_calendar_day)?,
        date_only: DateTime::parse_from_rfc3339(value).is_err(),
    })
}

impl ReportPeriod {
    #[must_use]
    pub fn window(self, now: DateTime<Utc>) -> (Option<DateTime<Utc>>, DateTime<Utc>) {
        match self {
            Self::LastDays(days) => (Some(now - Duration::days(days)), now),
            Self::AllTime => (None, now),
            Self::Range { since, until } => {
                (since.map(|bound| bound.timestamp), until.timestamp.min(now))
            }
        }
    }

    /// Applied bounds for report output. Never inverted: a start after `now`
    /// publishes a zero-width window at `until`.
    #[must_use]
    pub fn published_window(self, now: DateTime<Utc>) -> (Option<DateTime<Utc>>, DateTime<Utc>) {
        let (since, until) = self.window(now);
        match since {
            Some(since) if since > until => (Some(until), until),
            since => (since, until),
        }
    }

    #[must_use]
    pub fn label(self, now: DateTime<Utc>) -> String {
        match self {
            Self::LastDays(7) => "last 7 days".to_string(),
            Self::LastDays(30) => "last 30 days".to_string(),
            Self::LastDays(days) => format!("last {days} days"),
            Self::AllTime => "all time".to_string(),
            Self::Range { since, until } => {
                let (applied_since, applied_until) = self.window(now);
                if applied_since.is_some_and(|start| start > applied_until) {
                    return match since {
                        Some(since) if until.timestamp > now => format!(
                            "{} to {} (empty)",
                            format_range_bound(since),
                            format_range_bound(until)
                        ),
                        Some(since) => format!("from {} (empty)", format_range_bound(since)),
                        None => format!("through {} (empty)", format_range_bound(until)),
                    };
                }
                match since {
                    Some(since) => format!(
                        "{} to {}",
                        format_range_bound(since),
                        format_applied_until(applied_until, until)
                    ),
                    None => format!("through {}", format_applied_until(applied_until, until)),
                }
            }
        }
    }
}

fn format_range_bound(bound: ReportBound) -> String {
    if bound.date_only {
        bound.timestamp.format("%Y-%m-%d").to_string()
    } else {
        bound.timestamp.to_rfc3339()
    }
}

fn format_applied_until(applied_until: DateTime<Utc>, requested: ReportBound) -> String {
    format_range_bound(ReportBound {
        timestamp: applied_until,
        date_only: requested.date_only && applied_until == requested.timestamp,
    })
}

#[derive(Debug, Clone, Default)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: Option<i64>, // cents USD
    pub estimated_cost_micro_usd: Option<i64>,
}

impl UsageTotals {
    fn cost_accumulator(&self) -> CostAccumulator {
        let mut accumulator = CostAccumulator::default();
        accumulator.add_values(self.estimated_cost_micro_usd, self.estimated_cost_usd);
        accumulator
    }

    fn set_cost_from_accumulator(&mut self, accumulator: CostAccumulator) {
        self.estimated_cost_micro_usd = accumulator.micro_usd();
        self.estimated_cost_usd = accumulator.cents_rounded();
    }

    pub fn add_event(&mut self, event: &UsageEvent) {
        self.input_tokens = self
            .input_tokens
            .saturating_add(event.usage.input_tokens.unwrap_or(0));
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(event.usage.cache_creation_tokens.unwrap_or(0));
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(event.usage.cache_read_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(event.usage.output_tokens.unwrap_or(0));
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
        self.total_tokens = self
            .total_tokens
            .saturating_add(event.usage.computed_total());
        let mut cost = self.cost_accumulator();
        cost.add_estimated(&event.cost);
        self.set_cost_from_accumulator(cost);
    }

    pub fn add_summary(&mut self, summary: &UsageSummary) {
        self.input_tokens = self
            .input_tokens
            .saturating_add(summary.usage.input_tokens.unwrap_or(0));
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(summary.usage.cache_creation_tokens.unwrap_or(0));
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(summary.usage.cache_read_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(summary.usage.output_tokens.unwrap_or(0));
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(summary.usage.reasoning_tokens.unwrap_or(0));
        self.total_tokens = self
            .total_tokens
            .saturating_add(summary.usage.computed_total());
        let mut cost = self.cost_accumulator();
        cost.add_effective(&summary.cost);
        self.set_cost_from_accumulator(cost);
    }

    pub fn add_totals(&mut self, other: &UsageTotals) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(other.cache_creation_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        let mut cost = self.cost_accumulator();
        cost.add_values(other.estimated_cost_micro_usd, other.estimated_cost_usd);
        self.set_cost_from_accumulator(cost);
    }
}

#[derive(Debug, Clone)]
pub struct UsageReportRow {
    pub provider: String,
    pub account: String,
    pub events: u64,
    pub usage: UsageTotals,
    pub sources: BTreeSet<String>,
    pub paths: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct SummaryReportRow {
    pub provider: String,
    pub account: String,
    pub kind: String,
    pub summaries: u64,
    pub usage: UsageTotals,
    pub direct_event_usage: UsageTotals,
    pub exact_overlap_summaries: u64,
    pub observed_at: Option<DateTime<Utc>>,
    pub sources: BTreeSet<String>,
    pub paths: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct SubscriptionReportRow {
    pub subscription_id: SubscriptionId,
    pub provider: String,
    pub provider_account_id: ProviderAccountId,
    pub account: String,
    pub plan_name: String,
    pub price: i64, // minor units (cents) of the currency
    pub currency: String,
    pub billing_period: BillingPeriod,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: SubscriptionStatus,
    pub events: u64,
    pub usage: UsageTotals,
    pub value_minus_price_usd: Option<i64>, // cents USD
    pub value_to_price_ratio: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct UsageReport {
    pub label: String,
    pub since: Option<DateTime<Utc>>,
    pub until: DateTime<Utc>,
    pub rows: Vec<UsageReportRow>,
    pub summary_rows: Vec<SummaryReportRow>,
    pub subscription_rows: Vec<SubscriptionReportRow>,
    pub total_events: u64,
    pub total_usage: UsageTotals,
    pub total_summary_usage: UsageTotals,
}

#[derive(Debug, Clone, Default)]
struct UsagePrefix {
    usage: UsageTotals,
    events: u64,
    estimated_cost_samples: u64,
}

impl UsagePrefix {
    fn add_event(&mut self, event: &UsageEvent) {
        self.usage.add_event(event);
        self.events += 1;
        if event.cost.estimated_micro_usd().is_some() {
            self.estimated_cost_samples += 1;
        }
    }

    fn difference(&self, earlier: &Self) -> (u64, UsageTotals) {
        let estimated_cost_samples = self
            .estimated_cost_samples
            .saturating_sub(earlier.estimated_cost_samples);
        let exact_cost_micro_usd = if estimated_cost_samples == 0 {
            None
        } else {
            let earlier_micro_usd = if earlier.estimated_cost_samples == 0 {
                Some(0)
            } else {
                earlier.usage.estimated_cost_micro_usd
            };
            self.usage
                .estimated_cost_micro_usd
                .zip(earlier_micro_usd)
                .map(|(current, earlier)| current.saturating_sub(earlier))
        };
        let estimated_cost_usd = (estimated_cost_samples > 0).then(|| {
            exact_cost_micro_usd.map_or_else(
                || {
                    self.usage
                        .estimated_cost_usd
                        .unwrap_or(0)
                        .saturating_sub(earlier.usage.estimated_cost_usd.unwrap_or(0))
                },
                micro_usd_to_cents_rounded,
            )
        });
        (
            self.events.saturating_sub(earlier.events),
            UsageTotals {
                input_tokens: self
                    .usage
                    .input_tokens
                    .saturating_sub(earlier.usage.input_tokens),
                cache_creation_tokens: self
                    .usage
                    .cache_creation_tokens
                    .saturating_sub(earlier.usage.cache_creation_tokens),
                cached_input_tokens: self
                    .usage
                    .cached_input_tokens
                    .saturating_sub(earlier.usage.cached_input_tokens),
                output_tokens: self
                    .usage
                    .output_tokens
                    .saturating_sub(earlier.usage.output_tokens),
                reasoning_tokens: self
                    .usage
                    .reasoning_tokens
                    .saturating_sub(earlier.usage.reasoning_tokens),
                total_tokens: self
                    .usage
                    .total_tokens
                    .saturating_sub(earlier.usage.total_tokens),
                estimated_cost_usd,
                estimated_cost_micro_usd: exact_cost_micro_usd,
            },
        )
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EventUsageSeries {
    timestamps: Vec<DateTime<Utc>>,
    prefixes: Vec<UsagePrefix>,
}

impl EventUsageSeries {
    pub(crate) fn from_events(mut events: Vec<&UsageEvent>) -> Self {
        events.sort_by_key(|event| event.session.started_at);
        let mut timestamps = Vec::with_capacity(events.len());
        let mut prefixes = Vec::with_capacity(events.len() + 1);
        prefixes.push(UsagePrefix::default());
        for event in events {
            timestamps.push(event.session.started_at);
            let mut next = prefixes.last().cloned().unwrap_or_default();
            next.add_event(event);
            prefixes.push(next);
        }
        Self {
            timestamps,
            prefixes,
        }
    }

    pub(crate) fn usage_between(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        end_inclusive: bool,
    ) -> (u64, UsageTotals) {
        if end < start {
            return (0, UsageTotals::default());
        }
        let start_index = self
            .timestamps
            .partition_point(|timestamp| *timestamp < start);
        let end_index = if end_inclusive {
            self.timestamps
                .partition_point(|timestamp| *timestamp <= end)
        } else {
            self.timestamps
                .partition_point(|timestamp| *timestamp < end)
        };
        self.prefixes[end_index].difference(&self.prefixes[start_index.min(end_index)])
    }
}

#[derive(Debug, Clone, Default)]
struct EventUsageIndex {
    by_account_label: BTreeMap<(String, String), EventUsageSeries>,
    by_account_id: BTreeMap<(String, String), EventUsageSeries>,
}

impl EventUsageIndex {
    fn new(events: &[UsageEvent], accounts: &BTreeMap<&str, &ProviderAccount>) -> Self {
        let mut by_account_label = BTreeMap::<_, Vec<_>>::new();
        let mut by_account_id = BTreeMap::<_, Vec<_>>::new();
        for event in events {
            let label = report_account_label(event, accounts);
            by_account_label
                .entry((event.provider.clone(), label))
                .or_default()
                .push(event);
            if let Some(account_id) = event.provider_account_id.as_ref() {
                by_account_id
                    .entry((event.provider.clone(), account_id.0.clone()))
                    .or_default()
                    .push(event);
            }
        }
        Self {
            by_account_label: by_account_label
                .into_iter()
                .map(|(key, events)| (key, EventUsageSeries::from_events(events)))
                .collect(),
            by_account_id: by_account_id
                .into_iter()
                .map(|(key, events)| (key, EventUsageSeries::from_events(events)))
                .collect(),
        }
    }
}

#[must_use]
pub fn build_usage_report(
    events: &[UsageEvent],
    summaries: &[UsageSummary],
    sources: &[SourceLocation],
    accounts: &[ProviderAccount],
    subscriptions: &[Subscription],
    period: ReportPeriod,
    now: DateTime<Utc>,
) -> UsageReport {
    let (since, until) = period.window(now);
    let (published_since, published_until) = period.published_window(now);
    let label = period.label(now);

    let source_by_id: BTreeMap<_, _> = sources
        .iter()
        .map(|source| (source.source_id.0.as_str(), source))
        .collect();
    let account_by_id: BTreeMap<_, _> = accounts
        .iter()
        .map(|account| (account.provider_account_id.0.as_str(), account))
        .collect();
    let event_usage_index = EventUsageIndex::new(events, &account_by_id);
    let mut rows: BTreeMap<(String, String), UsageReportRow> = BTreeMap::new();

    for event in events {
        if since.is_some_and(|since| event.session.started_at < since)
            || event.session.started_at > until
        {
            continue;
        }

        let source = source_by_id.get(event.source_id.0.as_str()).copied();
        let account = report_account_label(event, &account_by_id);
        let key = (event.provider.clone(), account.clone());
        let row = rows.entry(key).or_insert_with(|| UsageReportRow {
            provider: event.provider.clone(),
            account,
            events: 0,
            usage: UsageTotals::default(),
            sources: BTreeSet::new(),
            paths: BTreeSet::new(),
        });
        row.events += 1;
        row.usage.add_event(event);
        row.sources.insert(event.source_id.0.clone());
        if let Some(source) = source {
            row.paths.insert(preview_path_label(source));
        }
    }

    let mut summary_rows: BTreeMap<(String, String, String), SummaryReportRow> = BTreeMap::new();
    if matches!(period, ReportPeriod::AllTime) {
        for summary in summaries {
            if summary.observed_at > until {
                continue;
            }

            let source = source_by_id.get(summary.source_id.0.as_str()).copied();
            let account =
                report_identity_label(summary.provider_account_id.as_ref(), &account_by_id);
            let kind = summary.metadata.summary_format.clone();
            let key = (summary.provider.clone(), account.clone(), kind.clone());
            let direct_overlap_usage =
                direct_usage_for_summary(summary, &account, &event_usage_index, until);
            let exact_overlap =
                summary_usage_matches_direct_overlap(summary, &direct_overlap_usage);
            let row = summary_rows
                .entry(key.clone())
                .or_insert_with(|| SummaryReportRow {
                    provider: summary.provider.clone(),
                    account,
                    kind,
                    summaries: 0,
                    usage: UsageTotals::default(),
                    direct_event_usage: UsageTotals::default(),
                    exact_overlap_summaries: 0,
                    observed_at: None,
                    sources: BTreeSet::new(),
                    paths: BTreeSet::new(),
                });
            row.summaries += 1;
            row.usage.add_summary(summary);
            row.direct_event_usage.add_totals(&direct_overlap_usage);
            if exact_overlap {
                row.exact_overlap_summaries += 1;
            }
            row.observed_at = Some(
                row.observed_at
                    .map(|observed_at| observed_at.max(summary.observed_at))
                    .unwrap_or(summary.observed_at),
            );
            row.sources.insert(summary.source_id.0.clone());
            if let Some(source) = source {
                row.paths.insert(preview_path_label(source));
            }
        }
    }

    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by(|left, right| {
        right
            .usage
            .total_tokens
            .cmp(&left.usage.total_tokens)
            .then_with(|| left.account.cmp(&right.account))
    });
    let total_events = rows.iter().map(|row| row.events).sum();
    let mut total_usage = UsageTotals::default();
    for row in &rows {
        total_usage.add_totals(&row.usage);
    }
    let mut summary_rows: Vec<_> = summary_rows.into_values().collect();
    summary_rows.sort_by(|left, right| {
        right
            .usage
            .total_tokens
            .cmp(&left.usage.total_tokens)
            .then_with(|| left.account.cmp(&right.account))
            .then_with(|| left.kind.cmp(&right.kind))
    });
    let mut total_summary_usage = UsageTotals::default();
    for row in &summary_rows {
        total_summary_usage.add_totals(&row.usage);
    }
    let subscription_rows = build_subscription_report_rows(
        subscriptions,
        &account_by_id,
        &event_usage_index,
        since,
        until,
    );

    UsageReport {
        label,
        since: published_since,
        until: published_until,
        rows,
        summary_rows,
        subscription_rows,
        total_events,
        total_usage,
        total_summary_usage,
    }
}

fn report_account_label(event: &UsageEvent, accounts: &BTreeMap<&str, &ProviderAccount>) -> String {
    report_identity_label(event.provider_account_id.as_ref(), accounts)
}

fn direct_usage_for_summary(
    summary: &UsageSummary,
    summary_account: &str,
    event_usage_index: &EventUsageIndex,
    now: DateTime<Utc>,
) -> UsageTotals {
    let start = summary.period_start.unwrap_or(summary.observed_at);
    let end = summary.period_end.unwrap_or(summary.observed_at).min(now);
    event_usage_index
        .by_account_label
        .get(&(summary.provider.clone(), summary_account.to_string()))
        .map(|series| series.usage_between(start, end, true).1)
        .unwrap_or_default()
}

fn summary_usage_matches_direct_overlap(summary: &UsageSummary, direct: &UsageTotals) -> bool {
    if direct.total_tokens == 0 || summary.usage.computed_total() != direct.total_tokens {
        return false;
    }
    let summary_input = summary.usage.input_tokens.unwrap_or(0);
    let direct_input_matches = direct.input_tokens == summary_input
        || direct
            .input_tokens
            .saturating_sub(direct.cached_input_tokens)
            == summary_input;
    direct_input_matches
        && summary.usage.cache_creation_tokens.unwrap_or(0) == direct.cache_creation_tokens
        && summary.usage.cache_read_tokens.unwrap_or(0) == direct.cached_input_tokens
        && summary.usage.output_tokens.unwrap_or(0) == direct.output_tokens
        && summary.usage.reasoning_tokens.unwrap_or(0) == direct.reasoning_tokens
}

fn report_identity_label(
    provider_account_id: Option<&ProviderAccountId>,
    accounts: &BTreeMap<&str, &ProviderAccount>,
) -> String {
    if let Some(account_id) = provider_account_id {
        if let Some(account) = accounts.get(account_id.0.as_str()) {
            return display_account_identity(account);
        }
    }
    provider_account_id
        .map(|id| id.0.clone())
        .unwrap_or_else(|| "unassigned".to_string())
}

pub(crate) fn preview_path_label(source: &SourceLocation) -> String {
    let path = source.path_label.as_deref().unwrap_or("unknown");
    if let Some(home) = home_dir() {
        let home = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home.as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

#[must_use]
pub fn timestamp_in_period(
    timestamp: DateTime<Utc>,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
) -> bool {
    timestamp >= started_at
        && ended_at
            .map(|ended_at| timestamp < ended_at)
            .unwrap_or(true)
}

#[must_use]
pub fn periods_overlap(
    left_started_at: DateTime<Utc>,
    left_ended_at: Option<DateTime<Utc>>,
    right_started_at: DateTime<Utc>,
    right_ended_at: Option<DateTime<Utc>>,
) -> bool {
    let left_end = left_ended_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
    let right_end = right_ended_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
    left_started_at < right_end && right_started_at < left_end
}

fn build_subscription_report_rows(
    subscriptions: &[Subscription],
    accounts: &BTreeMap<&str, &ProviderAccount>,
    event_usage_index: &EventUsageIndex,
    since: Option<DateTime<Utc>>,
    until: DateTime<Utc>,
) -> Vec<SubscriptionReportRow> {
    let mut rows = Vec::new();
    for subscription in subscriptions {
        let provider_account_id = &subscription.provider_account_id;
        let started_at = subscription.started_at;
        let ended_at = effective_subscription_ended_at(subscription);
        if !subscription_intersects_report_window(started_at, ended_at, since, until) {
            continue;
        }
        let range_start = since.map_or(started_at, |since| started_at.max(since));
        let (range_end, end_inclusive) = ended_at
            .filter(|ended_at| *ended_at <= until)
            .map_or((until, true), |ended_at| (ended_at, false));
        let (events_count, usage) = event_usage_index
            .by_account_id
            .get(&(subscription.provider.clone(), provider_account_id.0.clone()))
            .map(|series| series.usage_between(range_start, range_end, end_inclusive))
            .unwrap_or_default();
        let account = accounts
            .get(provider_account_id.0.as_str())
            .map(|account| display_account_identity(account))
            .unwrap_or_else(|| provider_account_id.0.clone());
        let (value_minus_price_usd, value_to_price_ratio) = subscription_value_metrics(
            subscription.price,
            &subscription.currency,
            usage.estimated_cost_usd,
        );
        rows.push(SubscriptionReportRow {
            subscription_id: subscription.subscription_id.clone(),
            provider: subscription.provider.clone(),
            provider_account_id: provider_account_id.clone(),
            account,
            plan_name: subscription.plan_name.clone(),
            price: subscription.price,
            currency: subscription.currency.clone(),
            billing_period: subscription.billing_period.clone(),
            started_at,
            ended_at,
            status: subscription.status.clone(),
            events: events_count,
            usage,
            value_minus_price_usd,
            value_to_price_ratio,
        });
    }
    rows.sort_by(|left, right| {
        right
            .usage
            .total_tokens
            .cmp(&left.usage.total_tokens)
            .then_with(|| left.started_at.cmp(&right.started_at))
            .then_with(|| left.plan_name.cmp(&right.plan_name))
    });
    rows
}

fn effective_subscription_ended_at(subscription: &Subscription) -> Option<DateTime<Utc>> {
    if is_legacy_open_verified_subscription(subscription) {
        None
    } else {
        subscription.ended_at
    }
}

fn is_legacy_open_verified_subscription(subscription: &Subscription) -> bool {
    subscription.status == SubscriptionStatus::Active
        && is_verified_subscription_source(&subscription.record_source)
        && subscription.ended_at.is_some()
        && subscription.ended_at == subscription.current_period_ends_at
}

fn is_verified_subscription_source(source: &IdentitySource) -> bool {
    matches!(
        source,
        IdentitySource::LocalAuth
            | IdentitySource::ProviderAuth
            | IdentitySource::ProviderApi
            | IdentitySource::CookieOauth
            | IdentitySource::CliProbe
    )
}

fn subscription_intersects_report_window(
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    since: Option<DateTime<Utc>>,
    until: DateTime<Utc>,
) -> bool {
    if since.is_some_and(|start| start > until) || started_at > until {
        return false;
    }
    let window_start = since.unwrap_or(DateTime::<Utc>::MIN_UTC);
    periods_overlap(
        started_at,
        ended_at,
        window_start,
        Some(until + Duration::seconds(1)),
    )
}

fn subscription_value_metrics(
    price_cents: i64,
    currency: &str,
    estimated_cost_usd_cents: Option<i64>,
) -> (Option<i64>, Option<f64>) {
    if !currency.eq_ignore_ascii_case("USD") || price_cents <= 0 {
        return (None, None);
    }
    estimated_cost_usd_cents
        .map(|est_cents| {
            (
                Some(est_cents - price_cents),
                Some(est_cents as f64 / price_cents as f64),
            )
        })
        .unwrap_or((None, None))
}
