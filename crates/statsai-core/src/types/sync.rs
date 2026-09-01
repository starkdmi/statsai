use crate::{
    project_contains_file_paths, project_has_stable_identity, AccountEvidenceSummaryV1,
    AccountPlanProjectionV1, CodeChangeMetric, ProjectInfo, ProviderAccount, ProviderAccountId,
    QuotaCycleContributionV1, SourceAccountAssignment, SourceAccountAssignmentId, SourceId,
    SourceLocation, Subscription, SubscriptionId, SummaryId, TaskSpan, TaskVerification,
    TaskVerificationId, UsageEvent, UsageSummary, WorkItem, WorkItemMember,
};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    /// Attributed quota-cycle contributions. Local quota records, payloads,
    /// plans, credits, and sample counts are deliberately absent from this type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota_cycle_contributions: Vec<QuotaCycleContributionV1>,
    /// Plan labels carrying only the canonical account reference, provider bounds,
    /// and evidence grade. Emails, provider user IDs, conversation and turn IDs,
    /// artifact paths, and raw provenance are deliberately absent from this type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_plan_observations: Vec<AccountPlanProjectionV1>,
    /// Aggregate coverage and conflict counts describing how well each account is
    /// evidenced. Individual observations never leave the device through this type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_evidence_summaries: Vec<AccountEvidenceSummaryV1>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota_cycle_contribution_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_plan_observation_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_evidence_summary_ids: Vec<String>,
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
    #[serde(default, skip_serializing_if = "sync_count_is_zero")]
    pub quota_cycle_contributions: u64,
    #[serde(default, skip_serializing_if = "sync_count_is_zero")]
    pub account_plan_observations: u64,
    #[serde(default, skip_serializing_if = "sync_count_is_zero")]
    pub account_evidence_summaries: u64,
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
