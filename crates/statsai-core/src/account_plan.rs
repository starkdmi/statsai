//! Account identity and provider-plan evidence contracts.

use crate::{hash_text, Confidence, ProviderAccountId, SourceId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION: &str = "account_identity_observation.v1";
pub const ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION: &str = "account_plan_observation.v1";
pub const CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION: &str = "conversation_account_binding.v1";
pub const ACCOUNT_PLAN_PROJECTION_SCHEMA_VERSION: &str = "account_plan_projection.v1";
pub const ACCOUNT_EVIDENCE_SUMMARY_SCHEMA_VERSION: &str = "account_evidence_summary.v1";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AccountEvidenceKind {
    AuthSnapshot,
    TelemetryIdentity,
    AuthReload,
    ResetHistory,
    LoginSuccess,
    QuotaStatus,
    LegacyLocalAuth,
}

impl AccountEvidenceKind {
    #[must_use]
    pub const fn is_strong_identity(self) -> bool {
        matches!(
            self,
            Self::AuthSnapshot | Self::TelemetryIdentity | Self::AuthReload | Self::ResetHistory
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AccountIdentityObservationV1 {
    pub schema_version: String,
    pub observation_id: String,
    pub provider: String,
    pub source_id: SourceId,
    pub provider_account_id: Option<ProviderAccountId>,
    pub provider_user_id_hash: Option<String>,
    pub email_hash: Option<String>,
    pub conversation_id_hash: Option<String>,
    pub turn_id_hash: Option<String>,
    pub observed_at: DateTime<Utc>,
    pub evidence_kind: AccountEvidenceKind,
    pub confidence: Confidence,
    pub auth_mode: Option<String>,
    pub application_version: Option<String>,
    pub parser_version: String,
    pub artifact_kind: String,
    pub artifact_path_hash: String,
    pub record_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AccountPlanObservationV1 {
    pub schema_version: String,
    pub observation_id: String,
    pub provider: String,
    pub source_id: SourceId,
    pub provider_account_id: Option<ProviderAccountId>,
    pub raw_plan_name: String,
    pub plan_name: String,
    pub observed_at: DateTime<Utc>,
    pub active_from: Option<DateTime<Utc>>,
    pub active_until: Option<DateTime<Utc>>,
    pub is_current_snapshot: bool,
    pub evidence_kind: AccountEvidenceKind,
    pub confidence: Confidence,
    pub parser_version: String,
    pub artifact_path_hash: String,
    pub record_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConversationAccountBindingV1 {
    pub schema_version: String,
    pub binding_id: String,
    pub provider: String,
    pub source_id: SourceId,
    pub provider_account_id: ProviderAccountId,
    pub conversation_id_hash: String,
    pub turn_id_hash: Option<String>,
    pub observed_at: DateTime<Utc>,
    pub evidence_kind: AccountEvidenceKind,
    pub confidence: Confidence,
}

pub const ACCOUNT_EVIDENCE_CHECKPOINT_SCHEMA_VERSION: &str = "account_evidence_checkpoint.v1";

/// Durable high-water mark for an incrementally scanned structured evidence artifact.
///
/// `artifact_path_hash` intentionally keeps the local path out of the Store payload and sync
/// surface while still distinguishing multiple telemetry databases for one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AccountEvidenceCheckpointV1 {
    pub schema_version: String,
    pub source_id: SourceId,
    pub artifact_path_hash: String,
    pub parser_version: String,
    pub maximum_row_id: i64,
    /// Fingerprint of the row at `maximum_row_id`, used to prove database continuity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_row_fingerprint: Option<String>,
    pub database_size: i64,
    pub database_modified_nanos: i64,
    pub wal_size: i64,
    pub wal_modified_nanos: i64,
}

/// Privacy-safe device contribution sent to the hosted service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AccountPlanProjectionV1 {
    pub schema_version: String,
    pub projection_id: String,
    pub semantic_fingerprint: String,
    pub device_id: String,
    pub provider: String,
    pub provider_account_id: ProviderAccountId,
    pub raw_plan_name: String,
    pub plan_name: String,
    pub observed_at: DateTime<Utc>,
    pub active_from: Option<DateTime<Utc>>,
    pub active_until: Option<DateTime<Utc>>,
    pub is_current_snapshot: bool,
    pub evidence_kind: AccountEvidenceKind,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AccountEvidenceSummaryV1 {
    pub schema_version: String,
    pub summary_id: String,
    pub device_id: String,
    pub provider: String,
    pub provider_account_id: ProviderAccountId,
    pub first_strong_observed_at: Option<DateTime<Utc>>,
    pub last_strong_observed_at: Option<DateTime<Utc>>,
    pub strong_observation_count: u64,
    pub directly_bound_conversations: u64,
    pub uncovered_gap_count: u64,
    pub conflict_count: u64,
    pub evidence_kinds: Vec<AccountEvidenceKind>,
}

#[must_use]
pub fn normalize_plan_name(raw: &str) -> String {
    raw.trim()
        .split(['_', '-', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                format!(
                    "{}{}",
                    first.to_ascii_uppercase(),
                    chars.as_str().to_ascii_lowercase()
                )
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[must_use]
pub fn account_identity_observation_id(
    source_id: &SourceId,
    kind: AccountEvidenceKind,
    observed_at: DateTime<Utc>,
    record_fingerprint: &str,
) -> String {
    format!(
        "identity_observation_{}",
        &hash_text(&format!(
            "account_identity_observation.v1:{}:{kind:?}:{}:{record_fingerprint}",
            source_id.0,
            observed_at.to_rfc3339()
        ))[..32]
    )
}

#[must_use]
pub fn account_plan_observation_id(
    source_id: &SourceId,
    provider_account_id: Option<&ProviderAccountId>,
    raw_plan_name: &str,
    observed_at: DateTime<Utc>,
    kind: AccountEvidenceKind,
) -> String {
    format!(
        "plan_observation_{}",
        &hash_text(&format!(
            "account_plan_observation.v1:{}:{}:{}:{}:{kind:?}",
            source_id.0,
            provider_account_id.map_or("unattributed", |id| id.0.as_str()),
            raw_plan_name.trim().to_ascii_lowercase(),
            observed_at.to_rfc3339()
        ))[..32]
    )
}

#[must_use]
pub fn conversation_account_binding_id(
    source_id: &SourceId,
    conversation_id_hash: &str,
    turn_id_hash: Option<&str>,
    provider_account_id: &ProviderAccountId,
) -> String {
    format!(
        "conversation_binding_{}",
        &hash_text(&format!(
            "conversation_account_binding.v1:{}:{conversation_id_hash}:{}:{}",
            source_id.0,
            turn_id_hash.unwrap_or("none"),
            provider_account_id.0
        ))[..32]
    )
}

#[must_use]
pub fn plan_projection_from_observation(
    observation: &AccountPlanObservationV1,
    device_id: &str,
) -> Option<AccountPlanProjectionV1> {
    let provider_account_id = observation.provider_account_id.clone()?;
    let semantic_fingerprint = hash_text(&format!(
        "account_plan_projection.v1:{}:{}:{}:{}:{}:{}:{}:{:?}",
        observation.provider,
        provider_account_id.0,
        observation.raw_plan_name.trim().to_ascii_lowercase(),
        observation.observed_at.to_rfc3339(),
        observation
            .active_from
            .map_or_else(|| "none".to_string(), |value| value.to_rfc3339()),
        observation
            .active_until
            .map_or_else(|| "none".to_string(), |value| value.to_rfc3339()),
        observation.is_current_snapshot,
        observation.evidence_kind
    ));
    let projection_id = format!(
        "account_plan_projection_{}",
        &hash_text(&format!("{device_id}:{semantic_fingerprint}"))[..32]
    );
    Some(AccountPlanProjectionV1 {
        schema_version: ACCOUNT_PLAN_PROJECTION_SCHEMA_VERSION.to_string(),
        projection_id,
        semantic_fingerprint,
        device_id: device_id.to_string(),
        provider: observation.provider.clone(),
        provider_account_id,
        raw_plan_name: observation.raw_plan_name.clone(),
        plan_name: observation.plan_name.clone(),
        observed_at: observation.observed_at,
        active_from: observation.active_from,
        active_until: observation.active_until,
        is_current_snapshot: observation.is_current_snapshot,
        evidence_kind: observation.evidence_kind,
        confidence: observation.confidence.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn unknown_plan_names_remain_valid_and_readable() {
        assert_eq!(
            normalize_plan_name("future_ultra-enterprise"),
            "Future Ultra Enterprise"
        );
    }

    #[test]
    fn plan_observation_ids_are_deterministic_and_account_scoped() {
        let source_id = SourceId("source".to_string());
        let observed_at = Utc
            .with_ymd_and_hms(2026, 8, 23, 0, 0, 0)
            .single()
            .expect("timestamp");
        let account_a = ProviderAccountId("account-a".to_string());
        let account_b = ProviderAccountId("account-b".to_string());
        let first = account_plan_observation_id(
            &source_id,
            Some(&account_a),
            "future_ultra",
            observed_at,
            AccountEvidenceKind::AuthSnapshot,
        );
        let repeat = account_plan_observation_id(
            &source_id,
            Some(&account_a),
            "future_ultra",
            observed_at,
            AccountEvidenceKind::AuthSnapshot,
        );
        let other_account = account_plan_observation_id(
            &source_id,
            Some(&account_b),
            "future_ultra",
            observed_at,
            AccountEvidenceKind::AuthSnapshot,
        );

        assert_eq!(first, repeat);
        assert_ne!(first, other_account);
    }
}
