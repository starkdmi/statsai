//! Schema types shared by every stage of code-change measurement.

use crate::{ProjectInfo, SourceId};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const CODE_CHANGE_METRIC_SCHEMA_VERSION: &str = "code_change_metric.v1";
pub const TRACE_EDIT_SCHEMA_VERSION: &str = "trace_edit.v1";

/// Rolling window of Git history each scan re-reads. Commits older than this
/// are no longer observed, so already-materialized metrics for earlier days are
/// retained instead of being rebuilt.
pub const GIT_COMMIT_OBSERVATION_DAYS: i64 = 90;
/// Future skew a sync target still accepts on a metric's day.
///
/// Committer dates and transcript timestamps come from whatever clock wrote
/// them, so a skewed machine can leave permanently future-dated records in
/// local history.
const MAX_REPORTABLE_FUTURE_DAYS: i64 = 1;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CodeCategory {
    Source,
    Test,
}

impl CodeCategory {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Test => "test",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MutationKind {
    UnifiedPatch,
    StructuredEdit,
    FileCreation,
    FileWrite,
}

impl MutationKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnifiedPatch => "unified_patch",
            Self::StructuredEdit => "structured_edit",
            Self::FileCreation => "file_creation",
            Self::FileWrite => "file_write",
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CodeChangeMetricKind {
    AgentEdit,
    Committed,
    TraceMatchedCommitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttributionConfidence {
    High,
    Medium,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStatus {
    Complete,
    Partial,
    Unavailable,
}

impl CoverageStatus {
    #[must_use]
    pub const fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unavailable, Self::Unavailable) => Self::Unavailable,
            (Self::Partial, _) | (_, Self::Partial) => Self::Partial,
            (Self::Unavailable, Self::Complete) | (Self::Complete, Self::Unavailable) => {
                Self::Partial
            }
            _ => Self::Complete,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeLineCounts {
    pub source_additions: u64,
    pub source_deletions: u64,
    pub test_additions: u64,
    pub test_deletions: u64,
    pub unclassified_lines_written: u64,
}

impl CodeLineCounts {
    #[must_use]
    pub const fn additions(self) -> u64 {
        self.source_additions.saturating_add(self.test_additions)
    }

    #[must_use]
    pub const fn deletions(self) -> u64 {
        self.source_deletions.saturating_add(self.test_deletions)
    }

    #[must_use]
    pub const fn churn(self) -> u64 {
        self.additions().saturating_add(self.deletions())
    }

    #[must_use]
    pub const fn net(self) -> i64 {
        self.additions() as i64 - self.deletions() as i64
    }

    pub fn add(&mut self, other: Self) {
        self.source_additions = self.source_additions.saturating_add(other.source_additions);
        self.source_deletions = self.source_deletions.saturating_add(other.source_deletions);
        self.test_additions = self.test_additions.saturating_add(other.test_additions);
        self.test_deletions = self.test_deletions.saturating_add(other.test_deletions);
        self.unclassified_lines_written = self
            .unclassified_lines_written
            .saturating_add(other.unclassified_lines_written);
    }

    #[must_use]
    pub const fn classified(category: CodeCategory, additions: u64, deletions: u64) -> Self {
        match category {
            CodeCategory::Source => Self {
                source_additions: additions,
                source_deletions: deletions,
                test_additions: 0,
                test_deletions: 0,
                unclassified_lines_written: 0,
            },
            CodeCategory::Test => Self {
                source_additions: 0,
                source_deletions: 0,
                test_additions: additions,
                test_deletions: deletions,
                unclassified_lines_written: 0,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceEditContext<'a> {
    pub provider: &'a str,
    pub source_id: &'a SourceId,
    /// Scan cache key of the archive file this edit was reconstructed from.
    /// Reconciling a deleted archive file removes its edits by this key, so it
    /// is carried explicitly rather than parsed back out of a record id.
    pub cache_key: &'a str,
    pub conversation_id: &'a str,
    pub source_record_id: &'a str,
    pub occurred_at: Option<DateTime<Utc>>,
    pub project: Option<&'a ProjectInfo>,
    pub repository_path: Option<&'a Path>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEdit {
    pub schema_version: String,
    pub trace_edit_id: String,
    pub provider: String,
    pub source_id: SourceId,
    /// Scan cache key of the archive file this edit came from. Local-only.
    ///
    /// Defaulted so payloads written before this field existed still load: the
    /// column that reconciliation deletes by is backfilled by migration, and
    /// the payload regains the value the next time the file is imported.
    #[serde(default)]
    pub cache_key: String,
    pub conversation_id: String,
    pub source_record_id: String,
    pub occurred_at: Option<DateTime<Utc>>,
    pub project_id: Option<String>,
    /// Local-only repository path. This field must never be included in sync payloads.
    pub repository_path: Option<PathBuf>,
    /// Local-only repository-relative path. This field must never be included in sync payloads.
    pub relative_path: PathBuf,
    pub category: CodeCategory,
    pub mutation_kind: MutationKind,
    pub counts: CodeLineCounts,
    /// Local-only ordered fingerprints used for conservative hunk matching.
    pub added_line_fingerprints: Vec<String>,
    /// Local-only ordered fingerprints used for conservative hunk matching.
    pub deleted_line_fingerprints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMutation {
    pub edits: Vec<TraceEdit>,
    pub coverage: CoverageStatus,
    pub unsupported_sections: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileChange {
    pub relative_path: PathBuf,
    pub category: CodeCategory,
    pub counts: CodeLineCounts,
    pub added_line_fingerprints: Vec<String>,
    pub deleted_line_fingerprints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommitChange {
    pub deduplication_id: String,
    pub repository_hash: String,
    pub commit_hash: String,
    pub committed_at: DateTime<Utc>,
    /// When the change was originally written, which survives the rewrites
    /// that reset `committed_at`. Absent only in scans recorded before this
    /// field existed; one refresh repopulates it.
    #[serde(default)]
    pub authored_at: Option<DateTime<Utc>>,
    pub parent_count: usize,
    pub project_id: Option<String>,
    pub files: Vec<GitFileChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitScan {
    pub repository_root: PathBuf,
    pub repository_hash: String,
    pub commits: Vec<GitCommitChange>,
    pub coverage: CoverageStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeChangeMatch {
    pub match_id: String,
    pub trace_edit_id: String,
    pub commit_deduplication_id: String,
    pub commit_hash: String,
    pub repository_hash: String,
    pub relative_path: PathBuf,
    pub committed_at: DateTime<Utc>,
    pub confidence: AttributionConfidence,
    pub ordered_line_overlap: f64,
    pub counts: CodeLineCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeChangeMetric {
    pub schema_version: String,
    pub metric_id: String,
    pub device_id: String,
    pub day: NaiveDate,
    pub project_id: Option<String>,
    pub repository_hash: Option<String>,
    /// Local-only Git object ID. Sync serialization deliberately omits it so a
    /// metric cannot be correlated with a public commit, message, or diff.
    #[serde(skip)]
    #[schemars(skip)]
    pub commit_hash: Option<String>,
    pub kind: CodeChangeMetricKind,
    pub counts: CodeLineCounts,
    pub attribution_confidence: Option<AttributionConfidence>,
    pub trace_coverage: CoverageStatus,
    pub git_coverage: CoverageStatus,
}

#[must_use]
pub fn sanitize_code_change_metric_for_sync(
    mut metric: CodeChangeMetric,
    include_projects: bool,
) -> CodeChangeMetric {
    metric.commit_hash = None;
    if !include_projects {
        metric.project_id = None;
        metric.repository_hash = None;
    }
    metric
}

#[derive(Debug, Error)]
pub enum CodeChangeMetricBuildError {
    #[error("missing persisted opaque identifier for a committed metric")]
    MissingCommittedMetricId,
}

#[derive(Debug, Error)]
pub enum GitScanError {
    #[error("path is not inside a Git repository: {0}")]
    NotRepository(PathBuf),
    #[error("Git command failed ({command}): {message}")]
    Command { command: String, message: String },
    #[error("invalid Git timestamp `{value}`")]
    InvalidTimestamp { value: String },
}

/// First day still covered by a fresh Git scan.
///
/// Metrics for earlier days can no longer be rederived from local history, so
/// callers retain the ones they already materialized.
#[must_use]
pub fn git_observation_start_day(now: DateTime<Utc>) -> NaiveDate {
    (now - Duration::days(GIT_COMMIT_OBSERVATION_DAYS)).date_naive()
}

/// Last day a metric may claim without being rejected as clock-skewed.
///
/// Sync targets refuse a whole batch containing a day beyond their future
/// skew, so a single future-dated commit would otherwise block every later
/// sync. Records past this bound are left unmeasured instead.
#[must_use]
pub fn max_reportable_day(now: DateTime<Utc>) -> NaiveDate {
    (now + Duration::days(MAX_REPORTABLE_FUTURE_DAYS)).date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_available_and_unavailable_inputs_report_partial_coverage() {
        assert_eq!(
            CoverageStatus::Complete.combine(CoverageStatus::Unavailable),
            CoverageStatus::Partial
        );
        assert_eq!(
            CoverageStatus::Unavailable.combine(CoverageStatus::Unavailable),
            CoverageStatus::Unavailable
        );
    }

    #[test]
    fn stored_payloads_written_before_newer_fields_still_load() {
        // Local payloads are persisted as JSON and are not rewritten by a
        // migration, so every field added later has to be optional or the next
        // release cannot read its own store.
        let trace_edit = r#"{"schema_version":"trace_edit.v1","trace_edit_id":"edit_1",
            "provider":"codex","source_id":"src","conversation_id":"c",
            "source_record_id":"r","occurred_at":null,"project_id":null,
            "repository_path":null,"relative_path":"src/lib.rs","category":"source",
            "mutation_kind":"structured_edit","counts":{"source_additions":1,
            "source_deletions":0,"test_additions":0,"test_deletions":0,
            "unclassified_lines_written":0},"added_line_fingerprints":[],
            "deleted_line_fingerprints":[]}"#;
        let restored: TraceEdit =
            serde_json::from_str(trace_edit).expect("trace edit without cache_key");
        assert!(restored.cache_key.is_empty());

        let commit = r#"{"deduplication_id":"dedup","repository_hash":"repo",
            "commit_hash":"abc","committed_at":"2026-08-01T00:00:00Z","parent_count":1,
            "project_id":null,"files":[]}"#;
        let restored: GitCommitChange =
            serde_json::from_str(commit).expect("commit without authored_at");
        assert_eq!(restored.authored_at, None);
    }

    #[test]
    fn synced_metric_json_contains_only_numeric_rollup_dimensions() {
        let metric = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "opaque-event".to_string(),
            device_id: "device".to_string(),
            day: NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            project_id: Some("opaque-project".to_string()),
            repository_hash: Some("repository-hash".to_string()),
            commit_hash: Some("commit-hash".to_string()),
            kind: CodeChangeMetricKind::Committed,
            counts: CodeLineCounts::classified(CodeCategory::Source, 4, 2),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Complete,
            git_coverage: CoverageStatus::Complete,
        };
        let json = serde_json::to_string(&metric).unwrap();
        for forbidden in [
            "path",
            "diff",
            "tool_argument",
            "commit_message",
            "commit_hash",
            "source_text",
            "added_line_fingerprints",
        ] {
            assert!(
                !json.contains(forbidden),
                "unexpected private field: {forbidden}"
            );
        }
    }

    #[test]
    fn synced_metric_deserialization_ignores_private_commit_hash() {
        let metric = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "opaque-event".to_string(),
            device_id: "device".to_string(),
            day: NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            project_id: Some("opaque-project".to_string()),
            repository_hash: Some("repository-hash".to_string()),
            commit_hash: None,
            kind: CodeChangeMetricKind::Committed,
            counts: CodeLineCounts::classified(CodeCategory::Source, 4, 2),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Complete,
            git_coverage: CoverageStatus::Complete,
        };
        let mut value = serde_json::to_value(metric).unwrap();
        value.as_object_mut().unwrap().insert(
            "commit_hash".to_string(),
            serde_json::Value::String("private-commit-hash".to_string()),
        );

        let deserialized: CodeChangeMetric = serde_json::from_value(value).unwrap();

        assert_eq!(deserialized.commit_hash, None);
    }
}
