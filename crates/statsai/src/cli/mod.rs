pub(super) mod account;
pub(super) mod args;
pub(super) mod auth;
pub(super) mod conversation;
pub(super) mod daemon;
mod dispatch;
pub(super) mod format;
pub(super) mod import;
pub(super) mod quota;
pub(super) mod report;
pub(super) mod scan;
pub(super) mod schema;
pub(super) mod service;
pub(super) mod source;
pub(super) mod status;
pub(super) mod store_admin;
pub(super) mod subscription;
pub(super) mod sync;
pub(super) mod task;

pub(crate) use account::*;
pub(crate) use args::*;
pub(crate) use auth::*;
pub(crate) use conversation::*;
pub(crate) use daemon::*;
pub(crate) use dispatch::*;
#[cfg(test)]
pub(crate) use format::*;
pub(crate) use import::*;
pub(crate) use quota::*;
pub(crate) use report::*;
pub(crate) use scan::*;
pub(crate) use schema::*;
pub(crate) use service::*;
pub(crate) use source::*;
pub(crate) use status::*;
pub(crate) use store_admin::*;
pub(crate) use subscription::*;
pub(crate) use sync::*;
pub(crate) use task::*;

#[cfg(test)]
use anyhow::{bail, Context, Result};
#[cfg(test)]
use chrono::Utc;
#[cfg(test)]
use clap::Parser;
#[cfg(test)]
use serde_json::{json, Value};
#[cfg(test)]
use statsai_adapters::VerifiedSubscriptionState;
#[cfg(test)]
use statsai_adapters::{
    retain_accounts_referenced_by_account_evidence, AccountEvidenceScan, ProviderAdapter,
    ScanCandidateFile, ScanDiagnostics, ScanOptions, SourceIdentityInference,
    VerifiedSourceObservation, VerifiedSourceState,
};
#[cfg(test)]
use statsai_core::{
    account_plan_observation_id, build_usage_report, conversation_account_binding_id, home_dir,
    normalize_email, provider_account_id, provider_account_id_from_identity,
    sanitize_code_change_metric_for_sync, source_account_assignment_id, ArchiveContentKind,
    ArchiveConversation, LocationOrigin, ProviderAccountId, QuotaObservationRecordV1,
    QuotaWindowV1, ReportPeriod, SourceAccountAssignment, SourceAccountAssignmentId, SourceId,
    SourceLocation, SourceVerificationMode, SyncAuthoritativeSnapshot, SyncBatch, UsageEvent,
    SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION, SYNC_BATCH_SCHEMA_VERSION,
};
#[cfg(test)]
use statsai_sdk::{ReportedUsageSummaryInput, ReportedUsageSummaryRecord};
#[cfg(test)]
use statsai_store::{
    apply_verified_source_state, reconcile_verified_source_state, verified_source_observation_hash,
    verified_source_state_hash,
};
#[cfg(test)]
use statsai_store::{upsert_provider_account, ScanFileStateEntry, UpsertProviderAccountInput};
#[cfg(test)]
use statsai_store::{Store, SyncPreferences};
#[cfg(test)]
use std::collections::{BTreeSet, HashMap, HashSet};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::time::Duration as StdDuration;

#[cfg(test)]
mod tests;
