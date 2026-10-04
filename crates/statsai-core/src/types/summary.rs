use super::event::{
    CostInfo, EventSource, MetricStats, ModelInfo, ParseEvidence, PrivacyInfo, ProjectInfo,
    UsageCounts,
};
use crate::cache::CacheHealthV1;
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
    /// Prompt-cache diagnostics, for providers whose events record call order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_health: Option<CacheHealthV1>,
}

impl SummaryMetrics {
    /// Whether no metric is set, in which case a summary carries `None`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.active_seconds.is_none()
            && self.tracked_requests.is_none()
            && self.tracked_output_tokens.is_none()
            && self.tracked_reasoning_tokens.is_none()
            && self.latency_ms.is_none()
            && self.time_to_first_token_ms.is_none()
            && self.generated_tps.is_none()
            && self.visible_tps.is_none()
            && self.overall_generated_tps.is_none()
            && self.overall_visible_tps.is_none()
            && self.cache_hit_ratio.is_none()
            && self.reasoning_share.is_none()
            && self.total_messages.is_none()
            && self.user_messages.is_none()
            && self.assistant_messages.is_none()
            && self.developer_messages.is_none()
            && self.cache_health.is_none()
    }
}

impl UsageSummary {
    /// The summary as a receiver without `cache_health` support stores it:
    /// the object removed, and metrics dropped when nothing else is left.
    #[must_use]
    pub fn without_cache_health(mut self) -> Self {
        if let Some(metrics) = self.metrics.as_mut() {
            metrics.cache_health = None;
        }
        if self.metrics.as_ref().is_some_and(SummaryMetrics::is_empty) {
            self.metrics = None;
        }
        self
    }
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
