use super::event::Confidence;
use crate::ids::{provider_account_id, ProviderAccountId, SourceAccountAssignmentId, SourceId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
pub enum PrivacyMode {
    MetadataOnly,
    TitlesLabels,
    EnrichedSummaries,
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

#[must_use]
pub fn normalize_provider_user_id(value: &str) -> String {
    value.trim().to_string()
}

#[must_use]
pub fn normalize_email(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

pub(crate) fn default_identity_source_unknown() -> IdentitySource {
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

pub fn display_account_identity(account: &ProviderAccount) -> String {
    account
        .account_label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| account.provider_account_id.0.clone())
}
