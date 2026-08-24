//! Provider-neutral quota observation and reconstructed-window contracts.

use crate::{EventId, ProviderAccountId, SourceId, UsageCounts};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const QUOTA_OBSERVATION_SCHEMA_VERSION: &str = "quota_observation.v1";
pub const QUOTA_WINDOW_OBSERVATION_SCHEMA_VERSION: &str = "quota_window_observation.v1";
pub const QUOTA_WINDOW_SCHEMA_VERSION: &str = "quota_window.v1";
pub const QUOTA_WINDOW_SYNC_PROJECTION_SCHEMA_VERSION: &str = "quota_window_sync_projection.v1";
pub const QUOTA_CYCLE_CONTRIBUTION_SCHEMA_VERSION: &str = "quota_cycle_contribution.v1";
pub const QUOTA_WEEKLY_WINDOW_MINUTES: u64 = 10_080;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotaUsageLinkKind {
    RecordEvent,
    TurnEvent,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotaTransitionKind {
    Initial,
    Early,
    OnOrAfterPreviousSchedule,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct QuotaCreditsV1 {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub balance: Option<String>,
    pub balance_raw: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct QuotaStatusV1 {
    pub plan_type: Option<String>,
    pub individual_limit: Option<Value>,
    pub spend_control_state: Option<String>,
    pub reached_type: Option<String>,
    pub credits: QuotaCreditsV1,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct QuotaProjectionCreditsV1 {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub balance: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct QuotaProjectionStatusV1 {
    pub plan_type: Option<String>,
    pub individual_limit: Option<Value>,
    pub spend_control_state: Option<String>,
    pub reached_type: Option<String>,
    pub credits: QuotaProjectionCreditsV1,
}

impl From<&QuotaStatusV1> for QuotaProjectionStatusV1 {
    fn from(status: &QuotaStatusV1) -> Self {
        Self {
            plan_type: status.plan_type.clone(),
            individual_limit: status.individual_limit.clone(),
            spend_control_state: status.spend_control_state.clone(),
            reached_type: status.reached_type.clone(),
            credits: QuotaProjectionCreditsV1 {
                has_credits: status.credits.has_credits,
                unlimited: status.credits.unlimited,
                balance: status.credits.balance.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QuotaObservationV1 {
    pub schema_version: String,
    pub observation_id: String,
    pub semantic_fingerprint: String,
    pub provider: String,
    pub source_id: SourceId,
    pub provider_account_id: Option<ProviderAccountId>,
    pub observed_at: DateTime<Utc>,
    pub source_file_path_hash: String,
    pub source_record_id: String,
    pub source_line_number: u64,
    pub payload_hash: String,
    pub usage_sample: Option<UsageCounts>,
    pub usage_event_id: Option<EventId>,
    pub usage_link_kind: QuotaUsageLinkKind,
    pub status: QuotaStatusV1,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QuotaWindowObservationV1 {
    pub schema_version: String,
    pub window_observation_id: String,
    pub observation_id: String,
    /// Provider field name such as `primary` or `secondary`; evidence only.
    pub provider_slot: String,
    pub limit_id: Option<String>,
    pub window_minutes: u64,
    pub used_percent: f64,
    pub resets_at: DateTime<Utc>,
    pub resets_at_epoch_seconds: i64,
}

/// One parsed source record plus the content-addressed provider payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaObservationRecordV1 {
    pub observation: QuotaObservationV1,
    pub windows: Vec<QuotaWindowObservationV1>,
    pub raw_rate_limits: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QuotaChangePointV1 {
    pub observed_at: DateTime<Utc>,
    pub used_percent: f64,
    pub resets_at: DateTime<Utc>,
    pub resets_at_epoch_seconds: i64,
    pub point_fingerprint: String,
    pub provider_slot: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct QuotaUsageTotalsV1 {
    pub event_count: u64,
    pub total_tokens: u64,
    pub estimated_cost_micro_usd: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QuotaWindowV1 {
    pub schema_version: String,
    pub window_id: String,
    pub provider: String,
    pub provider_account_id: Option<ProviderAccountId>,
    /// Local source partition for unattributed evidence; omitted after account attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<SourceId>,
    pub limit_id: Option<String>,
    pub window_minutes: u64,
    pub inferred_start: DateTime<Utc>,
    pub representative_reset: DateTime<Utc>,
    pub representative_reset_epoch_seconds: i64,
    pub reset_min: DateTime<Utc>,
    pub reset_min_epoch_seconds: i64,
    pub reset_max: DateTime<Utc>,
    pub reset_max_epoch_seconds: i64,
    pub first_observed_at: DateTime<Utc>,
    pub last_observed_at: DateTime<Utc>,
    pub sample_count: u64,
    pub first_used_percent: f64,
    pub latest_used_percent: f64,
    pub minimum_used_percent: f64,
    pub maximum_used_percent: f64,
    pub transition: QuotaTransitionKind,
    pub has_schedule_overlap: bool,
    pub change_points: Vec<QuotaChangePointV1>,
    pub latest_status: QuotaStatusV1,
    /// Account-scoped deduplicated usage, unavailable while the window is unattributed.
    pub usage_totals: Option<QuotaUsageTotalsV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QuotaWindowSyncProjectionV1 {
    pub schema_version: String,
    /// A deterministic device contribution, not a logical account-window ID.
    pub projection_id: String,
    pub device_id: String,
    pub provider: String,
    pub provider_account_id: ProviderAccountId,
    pub limit_id: Option<String>,
    pub window_minutes: u64,
    pub inferred_start: DateTime<Utc>,
    pub representative_reset: DateTime<Utc>,
    pub representative_reset_epoch_seconds: i64,
    pub reset_min: DateTime<Utc>,
    pub reset_min_epoch_seconds: i64,
    pub reset_max: DateTime<Utc>,
    pub reset_max_epoch_seconds: i64,
    pub first_observed_at: DateTime<Utc>,
    pub last_observed_at: DateTime<Utc>,
    pub sample_count: u64,
    pub first_used_percent: f64,
    pub latest_used_percent: f64,
    pub minimum_used_percent: f64,
    pub maximum_used_percent: f64,
    pub change_points: Vec<QuotaChangePointV1>,
    pub latest_status: QuotaProjectionStatusV1,
}

/// Timestamped first/last/min/max percentages observed on one UTC day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QuotaDailyEnvelopeV1 {
    pub day: String,
    pub first_observed_at: DateTime<Utc>,
    pub first_used_percent: f64,
    pub last_observed_at: DateTime<Utc>,
    pub last_used_percent: f64,
    pub minimum_used_percent: f64,
    pub maximum_used_percent: f64,
}

/// Exact usage for a partial UTC day at a cycle or schedule-transition boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct QuotaUsageSliceV1 {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_micro_usd: i64,
}

impl QuotaUsageSliceV1 {
    pub fn add_usage(&mut self, usage: &UsageCounts, estimated_cost_micro_usd: Option<i64>) {
        self.input_tokens = self
            .input_tokens
            .saturating_add(usage.input_tokens.unwrap_or(0));
        self.cache_creation_tokens = self.cache_creation_tokens.saturating_add(
            usage
                .cache_creation_tokens
                .unwrap_or(0)
                .saturating_add(usage.cache_creation_5m_tokens.unwrap_or(0))
                .saturating_add(usage.cache_creation_1h_tokens.unwrap_or(0)),
        );
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(usage.output_tokens.unwrap_or(0));
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(usage.reasoning_tokens.unwrap_or(0));
        self.total_tokens = self.total_tokens.saturating_add(usage.computed_total());
        if let Some(value) = estimated_cost_micro_usd {
            self.estimated_cost_micro_usd = self.estimated_cost_micro_usd.saturating_add(value);
        }
    }
}

/// Hosted device contribution for one attributed quota cycle. Omits raw events,
/// paths, source IDs, payloads, plans, credits, slots, fingerprints, and sample
/// counts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QuotaCycleContributionV1 {
    pub schema_version: String,
    /// Deterministic device contribution ID, not a logical account-cycle ID.
    pub contribution_id: String,
    pub provider: String,
    pub provider_account_id: ProviderAccountId,
    pub limit_id: Option<String>,
    pub window_minutes: u64,
    pub representative_reset: DateTime<Utc>,
    pub representative_reset_epoch_seconds: i64,
    /// True when this device locally reconstructed another cycle for the same
    /// scope whose schedule overlaps this one. Codex weekly cycles start lazily
    /// at first use, so a corroborated overlap means the neighbouring cycle was
    /// reset early (banked or server-granted), not that the data conflicts.
    #[serde(default)]
    pub has_schedule_overlap: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub daily_envelopes: Vec<QuotaDailyEnvelopeV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boundary_slices: Vec<QuotaUsageSliceV1>,
}
