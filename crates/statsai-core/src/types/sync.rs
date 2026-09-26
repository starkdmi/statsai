use crate::{
    project_contains_file_paths, project_has_stable_identity, AccountEvidenceSummaryV1,
    AccountPlanProjectionV1, ActivityCoverageV1, ActivityRollupV1, CodeChangeMetric, ProjectInfo,
    ProviderAccount, ProviderAccountId, QuotaCycleContributionV1, SessionRollupV1,
    SourceAccountAssignment, SourceAccountAssignmentId, SourceId, SourceLocation, Subscription,
    SubscriptionId, SummaryId, TaskSpan, TaskVerification, TaskVerificationId, UsageEvent,
    UsageSummary, WorkItem, WorkItemMember,
};
use chrono::{DateTime, Datelike, Duration, Utc};
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
    /// Daily tool/MCP/skill rollups. Names leave the device only when
    /// `include_activity` is on. Invocation IDs, paths, arguments, and outputs
    /// are deliberately absent from this type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_rollups: Vec<ActivityRollupV1>,
    /// Per-source coverage for activity kinds, including zero-call days.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_coverage: Vec<ActivityCoverageV1>,
    /// Opt-in session rollups. Empty batches omit the key. Each row is
    /// `session_rollup.v1` with the closed ingest key set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionRollupV1>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_rollup_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_coverage_ids: Vec<String>,
    /// Present only when this collector is opting into session retirement.
    /// An empty array prunes every hosted session for the device. Absence
    /// leaves hosted sessions alone, including on pre-session v6 collectors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_rollup_ids: Option<Vec<String>>,
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
    #[serde(default, skip_serializing_if = "sync_count_is_zero")]
    pub activity_rollups: u64,
    #[serde(default, skip_serializing_if = "sync_count_is_zero")]
    pub activity_coverage: u64,
    /// Present on `sync_ack.v6`. Earlier acknowledgements omit it.
    #[serde(default, skip_serializing_if = "sync_count_is_zero")]
    pub sessions: u64,
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

/// Ingest caps from `session_rollup.v1`. A value outside these bounds rejects
/// the whole batch, so the snapshot never lands and hosted rows stay put.
const SYNC_TOKEN_MAX: u64 = 1_000_000_000_000;
const SYNC_REQUEST_MAX: u64 = 1_000_000_000;
const SYNC_COST_MINOR_MAX: i64 = 1_000_000_000;
const SYNC_COST_MICRO_MAX: i64 = SYNC_COST_MINOR_MAX * 10_000;
const SYNC_SAFE_INT_MAX: u64 = (1_u64 << 53) - 1;
const SYNC_LABEL_UTF16_MAX: usize = 256;
const SYNC_PATH_LABEL_UTF16_MAX: usize = 1024;
const SYNC_SESSION_MODEL_MAX: usize = 64;
const SYNC_SESSION_PAYLOAD_MAX_BYTES: usize = 64 * 1024;
const SYNC_FUTURE_SKEW: Duration = Duration::seconds(24 * 60 * 60);

