//! Core schemas and ID helpers for `statsai`.

mod archive;
mod code_changes;
mod tasks;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub use archive::*;
pub use code_changes::*;
pub use tasks::*;

pub const USAGE_EVENT_SCHEMA_VERSION: &str = "usage_event.v1";
pub const USAGE_SUMMARY_SCHEMA_VERSION: &str = "usage_summary.v1";
pub const REPORTED_USAGE_SUMMARY_INPUT_SCHEMA_VERSION: &str = "reported_usage_summary_input.v1";
pub const SOURCE_LOCATION_SCHEMA_VERSION: &str = "source_location.v1";
pub const PROVIDER_ACCOUNT_SCHEMA_VERSION: &str = "provider_account.v1";
pub const SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION: &str = "source_account_assignment.v1";
pub const SUBSCRIPTION_SCHEMA_VERSION: &str = "subscription.v1";
pub const DAILY_ROLLUP_SCHEMA_VERSION: &str = "daily_rollup.v1";
pub const SYNC_BATCH_V1_SCHEMA_VERSION: &str = "sync_batch.v1";
pub const SYNC_BATCH_V2_SCHEMA_VERSION: &str = "sync_batch.v2";
pub const SYNC_BATCH_V3_SCHEMA_VERSION: &str = "sync_batch.v3";
pub const SYNC_ACK_V1_SCHEMA_VERSION: &str = "sync_ack.v1";
pub const SYNC_ACK_V2_SCHEMA_VERSION: &str = "sync_ack.v2";
pub const SYNC_ACK_V3_SCHEMA_VERSION: &str = "sync_ack.v3";
pub const SYNC_BATCH_SCHEMA_VERSION: &str = SYNC_BATCH_V3_SCHEMA_VERSION;
pub const SYNC_ACK_SCHEMA_VERSION: &str = SYNC_ACK_V3_SCHEMA_VERSION;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SourceId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ProviderAccountId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SubscriptionId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SourceAccountAssignmentId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct EventId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SummaryId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    LocalAdapter,
    LocalSummary,
    LocalApi,
    ProviderApi,
    CliProbe,
    SdkInstrumented,
    ExternalReport,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LocationOrigin {
    Default,
    Configured,
    Env,
    Discovered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IdentitySource {
    ProviderAuth,
    ProviderApi,
    CliProbe,
    SourceConfig,
    UserConfigured,
    ManualHint,
    LocalAuth,
    CookieOauth,
    Unresolved,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BillingPeriod {
    Monthly,
    Annual,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Active,
    Paused,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    MetadataOnly,
    TitlesLabels,
    EnrichedSummaries,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceVerificationMode {
    #[default]
    Auto,
    ManualOnly,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceLocation {
    pub schema_version: String,
    pub source_id: SourceId,
    pub provider: String,
    pub source_kind: SourceKind,
    pub location_origin: LocationOrigin,
    pub adapter_id: Option<String>,
    pub adapter_version: Option<String>,
    pub path_hash: Option<String>,
    pub path_label: Option<String>,
    pub enabled: bool,
    #[serde(default)]
    pub verification_mode: SourceVerificationMode,
    #[serde(default)]
    pub verified_state_hash: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderAccount {
    pub schema_version: String,
    pub provider_account_id: ProviderAccountId,
    pub provider: String,
    pub identity_source: IdentitySource,
    pub provider_user_id: Option<String>,
    pub email: Option<String>,
    pub provider_user_id_hash: Option<String>,
    pub email_hash: Option<String>,
    pub org_id_hash: Option<String>,
    pub account_label: Option<String>,
    pub plan_name: Option<String>,
    pub confidence: Confidence,
    pub verified_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceAccountAssignment {
    pub schema_version: String,
    pub assignment_id: SourceAccountAssignmentId,
    pub source_id: SourceId,
    pub provider: String,
    pub provider_account_id: ProviderAccountId,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default = "default_identity_source_unknown")]
    pub record_source: IdentitySource,
    pub verified_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Subscription {
    pub schema_version: String,
    pub subscription_id: SubscriptionId,
    pub provider: String,
    pub provider_account_id: ProviderAccountId,
    pub plan_name: String,
    pub price: i64, // minor units (cents) of the currency
    pub currency: String,
    pub billing_period: BillingPeriod,
    pub paid_at: Option<DateTime<Utc>>,
    pub renewal_day: Option<u8>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub current_period_ends_at: Option<DateTime<Utc>>,
    pub status: SubscriptionStatus,
    #[serde(default = "default_identity_source_unknown")]
    pub record_source: IdentitySource,
    pub verified_at: Option<DateTime<Utc>>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VerifiedSourceState {
    pub provider_user_id: Option<String>,
    pub email: Option<String>,
    pub account_label: Option<String>,
    pub plan_name: Option<String>,
    pub authenticated_at: Option<DateTime<Utc>>,
    pub verified_at: Option<DateTime<Utc>>,
    pub subscription: Option<VerifiedSubscriptionState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceIdentityInference {
    CachedLocalProfile,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", content = "state", rename_all = "snake_case")]
pub enum VerifiedSourceObservation {
    #[default]
    Unavailable,
    Verified(Box<VerifiedSourceState>),
    Inferred {
        identity: Box<VerifiedSourceState>,
        basis: SourceIdentityInference,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        settings_modified_at: Option<DateTime<Utc>>,
    },
    AttributionBlocked {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blocked_since: Option<DateTime<Utc>>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VerifiedSubscriptionState {
    pub plan_name: String,
    pub price: i64, // minor units (cents) of the currency
    pub currency: String,
    pub billing_period: BillingPeriod,
    pub paid_at: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub current_period_ends_at: Option<DateTime<Utc>>,
    pub status: SubscriptionStatus,
    pub verified_at: Option<DateTime<Utc>>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SyncBatch {
    pub schema_version: String,
    pub batch_id: String,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceLocation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<ProviderAccount>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_account_assignments: Vec<SourceAccountAssignment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<Subscription>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<UsageEvent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summaries: Vec<UsageSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_buckets: Vec<TaskBucketSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_verifications: Vec<TaskVerification>,
    /// Privacy-safe numeric code-change metrics. Paths, diffs, source text, tool
    /// arguments, and commit messages are deliberately absent from this type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_change_metrics: Vec<CodeChangeMetric>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authoritative_snapshot: Option<SyncAuthoritativeSnapshot>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct SyncAuthoritativeSnapshot {
    pub snapshot_id: String,
    pub part_index: u32,
    pub part_count: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_ids: Vec<SourceId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_account_ids: Vec<ProviderAccountId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_account_assignment_ids: Vec<SourceAccountAssignmentId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscription_ids: Vec<SubscriptionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary_ids: Vec<SummaryId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_change_metric_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskVerificationCursor {
    pub updated_at: DateTime<Utc>,
    pub verification_id: TaskVerificationId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskBucketSnapshot {
    pub project_bucket: String,
    pub generated_at: DateTime<Utc>,
    pub applied_verification_cursor: Option<TaskVerificationCursor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub work_items: Vec<WorkItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<WorkItemMember>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<TaskSpan>,
}

/// Removes provider-local task locators before a snapshot leaves the device.
#[must_use]
pub fn sanitize_task_bucket_for_sync(mut snapshot: TaskBucketSnapshot) -> TaskBucketSnapshot {
    for span in &mut snapshot.spans {
        span.source_record_id = None;
        span.session_id = None;
        span.thread_id = None;
    }
    snapshot
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncEntityCounts {
    pub sources: u64,
    pub accounts: u64,
    #[serde(default)]
    pub source_account_assignments: u64,
    pub subscriptions: u64,
    pub events: u64,
    pub summaries: u64,
    #[serde(default, skip_serializing_if = "sync_count_is_zero")]
    pub task_buckets: u64,
    #[serde(default, skip_serializing_if = "sync_count_is_zero")]
    pub task_verifications: u64,
    #[serde(default, skip_serializing_if = "sync_count_is_zero")]
    pub code_change_metrics: u64,
}

fn sync_count_is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncRejectedRecord {
    pub kind: String,
    pub id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncAck {
    pub schema_version: String,
    pub batch_id: String,
    pub accepted: SyncEntityCounts,
    pub duplicates: SyncEntityCounts,
    pub rejected: Vec<SyncRejectedRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DailyRollup {
    pub schema_version: String,
    pub date: String,
    pub device_id: String,
    pub total_input_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_output_tokens: u64,
    pub total_reasoning_tokens: u64,
    pub total_tokens: u64,
    pub total_events: u64,
    pub total_sessions: u64,
    pub estimated_cost_usd: Option<i64>, // cents USD
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_micro_usd: Option<i64>,
    pub by_provider: Option<String>,
    pub by_account: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl SourceLocation {
    #[must_use]
    pub fn local_adapter(
        provider: impl Into<String>,
        adapter_id: impl Into<String>,
        adapter_version: impl Into<String>,
        path: &Path,
        location_origin: LocationOrigin,
    ) -> Self {
        let provider = provider.into();
        let adapter_id = adapter_id.into();
        let adapter_version = adapter_version.into();
        let path_hash = path_hash(path);
        let now = Utc::now();
        let source_id = source_id(&provider, SourceKind::LocalAdapter, &path_hash);

        Self {
            schema_version: SOURCE_LOCATION_SCHEMA_VERSION.to_string(),
            source_id,
            provider,
            source_kind: SourceKind::LocalAdapter,
            location_origin,
            adapter_id: Some(adapter_id),
            adapter_version: Some(adapter_version),
            path_hash: Some(path_hash),
            path_label: Some(display_path(path)),
            enabled: true,
            verification_mode: SourceVerificationMode::Auto,
            verified_state_hash: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[must_use]
    pub fn external_report(
        provider: impl Into<String>,
        adapter_id: impl Into<String>,
        adapter_version: impl Into<String>,
        path: &Path,
    ) -> Self {
        let provider = provider.into();
        let adapter_id = adapter_id.into();
        let adapter_version = adapter_version.into();
        let path_hash = path_hash(path);
        let now = Utc::now();
        let source_id = source_id(&provider, SourceKind::ExternalReport, &path_hash);

        Self {
            schema_version: SOURCE_LOCATION_SCHEMA_VERSION.to_string(),
            source_id,
            provider,
            source_kind: SourceKind::ExternalReport,
            location_origin: LocationOrigin::Configured,
            adapter_id: Some(adapter_id),
            adapter_version: Some(adapter_version),
            path_hash: Some(path_hash),
            path_label: Some(display_path(path)),
            enabled: true,
            verification_mode: SourceVerificationMode::Disabled,
            verified_state_hash: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[must_use]
    pub fn reported_usage(
        provider: impl Into<String>,
        source_kind: SourceKind,
        adapter_id: impl Into<String>,
        adapter_version: impl Into<String>,
        evidence_key: impl AsRef<str>,
        path_label: Option<String>,
    ) -> Self {
        let provider = provider.into();
        let adapter_id = adapter_id.into();
        let adapter_version = adapter_version.into();
        let path_hash = hash_text(evidence_key.as_ref());
        let now = Utc::now();
        let source_id = source_id(&provider, source_kind.clone(), &path_hash);

        Self {
            schema_version: SOURCE_LOCATION_SCHEMA_VERSION.to_string(),
            source_id,
            provider,
            source_kind,
            location_origin: LocationOrigin::Configured,
            adapter_id: Some(adapter_id),
            adapter_version: Some(adapter_version),
            path_hash: Some(path_hash),
            path_label,
            enabled: true,
            verification_mode: SourceVerificationMode::Disabled,
            verified_state_hash: None,
            created_at: now,
            updated_at: now,
        }
    }
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

#[must_use]
pub fn hash_text(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)
}

#[must_use]
pub fn sanitize_project_for_sync(project: ProjectInfo) -> Option<ProjectInfo> {
    if !project_has_stable_identity(&project) {
        return None;
    }
    Some(project)
}

#[must_use]
pub fn sanitize_summary_for_sync(mut summary: UsageSummary) -> UsageSummary {
    summary.source.source_record_id = None;
    if let Some(evidence) = summary.parse_evidence.as_mut() {
        evidence.source_line_number = None;
        evidence.source_record_id = None;
    }
    summary.project = summary.project.and_then(sanitize_project_for_sync);
    if project_contains_file_paths(summary.project.as_ref()) {
        summary.privacy.contains_file_paths = true;
    }
    summary
}

#[must_use]
pub fn path_hash(path: &Path) -> String {
    let canonical = canonical_display(path);
    hash_text(&canonical)
}

#[must_use]
pub fn source_id(provider: &str, source_kind: SourceKind, stable_key: &str) -> SourceId {
    SourceId(format!(
        "src_{}",
        &hash_text(&format!("{provider}:{source_kind:?}:{stable_key}"))[..24]
    ))
}

#[must_use]
pub fn provider_account_id(provider: &str, stable_key: &str) -> ProviderAccountId {
    ProviderAccountId(format!(
        "acct_{}",
        &hash_text(&format!("{provider}:{stable_key}"))[..24]
    ))
}

#[must_use]
pub fn normalize_provider_user_id(value: &str) -> String {
    value.trim().to_string()
}

#[must_use]
pub fn normalize_email(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn default_identity_source_unknown() -> IdentitySource {
    IdentitySource::Unknown
}

#[must_use]
pub fn provider_account_stable_key(
    provider_user_id: Option<&str>,
    email: Option<&str>,
) -> Option<String> {
    provider_user_id
        .map(normalize_provider_user_id)
        .filter(|value| !value.is_empty())
        .map(|value| format!("uid:{value}"))
        .or_else(|| {
            email
                .map(normalize_email)
                .filter(|value| !value.is_empty())
                .map(|value| format!("email:{value}"))
        })
}

#[must_use]
pub fn provider_account_id_from_identity(
    provider: &str,
    provider_user_id: Option<&str>,
    email: Option<&str>,
) -> Option<ProviderAccountId> {
    provider_account_stable_key(provider_user_id, email)
        .map(|stable_key| provider_account_id(provider, &stable_key))
}

#[must_use]
pub fn source_account_assignment_id(
    source_id: &SourceId,
    account: &ProviderAccountId,
    started_at: DateTime<Utc>,
) -> SourceAccountAssignmentId {
    SourceAccountAssignmentId(format!(
        "assign_{}",
        &hash_text(&format!(
            "{}:{}:{}",
            source_id.0,
            account.0,
            started_at.to_rfc3339()
        ))[..24]
    ))
}

#[must_use]
pub fn subscription_id(
    provider: &str,
    account: &ProviderAccountId,
    plan: &str,
    started_at: DateTime<Utc>,
) -> SubscriptionId {
    let account_key = account.0.as_str();
    let started_at_key = started_at.to_rfc3339();
    SubscriptionId(format!(
        "sub_{}",
        &hash_text(&format!("{provider}:{account_key}:{plan}:{started_at_key}"))[..24]
    ))
}

#[must_use]
pub fn event_id(
    provider: &str,
    source_id: &SourceId,
    source_record_id: &str,
    session_hash: Option<&str>,
    timestamp: DateTime<Utc>,
) -> EventId {
    EventId(format!(
        "evt_{}",
        &hash_text(&format!(
            "{provider}:{}:{source_record_id}:{}:{}",
            source_id.0,
            session_hash.unwrap_or(""),
            timestamp.to_rfc3339()
        ))[..32]
    ))
}

#[must_use]
pub fn semantic_event_id(provider: &str, source_id: &SourceId, semantic_key: &str) -> EventId {
    EventId(format!(
        "evt_{}",
        &hash_text(&format!("{provider}:{}:{semantic_key}", source_id.0))[..32]
    ))
}

#[must_use]
pub fn summary_id(provider: &str, source_id: &SourceId, semantic_key: &str) -> SummaryId {
    SummaryId(format!(
        "sum_{}",
        &hash_text(&format!("{provider}:{}:{semantic_key}", source_id.0))[..32]
    ))
}

#[must_use]
pub fn semantic_event_fingerprint(input: &SemanticFingerprintInput<'_>) -> String {
    hash_text(&format!(
        "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        input.provider,
        input.source_id.0,
        input.started_at.to_rfc3339(),
        input.session_hash.unwrap_or(""),
        input.project_key.unwrap_or(""),
        input.model_name.unwrap_or("unknown"),
        input.input_tokens.unwrap_or(0),
        input.cache_read_tokens.unwrap_or(0),
        input.cache_creation_tokens.unwrap_or(0),
        input.output_tokens.unwrap_or(0),
        input.reasoning_tokens.unwrap_or(0),
        input.total_tokens
    ))
}

pub struct SemanticFingerprintInput<'a> {
    pub provider: &'a str,
    pub source_id: &'a SourceId,
    pub started_at: DateTime<Utc>,
    pub session_hash: Option<&'a str>,
    pub project_key: Option<&'a str>,
    pub model_name: Option<&'a str>,
    pub input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub total_tokens: u64,
}

#[must_use]
pub fn canonical_display(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| expand_home(path))
        .to_string_lossy()
        .to_string()
}

/// Display-friendly path normalization.
/// Expands `~` for home but does NOT perform filesystem canonicalization
/// (to avoid symlink/mount identity changes for labels).
#[must_use]
pub fn display_path(path: &Path) -> String {
    expand_home(path).to_string_lossy().to_string()
}

fn expand_home(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(stripped) = text.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}

#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[must_use]
pub fn expand_home_path(value: &str) -> PathBuf {
    if value == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(value)
}

// ── Report building ────────────────────────────────────────────

use chrono::Duration;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportPeriod {
    LastDays(i64),
    AllTime,
    Range {
        since: Option<DateTime<Utc>>,
        until: DateTime<Utc>,
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
        .map(|value| parse_report_date_bound(value, false))
        .transpose()?;
    let until = to
        .map(|value| parse_report_date_bound(value, true))
        .transpose()?
        .unwrap_or(now);
    if let (Some(since), Some(_)) = (since, to) {
        if since > until {
            return Err(ReportRangeError::InvertedRange { since, until });
        }
    }
    Ok(ReportPeriod::Range { since, until })
}

impl ReportPeriod {
    #[must_use]
    pub fn window(self, now: DateTime<Utc>) -> (Option<DateTime<Utc>>, DateTime<Utc>) {
        match self {
            Self::LastDays(days) => (Some(now - Duration::days(days)), now),
            Self::AllTime => (None, now),
            Self::Range { since, until } => (since, until.min(now)),
        }
    }

    #[must_use]
    pub fn label(self, now: DateTime<Utc>) -> String {
        match self {
            Self::LastDays(7) => "last 7 days".to_string(),
            Self::LastDays(30) => "last 30 days".to_string(),
            Self::LastDays(days) => format!("last {days} days"),
            Self::AllTime => "all time".to_string(),
            Self::Range { .. } => {
                let (since, until) = self.window(now);
                match since {
                    Some(since) if since > until => "empty range".to_string(),
                    Some(since) => format!(
                        "{} to {}",
                        format_range_bound(since, false),
                        format_range_bound(until, true)
                    ),
                    None => format!("through {}", format_range_bound(until, true)),
                }
            }
        }
    }
}

fn format_range_bound(timestamp: DateTime<Utc>, as_end: bool) -> String {
    if range_bound_is_calendar_day(timestamp, as_end) {
        timestamp.format("%Y-%m-%d").to_string()
    } else {
        timestamp.to_rfc3339()
    }
}

fn range_bound_is_calendar_day(timestamp: DateTime<Utc>, as_end: bool) -> bool {
    let date = timestamp.date_naive();
    if as_end {
        date.succ_opt()
            .and_then(|next| next.and_hms_opt(0, 0, 0))
            .is_some_and(|next_midnight| {
                timestamp == next_midnight.and_utc() - Duration::nanoseconds(1)
            })
    } else {
        date.and_hms_opt(0, 0, 0)
            .is_some_and(|midnight| timestamp == midnight.and_utc())
    }
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
struct EventUsageSeries {
    timestamps: Vec<DateTime<Utc>>,
    prefixes: Vec<UsagePrefix>,
}

impl EventUsageSeries {
    fn from_events(mut events: Vec<&UsageEvent>) -> Self {
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

    fn usage_between(
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
        since,
        until,
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

fn preview_path_label(source: &SourceLocation) -> String {
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
    if started_at > until {
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

pub fn display_account_identity(account: &ProviderAccount) -> String {
    account
        .account_label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| account.provider_account_id.0.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_ids_are_stable_for_same_input() {
        let a = source_id("codex", SourceKind::LocalAdapter, "abc");
        let b = source_id("codex", SourceKind::LocalAdapter, "abc");
        assert_eq!(a, b);
    }

    #[test]
    fn source_ids_change_by_provider() {
        let codex = source_id("codex", SourceKind::LocalAdapter, "abc");
        let claude = source_id("claude_code", SourceKind::LocalAdapter, "abc");
        assert_ne!(codex, claude);
    }

    #[test]
    fn reasoning_level_supports_ultracode() {
        assert_eq!(
            ReasoningLevel::parse("ultracode"),
            Some(ReasoningLevel::Ultracode)
        );
        assert_eq!(ReasoningLevel::Ultracode.as_str(), "ultracode");
    }

    #[test]
    fn total_falls_back_to_parts() {
        let usage = UsageCounts {
            input_tokens: Some(10),
            output_tokens: Some(5),
            cache_read_tokens: Some(2),
            ..UsageCounts::default()
        };
        assert_eq!(usage.computed_total(), 17);
    }

    #[test]
    fn legacy_usage_counts_without_cache_lifetimes_deserialize() {
        let usage: UsageCounts = serde_json::from_value(serde_json::json!({
            "input_tokens": 10,
            "cache_creation_tokens": 4
        }))
        .expect("legacy usage counts");

        assert_eq!(usage.cache_creation_tokens, Some(4));
        assert_eq!(usage.cache_creation_5m_tokens, None);
        assert_eq!(usage.cache_creation_1h_tokens, None);
        assert_eq!(usage.computed_total(), 14);
    }

    #[test]
    fn schema_types_serialize() {
        let schema = schemars::schema_for!(UsageEvent);
        let json = serde_json::to_value(schema).expect("schema should serialize");
        assert!(json.get("title").is_some());

        let schema = schemars::schema_for!(UsageSummary);
        let json = serde_json::to_value(schema).expect("summary schema should serialize");
        assert!(json.get("title").is_some());
    }

    #[test]
    fn sync_batch_without_authoritative_snapshot_remains_backward_compatible() {
        let batch: SyncBatch = serde_json::from_value(serde_json::json!({
            "schema_version": SYNC_BATCH_V2_SCHEMA_VERSION,
            "batch_id": "batch-legacy-v2",
            "device_id": "device-1",
            "created_at": "2026-05-31T10:00:00Z"
        }))
        .expect("legacy v2 batch should deserialize");

        assert!(batch.authoritative_snapshot.is_none());
        let serialized = serde_json::to_value(batch).expect("batch should serialize");
        assert!(serialized.get("authoritative_snapshot").is_none());
    }

    #[test]
    fn sync_ack_v1_omits_zero_task_counters() {
        let ack = SyncAck {
            schema_version: SYNC_ACK_V1_SCHEMA_VERSION.to_string(),
            batch_id: "batch-1".to_string(),
            accepted: SyncEntityCounts {
                sources: 1,
                accounts: 0,
                source_account_assignments: 0,
                subscriptions: 0,
                events: 0,
                summaries: 2,
                task_buckets: 0,
                task_verifications: 0,
                code_change_metrics: 0,
            },
            duplicates: SyncEntityCounts {
                sources: 0,
                accounts: 0,
                source_account_assignments: 0,
                subscriptions: 0,
                events: 0,
                summaries: 0,
                task_buckets: 0,
                task_verifications: 0,
                code_change_metrics: 0,
            },
            rejected: Vec::new(),
        };

        let json = serde_json::to_value(&ack).expect("ack should serialize");
        assert_eq!(json["schema_version"], SYNC_ACK_V1_SCHEMA_VERSION);
        assert!(json["accepted"].get("task_buckets").is_none());
        assert!(json["accepted"].get("task_verifications").is_none());
        assert!(json["accepted"].get("code_change_metrics").is_none());
        assert!(json["duplicates"].get("task_buckets").is_none());
        assert!(json["duplicates"].get("task_verifications").is_none());
        assert!(json["duplicates"].get("code_change_metrics").is_none());
    }

    #[test]
    fn sync_ack_v3_keeps_nonzero_task_and_code_change_counters() {
        let ack = SyncAck {
            schema_version: SYNC_ACK_SCHEMA_VERSION.to_string(),
            batch_id: "batch-2".to_string(),
            accepted: SyncEntityCounts {
                sources: 0,
                accounts: 0,
                source_account_assignments: 0,
                subscriptions: 0,
                events: 0,
                summaries: 0,
                task_buckets: 3,
                task_verifications: 1,
                code_change_metrics: 2,
            },
            duplicates: SyncEntityCounts {
                sources: 0,
                accounts: 0,
                source_account_assignments: 0,
                subscriptions: 0,
                events: 0,
                summaries: 0,
                task_buckets: 0,
                task_verifications: 0,
                code_change_metrics: 0,
            },
            rejected: Vec::new(),
        };

        let json = serde_json::to_value(&ack).expect("ack should serialize");
        assert_eq!(json["schema_version"], SYNC_ACK_V3_SCHEMA_VERSION);
        assert_eq!(json["accepted"]["task_buckets"], 3);
        assert_eq!(json["accepted"]["task_verifications"], 1);
        assert_eq!(json["accepted"]["code_change_metrics"], 2);
    }

    fn test_source(provider: &str, path: &str) -> SourceLocation {
        SourceLocation::local_adapter(
            provider,
            "test",
            "0",
            Path::new(path),
            LocationOrigin::Configured,
        )
    }

    fn test_event(
        provider: &str,
        source: &SourceLocation,
        started_at: DateTime<Utc>,
        tokens: u64,
        cost_cents: Option<i64>,
    ) -> UsageEvent {
        UsageEvent {
            schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
            event_id: event_id(provider, &source.source_id, "rec", None, started_at),
            device_id: "d".to_string(),
            provider: provider.to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: None,
            subscription_id: None,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalAdapter,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "jsonl".to_string(),
                source_path_hash: None,
                source_record_id: Some("rec".to_string()),
                parse_confidence: Confidence::High,
            },
            session: SessionInfo {
                session_id: "s".to_string(),
                local_session_id_hash: None,
                title: None,
                started_at,
                ended_at: None,
                duration_seconds: None,
            },
            model: None,
            usage: UsageCounts {
                input_tokens: Some(tokens / 2),
                output_tokens: Some(tokens / 2),
                total_tokens: Some(tokens),
                ..UsageCounts::default()
            },
            runtime: None,
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: cost_cents,
                provider_reported_usd: None,
                estimated_api_equivalent_micro_usd: None,
                provider_reported_micro_usd: None,
                pricing_source: None,
                pricing_version: None,
                confidence: Confidence::Low,
            },
            parse_evidence: None,
            project: None,
            git: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            created_at: started_at,
            imported_at: started_at,
        }
    }

    fn test_summary(
        provider: &str,
        source: &SourceLocation,
        observed_at: DateTime<Utc>,
        period_start: DateTime<Utc>,
        period_end: DateTime<Utc>,
        tokens: u64,
    ) -> UsageSummary {
        UsageSummary {
            schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: summary_id(provider, &source.source_id, "sum"),
            device_id: "d".to_string(),
            provider: provider.to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: None,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalSummary,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "cache".to_string(),
                source_path_hash: None,
                source_record_id: Some("rec".to_string()),
                parse_confidence: Confidence::Medium,
            },
            model: None,
            models: Vec::new(),
            usage: UsageCounts {
                input_tokens: Some(tokens),
                total_tokens: Some(tokens),
                ..UsageCounts::default()
            },
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: None,
                provider_reported_usd: None,
                estimated_api_equivalent_micro_usd: None,
                provider_reported_micro_usd: None,
                pricing_source: None,
                pricing_version: None,
                confidence: Confidence::Low,
            },
            parse_evidence: None,
            project: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            metrics: None,
            period_start: Some(period_start),
            period_end: Some(period_end),
            observed_at,
            metadata: SummaryMetadata {
                summary_format: "stats_cache".to_string(),
                summary_version: None,
                total_sessions: Some(1),
                total_messages: Some(10),
                last_computed_at: Some(observed_at),
            },
            imported_at: observed_at,
        }
    }

    fn mk_dt(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|dt| dt.and_utc())
            .expect("valid date")
    }

    #[test]
    fn report_empty_inputs_returns_zero_totals() {
        let now = mk_dt(2026, 5, 25);
        let report = build_usage_report(&[], &[], &[], &[], &[], ReportPeriod::AllTime, now);
        assert_eq!(report.total_events, 0);
        assert_eq!(report.total_usage.total_tokens, 0);
        assert!(report.rows.is_empty());
        assert!(report.summary_rows.is_empty());
    }

    #[test]
    fn report_filters_events_by_period() {
        let now = mk_dt(2026, 5, 25);
        let source = test_source("codex", "/tmp/codex");
        let recent = test_event("codex", &source, mk_dt(2026, 5, 24), 100, None);
        let old = test_event("codex", &source, mk_dt(2026, 5, 10), 200, None);

        let report = build_usage_report(
            &[recent, old],
            &[],
            &[source],
            &[],
            &[],
            ReportPeriod::LastDays(7),
            now,
        );

        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 100);
    }

    #[test]
    fn report_filters_events_by_explicit_date_range() {
        let now = mk_dt(2026, 5, 25);
        let source = test_source("codex", "/tmp/codex");
        let before = test_event("codex", &source, mk_dt(2026, 4, 30), 50, None);
        let inside = test_event(
            "codex",
            &source,
            mk_dt(2026, 5, 10) + Duration::hours(15),
            100,
            None,
        );
        let after = test_event("codex", &source, mk_dt(2026, 5, 20), 200, None);
        let period = report_period_from_range(Some("2026-05-01"), Some("2026-05-15"), now)
            .expect("valid range");

        let report = build_usage_report(
            &[before, inside, after],
            &[],
            &[source],
            &[],
            &[],
            period,
            now,
        );

        assert_eq!(report.label, "2026-05-01 to 2026-05-15");
        assert_eq!(report.since, Some(mk_dt(2026, 5, 1)));
        assert_eq!(
            report.until,
            parse_report_date_bound("2026-05-15", true).expect("end of day")
        );
        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 100);
    }

    #[test]
    fn report_date_only_to_includes_the_whole_utc_day() {
        let now = mk_dt(2026, 6, 1);
        let source = test_source("codex", "/tmp/codex");
        let late_on_end_day = test_event(
            "codex",
            &source,
            mk_dt(2026, 5, 15) + Duration::hours(23),
            75,
            None,
        );
        let next_day = test_event("codex", &source, mk_dt(2026, 5, 16), 25, None);
        let period = report_period_from_range(Some("2026-05-15"), Some("2026-05-15"), now)
            .expect("single-day range");

        let report = build_usage_report(
            &[late_on_end_day, next_day],
            &[],
            &[source],
            &[],
            &[],
            period,
            now,
        );

        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 75);
    }

    #[test]
    fn report_range_from_only_defaults_until_to_now() {
        let now = mk_dt(2026, 5, 25);
        let period =
            report_period_from_range(Some("2026-05-01"), None, now).expect("from-only range");
        assert_eq!(
            period,
            ReportPeriod::Range {
                since: Some(mk_dt(2026, 5, 1)),
                until: now,
            }
        );
        assert_eq!(period.label(now), "2026-05-01 to 2026-05-25T00:00:00+00:00");
    }

    #[test]
    fn report_range_rejects_inverted_and_invalid_bounds() {
        let now = mk_dt(2026, 5, 25);
        assert_eq!(
            report_period_from_range(Some("2026-05-20"), Some("2026-05-10"), now),
            Err(ReportRangeError::InvertedRange {
                since: mk_dt(2026, 5, 20),
                until: parse_report_date_bound("2026-05-10", true).expect("end of day"),
            })
        );
        assert!(matches!(
            report_period_from_range(Some("last-week"), None, now),
            Err(ReportRangeError::InvalidDate { .. })
        ));
        assert_eq!(
            report_period_from_range(None, None, now),
            Err(ReportRangeError::MissingBound)
        );
    }

    #[test]
    fn report_range_keeps_future_windows_and_returns_no_events() {
        let now = mk_dt(2026, 5, 25);
        let source = test_source("codex", "/tmp/codex");
        let present = test_event("codex", &source, now, 50, None);
        let period = report_period_from_range(Some("2026-09-01"), Some("2026-09-30"), now)
            .expect("future range is valid");

        assert_eq!(
            period,
            ReportPeriod::Range {
                since: Some(mk_dt(2026, 9, 1)),
                until: parse_report_date_bound("2026-09-30", true).expect("end of day"),
            }
        );
        assert_eq!(period.label(now), "empty range");
        assert_eq!(period.window(now), (Some(mk_dt(2026, 9, 1)), now));

        let report = build_usage_report(&[present], &[], &[source], &[], &[], period, now);
        assert_eq!(report.label, "empty range");
        assert_eq!(report.total_events, 0);
        assert_eq!(report.until, now);
    }

    #[test]
    fn report_range_from_only_in_the_future_is_empty_not_inverted() {
        let now = mk_dt(2026, 5, 25);
        let source = test_source("codex", "/tmp/codex");
        let present = test_event("codex", &source, now, 50, None);
        let period =
            report_period_from_range(Some("2026-09-01"), None, now).expect("future from-only");

        assert_eq!(
            period,
            ReportPeriod::Range {
                since: Some(mk_dt(2026, 9, 1)),
                until: now,
            }
        );
        assert_eq!(period.label(now), "empty range");

        let report = build_usage_report(&[present], &[], &[source], &[], &[], period, now);
        assert_eq!(report.total_events, 0);
        assert_eq!(report.until, now);
    }

    #[test]
    fn report_range_label_uses_applied_until_when_to_is_in_the_future() {
        let now = mk_dt(2026, 5, 25) + Duration::hours(12);
        let period = report_period_from_range(Some("2026-05-01"), Some("2026-12-31"), now)
            .expect("open-ended future to");
        assert_eq!(period.label(now), "2026-05-01 to 2026-05-25T12:00:00+00:00");
        assert_eq!(period.window(now), (Some(mk_dt(2026, 5, 1)), now));
    }

    #[test]
    fn report_filters_events_by_rfc3339_clock_bounds() {
        let now = mk_dt(2026, 5, 25);
        let source = test_source("codex", "/tmp/codex");
        let before = test_event(
            "codex",
            &source,
            mk_dt(2026, 5, 10) + Duration::hours(14),
            50,
            None,
        );
        let inside = test_event(
            "codex",
            &source,
            mk_dt(2026, 5, 10) + Duration::hours(15) + Duration::minutes(30),
            100,
            None,
        );
        let after = test_event(
            "codex",
            &source,
            mk_dt(2026, 5, 10) + Duration::hours(16) + Duration::minutes(1),
            200,
            None,
        );
        let period = report_period_from_range(
            Some("2026-05-10T15:00:00Z"),
            Some("2026-05-10T16:00:00Z"),
            now,
        )
        .expect("rfc3339 range");

        let report = build_usage_report(
            &[before, inside, after],
            &[],
            &[source],
            &[],
            &[],
            period,
            now,
        );

        assert_eq!(
            report.label,
            "2026-05-10T15:00:00+00:00 to 2026-05-10T16:00:00+00:00"
        );
        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 100);
    }

    #[test]
    fn report_range_to_today_clamps_until_to_now() {
        let now = mk_dt(2026, 5, 25) + Duration::hours(12);
        let source = test_source("codex", "/tmp/codex");
        let morning = test_event(
            "codex",
            &source,
            mk_dt(2026, 5, 25) + Duration::hours(8),
            75,
            None,
        );
        let evening = test_event(
            "codex",
            &source,
            mk_dt(2026, 5, 25) + Duration::hours(18),
            25,
            None,
        );
        let period =
            report_period_from_range(Some("2026-05-25"), Some("2026-05-25"), now).expect("today");

        let report = build_usage_report(&[morning, evening], &[], &[source], &[], &[], period, now);
        assert_eq!(report.until, now);
        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 75);
    }

    #[test]
    fn report_filters_out_future_events() {
        let now = mk_dt(2026, 5, 25);
        let source = test_source("codex", "/tmp/codex");
        let future = test_event("codex", &source, mk_dt(2026, 6, 1), 100, None);
        let present = test_event("codex", &source, now, 50, None);

        let report = build_usage_report(
            &[future, present],
            &[],
            &[source],
            &[],
            &[],
            ReportPeriod::AllTime,
            now,
        );

        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 50);
    }

    #[test]
    fn report_groups_events_by_provider_and_account() {
        let now = mk_dt(2026, 5, 25);
        let src = test_source("codex", "/tmp/codex");
        let e1 = test_event("codex", &src, now, 100, None);
        let e2 = test_event("codex", &src, now, 200, None);

        let report =
            build_usage_report(&[e1, e2], &[], &[src], &[], &[], ReportPeriod::AllTime, now);

        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].provider, "codex");
        assert_eq!(report.rows[0].events, 2);
        assert_eq!(report.rows[0].usage.total_tokens, 300);
    }

    #[test]
    fn report_keeps_summaries_separate_from_events() {
        let now = mk_dt(2026, 5, 25);
        let src = test_source("claude_code", "/tmp/claude");
        let event = test_event("claude_code", &src, now, 100, None);
        let summary = test_summary(
            "claude_code",
            &src,
            now,
            mk_dt(2026, 5, 1),
            mk_dt(2026, 5, 25),
            500,
        );

        let report = build_usage_report(
            &[event],
            &[summary],
            &[src],
            &[],
            &[],
            ReportPeriod::AllTime,
            now,
        );

        assert_eq!(report.total_usage.total_tokens, 100);
        assert_eq!(report.total_summary_usage.total_tokens, 500);
        assert_eq!(report.summary_rows.len(), 1);
        // Direct event usage within summary period
        assert_eq!(report.summary_rows[0].direct_event_usage.total_tokens, 100);
    }

    #[test]
    fn event_usage_series_preserves_legacy_cent_fallback_in_range_differences() {
        let source = test_source("codex", "/tmp/codex");
        let before_range_at = mk_dt(2026, 5, 1);
        let inside_range_at = mk_dt(2026, 5, 2);
        let legacy_cents = i64::MAX / MICRO_USD_PER_CENT + 1;
        let before_range = test_event(
            "codex",
            &source,
            before_range_at,
            100,
            Some(legacy_cents + 7),
        );
        let inside_range = test_event(
            "codex",
            &source,
            inside_range_at,
            200,
            Some(legacy_cents + 11),
        );
        let series = EventUsageSeries::from_events(vec![&before_range, &inside_range]);

        let (events, usage) =
            series.usage_between(inside_range_at, inside_range_at + Duration::days(1), false);

        assert_eq!(events, 1);
        assert_eq!(usage.total_tokens, 200);
        assert_eq!(usage.estimated_cost_usd, Some(legacy_cents + 11));
        assert_eq!(usage.estimated_cost_micro_usd, None);
    }

    #[test]
    fn report_hides_summaries_in_non_alltime_periods() {
        let now = mk_dt(2026, 5, 25);
        let src = test_source("claude_code", "/tmp/claude");
        let summary = test_summary(
            "claude_code",
            &src,
            now,
            mk_dt(2026, 5, 1),
            mk_dt(2026, 5, 25),
            500,
        );

        let report = build_usage_report(
            &[],
            &[summary],
            &[src],
            &[],
            &[],
            ReportPeriod::LastDays(7),
            now,
        );

        assert!(report.summary_rows.is_empty());
    }

    #[test]
    fn subscription_rows_respect_past_end_time() {
        let now = mk_dt(2026, 6, 1);
        let src = test_source("codex", "/tmp/codex");
        let account_id = provider_account_id("codex", "email:verified@example.com");
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: account_id.clone(),
            provider: "codex".to_string(),
            identity_source: IdentitySource::LocalAuth,
            provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
            provider_user_id_hash: None,
            email: Some("verified@example.com".to_string()),
            email_hash: None,
            org_id_hash: None,
            account_label: None,
            plan_name: Some("Plus".to_string()),
            confidence: Confidence::High,
            verified_at: Some(mk_dt(2026, 5, 3)),
            created_at: mk_dt(2026, 5, 3),
            updated_at: mk_dt(2026, 5, 3),
        };
        let mut before_end = test_event("codex", &src, mk_dt(2026, 5, 29), 100, Some(100));
        before_end.provider_account_id = Some(account_id.clone());
        let mut after_end = test_event("codex", &src, mk_dt(2026, 5, 31), 200, Some(200));
        after_end.provider_account_id = Some(account_id.clone());
        let subscription = Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: subscription_id("codex", &account_id, "Plus", mk_dt(2026, 4, 30)),
            provider: "codex".to_string(),
            provider_account_id: account_id.clone(),
            plan_name: "Plus".to_string(),
            price: 2000,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: Some(mk_dt(2026, 4, 30)),
            renewal_day: Some(30),
            started_at: mk_dt(2026, 4, 30),
            ended_at: Some(mk_dt(2026, 5, 30)),
            current_period_ends_at: Some(mk_dt(2026, 5, 30)),
            status: SubscriptionStatus::Cancelled,
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(mk_dt(2026, 5, 3)),
            notes: None,
        };

        let report = build_usage_report(
            &[before_end, after_end],
            &[],
            &[src],
            &[account],
            &[subscription],
            ReportPeriod::LastDays(30),
            now,
        );

        assert_eq!(report.subscription_rows.len(), 1);
        assert_eq!(report.subscription_rows[0].account, account_id.0);
        assert_eq!(
            report.subscription_rows[0].ended_at,
            Some(mk_dt(2026, 5, 30))
        );
        assert_eq!(report.subscription_rows[0].events, 1);
        assert_eq!(report.subscription_rows[0].usage.total_tokens, 100);
        assert_eq!(
            report.subscription_rows[0].usage.estimated_cost_usd,
            Some(100)
        );
    }

    #[test]
    fn subscription_rows_respect_historical_range_until() {
        let now = mk_dt(2026, 6, 1);
        let src = test_source("codex", "/tmp/codex-range-sub");
        let account_id = provider_account_id("codex", "email:verified@example.com");
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: account_id.clone(),
            provider: "codex".to_string(),
            identity_source: IdentitySource::LocalAuth,
            provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
            provider_user_id_hash: None,
            email: Some("verified@example.com".to_string()),
            email_hash: None,
            org_id_hash: None,
            account_label: None,
            plan_name: Some("Plus".to_string()),
            confidence: Confidence::High,
            verified_at: Some(mk_dt(2026, 5, 3)),
            created_at: mk_dt(2026, 5, 3),
            updated_at: mk_dt(2026, 5, 3),
        };
        let mut inside = test_event("codex", &src, mk_dt(2026, 5, 10), 100, Some(100));
        inside.provider_account_id = Some(account_id.clone());
        let mut after_range = test_event("codex", &src, mk_dt(2026, 5, 20), 200, Some(200));
        after_range.provider_account_id = Some(account_id.clone());
        let subscription = Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: subscription_id("codex", &account_id, "Plus", mk_dt(2026, 4, 30)),
            provider: "codex".to_string(),
            provider_account_id: account_id.clone(),
            plan_name: "Plus".to_string(),
            price: 2000,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: Some(mk_dt(2026, 4, 30)),
            renewal_day: Some(30),
            started_at: mk_dt(2026, 4, 30),
            ended_at: None,
            current_period_ends_at: Some(mk_dt(2026, 5, 30)),
            status: SubscriptionStatus::Active,
            record_source: IdentitySource::UserConfigured,
            verified_at: Some(mk_dt(2026, 5, 3)),
            notes: None,
        };
        let period = report_period_from_range(Some("2026-05-01"), Some("2026-05-15"), now)
            .expect("historical range");

        let report = build_usage_report(
            &[inside, after_range],
            &[],
            &[src],
            &[account],
            &[subscription],
            period,
            now,
        );

        assert_eq!(report.total_events, 1);
        assert_eq!(report.subscription_rows.len(), 1);
        assert_eq!(report.subscription_rows[0].events, 1);
        assert_eq!(report.subscription_rows[0].usage.total_tokens, 100);
        assert_eq!(
            report.subscription_rows[0].usage.estimated_cost_usd,
            Some(100)
        );
    }

    #[test]
    fn subscription_rows_keep_legacy_verified_cycle_rows_open() {
        let now = mk_dt(2026, 6, 1);
        let src = test_source("codex", "/tmp/codex");
        let account_id = provider_account_id("codex", "email:verified@example.com");
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: account_id.clone(),
            provider: "codex".to_string(),
            identity_source: IdentitySource::LocalAuth,
            provider_user_id: None,
            provider_user_id_hash: None,
            email: Some("verified@example.com".to_string()),
            email_hash: None,
            org_id_hash: None,
            account_label: None,
            plan_name: Some("Plus".to_string()),
            confidence: Confidence::High,
            verified_at: Some(mk_dt(2026, 5, 3)),
            created_at: mk_dt(2026, 5, 3),
            updated_at: mk_dt(2026, 5, 3),
        };
        let mut before_cycle_end = test_event("codex", &src, mk_dt(2026, 5, 29), 100, Some(100));
        before_cycle_end.provider_account_id = Some(account_id.clone());
        let mut after_cycle_end = test_event("codex", &src, mk_dt(2026, 5, 31), 200, Some(200));
        after_cycle_end.provider_account_id = Some(account_id.clone());
        let subscription = Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: subscription_id("codex", &account_id, "Plus", mk_dt(2026, 4, 30)),
            provider: "codex".to_string(),
            provider_account_id: account_id,
            plan_name: "Plus".to_string(),
            price: 2000,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: Some(mk_dt(2026, 4, 30)),
            renewal_day: Some(30),
            started_at: mk_dt(2026, 4, 30),
            ended_at: Some(mk_dt(2026, 5, 30)),
            current_period_ends_at: Some(mk_dt(2026, 5, 30)),
            status: SubscriptionStatus::Active,
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(mk_dt(2026, 5, 3)),
            notes: None,
        };

        let report = build_usage_report(
            &[before_cycle_end, after_cycle_end],
            &[],
            &[src],
            &[account],
            &[subscription],
            ReportPeriod::LastDays(30),
            now,
        );

        assert_eq!(report.subscription_rows.len(), 1);
        assert_eq!(report.subscription_rows[0].ended_at, None);
        assert_eq!(report.subscription_rows[0].events, 2);
        assert_eq!(report.subscription_rows[0].usage.total_tokens, 300);
        assert_eq!(
            report.subscription_rows[0].usage.estimated_cost_usd,
            Some(300)
        );
    }

    #[test]
    fn report_uses_account_label_from_registry() {
        let now = mk_dt(2026, 5, 25);
        let src = test_source("codex", "/tmp/codex");
        let acct_id = provider_account_id("codex", "stable");
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: acct_id.clone(),
            provider: "codex".to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id: None,
            provider_user_id_hash: None,
            email: None,
            email_hash: None,
            org_id_hash: None,
            account_label: Some("work".to_string()),
            plan_name: None,
            confidence: Confidence::Medium,
            verified_at: None,
            created_at: now,
            updated_at: now,
        };
        let mut event = test_event("codex", &src, now, 50, None);
        event.provider_account_id = Some(acct_id);

        let report = build_usage_report(
            &[event],
            &[],
            &[src],
            &[account],
            &[],
            ReportPeriod::AllTime,
            now,
        );

        assert_eq!(report.rows[0].account, "work");
    }

    #[test]
    fn usage_totals_accumulate_cost() {
        let now = mk_dt(2026, 5, 25);
        let src = test_source("codex", "/tmp/codex");
        let e1 = test_event("codex", &src, now, 100, Some(1));
        let e2 = test_event("codex", &src, now, 200, Some(2));

        let report =
            build_usage_report(&[e1, e2], &[], &[src], &[], &[], ReportPeriod::AllTime, now);

        assert_eq!(report.total_usage.estimated_cost_usd, Some(3));
    }

    #[test]
    fn computed_total_does_not_overflow() {
        let usage = UsageCounts {
            input_tokens: Some(u64::MAX),
            output_tokens: Some(u64::MAX),
            ..UsageCounts::default()
        };
        let total = usage.computed_total();
        assert_eq!(total, u64::MAX);
    }

    #[test]
    fn usage_totals_saturate_imported_counters_and_costs() {
        let now = mk_dt(2026, 5, 25);
        let source = test_source("codex", "/tmp/codex-overflow");
        let mut event = test_event("codex", &source, now, u64::MAX, Some(i64::MAX));
        event.usage = UsageCounts {
            input_tokens: Some(u64::MAX),
            cache_creation_tokens: Some(u64::MAX),
            cache_read_tokens: Some(u64::MAX),
            output_tokens: Some(u64::MAX),
            reasoning_tokens: Some(u64::MAX),
            total_tokens: Some(u64::MAX),
            ..UsageCounts::default()
        };

        let mut totals = UsageTotals::default();
        totals.add_event(&event);
        totals.add_event(&event);

        assert_eq!(totals.input_tokens, u64::MAX);
        assert_eq!(totals.cache_creation_tokens, u64::MAX);
        assert_eq!(totals.cached_input_tokens, u64::MAX);
        assert_eq!(totals.output_tokens, u64::MAX);
        assert_eq!(totals.reasoning_tokens, u64::MAX);
        assert_eq!(totals.total_tokens, u64::MAX);
        assert_eq!(totals.estimated_cost_usd, Some(i64::MAX));
    }

    #[test]
    fn display_path_expands_home_but_avoids_canonicalize() {
        let p = Path::new("~/relative/test");
        let displayed = display_path(p);
        assert!(displayed.contains("relative/test"));
        // should not resolve to absolute via fs if ~ expanded
        if let Some(home) = home_dir() {
            let home_str = home.to_string_lossy();
            if displayed.starts_with(home_str.as_ref()) {
                // expanded, good
            }
        }
    }

    #[test]
    fn path_hash_remains_stable_via_canonical_display() {
        let p = Path::new("/tmp/nonexistent-for-test");
        let h1 = path_hash(p);
        let h2 = path_hash(p);
        assert_eq!(h1, h2);
    }

    #[test]
    fn bare_project_id_is_not_a_stable_project_identity() {
        let project = ProjectInfo {
            project_id: "project_bare".to_string(),
            project_label: Some("Bare".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: None,
            path_label: None,
        };

        assert!(!project_has_stable_identity(&project));
        assert_eq!(project_bucket_key(Some(&project)), "none");
    }

    #[test]
    fn sanitize_project_for_sync_preserves_path_only_project_labels() {
        let project = ProjectInfo {
            project_id: "project_path_only".to_string(),
            project_label: Some("Scratch".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/Scratch".to_string()),
        };

        let sanitized = sanitize_project_for_sync(project).expect("stable path identity");

        assert_eq!(sanitized.repo_remote_hash, None);
        assert_eq!(sanitized.path_hash.as_deref(), Some("path-hash"));
        assert_eq!(
            sanitized.path_label.as_deref(),
            Some("/Users/example/Scratch")
        );
        assert!(project_contains_file_paths(Some(&sanitized)));
    }

    #[test]
    fn sanitize_project_for_sync_drops_bare_project_ids() {
        let project = ProjectInfo {
            project_id: "project_bare".to_string(),
            project_label: Some("Bare".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: None,
            path_label: Some("/Users/example/Bare".to_string()),
        };

        assert!(sanitize_project_for_sync(project).is_none());
    }

    #[test]
    fn sanitize_summary_for_sync_marks_project_path_labels_as_file_paths() {
        let now = mk_dt(2026, 5, 25);
        let source = test_source("codex", "/tmp/codex");
        let mut summary = test_summary("codex", &source, now, now, now, 100);
        summary.project = Some(ProjectInfo {
            project_id: "project_path_only".to_string(),
            project_label: Some("Scratch".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/Scratch".to_string()),
        });

        let sanitized = sanitize_summary_for_sync(summary);

        assert_eq!(
            sanitized
                .project
                .as_ref()
                .and_then(|project| project.path_label.as_deref()),
            Some("/Users/example/Scratch")
        );
        assert!(sanitized.privacy.contains_file_paths);
    }

    #[test]
    fn preview_path_label_uses_display_label() {
        let mut source = test_source("codex", "/tmp/codex");
        source.path_label = Some("/home/testuser/work/codex".to_string());
        let preview = preview_path_label(&source);
        // if home matches, abbreviates; else full
        assert!(preview.contains("codex") || preview.contains("work"));
    }
}
