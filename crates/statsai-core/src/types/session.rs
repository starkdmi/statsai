use super::event::{Confidence, CostInfo, ModelInfo, ProjectInfo, UsageCounts};
use super::summary::{SummaryModelMetrics, SummaryModelUsage};
use crate::ids::{ProviderAccountId, SourceId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const SESSION_ROLLUP_SCHEMA_VERSION: &str = "session_rollup.v1";

/// Where a session title was resolved from. Event titles win over task spans,
/// which win over archived conversations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionTitleSource {
    Event,
    TaskSpan,
    Archive,
}

/// One local session, aggregated from the usage events that share its hashed id.
///
/// `session_id` is the hashed `session_…` identity already stored on each event.
/// Raw provider session ids never appear on this record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionRollupV1 {
    pub schema_version: String,
    pub session_id: String,
    pub device_id: String,
    pub provider: String,
    pub source_id: SourceId,
    pub provider_account_id: Option<ProviderAccountId>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// `None` when the session recorded only a start. Unknown durations go out
    /// as null; ingest stores them without an end so they stay out of medians.
    pub duration_seconds: Option<u64>,
    /// Closed usage object. Cache-lifetime splits stay local; the ingest allowlist
    /// does not include them.
    #[serde(with = "session_usage_serde")]
    #[schemars(with = "SessionUsageWire")]
    pub usage: UsageCounts,
    pub requests: u64,
    /// Closed cost object. Currency, pricing source, and confidence stay local.
    #[serde(with = "session_cost_serde")]
    #[schemars(with = "SessionCostWire")]
    pub cost: CostInfo,
    #[serde(with = "session_models_serde")]
    #[schemars(with = "Vec<SessionModelWire>")]
    pub models: Vec<SummaryModelUsage>,
    pub primary_model: Option<String>,
    pub total_messages: Option<u64>,
    pub user_messages: Option<u64>,
    pub assistant_messages: Option<u64>,
    pub developer_messages: Option<u64>,
    pub project: Option<ProjectInfo>,
    pub title: Option<String>,
    pub title_source: Option<SessionTitleSource>,
    pub updated_at: DateTime<Utc>,
}

/// Usage keys accepted on `session_rollup.v1`. Five-minute and one-hour cache
/// splits are folded into `cache_creation_tokens` before this leaves the device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct SessionUsageWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_prompt_eval_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_eval_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<u64>,
}

/// Cost keys accepted on `session_rollup.v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct SessionCostWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_reported_usd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_api_equivalent_usd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_reported_micro_usd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_api_equivalent_micro_usd: Option<i64>,
}

/// One model row on `session_rollup.v1`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionModelWire {
    pub model: ModelInfo,
    pub usage: SessionUsageWire,
    pub cost: SessionCostWire,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<SummaryModelMetrics>,
}

impl From<&UsageCounts> for SessionUsageWire {
    fn from(usage: &UsageCounts) -> Self {
        Self {
            total_tokens: usage.total_tokens,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            local_prompt_eval_tokens: usage.local_prompt_eval_tokens,
            local_eval_tokens: usage.local_eval_tokens,
            requests: usage.requests,
        }
    }
}

impl From<SessionUsageWire> for UsageCounts {
    fn from(usage: SessionUsageWire) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            cache_creation_5m_tokens: None,
            cache_creation_1h_tokens: None,
            cache_read_tokens: usage.cache_read_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            total_tokens: usage.total_tokens,
            requests: usage.requests,
            local_prompt_eval_tokens: usage.local_prompt_eval_tokens,
            local_eval_tokens: usage.local_eval_tokens,
        }
    }
}

impl From<&CostInfo> for SessionCostWire {
    fn from(cost: &CostInfo) -> Self {
        Self {
            provider_reported_usd: cost.provider_reported_usd,
            estimated_api_equivalent_usd: cost.estimated_api_equivalent_usd,
            provider_reported_micro_usd: cost.provider_reported_micro_usd,
            estimated_api_equivalent_micro_usd: cost.estimated_api_equivalent_micro_usd,
        }
    }
}

impl From<SessionCostWire> for CostInfo {
    fn from(cost: SessionCostWire) -> Self {
        Self {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: cost.estimated_api_equivalent_usd,
            provider_reported_usd: cost.provider_reported_usd,
            estimated_api_equivalent_micro_usd: cost.estimated_api_equivalent_micro_usd,
            provider_reported_micro_usd: cost.provider_reported_micro_usd,
            pricing_source: None,
            pricing_version: None,
            confidence: Confidence::Medium,
        }
    }
}

impl From<&SummaryModelUsage> for SessionModelWire {
    fn from(usage: &SummaryModelUsage) -> Self {
        Self {
            model: usage.model.clone(),
            usage: SessionUsageWire::from(&usage.usage),
            cost: SessionCostWire::from(&usage.cost),
            metrics: usage.metrics.clone(),
        }
    }
}

impl From<SessionModelWire> for SummaryModelUsage {
    fn from(usage: SessionModelWire) -> Self {
        Self {
            model: usage.model,
            usage: UsageCounts::from(usage.usage),
            cost: CostInfo::from(usage.cost),
            metrics: usage.metrics,
        }
    }
}

mod session_usage_serde {
    use super::{SessionUsageWire, UsageCounts};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S>(usage: &UsageCounts, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SessionUsageWire::from(usage).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<UsageCounts, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(UsageCounts::from(SessionUsageWire::deserialize(
            deserializer,
        )?))
    }
}

mod session_cost_serde {
    use super::{CostInfo, SessionCostWire};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S>(cost: &CostInfo, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SessionCostWire::from(cost).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<CostInfo, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(CostInfo::from(SessionCostWire::deserialize(deserializer)?))
    }
}

mod session_models_serde {
    use super::{SessionModelWire, SummaryModelUsage};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S>(
        models: &[SummaryModelUsage],
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let wire = models
            .iter()
            .map(SessionModelWire::from)
            .collect::<Vec<_>>();
        wire.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<SummaryModelUsage>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = Vec::<SessionModelWire>::deserialize(deserializer)?;
        Ok(wire.into_iter().map(SummaryModelUsage::from).collect())
    }
}
