use super::event::{
    CostInfo, EventSource, MetricStats, ModelInfo, ParseEvidence, PrivacyInfo, ProjectInfo,
    UsageCounts,
};
use crate::ids::{ProviderAccountId, SourceId, SummaryId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SummaryMetrics {
    pub active_seconds: Option<f64>,
    pub tracked_requests: Option<u64>,
    pub tracked_output_tokens: Option<u64>,
    pub tracked_reasoning_tokens: Option<u64>,
    /// Aggregated end-to-end request or turn duration, not TTFT.
    pub latency_ms: Option<MetricStats>,
    pub time_to_first_token_ms: Option<MetricStats>,
    /// Per-turn generated throughput distribution across tracked turns.
    pub generated_tps: Option<MetricStats>,
    /// Per-turn visible throughput distribution across tracked turns.
    pub visible_tps: Option<MetricStats>,
    /// Overall generated throughput across tracked active time.
    pub overall_generated_tps: Option<f64>,
    /// Overall visible throughput across tracked active time.
    pub overall_visible_tps: Option<f64>,
    pub cache_hit_ratio: Option<MetricStats>,
    pub reasoning_share: Option<MetricStats>,
    pub total_messages: Option<u64>,
    pub user_messages: Option<u64>,
    pub assistant_messages: Option<u64>,
    pub developer_messages: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SummaryModelUsage {
    pub model: ModelInfo,
    pub usage: UsageCounts,
    pub cost: CostInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<SummaryModelMetrics>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SummaryMetricTotals {
    pub samples: u64,
    pub sum: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SummaryModelMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_tps: Option<SummaryMetricTotals>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SummaryMetadata {
    pub summary_format: String,
    pub summary_version: Option<String>,
    pub total_sessions: Option<u64>,
    pub total_messages: Option<u64>,
    pub last_computed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UsageSummary {
    pub schema_version: String,
    pub summary_id: SummaryId,
    pub device_id: String,
    pub provider: String,
    pub source_id: SourceId,
    pub provider_account_id: Option<ProviderAccountId>,
    pub source: EventSource,
    pub model: Option<ModelInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<SummaryModelUsage>,
    pub usage: UsageCounts,
    pub cost: CostInfo,
    pub parse_evidence: Option<ParseEvidence>,
    pub project: Option<ProjectInfo>,
    pub privacy: PrivacyInfo,
    pub metrics: Option<SummaryMetrics>,
    pub period_start: Option<DateTime<Utc>>,
    pub period_end: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub metadata: SummaryMetadata,
    pub imported_at: DateTime<Utc>,
}