/// Drops unstable projects and generic or secret-looking titles before a
/// session rollup leaves the device. `source_id` stays, matching summaries.
/// Nested usage and cost already serialize to the closed ingest keys.
///
/// Values the ingest would reject are repaired here. One bad row fails the
/// batch, and session chunks are sent before the snapshot, so a permanent
/// rejection also leaves hosted rows that should have been retired.
#[must_use]
pub fn sanitize_session_rollup_for_sync(mut rollup: SessionRollupV1) -> SessionRollupV1 {
    clamp_session_timestamps(&mut rollup);
    rollup.project = rollup
        .project
        .and_then(sanitize_project_for_sync)
        .map(clamp_project_labels);
    clamp_session_title(&mut rollup);
    rollup.primary_model = rollup
        .primary_model
        .and_then(|label| sync_label(&label, SYNC_LABEL_UTF16_MAX));
    clamp_usage_counts(&mut rollup.usage);
    rollup.requests = rollup.requests.min(SYNC_REQUEST_MAX);
    clamp_cost(&mut rollup.cost);
    clamp_message_count(&mut rollup.total_messages);
    clamp_message_count(&mut rollup.user_messages);
    clamp_message_count(&mut rollup.assistant_messages);
    clamp_message_count(&mut rollup.developer_messages);
    for model in &mut rollup.models {
        clamp_usage_counts(&mut model.usage);
        clamp_cost(&mut model.cost);
        model.model.name = model
            .model
            .name
            .as_deref()
            .and_then(|label| sync_label(label, SYNC_LABEL_UTF16_MAX));
        model.model.normalized_name = model
            .model
            .normalized_name
            .as_deref()
            .and_then(|label| sync_label(label, SYNC_LABEL_UTF16_MAX));
        model.model.provider_model_id = model
            .model
            .provider_model_id
            .as_deref()
            .and_then(|label| sync_label(label, SYNC_LABEL_UTF16_MAX));
        model.model.speed = model
            .model
            .speed
            .as_deref()
            .and_then(|label| sync_label(label, SYNC_LABEL_UTF16_MAX));
        model.model.reasoning_level_raw = model
            .model
            .reasoning_level_raw
            .as_deref()
            .and_then(|label| sync_label(label, SYNC_LABEL_UTF16_MAX));
    }
    if rollup.models.len() > SYNC_SESSION_MODEL_MAX {
        sort_models_by_tokens(&mut rollup.models);
        rollup.models.truncate(SYNC_SESSION_MODEL_MAX);
    }
    shrink_session_payload(&mut rollup);
    rollup
}

fn clamp_session_timestamps(rollup: &mut SessionRollupV1) {
    let latest = Utc::now() + SYNC_FUTURE_SKEW;
    // Prefer a timestamp already on the row so a clock step does not give the
    // repaired payload a new hash on every sync.
    let fallback = [rollup.updated_at, rollup.started_at, rollup.ended_at]
        .into_iter()
        .find(|value| sync_timestamp_ok(*value, latest))
        .unwrap_or_else(|| Utc::now().min(latest));
    rollup.started_at = clamp_sync_timestamp(rollup.started_at, latest, fallback);
    rollup.ended_at = clamp_sync_timestamp(rollup.ended_at, latest, fallback);
    if rollup.ended_at < rollup.started_at {
        rollup.ended_at = rollup.started_at;
    }
    rollup.updated_at = clamp_sync_timestamp(rollup.updated_at, latest, fallback);
    // An unknown duration stays unknown; clamped timestamps must not turn it
    // into a measured zero.
    if rollup.duration_seconds.is_none() {
        return;
    }
    let seconds = rollup
        .ended_at
        .signed_duration_since(rollup.started_at)
        .num_seconds();
    rollup.duration_seconds = u64::try_from(seconds)
        .ok()
        .map(|value| value.min(SYNC_SAFE_INT_MAX));
}

fn sync_timestamp_ok(value: DateTime<Utc>, latest: DateTime<Utc>) -> bool {
    (1..=9999).contains(&value.year()) && value <= latest
}

fn clamp_sync_timestamp(
    value: DateTime<Utc>,
    latest: DateTime<Utc>,
    fallback: DateTime<Utc>,
) -> DateTime<Utc> {
    if sync_timestamp_ok(value, latest) {
        value
    } else {
        fallback
    }
}

fn clamp_project_labels(mut project: ProjectInfo) -> ProjectInfo {
    project.project_label = project
        .project_label
        .as_deref()
        .and_then(|label| sync_label(label, SYNC_LABEL_UTF16_MAX));
    project.repo_label = project
        .repo_label
        .as_deref()
        .and_then(|label| sync_label(label, SYNC_LABEL_UTF16_MAX));
    project.branch_label = project
        .branch_label
        .as_deref()
        .and_then(|label| sync_label(label, SYNC_LABEL_UTF16_MAX));
    project.path_label = project
        .path_label
        .as_deref()
        .and_then(|label| sync_label(label, SYNC_PATH_LABEL_UTF16_MAX));
    project
}

fn clamp_session_title(rollup: &mut SessionRollupV1) {
    let title = rollup
        .title
        .as_deref()
        .and_then(|label| sync_label(label, SYNC_LABEL_UTF16_MAX))
        .filter(|label| !crate::task_title_is_generic(Some(label)));
    if title.is_none() {
        rollup.title_source = None;
    }
    rollup.title = title;
}

