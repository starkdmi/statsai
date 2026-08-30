use super::account::{IdentitySource, PrivacyMode};
use super::source::{LocationOrigin, SourceKind};
use crate::ids::{EventId, ProviderAccountId, SourceId, SubscriptionId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventSource {
    pub adapter_id: String,
    pub adapter_version: String,
    pub source_kind: SourceKind,
    pub location_origin: Option<LocationOrigin>,
    pub source_type: String,
    pub source_path_hash: Option<String>,
    pub source_record_id: Option<String>,
    pub parse_confidence: Confidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    pub session_id: String,
    pub local_session_id_hash: Option<String>,
    pub title: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultracode,
}

impl ReasoningLevel {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            "ultracode" => Some(Self::Ultracode),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Ultracode => "ultracode",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct ModelInfo {
    pub name: Option<String>,
    pub normalized_name: Option<String>,
    pub provider_model_id: Option<String>,
    /// Provider-reported inference speed, such as Claude's `standard` or `fast`.
    pub speed: Option<String>,
    pub reasoning_level: Option<ReasoningLevel>,
    pub reasoning_level_raw: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct UsageCounts {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    /// Anthropic cache writes using the default five-minute lifetime.
    pub cache_creation_5m_tokens: Option<u64>,
    /// Anthropic cache writes using the extended one-hour lifetime.
    pub cache_creation_1h_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub requests: Option<u64>,
    pub local_prompt_eval_tokens: Option<u64>,
    pub local_eval_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeInfo {
    pub runtime_name: Option<String>,
    pub host_id: Option<String>,
    /// End-to-end request or turn duration, not time to first token.
    pub latency_ms: Option<u64>,
    /// Provenance of latency_ms when the adapter can distinguish it.
    pub latency_source: Option<LatencySource>,
    /// Time from request start until the first visible token arrives.
    pub time_to_first_token_ms: Option<u64>,
    pub prompt_eval_duration_ms: Option<u64>,
    pub eval_duration_ms: Option<u64>,
    pub total_messages: Option<u64>,
    pub user_messages: Option<u64>,
    pub assistant_messages: Option<u64>,
    pub developer_messages: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LatencySource {
    Explicit,
    Inferred,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MetricStats {
    pub samples: u64,
    pub avg: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub sum: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CostInfo {
    pub currency: String,
    pub estimated_api_equivalent_usd: Option<i64>, // cents USD
    pub provider_reported_usd: Option<i64>,        // cents USD
    /// Exact estimated cost in millionths of a USD. New producers populate this
    /// alongside the legacy cent field so older consumers remain compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_api_equivalent_micro_usd: Option<i64>,
    /// Exact provider-reported cost in millionths of a USD, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_reported_micro_usd: Option<i64>,
    pub pricing_source: Option<String>,
    pub pricing_version: Option<String>,
    pub confidence: Confidence,
}

pub const MICRO_USD_PER_CENT: i64 = 10_000;

#[must_use]
pub fn micro_usd_to_cents_rounded(micro_usd: i64) -> i64 {
    let half_cent = MICRO_USD_PER_CENT / 2;
    if micro_usd >= 0 {
        micro_usd.saturating_add(half_cent) / MICRO_USD_PER_CENT
    } else {
        micro_usd.saturating_sub(half_cent) / MICRO_USD_PER_CENT
    }
}

impl CostInfo {
    /// Returns the exact estimated cost when present, or converts a legacy
    /// whole-cent value for payloads created before micro-USD support.
    #[must_use]
    pub fn estimated_micro_usd(&self) -> Option<i64> {
        self.estimated_api_equivalent_micro_usd.or_else(|| {
            self.estimated_api_equivalent_usd
                .map(|cents| cents.saturating_mul(MICRO_USD_PER_CENT))
        })
    }

    /// Returns the exact provider cost when present, or converts a legacy
    /// whole-cent value for payloads created before micro-USD support.
    #[must_use]
    pub fn provider_reported_micro_usd_value(&self) -> Option<i64> {
        self.provider_reported_micro_usd.or_else(|| {
            self.provider_reported_usd
                .map(|cents| cents.saturating_mul(MICRO_USD_PER_CENT))
        })
    }

    pub fn set_estimated_micro_usd(&mut self, micro_usd: i64) {
        self.estimated_api_equivalent_micro_usd = Some(micro_usd);
        self.estimated_api_equivalent_usd = Some(micro_usd_to_cents_rounded(micro_usd));
    }

    pub fn set_provider_reported_micro_usd(&mut self, micro_usd: i64) {
        self.provider_reported_micro_usd = Some(micro_usd);
        self.provider_reported_usd = Some(micro_usd_to_cents_rounded(micro_usd));
    }
}

/// Accumulates exact micro-USD values and rounds only when a cent value is
/// requested. Legacy cent-only inputs remain supported, including values too
/// large to represent as micro-USD.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CostAccumulator {
    has_value: bool,
    micro_usd: Option<i64>,
    fallback_cents: i64,
}

impl CostAccumulator {
    pub fn add_values(&mut self, exact_micro_usd: Option<i64>, legacy_cents: Option<i64>) {
        let Some(fallback_cents) = exact_micro_usd
            .map(micro_usd_to_cents_rounded)
            .or(legacy_cents)
        else {
            return;
        };
        let converted_micro_usd = exact_micro_usd
            .or_else(|| legacy_cents.and_then(|cents| cents.checked_mul(MICRO_USD_PER_CENT)));

        if self.has_value {
            self.micro_usd = self
                .micro_usd
                .zip(converted_micro_usd)
                .and_then(|(current, next)| current.checked_add(next));
        } else {
            self.micro_usd = converted_micro_usd;
            self.has_value = true;
        }
        self.fallback_cents = self.fallback_cents.saturating_add(fallback_cents);
    }

    pub fn add_estimated(&mut self, cost: &CostInfo) {
        self.add_values(
            cost.estimated_api_equivalent_micro_usd,
            cost.estimated_api_equivalent_usd,
        );
    }

    pub fn add_effective(&mut self, cost: &CostInfo) {
        if cost.provider_reported_micro_usd.is_some() || cost.provider_reported_usd.is_some() {
            self.add_values(cost.provider_reported_micro_usd, cost.provider_reported_usd);
        } else {
            self.add_estimated(cost);
        }
    }

    #[must_use]
    pub fn micro_usd(&self) -> Option<i64> {
        self.has_value.then_some(self.micro_usd).flatten()
    }

    #[must_use]
    pub fn cents_rounded(&self) -> Option<i64> {
        self.has_value.then(|| {
            self.micro_usd
                .map_or(self.fallback_cents, micro_usd_to_cents_rounded)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ParseEvidence {
    pub event_key_version: String,
    pub source_file_path_hash: Option<String>,
    pub source_line_number: Option<u64>,
    pub source_record_id: Option<String>,
    pub model_inferred: bool,
    pub timestamp_inferred: bool,
    pub account_identity_source: IdentitySource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectInfo {
    pub project_id: String,
    pub project_label: Option<String>,
    pub repo_remote_hash: Option<String>,
    pub repo_label: Option<String>,
    pub branch_hash: Option<String>,
    pub branch_label: Option<String>,
    pub path_hash: Option<String>,
    pub path_label: Option<String>,
}

#[must_use]
pub fn project_has_stable_identity(project: &ProjectInfo) -> bool {
    project
        .repo_remote_hash
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || project
            .path_hash
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
}

#[must_use]
pub fn project_has_remote_identity(project: &ProjectInfo) -> bool {
    project
        .repo_remote_hash
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
}

#[must_use]
pub fn project_contains_file_paths(project: Option<&ProjectInfo>) -> bool {
    project
        .and_then(|project| project.path_label.as_deref())
        .is_some_and(|value| !value.trim().is_empty())
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[must_use]
pub fn project_bucket_key(project: Option<&ProjectInfo>) -> String {
    let Some(project) = project else {
        return "none".to_string();
    };
    if !project_has_stable_identity(project) {
        return "none".to_string();
    }
    if project.path_hash.is_some()
        || project.repo_remote_hash.is_some()
        || project.branch_hash.is_some()
    {
        return format!(
            "repo:{}|path:{}|branch:{}",
            project.repo_remote_hash.as_deref().unwrap_or("none"),
            project.path_hash.as_deref().unwrap_or("none"),
            project.branch_hash.as_deref().unwrap_or("none")
        );
    }
    project.project_id.clone()
}

/// The checkout a daily rollup bucket belongs to.
///
/// A daily rollup answers "how much work happened here on this day", and "here"
/// is a working directory on a branch. The git remote is not part of that: a
/// repository rename leaves the same checkout in the same place doing the same
/// work, and keying on it split one day into two records that the backend then
/// matched on location and collapsed, dropping one side's tokens.
///
/// Deliberately separate from [`project_bucket_key`], which also keys persisted
/// task spans. Those are already stored under the remote-inclusive key, so
/// changing it underneath them would split task history between spans scanned
/// before and after an upgrade.
///
/// This is also narrower than project identity. Projects are keyed on the
/// remote and own many locations, and the backend already moves a location
/// (with its history) between projects when its remote changes. That is what
/// keeps a rename and a folder move attributed to one project; this key only
/// decides which rollup rows exist.
#[must_use]
pub fn daily_rollup_project_key(project: Option<&ProjectInfo>) -> String {
    let Some(project) = project else {
        return "none".to_string();
    };
    if !project_has_stable_identity(project) {
        return "none".to_string();
    }
    let checkout = non_empty(project.path_hash.as_deref())
        .map(|path_hash| format!("path:{path_hash}"))
        .or_else(|| {
            non_empty(project.repo_remote_hash.as_deref()).map(|remote| format!("repo:{remote}"))
        });
    match checkout {
        Some(checkout) => format!(
            "{checkout}|branch:{}",
            project.branch_hash.as_deref().unwrap_or("none")
        ),
        None => project.project_id.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitInfo {
    pub nearby_commit_hashes: Vec<String>,
    pub nearby_commit_messages: Vec<String>,
    pub correlation_confidence: Option<Confidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrivacyInfo {
    pub mode: PrivacyMode,
    pub contains_prompt_text: bool,
    pub contains_response_text: bool,
    pub contains_file_paths: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UsageEvent {
    pub schema_version: String,
    pub event_id: EventId,
    pub device_id: String,
    pub provider: String,
    pub source_id: SourceId,
    pub provider_account_id: Option<ProviderAccountId>,
    pub subscription_id: Option<SubscriptionId>,
    pub source: EventSource,
    pub session: SessionInfo,
    pub model: Option<ModelInfo>,
    pub usage: UsageCounts,
    pub runtime: Option<RuntimeInfo>,
    pub cost: CostInfo,
    pub parse_evidence: Option<ParseEvidence>,
    pub project: Option<ProjectInfo>,
    pub git: Option<GitInfo>,
    pub privacy: PrivacyInfo,
    pub created_at: DateTime<Utc>,
    pub imported_at: DateTime<Utc>,
}

impl UsageCounts {
    #[must_use]
    pub fn computed_total(&self) -> u64 {
        self.total_tokens.unwrap_or_else(|| {
            self.input_tokens
                .unwrap_or(0)
                .saturating_add(self.output_tokens.unwrap_or(0))
                .saturating_add(self.cache_creation_tokens.unwrap_or(0))
                .saturating_add(self.cache_read_tokens.unwrap_or(0))
                .saturating_add(self.reasoning_tokens.unwrap_or(0))
                .saturating_add(self.local_prompt_eval_tokens.unwrap_or(0))
                .saturating_add(self.local_eval_tokens.unwrap_or(0))
        })
    }
}
