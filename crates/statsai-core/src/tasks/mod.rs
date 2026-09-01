//! Task collection domain types and helpers for local rebuilding plus hosted sync snapshots.

mod normalize;
mod titles;

pub use normalize::*;
pub use titles::*;

use crate::{
    hash_text, Confidence, EventId, GitInfo, ProjectInfo, SourceId, SummaryId, UsageCounts,
};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const TASK_SPAN_SCHEMA_VERSION: &str = "task_span.v1";

pub const WORK_ITEM_SCHEMA_VERSION: &str = "work_item.v1";

pub const TASK_VERIFICATION_SCHEMA_VERSION: &str = "task_verification.v2";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TaskSpanId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct WorkItemId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TaskVerificationId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Auto,
    NeedsReview,
    Verified,
    RejectedMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskVerdict {
    Meta,
    System,
    Noise,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum TaskVerificationAction {
    Accept {
        work_item_id: WorkItemId,
        anchor_span_id: TaskSpanId,
    },
    Reject {
        work_item_id: WorkItemId,
        anchor_span_id: TaskSpanId,
        reason: TaskVerdict,
    },
    Rename {
        work_item_id: WorkItemId,
        anchor_span_id: TaskSpanId,
        title: String,
    },
    Split {
        after_span_id: TaskSpanId,
        #[serde(default)]
        before_span_id: Option<TaskSpanId>,
        left_title: Option<String>,
        right_title: Option<String>,
    },
    Merge {
        left_work_item_id: WorkItemId,
        right_work_item_id: WorkItemId,
        left_anchor_span_id: TaskSpanId,
        right_anchor_span_id: TaskSpanId,
        title: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskVerification {
    pub schema_version: String,
    pub verification_id: TaskVerificationId,
    pub action_key: String,
    pub action: TaskVerificationAction,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskSpan {
    pub schema_version: String,
    pub span_id: TaskSpanId,
    pub provider: String,
    pub source_id: SourceId,
    pub span_kind: String,
    pub source_record_id: Option<String>,
    pub source_file_path_hash: Option<String>,
    pub summary_id: Option<SummaryId>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub title: String,
    pub normalized_title: String,
    pub title_source: Option<String>,
    pub summary_preview: Option<String>,
    pub todo_excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issue_keys: Vec<String>,
    pub branch_family: Option<String>,
    pub project_bucket: String,
    pub project: Option<ProjectInfo>,
    pub git: Option<GitInfo>,
    pub usage: UsageCounts,
    pub estimated_cost_usd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_micro_usd: Option<i64>,
    #[serde(default)]
    pub event_count: u64,
    #[serde(default)]
    pub has_usage_evidence: bool,
    #[serde(default)]
    pub total_messages: u64,
    #[serde(default)]
    pub user_messages: u64,
    #[serde(default)]
    pub assistant_messages: u64,
    #[serde(default)]
    pub developer_messages: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_event_ids: Vec<EventId>,
    pub confidence: Confidence,
    pub is_meta: bool,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_seconds: Option<u64>,
}

impl TaskSpan {
    #[must_use]
    pub fn effective_ended_at(&self) -> DateTime<Utc> {
        self.ended_at.unwrap_or(self.started_at)
    }

    #[must_use]
    pub fn effective_event_count(&self) -> u64 {
        self.event_count.max(self.linked_event_ids.len() as u64)
    }

    #[must_use]
    pub fn effective_has_usage_evidence(&self) -> bool {
        self.has_usage_evidence || !self.linked_event_ids.is_empty()
    }

    #[must_use]
    pub fn has_git_anchor(&self) -> bool {
        self.project
            .as_ref()
            .and_then(|project| project.branch_label.as_deref())
            .is_some_and(|value| !value.trim().is_empty())
            || self
                .git
                .as_ref()
                .is_some_and(|git| !git.nearby_commit_hashes.is_empty())
    }
}

impl TaskVerificationAction {
    #[must_use]
    pub fn anchor_span_id(&self) -> Option<&TaskSpanId> {
        match self {
            Self::Accept { anchor_span_id, .. }
            | Self::Reject { anchor_span_id, .. }
            | Self::Rename { anchor_span_id, .. } => Some(anchor_span_id),
            Self::Split { .. } | Self::Merge { .. } => None,
        }
    }

    #[must_use]
    pub fn action_kind(&self) -> &'static str {
        match self {
            Self::Accept { .. } => "accept",
            Self::Reject { .. } => "reject",
            Self::Rename { .. } => "rename",
            Self::Split { .. } => "split",
            Self::Merge { .. } => "merge",
        }
    }

    #[must_use]
    pub fn action_key(&self) -> String {
        match self {
            Self::Accept { anchor_span_id, .. } | Self::Reject { anchor_span_id, .. } => {
                format!("status:{}", anchor_span_id.0)
            }
            Self::Rename { anchor_span_id, .. } => format!("rename:{}", anchor_span_id.0),
            Self::Split {
                after_span_id,
                before_span_id,
                ..
            } => {
                if let Some(before_span_id) = before_span_id {
                    format!("split:{}:{}", after_span_id.0, before_span_id.0)
                } else {
                    format!("split:{}", after_span_id.0)
                }
            }
            Self::Merge {
                left_anchor_span_id,
                right_anchor_span_id,
                ..
            } => {
                let (left, right) = if left_anchor_span_id.0 <= right_anchor_span_id.0 {
                    (&left_anchor_span_id.0, &right_anchor_span_id.0)
                } else {
                    (&right_anchor_span_id.0, &left_anchor_span_id.0)
                };
                format!("merge:{left}:{right}")
            }
        }
    }

    #[must_use]
    pub fn span_ids(&self) -> Vec<&TaskSpanId> {
        match self {
            Self::Accept { anchor_span_id, .. }
            | Self::Reject { anchor_span_id, .. }
            | Self::Rename { anchor_span_id, .. } => vec![anchor_span_id],
            Self::Split {
                after_span_id,
                before_span_id,
                ..
            } => {
                let mut span_ids = vec![after_span_id];
                if let Some(before_span_id) = before_span_id {
                    span_ids.push(before_span_id);
                }
                span_ids
            }
            Self::Merge {
                left_anchor_span_id,
                right_anchor_span_id,
                ..
            } => vec![left_anchor_span_id, right_anchor_span_id],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorkItem {
    pub schema_version: String,
    pub work_item_id: WorkItemId,
    pub anchor_span_id: TaskSpanId,
    pub tail_span_id: TaskSpanId,
    pub project_bucket: String,
    pub title: String,
    pub normalized_title: String,
    pub status: TaskStatus,
    pub confidence: Confidence,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub duration_seconds: Option<u64>,
    pub span_count: u64,
    pub event_count: u64,
    pub total_input_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_output_tokens: u64,
    pub total_reasoning_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_micro_usd: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issue_keys: Vec<String>,
    pub repo_label: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branch_labels: Vec<String>,
    pub path_label: Option<String>,
    pub summary_preview: Option<String>,
    pub todo_excerpt: Option<String>,
    pub no_git: bool,
    pub cross_provider: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continuation_reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkItemMember {
    pub work_item_id: WorkItemId,
    pub span_id: TaskSpanId,
    pub ordinal: usize,
}

#[must_use]
pub fn task_span_id(provider: &str, source_id: &SourceId, semantic_key: &str) -> TaskSpanId {
    TaskSpanId(format!(
        "span_{}",
        &hash_text(&format!("{provider}:{}:{semantic_key}", source_id.0))[..32]
    ))
}

#[must_use]
pub fn work_item_id(project_bucket: &str, span_ids: &[TaskSpanId]) -> WorkItemId {
    let span_key = span_ids
        .iter()
        .map(|span_id| span_id.0.as_str())
        .collect::<Vec<_>>()
        .join(":");
    WorkItemId(format!(
        "work_{}",
        &hash_text(&format!("{project_bucket}:{span_key}"))[..32]
    ))
}

#[must_use]
pub fn task_verification_id(action_kind: &str, action_key: &str) -> TaskVerificationId {
    TaskVerificationId(format!(
        "tvf_{}",
        &hash_text(&format!("{action_kind}:{action_key}"))[..32]
    ))
}

#[cfg(test)]
mod tests;
