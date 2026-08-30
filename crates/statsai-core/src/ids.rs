use crate::paths::hash_text;
use crate::types::SourceKind;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct SourceId(pub String);

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
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