fn clamp_usage_counts(usage: &mut crate::UsageCounts) {
    clamp_token(&mut usage.input_tokens);
    clamp_token(&mut usage.output_tokens);
    clamp_token(&mut usage.cache_creation_tokens);
    clamp_token(&mut usage.cache_creation_5m_tokens);
    clamp_token(&mut usage.cache_creation_1h_tokens);
    clamp_token(&mut usage.cache_read_tokens);
    clamp_token(&mut usage.reasoning_tokens);
    clamp_token(&mut usage.total_tokens);
    clamp_token(&mut usage.local_prompt_eval_tokens);
    clamp_token(&mut usage.local_eval_tokens);
    if let Some(requests) = usage.requests.as_mut() {
        *requests = (*requests).min(SYNC_REQUEST_MAX);
    }
}

fn clamp_token(value: &mut Option<u64>) {
    if let Some(amount) = value.as_mut() {
        *amount = (*amount).min(SYNC_TOKEN_MAX);
    }
}

fn clamp_message_count(value: &mut Option<u64>) {
    if let Some(amount) = value.as_mut() {
        *amount = (*amount).min(SYNC_SAFE_INT_MAX);
    }
}

fn clamp_cost(cost: &mut crate::CostInfo) {
    clamp_cost_field(&mut cost.provider_reported_usd, SYNC_COST_MINOR_MAX);
    clamp_cost_field(&mut cost.estimated_api_equivalent_usd, SYNC_COST_MINOR_MAX);
    clamp_cost_field(&mut cost.provider_reported_micro_usd, SYNC_COST_MICRO_MAX);
    clamp_cost_field(
        &mut cost.estimated_api_equivalent_micro_usd,
        SYNC_COST_MICRO_MAX,
    );
}

fn clamp_cost_field(value: &mut Option<i64>, max: i64) {
    if let Some(amount) = value.as_mut() {
        *amount = (*amount).clamp(0, max);
    }
}

fn sort_models_by_tokens(models: &mut [crate::SummaryModelUsage]) {
    models.sort_by(|left, right| {
        right
            .usage
            .computed_total()
            .cmp(&left.usage.computed_total())
            .then_with(|| model_wire_key(&left.model).cmp(&model_wire_key(&right.model)))
    });
}

fn model_wire_key(model: &crate::ModelInfo) -> String {
    model
        .normalized_name
        .clone()
        .or_else(|| model.name.clone())
        .or_else(|| model.provider_model_id.clone())
        .unwrap_or_default()
}

fn shrink_session_payload(rollup: &mut SessionRollupV1) {
    if payload_within_limit(rollup) {
        return;
    }
    if rollup.models.len() > 1 {
        sort_models_by_tokens(&mut rollup.models);
    }
    while rollup.models.len() > 1 && !payload_within_limit(rollup) {
        rollup.models.pop();
    }
    if payload_within_limit(rollup) {
        return;
    }
    if let Some(project) = rollup.project.as_mut() {
        project.path_label = None;
        project.project_label = None;
        project.repo_label = None;
        project.branch_label = None;
    }
    rollup.title = None;
    rollup.title_source = None;
    if !payload_within_limit(rollup) {
        rollup.models.clear();
    }
}

fn payload_within_limit(rollup: &SessionRollupV1) -> bool {
    serde_json::to_vec(rollup)
        .map(|payload| payload.len() <= SYNC_SESSION_PAYLOAD_MAX_BYTES)
        .unwrap_or(false)
}

fn sync_label(value: &str, max_utf16: usize) -> Option<String> {
    let cleaned = value
        .chars()
        .map(|ch| {
            if is_disallowed_sync_control(ch) {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = truncate_utf16(collapsed.trim(), max_utf16);
    let truncated = truncated.trim();
    if truncated.is_empty() {
        None
    } else {
        Some(truncated.to_string())
    }
}

fn is_disallowed_sync_control(ch: char) -> bool {
    ('\u{0000}'..='\u{001f}').contains(&ch) || ch == '\u{007f}'
}

fn truncate_utf16(value: &str, max_units: usize) -> String {
    let mut units = 0usize;
    let mut end = 0usize;
    for (index, ch) in value.char_indices() {
        let next = units + ch.len_utf16();
        if next > max_units {
            break;
        }
        units = next;
        end = index + ch.len_utf8();
    }
    value[..end].to_string()
}
