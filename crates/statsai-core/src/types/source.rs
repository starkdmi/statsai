use super::subscription::VerifiedSubscriptionState;
use crate::ids::{source_id, SourceId};
use crate::paths::{display_path, hash_text, path_hash};
use crate::SOURCE_LOCATION_SCHEMA_VERSION;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

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
