use crate::{
    BillingPeriod, CostAccumulator, ProviderAccountId, SubscriptionId, SubscriptionStatus,
    UsageEvent, UsageSummary,
};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeSet;

mod build;
pub use build::*;

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
