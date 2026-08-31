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

mod cycles;
mod observations;
mod reconstruct;
mod sync;
mod windows;

pub(crate) use cycles::*;
pub(crate) use reconstruct::*;

pub(crate) const RESET_CLUSTER_TOLERANCE_SECONDS: i64 = 5 * 60;
/// A window cannot be observed once it has reset: the provider issues a fresh
/// `resets_at` from that moment on. An observation that post-dates its own reset
/// by more than provider recomputation lag is therefore a replay of historical
/// evidence recorded at import time, and must not extend the closed window.
///
/// The bound is an hour because the provider has been seen serving an elapsed
/// window for 37 continuous minutes: roughly 180 separate requests, seconds
/// apart, every one still carrying the expired `resets_at` at a steady 96%.
/// Discarding those loses the closing figure of a cycle spent to exhaustion.
/// Genuine lag has never been observed to outlast that; the stale records past
/// it sit 14 hours to 3 days late, which is import time, not lag.
pub(crate) const STALE_OBSERVATION_TOLERANCE_SECONDS: i64 = 60 * 60;
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
    pub discarded: QuotaDiscardCounts,
    pub assignment_overlap_warnings: Vec<String>,
}

/// What reconstruction threw away, and why.
///
/// Each rule discards evidence that cannot describe a real cycle, and each is
/// silent by design. Counting the discards is what tells a provider change
/// apart from a quiet week: if these numbers climb, the rules have started
/// firing on data they were never meant to judge.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct QuotaDiscardCounts {
    /// Observations recorded more than an hour after the window they describe
    /// had already reset.
    pub replayed_observations: u64,
    /// Windows that reported zero for their whole life and were superseded.
    pub unused_windows: u64,
    /// Schedules another schedule was reported both before and after.
    pub bracketed_schedules: u64,
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
}

#[cfg(test)]
mod tests;
