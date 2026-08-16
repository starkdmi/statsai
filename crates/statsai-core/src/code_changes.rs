//! Local code-change measurement, Git inspection, and conservative trace matching.

use crate::{hash_text, ProjectInfo, SourceId};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use thiserror::Error;

#[cfg(test)]
thread_local! {
    static TEST_GIT_COMMAND_COUNT: Cell<usize> = const { Cell::new(0) };
}

pub const CODE_CHANGE_METRIC_SCHEMA_VERSION: &str = "code_change_metric.v1";
pub const TRACE_EDIT_SCHEMA_VERSION: &str = "trace_edit.v1";
const GIT_PATCH_BATCH_SIZE: usize = 256;
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
/// Distinct changed lines an edit must carry before it may be attributed at all.
///
/// Below this floor an ordered overlap says nothing about authorship, so the
/// match is withheld at every confidence level rather than only at High.
const MIN_ATTRIBUTION_DISTINCT_FINGERPRINTS: usize = 3;

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

#[must_use]
pub fn classify_code_path(path: &Path) -> Option<CodeCategory> {
    if is_excluded_code_path(path) {
        return None;
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())?
        .to_ascii_lowercase();
    if !matches!(
        extension.as_str(),
        "asm"
            | "astro"
            | "bash"
            | "c"
            | "cc"
            | "clj"
            | "cljc"
            | "cljs"
            | "cpp"
            | "cs"
            | "css"
            | "cts"
            | "cxx"
            | "dart"
            | "erl"
            | "ex"
            | "exs"
            | "fish"
            | "fs"
            | "fsi"
            | "fsx"
            | "go"
            | "gql"
            | "graphql"
            | "groovy"
            | "h"
            | "hh"
            | "hpp"
            | "hrl"
            | "hs"
            | "htm"
            | "html"
            | "hxx"
            | "java"
            | "jl"
            | "js"
            | "jsx"
            | "kt"
            | "kts"
            | "less"
            | "lhs"
            | "lua"
            | "m"
            | "ml"
            | "mli"
            | "mm"
            | "mjs"
            | "mts"
            | "nim"
            | "php"
            | "pl"
            | "pm"
            | "proto"
            | "ps1"
            | "psm1"
            | "py"
            | "pyi"
            | "pyw"
            | "r"
            | "raku"
            | "rb"
            | "rs"
            | "s"
            | "sass"
            | "scala"
            | "sc"
            | "scss"
            | "sh"
            | "sol"
            | "sql"
            | "svelte"
            | "swift"
            | "ts"
            | "tsx"
            | "vb"
            | "vue"
            | "wat"
            | "zig"
            | "zsh"
    ) {
        return None;
    }
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_test = normalized.split('/').any(|part| {
        matches!(
            part,
            "test" | "tests" | "spec" | "specs" | "__tests__" | "fixtures"
        )
    }) || file_name.contains(".test.")
        || file_name.contains(".spec.")
        || file_name.ends_with("_test.rs")
        || file_name.ends_with("_tests.rs")
        || file_name.ends_with("_test.go")
        || file_name.ends_with("tests.swift")
        || file_name.starts_with("test_")
        || file_name.ends_with("_test.py");
    Some(if is_test {
        CodeCategory::Test
    } else {
        CodeCategory::Source
    })
}

#[must_use]
pub fn is_excluded_code_path(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let excluded_directory = normalized.split('/').any(|part| {
        matches!(
            part,
            ".git"
                | "node_modules"
                | "vendor"
                | "vendors"
                | "dist"
                | "build"
                | "target"
                | ".next"
                | ".cache"
                | "coverage"
                | "generated"
                | "__generated__"
                | "gen"
                | "out"
                | ".turbo"
                | ".gradle"
                | ".dart_tool"
                | "pods"
                | "deriveddata"
        )
    });
    let lockfile = matches!(
        file_name.as_str(),
        "cargo.lock"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "poetry.lock"
            | "uv.lock"
            | "pipfile.lock"
            | "composer.lock"
            | "gemfile.lock"
            | "go.sum"
            | "go.work.sum"
            | "bun.lock"
            | "bun.lockb"
            | "podfile.lock"
            | "package.resolved"
            | "gradle.lockfile"
    );
    excluded_directory
        || lockfile
        || file_name.ends_with(".min.js")
        || file_name.ends_with(".min.css")
        || file_name.ends_with(".map")
}

#[must_use]
pub fn parse_unified_patch(context: &TraceEditContext<'_>, patch: &str) -> ParsedMutation {
    #[derive(Default)]
    struct Section {
        index: usize,
        path: Option<PathBuf>,
        creation: bool,
        deletion: bool,
        additions: Vec<String>,
        deletions: Vec<String>,
        saw_hunk: bool,
    }

    fn finish_section(
        context: &TraceEditContext<'_>,
        section: &mut Section,
        edits: &mut Vec<TraceEdit>,
        unsupported: &mut u64,
        observed: &mut u64,
    ) {
        let Some(path) = section.path.take() else {
            // A hunk body that precedes any file header belongs to no file.
            // Leaving it in place would carry its lines into the next section,
            // attributing them to the wrong file and counting that file's churn
            // twice, so it is dropped and reported as unmeasured instead.
            if section.saw_hunk {
                *unsupported = unsupported.saturating_add(1);
            }
            *section = Section::default();
            return;
        };
        // Some agents emit absolute paths in patch headers. Attribution and
        // classification both expect repository-relative paths.
        let path = repository_relative_path(context.repository_path, path);
        let Some(category) = classify_code_path(&path) else {
            *observed = observed.saturating_add(1);
            *section = Section::default();
            return;
        };
        if !section.saw_hunk || (section.deletion && section.deletions.is_empty()) {
            *unsupported = unsupported.saturating_add(1);
            *section = Section::default();
            return;
        }
        *observed = observed.saturating_add(1);
        let kind = if section.creation {
            MutationKind::FileCreation
        } else {
            MutationKind::UnifiedPatch
        };
        edits.push(build_trace_edit(
            context,
            path,
            TraceEditMutation {
                category,
                kind,
                section_index: section.index,
                additions: &section.additions,
                deletions: &section.deletions,
                lines_written: 0,
            },
        ));
        *section = Section::default();
    }

    let mut edits = Vec::new();
    let mut unsupported_sections = 0u64;
    let mut observed_sections = 0u64;
    let mut section = Section::default();
    let mut next_section_index = 0_usize;
    let mut in_hunk = false;
    let lines = logical_lines(patch);
    for (line_index, raw_line) in lines.iter().enumerate() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let next_line = lines
            .get(line_index + 1)
            .map(|value| value.strip_suffix('\r').unwrap_or(value));
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            finish_section(
                context,
                &mut section,
                &mut edits,
                &mut unsupported_sections,
                &mut observed_sections,
            );
            section.index = next_section_index;
            next_section_index = next_section_index.saturating_add(1);
            section.path = clean_patch_path(path, PatchPathStyle::Literal);
            section.creation = true;
            in_hunk = true;
            section.saw_hunk = true;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            finish_section(
                context,
                &mut section,
                &mut edits,
                &mut unsupported_sections,
                &mut observed_sections,
            );
            section.index = next_section_index;
            next_section_index = next_section_index.saturating_add(1);
            section.path = clean_patch_path(path, PatchPathStyle::Literal);
            in_hunk = false;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            finish_section(
                context,
                &mut section,
                &mut edits,
                &mut unsupported_sections,
                &mut observed_sections,
            );
            section.index = next_section_index;
            next_section_index = next_section_index.saturating_add(1);
            section.path = clean_patch_path(path, PatchPathStyle::Literal);
            section.deletion = true;
            in_hunk = true;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Move to: ") {
            section.path = clean_patch_path(path, PatchPathStyle::Literal);
            continue;
        }
        if let Some(paths) = line.strip_prefix("diff --git ") {
            finish_section(
                context,
                &mut section,
                &mut edits,
                &mut unsupported_sections,
                &mut observed_sections,
            );
            section.index = next_section_index;
            next_section_index = next_section_index.saturating_add(1);
            section.path = paths
                .split_whitespace()
                .nth(1)
                .and_then(|path| clean_patch_path(path, PatchPathStyle::GitDestination));
            in_hunk = false;
            continue;
        }
        // A `---`/`+++` pair is the only file header in a plain unified diff
        // that carries no `diff --git` line. Requiring the pair keeps a deleted
        // body line such as `-- text` from being mistaken for a header, and
        // ending the previous section here stops a second file's hunks from
        // being attributed to the first one.
        if line.starts_with("--- ") && next_line.is_some_and(|next| next.starts_with("+++ ")) {
            if section.saw_hunk {
                finish_section(
                    context,
                    &mut section,
                    &mut edits,
                    &mut unsupported_sections,
                    &mut observed_sections,
                );
                section.index = next_section_index;
                next_section_index = next_section_index.saturating_add(1);
            }
            in_hunk = false;
            if line == "--- /dev/null" {
                section.creation = true;
            } else if next_line == Some("+++ /dev/null") {
                // A deletion carries its path on the source side only.
                section.path =
                    clean_patch_path(line.trim_start_matches("--- "), PatchPathStyle::GitSource);
                section.deletion = true;
            }
            continue;
        }
        // Outside a hunk a lone `+++` still names the destination file; inside
        // one it is body text such as `+++ still a line`.
        if !in_hunk {
            if let Some(path) = line.strip_prefix("+++ ") {
                if path != "/dev/null" {
                    section.path = clean_patch_path(path, PatchPathStyle::GitDestination);
                }
                continue;
            }
        }
        if line.starts_with("@@") {
            in_hunk = true;
            section.saw_hunk = true;
            continue;
        }
        if line.starts_with("*** End Patch") || line.starts_with("*** End of File") {
            continue;
        }
        if in_hunk {
            if let Some(added) = line.strip_prefix('+') {
                section.additions.push(added.to_string());
            } else if let Some(deleted) = line.strip_prefix('-') {
                section.deletions.push(deleted.to_string());
                if section.deletion {
                    section.saw_hunk = true;
                }
            }
        }
    }
    finish_section(
        context,
        &mut section,
        &mut edits,
        &mut unsupported_sections,
        &mut observed_sections,
    );
    ParsedMutation {
        coverage: if unsupported_sections == 0 && observed_sections > 0 {
            CoverageStatus::Complete
        } else if observed_sections == 0 {
            CoverageStatus::Unavailable
        } else {
            CoverageStatus::Partial
        },
        edits,
        unsupported_sections,
    }
}

#[must_use]
pub fn parse_structured_edit(
    context: &TraceEditContext<'_>,
    path: &Path,
    old_string: &str,
    new_string: &str,
    section_index: usize,
) -> ParsedMutation {
    let Some(category) = classify_code_path(path) else {
        return ParsedMutation {
            edits: Vec::new(),
            coverage: CoverageStatus::Complete,
            unsupported_sections: 0,
        };
    };
    let old_lines = owned_logical_lines(old_string);
    let new_lines = owned_logical_lines(new_string);
    let (additions, deletions) = changed_lines(&old_lines, &new_lines);
    ParsedMutation {
        edits: vec![build_trace_edit(
            context,
            path.to_path_buf(),
            TraceEditMutation {
                category,
                kind: MutationKind::StructuredEdit,
                section_index,
                additions: &additions,
                deletions: &deletions,
                lines_written: 0,
            },
        )],
        coverage: CoverageStatus::Complete,
        unsupported_sections: 0,
    }
}

#[must_use]
pub fn parse_full_file_write(
    context: &TraceEditContext<'_>,
    path: &Path,
    content: &str,
    creation_known: bool,
) -> ParsedMutation {
    let Some(category) = classify_code_path(path) else {
        return ParsedMutation {
            edits: Vec::new(),
            coverage: CoverageStatus::Complete,
            unsupported_sections: 0,
        };
    };
    let lines = owned_logical_lines(content);
    let (additions, lines_written, kind) = if creation_known {
        (lines.as_slice(), 0, MutationKind::FileCreation)
    } else {
        (&[][..], lines.len() as u64, MutationKind::FileWrite)
    };
    ParsedMutation {
        edits: vec![build_trace_edit(
            context,
            path.to_path_buf(),
            TraceEditMutation {
                category,
                kind,
                section_index: 0,
                additions,
                deletions: &[],
                lines_written,
            },
        )],
        coverage: CoverageStatus::Complete,
        unsupported_sections: 0,
    }
}

struct TraceEditMutation<'a> {
    category: CodeCategory,
    kind: MutationKind,
    section_index: usize,
    additions: &'a [String],
    deletions: &'a [String],
    lines_written: u64,
}

fn build_trace_edit(
    context: &TraceEditContext<'_>,
    relative_path: PathBuf,
    mutation: TraceEditMutation<'_>,
) -> TraceEdit {
    let added_line_fingerprints = mutation
        .additions
        .iter()
        .map(|line| hash_text(line))
        .collect::<Vec<_>>();
    let deleted_line_fingerprints = mutation
        .deletions
        .iter()
        .map(|line| hash_text(line))
        .collect::<Vec<_>>();
    let mut counts = CodeLineCounts::classified(
        mutation.category,
        mutation.additions.len() as u64,
        mutation.deletions.len() as u64,
    );
    counts.unclassified_lines_written = mutation.lines_written;
    let fingerprint = hash_text(&format!(
        "{}:{}:{}:{}:{}:{}:{:?}:{:?}",
        context.provider,
        context.source_id.0,
        context.conversation_id,
        context.source_record_id,
        relative_path.display(),
        mutation.section_index,
        added_line_fingerprints,
        deleted_line_fingerprints
    ));
    TraceEdit {
        schema_version: TRACE_EDIT_SCHEMA_VERSION.to_string(),
        trace_edit_id: format!("edit_{}", &fingerprint[..24]),
        provider: context.provider.to_string(),
        source_id: context.source_id.clone(),
        cache_key: context.cache_key.to_string(),
        conversation_id: context.conversation_id.to_string(),
        source_record_id: context.source_record_id.to_string(),
        occurred_at: context.occurred_at,
        project_id: context.project.map(|project| project.project_id.clone()),
        repository_path: context.repository_path.map(Path::to_path_buf),
        relative_path,
        category: mutation.category,
        mutation_kind: mutation.kind,
        counts,
        added_line_fingerprints,
        deleted_line_fingerprints,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchPathStyle {
    /// `apply_patch` headers carry the repository path verbatim.
    Literal,
    /// Git destination paths carry the `b/` prefix from `diff --git`/`+++`.
    GitDestination,
    /// Git source paths carry the `a/` prefix from `---`.
    GitSource,
}

fn clean_patch_path(value: &str, style: PatchPathStyle) -> Option<PathBuf> {
    let value = value.trim().trim_matches('"');
    if value.is_empty() || value == "/dev/null" {
        return None;
    }
    // Only Git adds the `b/` destination prefix. Stripping it unconditionally
    // would rewrite a real top-level `a/` or `b/` directory.
    let value = match style {
        PatchPathStyle::Literal => value,
        PatchPathStyle::GitDestination => value.strip_prefix("b/").unwrap_or(value),
        PatchPathStyle::GitSource => value.strip_prefix("a/").unwrap_or(value),
    };
    Some(PathBuf::from(value))
}

/// Rebases an edited file's path onto the repository that contains it.
///
/// Paths are only ever made shorter, never reinterpreted: an already-relative
/// path is returned untouched, because it is relative to the repository
/// already and stripping a prefix from it a second time would silently drop a
/// directory level and point the edit at the wrong file.
pub fn repository_relative_path(repository_path: Option<&Path>, path: PathBuf) -> PathBuf {
    if path.is_relative() {
        return path;
    }
    let relative = repository_path
        .and_then(|root| path.strip_prefix(root).ok())
        .map(Path::to_path_buf);
    relative.unwrap_or(path)
}

fn logical_lines(value: &str) -> Vec<&str> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split_terminator('\n').collect()
    }
}

fn owned_logical_lines(value: &str) -> Vec<String> {
    logical_lines(value)
        .into_iter()
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect()
}

fn changed_lines(old: &[String], new: &[String]) -> (Vec<String>, Vec<String>) {
    if old.len().saturating_mul(new.len()) > 4_000_000 {
        let prefix = old
            .iter()
            .zip(new)
            .take_while(|(left, right)| left == right)
            .count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(left, right)| left == right)
            .count();
        return (
            new[prefix..new.len().saturating_sub(suffix)].to_vec(),
            old[prefix..old.len().saturating_sub(suffix)].to_vec(),
        );
    }
    let width = new.len() + 1;
    let mut lcs = vec![0u32; (old.len() + 1).saturating_mul(width)];
    for old_index in (0..old.len()).rev() {
        for new_index in (0..new.len()).rev() {
            let index = old_index * width + new_index;
            lcs[index] = if old[old_index] == new[new_index] {
                1 + lcs[(old_index + 1) * width + new_index + 1]
            } else {
                lcs[(old_index + 1) * width + new_index].max(lcs[old_index * width + new_index + 1])
            };
        }
    }
    let (mut old_index, mut new_index) = (0usize, 0usize);
    let mut additions = Vec::new();
    let mut deletions = Vec::new();
    while old_index < old.len() && new_index < new.len() {
        if old[old_index] == new[new_index] {
            old_index += 1;
            new_index += 1;
        } else if lcs[(old_index + 1) * width + new_index] >= lcs[old_index * width + new_index + 1]
        {
            deletions.push(old[old_index].clone());
            old_index += 1;
        } else {
            additions.push(new[new_index].clone());
            new_index += 1;
        }
    }
    deletions.extend_from_slice(&old[old_index..]);
    additions.extend_from_slice(&new[new_index..]);
    (additions, deletions)
}

/// Inspect commits already present in a local Git object database.
///
/// The implementation invokes only local, read-only Git commands and disables
/// prompts, optional locks, external diff drivers, and text conversion.
pub fn scan_local_git_repository(
    path: &Path,
    project_id: Option<&str>,
) -> Result<GitScan, GitScanError> {
    scan_local_git_repository_cached(path, project_id, &[])
}

/// Inspect recent, locally attributable commits while reusing parsed commits.
///
/// Commit hashes cover their trees and parent relationships, so a cached patch
/// remains valid even when mutable repository identity metadata changes. The
/// scan is bounded to the current HEAD and local branches, the configured Git
/// committer identity, and the longest rolling dashboard window.
pub fn scan_local_git_repository_cached(
    path: &Path,
    project_id: Option<&str>,
    cached_commits: &[GitCommitChange],
) -> Result<GitScan, GitScanError> {
    let root = resolve_git_repository_root(path)?;
    let repository_hash = repository_identity_hash(&root)?;
    let configured_email = run_git_allow_missing(&root, &["config", "--get", "user.email"])?;
    let configured_email = configured_email.trim();
    if configured_email.is_empty() {
        return Ok(GitScan {
            repository_root: root,
            repository_hash,
            commits: Vec::new(),
            coverage: CoverageStatus::Unavailable,
        });
    }
    let now = Utc::now();
    let observation_start = (now - Duration::days(GIT_COMMIT_OBSERVATION_DAYS)).to_rfc3339();
    let latest_reportable_day = max_reportable_day(now);
    let since = format!("--since={observation_start}");
    let log = run_git(
        &root,
        &[
            "log",
            // A repository whose first commit has not landed yet has an unborn
            // HEAD, which `git log` otherwise rejects as an unknown revision,
            // failing the whole scan. Ignoring the missing revision leaves a
            // detached HEAD still covered.
            "--ignore-missing",
            "HEAD",
            "--branches",
            &since,
            "--format=%H%x09%cI%x09%aI%x09%P%x09%ce",
            "--no-show-signature",
        ],
    )?;
    let cached_by_hash = cached_commits
        .iter()
        .map(|commit| (commit.commit_hash.as_str(), commit))
        .collect::<BTreeMap<_, _>>();
    let mut metadata = Vec::new();
    let mut future_dated_commits = 0_u64;
    for line in log.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.splitn(5, '\t');
        let Some(commit_hash) = fields.next() else {
            continue;
        };
        let timestamp = fields.next().unwrap_or_default();
        let author_timestamp = fields.next().unwrap_or_default();
        let parents = fields
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>();
        let committer_email = fields.next().unwrap_or_default().trim();
        if !committer_email.eq_ignore_ascii_case(configured_email) {
            continue;
        }
        // A merge diff replays every line the merged branch already contributed
        // through its own commits, so counting merges would double the churn of
        // every merged branch. Conflict resolutions carried only by the merge
        // commit are deliberately left uncounted rather than inflating totals.
        if parents.len() > 1 {
            continue;
        }
        let committed_at = DateTime::parse_from_rfc3339(timestamp)
            .map_err(|_| GitScanError::InvalidTimestamp {
                value: timestamp.to_string(),
            })?
            .with_timezone(&Utc);
        // A committer date is whatever the authoring clock said. One that is
        // implausibly far ahead would be rejected by the sync target and take
        // the whole batch down with it, so it is reported as unmeasured churn
        // through the scan's coverage instead.
        if committed_at.date_naive() > latest_reportable_day {
            future_dated_commits = future_dated_commits.saturating_add(1);
            continue;
        }
        // A rewritten history keeps the original author date, so it is the
        // honest anchor for when the work was written.
        let authored_at = DateTime::parse_from_rfc3339(author_timestamp)
            .ok()
            .map(|value| value.with_timezone(&Utc));
        metadata.push((
            commit_hash.to_string(),
            committed_at,
            authored_at,
            parents.len(),
        ));
    }
    let uncached_hashes = metadata
        .iter()
        .filter(|(commit_hash, _, _, _)| !cached_by_hash.contains_key(commit_hash.as_str()))
        .map(|(commit_hash, _, _, _)| commit_hash.as_str())
        .collect::<Vec<_>>();
    let mut patches_by_hash = git_patches_for_commits(&root, &uncached_hashes)?;
    let mut commits = Vec::with_capacity(metadata.len());
    for (commit_hash, committed_at, authored_at, parent_count) in metadata {
        if let Some(cached) = cached_by_hash.get(commit_hash.as_str()) {
            let mut commit = (*cached).clone();
            commit.deduplication_id =
                hash_text(&format!("git-commit.v1:{repository_hash}:{commit_hash}"));
            commit.repository_hash.clone_from(&repository_hash);
            commit.committed_at = committed_at;
            commit.authored_at = authored_at;
            commit.parent_count = parent_count;
            commit.project_id = project_id.map(ToOwned::to_owned);
            commits.push(commit);
            continue;
        }
        let files = patches_by_hash
            .remove(&commit_hash)
            .ok_or_else(|| GitScanError::Command {
                command: "git log --patch".to_string(),
                message: format!("batched patch output omitted commit {commit_hash}"),
            })?;
        commits.push(GitCommitChange {
            deduplication_id: hash_text(&format!("git-commit.v1:{repository_hash}:{commit_hash}")),
            repository_hash: repository_hash.clone(),
            commit_hash,
            committed_at,
            authored_at,
            parent_count,
            project_id: project_id.map(ToOwned::to_owned),
            files,
        });
    }
    Ok(GitScan {
        repository_root: root,
        repository_hash,
        commits,
        coverage: if future_dated_commits > 0 {
            CoverageStatus::Partial
        } else {
            CoverageStatus::Complete
        },
    })
}

/// Resolves the Git root that owns `path` with a single read-only command.
///
/// Several projects commonly share one repository. Resolving roots first lets
/// callers inspect each repository once instead of once per project path.
pub fn resolve_git_repository_root(path: &Path) -> Result<PathBuf, GitScanError> {
    let root_output = run_git(path, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root_output.trim());
    if root.as_os_str().is_empty() {
        return Err(GitScanError::NotRepository(path.to_path_buf()));
    }
    Ok(root)
}

fn git_patches_for_commits(
    root: &Path,
    commit_hashes: &[&str],
) -> Result<BTreeMap<String, Vec<GitFileChange>>, GitScanError> {
    if commit_hashes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut patches = BTreeMap::new();
    for commit_batch in commit_hashes.chunks(GIT_PATCH_BATCH_SIZE) {
        let mut args = vec![
            "log",
            "--no-walk=unsorted",
            "--format=%x1e%H%x1f",
            "--no-show-signature",
            "--patch",
            "--root",
            "--diff-merges=first-parent",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--unified=0",
        ];
        args.extend_from_slice(commit_batch);
        let output = run_git(root, &args)?;
        for record in output.split('\u{1e}').skip(1) {
            let Some((commit_hash, patch)) = record.split_once('\u{1f}') else {
                continue;
            };
            patches.insert(commit_hash.trim().to_string(), git_files_from_patch(patch));
        }
    }
    Ok(patches)
}

fn repository_identity_hash(root: &Path) -> Result<String, GitScanError> {
    let remote = run_git_allow_missing(root, &["config", "--get", "remote.origin.url"])?;
    let identity = if remote.trim().is_empty() {
        let roots = run_git(root, &["rev-list", "--max-parents=0", "--all"])?;
        let mut root_hashes = roots.lines().collect::<Vec<_>>();
        root_hashes.sort_unstable();
        if root_hashes.is_empty() {
            // A repository with neither an origin nor a single commit has no
            // shared identity to derive yet, and every such repository would
            // otherwise hash to the same empty root list and merge unrelated
            // work. The local path keeps them distinct until the first commit
            // supplies a stable root; the scan then re-keys the repository, and
            // superseded-hash handling retires the placeholder row.
            format!("path:{}", root.display())
        } else {
            format!("roots:{}", root_hashes.join(","))
        }
    } else {
        let remote = normalize_git_remote(&remote).unwrap_or_else(|| remote.trim().to_string());
        format!("remote:{remote}")
    };
    Ok(hash_text(&format!("repository.v1:{identity}")))
}

/// Converts common Git remote transports into a stable lowercase host/path identity.
#[must_use]
pub fn normalize_git_remote(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let host_and_path = if let Some(rest) = trimmed.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        format!("{host}/{path}")
    } else if let Some((_, rest)) = trimmed.split_once("://") {
        let rest = rest.trim_start_matches('/');
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next().unwrap_or(authority);
        format!("{host}/{path}")
    } else {
        trimmed.to_string()
    };

    let mut parts = host_and_path
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }
    if let Some(last) = parts.last_mut() {
        if let Some(stripped) = last.strip_suffix(".git") {
            *last = stripped.to_string();
        }
    }
    Some(parts.join("/"))
}

fn run_git(path: &Path, args: &[&str]) -> Result<String, GitScanError> {
    let output = git_command(path, args)
        .output()
        .map_err(|error| GitScanError::Command {
            command: format!("git {}", args.join(" ")),
            message: error.to_string(),
        })?;
    output_text(args, output, false)
}

fn run_git_allow_missing(path: &Path, args: &[&str]) -> Result<String, GitScanError> {
    let output = git_command(path, args)
        .output()
        .map_err(|error| GitScanError::Command {
            command: format!("git {}", args.join(" ")),
            message: error.to_string(),
        })?;
    output_text(args, output, true)
}

fn git_command(path: &Path, args: &[&str]) -> Command {
    #[cfg(test)]
    TEST_GIT_COMMAND_COUNT.set(TEST_GIT_COMMAND_COUNT.get().saturating_add(1));
    let mut command = Command::new("git");
    command
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .args(args);
    command
}

fn output_text(args: &[&str], output: Output, allow_failure: bool) -> Result<String, GitScanError> {
    if !output.status.success() && !allow_failure {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if message.contains("not a git repository") {
            GitScanError::NotRepository(PathBuf::from("."))
        } else {
            GitScanError::Command {
                command: format!("git {}", args.join(" ")),
                message,
            }
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_files_from_patch(patch: &str) -> Vec<GitFileChange> {
    let context = TraceEditContext {
        provider: "git",
        source_id: &SourceId("git".to_string()),
        cache_key: "git",
        conversation_id: "git",
        source_record_id: "git",
        occurred_at: None,
        project: None,
        repository_path: None,
    };
    parse_unified_patch(&context, patch)
        .edits
        .into_iter()
        .map(|edit| GitFileChange {
            relative_path: edit.relative_path,
            category: edit.category,
            counts: edit.counts,
            added_line_fingerprints: edit.added_line_fingerprints,
            deleted_line_fingerprints: edit.deleted_line_fingerprints,
        })
        .collect()
}

#[must_use]
pub fn match_trace_edits_to_commits(
    trace_edits: &[TraceEdit],
    git_scans: &[GitScan],
) -> Vec<CodeChangeMatch> {
    // Grouping each scan's changed files by path once keeps matching linear in
    // the number of candidate commits per file instead of rescanning every
    // commit in every repository for every trace edit.
    let files_by_path = git_scans
        .iter()
        .map(|scan| {
            let mut index: BTreeMap<&Path, Vec<(&GitCommitChange, &GitFileChange)>> =
                BTreeMap::new();
            for commit in &scan.commits {
                // Merge diffs replay a parent commit's entire patch. Using them
                // for attribution would turn an otherwise exact match into an
                // ambiguity or double-attribute the same edit.
                if commit.parent_count > 1 {
                    continue;
                }
                for file in &commit.files {
                    index
                        .entry(file.relative_path.as_path())
                        .or_default()
                        .push((commit, file));
                }
            }
            index
        })
        .collect::<Vec<_>>();
    let scan_roots = canonical_scan_roots(git_scans);
    let mut canonical_paths = CanonicalPathCache::default();
    let mut matches = Vec::new();
    let mut matched_trace_ids = BTreeSet::new();
    for trace in trace_edits {
        let Some(trace_time) = trace.occurred_at else {
            continue;
        };
        // Without a repository path no scan can place the edit's file.
        let Some(repository_path) = trace.repository_path.as_deref() else {
            continue;
        };
        // An edit carrying too little distinct content cannot single out a
        // commit: ubiquitous lines such as a lone `}` or a blank line reach a
        // perfect overlap against whichever commit happens to touch the file,
        // which would credit an agent for otherwise human-written work. Such an
        // edit still counts as an applied agent edit and its commit still
        // counts as committed churn; only the attribution between them is
        // withheld.
        let distinct_fingerprints = trace
            .added_line_fingerprints
            .iter()
            .chain(&trace.deleted_line_fingerprints)
            .collect::<BTreeSet<_>>()
            .len();
        if distinct_fingerprints < MIN_ATTRIBUTION_DISTINCT_FINGERPRINTS {
            continue;
        }
        let canonical_repository_path = canonical_paths.resolve(repository_path).to_path_buf();
        let mut best: Option<(AttributionConfidence, f64, &GitCommitChange, &GitFileChange)> = None;
        let mut ambiguous = false;
        for ((scan, index), scan_root) in git_scans.iter().zip(&files_by_path).zip(&scan_roots) {
            let Some(trace_relative_path) =
                trace_path_in_scan(trace, scan, &canonical_repository_path, scan_root)
            else {
                continue;
            };
            let Some(candidates) = index.get(trace_relative_path.as_path()) else {
                continue;
            };
            for (commit, file) in candidates.iter().copied() {
                let Some(delay) = commit_delay_since_trace(commit, trace_time) else {
                    continue;
                };
                if delay.num_seconds() > 30 * 24 * 60 * 60 {
                    continue;
                }
                let added_overlap = ordered_overlap_ratio(
                    &trace.added_line_fingerprints,
                    &file.added_line_fingerprints,
                );
                let deleted_overlap = ordered_overlap_ratio(
                    &trace.deleted_line_fingerprints,
                    &file.deleted_line_fingerprints,
                );
                let overlap = weighted_overlap(
                    added_overlap,
                    trace.added_line_fingerprints.len(),
                    deleted_overlap,
                    trace.deleted_line_fingerprints.len(),
                );
                let confidence = if delay.num_seconds() <= 7 * 24 * 60 * 60
                    && added_overlap == 1.0
                    && deleted_overlap == 1.0
                {
                    Some(AttributionConfidence::High)
                } else if overlap >= 0.8 {
                    Some(AttributionConfidence::Medium)
                } else {
                    None
                };
                let Some(confidence) = confidence else {
                    continue;
                };
                let rank = |value: AttributionConfidence| match value {
                    AttributionConfidence::High => 2,
                    AttributionConfidence::Medium => 1,
                };
                let is_better = best.as_ref().is_none_or(|current| {
                    rank(confidence) > rank(current.0)
                        || (rank(confidence) == rank(current.0) && overlap > current.1)
                });
                let is_equally_strong = best.as_ref().is_some_and(|current| {
                    rank(confidence) == rank(current.0)
                        && overlap == current.1
                        && commit.deduplication_id != current.2.deduplication_id
                });
                if is_better {
                    best = Some((confidence, overlap, commit, file));
                    ambiguous = false;
                } else if is_equally_strong {
                    ambiguous = true;
                }
            }
        }
        if let Some((confidence, overlap, commit, file)) = best.filter(|_| !ambiguous) {
            if matched_trace_ids.insert(trace.trace_edit_id.clone()) {
                matches.push(CodeChangeMatch {
                    match_id: hash_text(&format!(
                        "code-match.v1:{}:{}:{}",
                        trace.trace_edit_id,
                        commit.deduplication_id,
                        file.relative_path.display()
                    )),
                    trace_edit_id: trace.trace_edit_id.clone(),
                    commit_deduplication_id: commit.deduplication_id.clone(),
                    commit_hash: commit.commit_hash.clone(),
                    repository_hash: commit.repository_hash.clone(),
                    relative_path: file.relative_path.clone(),
                    committed_at: commit.committed_at,
                    confidence,
                    ordered_line_overlap: overlap,
                    counts: trace.counts,
                });
            }
        }
    }
    matches
}

/// How long after a trace edit the commit carrying it was written.
///
/// A rebase, amend, or squash rewrites the committer date, so a month of work
/// can suddenly claim to have been committed today. The author date survives
/// those rewrites, so the shorter non-negative gap of the two is the honest
/// one: without it, rebasing work older than the matching window destroys its
/// attribution permanently. `None` means the commit predates the edit and
/// cannot carry it.
fn commit_delay_since_trace(
    commit: &GitCommitChange,
    trace_time: DateTime<Utc>,
) -> Option<Duration> {
    [commit.authored_at, Some(commit.committed_at)]
        .into_iter()
        .flatten()
        .map(|written_at| written_at.signed_duration_since(trace_time))
        .filter(|delay| delay.num_seconds() >= 0)
        .min()
}

/// Locates a trace edit's file inside one scanned repository.
///
/// A project and a repository nest either way around. The project can sit
/// inside the repository, in which case the edit's path needs the project's
/// offset prepended. The repository can equally sit inside the project: an
/// agent working in a multi-repository workspace records the workspace as its
/// project, so the edit's path already leads with the repository's directory
/// and that leading component has to come off instead. Handling only the first
/// direction silently drops every edit made in such a workspace.
///
/// Symlinked project paths only agree with a scan root after canonicalization,
/// which costs a syscall per component. Both canonical forms are therefore
/// resolved once by the caller and passed in: this runs for every
/// (edit, repository) pair, and archives reach hundreds of thousands of edits.
fn trace_path_in_scan(
    trace: &TraceEdit,
    scan: &GitScan,
    canonical_repository_path: &Path,
    canonical_scan_root: &Path,
) -> Option<PathBuf> {
    let repository_path = trace.repository_path.as_deref()?;
    if repository_path == scan.repository_root || canonical_repository_path == canonical_scan_root {
        return repository_contained_path(trace.relative_path.clone());
    }
    if let Some(project_offset) = repository_path
        .strip_prefix(&scan.repository_root)
        .ok()
        .or_else(|| {
            canonical_repository_path
                .strip_prefix(canonical_scan_root)
                .ok()
        })
    {
        return repository_contained_path(project_offset.join(&trace.relative_path));
    }
    let repository_offset = scan
        .repository_root
        .strip_prefix(repository_path)
        .ok()
        .or_else(|| {
            canonical_scan_root
                .strip_prefix(canonical_repository_path)
                .ok()
        })?;
    repository_contained_path(
        trace
            .relative_path
            .strip_prefix(repository_offset)
            .ok()?
            .to_path_buf(),
    )
}

/// Accepts a repository-relative path only while it stays inside the repository.
///
/// A tool call can name a file the project does not contain, either absolutely
/// or by escaping with `..`, and reconstruction keeps that path verbatim when it
/// cannot be made relative. Such an edit is real but belongs to no scanned
/// repository, so it must not inherit this one's identity. Note that `join`
/// with an absolute path discards the prefix, so this also covers the nested
/// project case.
fn repository_contained_path(path: PathBuf) -> Option<PathBuf> {
    let mut depth = 0_usize;
    for component in path.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => depth = depth.checked_sub(1)?,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(path)
}

/// Canonical repository roots, positionally aligned with `git_scans`.
fn canonical_scan_roots(git_scans: &[GitScan]) -> Vec<PathBuf> {
    git_scans
        .iter()
        .map(|scan| canonical_path(&scan.repository_root))
        .collect()
}

fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Memoizes canonical paths, since trace edits share a handful of project roots.
#[derive(Default)]
struct CanonicalPathCache(BTreeMap<PathBuf, PathBuf>);

impl CanonicalPathCache {
    fn resolve(&mut self, path: &Path) -> &Path {
        if !self.0.contains_key(path) {
            self.0.insert(path.to_path_buf(), canonical_path(path));
        }
        &self.0[path]
    }
}

fn ordered_overlap_ratio(needle: &[String], haystack: &[String]) -> f64 {
    if needle.is_empty() {
        return 1.0;
    }
    let mut matched = 0usize;
    for value in haystack {
        if needle.get(matched) == Some(value) {
            matched += 1;
            if matched == needle.len() {
                break;
            }
        }
    }
    matched as f64 / needle.len() as f64
}

fn weighted_overlap(added: f64, added_count: usize, deleted: f64, deleted_count: usize) -> f64 {
    let total = added_count.saturating_add(deleted_count);
    if total == 0 {
        0.0
    } else {
        (added * added_count as f64 + deleted * deleted_count as f64) / total as f64
    }
}

/// Metrics for one refresh together with the coverage they were built under.
///
/// Building can itself discover unmeasurable churn, so the effective trace
/// coverage is reported back rather than left at the caller's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeChangeMetricBuild {
    pub metrics: Vec<CodeChangeMetric>,
    pub trace_coverage: CoverageStatus,
}

pub fn build_code_change_metrics(
    now: DateTime<Utc>,
    device_id: &str,
    trace_edits: &[TraceEdit],
    git_scans: &[GitScan],
    matches: &[CodeChangeMatch],
    committed_metric_ids: &BTreeMap<String, String>,
    trace_coverage: CoverageStatus,
) -> Result<CodeChangeMetricBuild, CodeChangeMetricBuildError> {
    let git_coverage = git_scans
        .iter()
        .map(|scan| scan.coverage)
        .reduce(CoverageStatus::combine)
        .unwrap_or(CoverageStatus::Unavailable);
    let latest_reportable_day = max_reportable_day(now);
    let mut metrics = Vec::new();
    // Agent edits are the only unbounded input: one archive can hold hundreds
    // of thousands of them. They are published on the daily, project, and
    // repository dimensions the dashboard reads, so the metric count stays
    // proportional to observed days rather than to individual edits.
    let mut agent_edits =
        BTreeMap::<(NaiveDate, Option<String>, Option<String>), CodeLineCounts>::new();
    let scan_roots = canonical_scan_roots(git_scans);
    let mut canonical_paths = CanonicalPathCache::default();
    let mut skipped_edits = 0_u64;
    for edit in trace_edits {
        let Some(occurred_at) = edit.occurred_at else {
            continue;
        };
        let day = occurred_at.date_naive();
        if day > latest_reportable_day {
            skipped_edits = skipped_edits.saturating_add(1);
            continue;
        }
        let repository_hash = edit.repository_path.as_deref().and_then(|repository_path| {
            let canonical_repository_path = canonical_paths.resolve(repository_path).to_path_buf();
            repository_hash_for_trace(edit, git_scans, &scan_roots, &canonical_repository_path)
        });
        agent_edits
            .entry((day, edit.project_id.clone(), repository_hash))
            .or_default()
            .add(edit.counts);
    }
    // A clock-skewed edit is churn the archive recorded and this build declined
    // to publish. Leaving coverage untouched would let the surviving metrics
    // claim they describe the period completely.
    let trace_coverage = if skipped_edits > 0 {
        trace_coverage.combine(CoverageStatus::Partial)
    } else {
        trace_coverage
    };
    for ((day, project_id, repository_hash), counts) in agent_edits {
        metrics.push(CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: agent_edit_metric_id(device_id, day, &project_id, &repository_hash),
            device_id: device_id.to_string(),
            day,
            project_id,
            repository_hash,
            commit_hash: None,
            kind: CodeChangeMetricKind::AgentEdit,
            counts,
            attribution_confidence: None,
            trace_coverage,
            git_coverage,
        });
    }
    for scan in git_scans {
        for commit in &scan.commits {
            let metric_id = committed_metric_ids
                .get(&commit.deduplication_id)
                .ok_or(CodeChangeMetricBuildError::MissingCommittedMetricId)?;
            let mut counts = CodeLineCounts::default();
            for file in &commit.files {
                counts.add(file.counts);
            }
            metrics.push(CodeChangeMetric {
                schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
                metric_id: metric_id.clone(),
                device_id: device_id.to_string(),
                day: commit.committed_at.date_naive(),
                project_id: commit.project_id.clone(),
                repository_hash: Some(commit.repository_hash.clone()),
                commit_hash: Some(commit.commit_hash.clone()),
                kind: CodeChangeMetricKind::Committed,
                counts,
                attribution_confidence: None,
                trace_coverage,
                git_coverage,
            });
        }
    }
    // Indexed once: a match carries no project of its own, and scanning every
    // trace edit per match is quadratic on archives this size.
    let project_by_trace_edit = trace_edits
        .iter()
        .map(|edit| (edit.trace_edit_id.as_str(), edit.project_id.as_deref()))
        .collect::<BTreeMap<_, _>>();
    for matched in matches {
        if matched.committed_at.date_naive() > latest_reportable_day {
            continue;
        }
        metrics.push(CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: matched.match_id.clone(),
            device_id: device_id.to_string(),
            day: matched.committed_at.date_naive(),
            project_id: project_by_trace_edit
                .get(matched.trace_edit_id.as_str())
                .copied()
                .flatten()
                .map(ToOwned::to_owned),
            repository_hash: Some(matched.repository_hash.clone()),
            commit_hash: Some(matched.commit_hash.clone()),
            kind: CodeChangeMetricKind::TraceMatchedCommitted,
            counts: matched.counts,
            attribution_confidence: Some(matched.confidence),
            trace_coverage,
            git_coverage,
        });
    }
    metrics.sort_by(|left, right| left.metric_id.cmp(&right.metric_id));
    metrics.dedup_by(|left, right| left.metric_id == right.metric_id);
    Ok(CodeChangeMetricBuild {
        metrics,
        trace_coverage,
    })
}

/// Opaque, device-scoped identifier for one day of aggregated agent edits.
///
/// The dimensions are hashed rather than concatenated so a metric ID never
/// leaks a project or repository identity, and the record separator keeps a
/// value that contains the delimiter from colliding with a different split.
fn agent_edit_metric_id(
    device_id: &str,
    day: NaiveDate,
    project_id: &Option<String>,
    repository_hash: &Option<String>,
) -> String {
    hash_text(&format!(
        "agent-edit-day.v1\u{1e}{device_id}\u{1e}{day}\u{1e}{}\u{1e}{}",
        project_id.as_deref().unwrap_or_default(),
        repository_hash.as_deref().unwrap_or_default()
    ))
}

fn repository_hash_for_trace(
    edit: &TraceEdit,
    scans: &[GitScan],
    scan_roots: &[PathBuf],
    canonical_repository_path: &Path,
) -> Option<String> {
    scans
        .iter()
        .zip(scan_roots)
        .filter(|(scan, scan_root)| {
            trace_path_in_scan(edit, scan, canonical_repository_path, scan_root).is_some()
        })
        .max_by_key(|(scan, _)| scan.repository_root.components().count())
        .map(|(scan, _)| scan.repository_hash.clone())
}

#[must_use]
pub fn aggregate_code_change_metrics(
    metrics: &[CodeChangeMetric],
) -> BTreeMap<(NaiveDate, CodeChangeMetricKind), CodeLineCounts> {
    let mut totals = BTreeMap::new();
    for metric in metrics {
        totals
            .entry((metric.day, metric.kind))
            .or_insert_with(CodeLineCounts::default)
            .add(metric.counts);
    }
    totals
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn context<'a>(source: &'a SourceId) -> TraceEditContext<'a> {
        TraceEditContext {
            provider: "codex",
            source_id: source,
            cache_key: "archive.jsonl",
            conversation_id: "conversation",
            source_record_id: "record:1",
            occurred_at: Some(
                DateTime::parse_from_rfc3339("2026-08-01T10:00:00Z")
                    .unwrap()
                    .into(),
            ),
            project: None,
            repository_path: None,
        }
    }

    #[test]
    fn an_already_relative_edit_path_is_never_rebased_a_second_time() {
        // Providers record a project path as a display label, which is not
        // guaranteed to be absolute. When both it and the edited path are
        // relative, stripping the prefix would drop a directory level and
        // attribute the edit to a file that was never touched.
        assert_eq!(
            repository_relative_path(
                Some(Path::new("crates/statsai-core")),
                PathBuf::from("crates/statsai-core/src/lib.rs")
            ),
            PathBuf::from("crates/statsai-core/src/lib.rs")
        );
        // An absolute path is still rebased onto the repository holding it.
        assert_eq!(
            repository_relative_path(Some(Path::new("/repo")), PathBuf::from("/repo/src/lib.rs")),
            PathBuf::from("src/lib.rs")
        );
        // One that lies outside the repository keeps its own path.
        assert_eq!(
            repository_relative_path(
                Some(Path::new("/repo")),
                PathBuf::from("/elsewhere/src/lib.rs")
            ),
            PathBuf::from("/elsewhere/src/lib.rs")
        );
    }

    #[test]
    fn parses_apply_patch_and_separates_test_code() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "*** Begin Patch\r\n*** Update File: src/lib.rs\r\n@@\r\n-old\r\n+new\r\n*** Add File: tests/new_test.rs\r\n+one\r\n+two\r\n*** End Patch\r\n",
        );
        assert_eq!(parsed.coverage, CoverageStatus::Complete);
        assert_eq!(parsed.edits.len(), 2);
        assert_eq!(parsed.edits[0].counts.source_additions, 1);
        assert_eq!(parsed.edits[0].counts.source_deletions, 1);
        assert_eq!(parsed.edits[1].counts.test_additions, 2);
    }

    #[test]
    fn repeated_identical_patch_sections_have_distinct_stable_trace_ids() {
        let source = SourceId("source".to_string());
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch\n";

        let first = parse_unified_patch(&context(&source), patch);
        let repeated = parse_unified_patch(&context(&source), patch);

        assert_eq!(first.edits.len(), 2);
        assert_ne!(first.edits[0].trace_edit_id, first.edits[1].trace_edit_id);
        assert_eq!(
            first
                .edits
                .iter()
                .map(|edit| edit.trace_edit_id.as_str())
                .collect::<Vec<_>>(),
            repeated
                .edits
                .iter()
                .map(|edit| edit.trace_edit_id.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn apply_patch_move_uses_the_destination_path() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "*** Begin Patch\n*** Update File: src/old.rs\n*** Move to: src/new.rs\n@@\n-old\n+new\n*** End Patch\n",
        );
        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/new.rs"));
    }

    #[test]
    fn apply_patch_delete_without_body_is_unavailable_instead_of_zero_lines() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "*** Begin Patch\n*** Delete File: src/old.rs\n*** End Patch\n",
        );

        assert!(parsed.edits.is_empty());
        assert_eq!(parsed.unsupported_sections, 1);
        assert_eq!(parsed.coverage, CoverageStatus::Unavailable);
    }

    #[test]
    fn non_code_patch_is_ignored_with_complete_coverage() {
        let source = SourceId("source".to_string());
        for patch in [
            "*** Begin Patch\n*** Update File: README.md\n@@\n-old\n+new\n*** End Patch\n",
            "*** Begin Patch\n*** Delete File: README.md\n*** End Patch\n",
        ] {
            let parsed = parse_unified_patch(&context(&source), patch);
            assert!(parsed.edits.is_empty());
            assert_eq!(parsed.unsupported_sections, 0);
            assert_eq!(parsed.coverage, CoverageStatus::Complete);
        }
    }

    #[test]
    fn apply_patch_keeps_literal_paths_that_look_like_git_prefixes() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "*** Begin Patch\n*** Update File: a/lib.rs\n@@\n-old\n+new\n*** End Patch\n",
        );

        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].relative_path, Path::new("a/lib.rs"));
    }

    #[test]
    fn git_patch_body_lines_are_not_mistaken_for_file_headers() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,0 +1,2 @@\n+++ still a body line\n+--- /dev/null\n",
        );

        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/lib.rs"));
        assert_eq!(parsed.edits[0].counts.source_additions, 2);
        assert_eq!(parsed.edits[0].mutation_kind, MutationKind::UnifiedPatch);
    }

    #[test]
    fn plain_unified_diffs_separate_files_without_git_headers() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "--- a/src/first.rs\n+++ b/src/first.rs\n@@ -1,0 +1,1 @@\n+one\n--- a/src/second.rs\n+++ b/src/second.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n",
        );

        assert_eq!(parsed.edits.len(), 2);
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/first.rs"));
        assert_eq!(parsed.edits[0].counts.source_additions, 1);
        assert_eq!(parsed.edits[0].counts.source_deletions, 0);
        assert_eq!(parsed.edits[1].relative_path, Path::new("src/second.rs"));
        assert_eq!(parsed.edits[1].counts.source_additions, 1);
        assert_eq!(parsed.edits[1].counts.source_deletions, 1);
    }

    #[test]
    fn plain_unified_diff_deletions_keep_the_source_side_path() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "--- a/src/gone.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n",
        );

        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/gone.rs"));
        assert_eq!(parsed.edits[0].counts.source_deletions, 2);
    }

    #[test]
    fn absolute_patch_paths_are_normalized_to_the_repository_root() {
        let source = SourceId("source".to_string());
        let repository = PathBuf::from("/repo");
        let mut context = context(&source);
        context.repository_path = Some(&repository);

        let parsed = parse_unified_patch(
            &context,
            "*** Begin Patch\n*** Update File: /repo/src/lib.rs\n@@\n-old\n+new\n*** End Patch\n",
        );

        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/lib.rs"));
    }

    #[test]
    fn structured_edit_uses_line_diff_and_handles_missing_final_newline() {
        let source = SourceId("source".to_string());
        let parsed = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "same\nold",
            "same\nnew",
            0,
        );
        assert_eq!(parsed.edits[0].counts.source_additions, 1);
        assert_eq!(parsed.edits[0].counts.source_deletions, 1);
    }

    #[test]
    fn overwrite_reports_lines_written_without_inventing_a_diff() {
        let source = SourceId("source".to_string());
        let parsed = parse_full_file_write(
            &context(&source),
            Path::new("src/lib.rs"),
            "one\ntwo\n",
            false,
        );
        assert_eq!(parsed.edits[0].counts.additions(), 0);
        assert_eq!(parsed.edits[0].counts.unclassified_lines_written, 2);
    }

    #[test]
    fn ignores_lockfiles_generated_directories_and_minified_files() {
        assert!(classify_code_path(Path::new("Cargo.lock")).is_none());
        assert!(classify_code_path(Path::new("node_modules/pkg/index.js")).is_none());
        assert!(classify_code_path(Path::new("src/app.min.js")).is_none());
        assert_eq!(
            classify_code_path(Path::new("src/app.rs")),
            Some(CodeCategory::Source)
        );
    }

    #[test]
    fn ignores_documentation_configuration_manifests_and_unknown_text() {
        for path in [
            "README.md",
            "docs/guide.txt",
            "Cargo.toml",
            "package.json",
            ".github/workflows/ci.yml",
            "tests/fixtures/example.md",
        ] {
            assert_eq!(classify_code_path(Path::new(path)), None, "{path}");
        }
        assert_eq!(
            classify_code_path(Path::new("src/component.tsx")),
            Some(CodeCategory::Source)
        );
        assert_eq!(
            classify_code_path(Path::new("tests/component_test.py")),
            Some(CodeCategory::Test)
        );
    }

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
    fn local_git_scan_counts_commits_without_remote_access() {
        let temp = TempDir::new().unwrap();
        run_test_git(temp.path(), &["init", "-q"]);
        run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
        run_test_git(temp.path(), &["config", "user.name", "Test"]);
        run_test_git(
            temp.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://127.0.0.1:1/unreachable.git",
            ],
        );
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/lib.rs"), "one\ntwo\n").unwrap();
        run_test_git(temp.path(), &["add", "src/lib.rs"]);
        run_test_git(temp.path(), &["commit", "-qm", "initial"]);

        let scan = scan_local_git_repository(temp.path(), Some("project")).unwrap();
        assert_eq!(scan.commits.len(), 1);
        assert_eq!(scan.commits[0].files[0].counts.source_additions, 2);
        assert_eq!(scan.commits[0].files[0].counts.source_deletions, 0);
    }

    #[test]
    fn git_scan_only_counts_recent_local_branch_commits_from_configured_identity() {
        let temp = TempDir::new().unwrap();
        run_test_git(temp.path(), &["init", "-q"]);
        run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
        run_test_git(temp.path(), &["config", "user.name", "Test"]);

        commit_test_file(
            temp.path(),
            "old.rs",
            "old\n",
            "old local",
            "test@example.com",
            Utc::now() - chrono::Duration::days(120),
        );
        let main_branch = test_git_stdout(temp.path(), &["branch", "--show-current"]);
        commit_test_file(
            temp.path(),
            "foreign.rs",
            "foreign\n",
            "recent foreign",
            "other@example.com",
            Utc::now() - chrono::Duration::days(2),
        );
        commit_test_file(
            temp.path(),
            "recent.rs",
            "recent\n",
            "recent local",
            "test@example.com",
            Utc::now() - chrono::Duration::days(1),
        );

        run_test_git(temp.path(), &["checkout", "-qb", "imported"]);
        commit_test_file(
            temp.path(),
            "remote.rs",
            "remote\n",
            "remote-tracking only",
            "test@example.com",
            Utc::now() - chrono::Duration::days(1),
        );
        let remote_hash = test_git_stdout(temp.path(), &["rev-parse", "HEAD"]);
        run_test_git(temp.path(), &["checkout", "-q", &main_branch]);
        run_test_git(
            temp.path(),
            &["update-ref", "refs/remotes/origin/imported", &remote_hash],
        );
        run_test_git(temp.path(), &["branch", "-D", "imported"]);

        let scan = scan_local_git_repository(temp.path(), None).unwrap();

        assert_eq!(scan.commits.len(), 1);
        assert!(scan.commits[0]
            .files
            .iter()
            .any(|file| file.relative_path == Path::new("recent.rs")));
    }

    #[test]
    fn cached_git_scan_reuses_reachable_commits_and_inspects_new_commits() {
        let temp = TempDir::new().unwrap();
        run_test_git(temp.path(), &["init", "-q"]);
        run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
        run_test_git(temp.path(), &["config", "user.name", "Test"]);
        fs::write(temp.path().join("first.rs"), "first\n").unwrap();
        run_test_git(temp.path(), &["add", "first.rs"]);
        run_test_git(temp.path(), &["commit", "-qm", "first"]);

        let mut cached = scan_local_git_repository(temp.path(), None).unwrap();
        let cached_hash = cached.commits[0].commit_hash.clone();
        cached.commits[0].files[0].counts.source_additions = 99;
        fs::write(temp.path().join("second.rs"), "second\n").unwrap();
        run_test_git(temp.path(), &["add", "second.rs"]);
        run_test_git(temp.path(), &["commit", "-qm", "second"]);

        let refreshed =
            scan_local_git_repository_cached(temp.path(), None, &cached.commits).unwrap();
        assert_eq!(refreshed.commits.len(), 2);
        let reused = refreshed
            .commits
            .iter()
            .find(|commit| commit.commit_hash == cached_hash)
            .expect("cached commit remains reachable");
        assert_eq!(reused.files[0].counts.source_additions, 99);
        assert!(refreshed.commits.iter().any(|commit| {
            commit
                .files
                .iter()
                .any(|file| file.relative_path == Path::new("second.rs"))
        }));
    }

    #[test]
    fn initial_git_scan_batches_uncached_commit_patches() {
        let temp = TempDir::new().unwrap();
        run_test_git(temp.path(), &["init", "-q"]);
        run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
        run_test_git(temp.path(), &["config", "user.name", "Test"]);
        for index in 0..3 {
            fs::write(
                temp.path().join(format!("file-{index}.rs")),
                format!("{index}\n"),
            )
            .unwrap();
            run_test_git(temp.path(), &["add", "."]);
            run_test_git(temp.path(), &["commit", "-qm", &format!("commit {index}")]);
        }

        TEST_GIT_COMMAND_COUNT.set(0);
        let scan = scan_local_git_repository(temp.path(), None).unwrap();

        assert_eq!(scan.commits.len(), 3);
        assert_eq!(
            TEST_GIT_COMMAND_COUNT.get(),
            6,
            "root lookup, identity config/roots, committer lookup, metadata log, and one patch batch"
        );
    }

    #[test]
    fn git_scan_handles_renames_and_excludes_binary_files() {
        let temp = TempDir::new().unwrap();
        run_test_git(temp.path(), &["init", "-q"]);
        run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
        run_test_git(temp.path(), &["config", "user.name", "Test"]);
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/lib.rs"), "one\ntwo\n").unwrap();
        run_test_git(temp.path(), &["add", "src/lib.rs"]);
        run_test_git(temp.path(), &["commit", "-qm", "initial"]);
        fs::write(temp.path().join("image.bin"), [0, 159, 146, 150]).unwrap();
        run_test_git(temp.path(), &["add", "image.bin"]);
        run_test_git(temp.path(), &["commit", "-qm", "binary"]);
        fs::rename(
            temp.path().join("src/lib.rs"),
            temp.path().join("src/main.rs"),
        )
        .unwrap();
        fs::write(temp.path().join("src/main.rs"), "one\ntwo\nthree\n").unwrap();
        run_test_git(temp.path(), &["add", "-A"]);
        run_test_git(temp.path(), &["commit", "-qm", "rename"]);

        let scan = scan_local_git_repository(temp.path(), None).unwrap();
        assert_eq!(scan.commits.len(), 3);
        let binary_commit = scan
            .commits
            .iter()
            .find(|commit| commit.files.is_empty())
            .expect("binary-only commit is excluded");
        assert!(binary_commit.files.is_empty());
        let renamed = scan
            .commits
            .iter()
            .flat_map(|commit| &commit.files)
            .find(|file| file.relative_path == Path::new("src/main.rs"))
            .expect("renamed file");
        assert_eq!(renamed.counts.source_additions, 1);
    }

    #[test]
    fn git_scan_handles_linked_worktrees_branches_and_merges() {
        let temp = TempDir::new().unwrap();
        let repository = temp.path().join("repository");
        let worktree = temp.path().join("feature-worktree");
        fs::create_dir_all(&repository).unwrap();
        run_test_git(&repository, &["init", "-q"]);
        run_test_git(&repository, &["config", "user.email", "test@example.com"]);
        run_test_git(&repository, &["config", "user.name", "Test"]);
        fs::write(repository.join("base.rs"), "base\n").unwrap();
        run_test_git(&repository, &["add", "base.rs"]);
        run_test_git(&repository, &["commit", "-qm", "base"]);

        let worktree_text = worktree.to_string_lossy().into_owned();
        run_test_git(
            &repository,
            &["worktree", "add", "-q", "-b", "feature", &worktree_text],
        );
        fs::write(worktree.join("feature.rs"), "feature\n").unwrap();
        run_test_git(&worktree, &["add", "feature.rs"]);
        run_test_git(&worktree, &["commit", "-qm", "feature"]);

        fs::write(repository.join("main.rs"), "main\n").unwrap();
        run_test_git(&repository, &["add", "main.rs"]);
        run_test_git(&repository, &["commit", "-qm", "main"]);
        run_test_git(
            &repository,
            &["merge", "--no-ff", "-qm", "merge", "feature"],
        );

        let main_scan = scan_local_git_repository(&repository, None).unwrap();
        let worktree_scan = scan_local_git_repository(&worktree, None).unwrap();
        assert_eq!(main_scan.repository_hash, worktree_scan.repository_hash);
        assert_eq!(main_scan.commits.len(), worktree_scan.commits.len());
        assert!(
            main_scan
                .commits
                .iter()
                .all(|commit| commit.parent_count <= 1),
            "merge commits replay branch churn and must not be counted"
        );
        assert_eq!(
            main_scan
                .commits
                .iter()
                .flat_map(|commit| &commit.files)
                .filter(|file| file.relative_path == Path::new("feature.rs"))
                .map(|file| file.counts.source_additions)
                .sum::<u64>(),
            1,
            "a merged branch's additions are counted once"
        );
    }

    #[test]
    fn repository_identity_normalizes_ssh_and_https_origin_urls() {
        let repository = TempDir::new().unwrap();
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        fs::write(repository.path().join("main.rs"), "fn main() {}\n").unwrap();
        run_test_git(repository.path(), &["add", "main.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);
        run_test_git(
            repository.path(),
            &["remote", "add", "origin", "git@github.com:Owner/Repo.git"],
        );

        let ssh_scan = scan_local_git_repository(repository.path(), None).unwrap();
        run_test_git(
            repository.path(),
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/Owner/Repo.git",
            ],
        );
        let https_scan = scan_local_git_repository(repository.path(), None).unwrap();

        assert_eq!(ssh_scan.repository_hash, https_scan.repository_hash);
        assert_eq!(
            ssh_scan.commits[0].deduplication_id,
            https_scan.commits[0].deduplication_id
        );
    }

    #[test]
    fn remote_repository_identity_ignores_different_local_root_sets() {
        let repository = TempDir::new().unwrap();
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        run_test_git(
            repository.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repository.git",
            ],
        );
        fs::write(repository.path().join("main.rs"), "fn main() {}\n").unwrap();
        run_test_git(repository.path(), &["add", "main.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);
        let shared_commit = test_git_stdout(repository.path(), &["rev-parse", "HEAD"]);
        let initial_scan = scan_local_git_repository(repository.path(), None).unwrap();

        run_test_git(
            repository.path(),
            &["checkout", "-q", "--orphan", "unrelated"],
        );
        fs::write(
            repository.path().join("unrelated.rs"),
            "pub fn unrelated() {}\n",
        )
        .unwrap();
        run_test_git(repository.path(), &["add", "unrelated.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "unrelated root"]);
        let expanded_scan = scan_local_git_repository(repository.path(), None).unwrap();

        assert_eq!(initial_scan.repository_hash, expanded_scan.repository_hash);
        let initial_metric_key = initial_scan
            .commits
            .iter()
            .find(|commit| commit.commit_hash == shared_commit)
            .unwrap()
            .deduplication_id
            .clone();
        let expanded_metric_key = expanded_scan
            .commits
            .iter()
            .find(|commit| commit.commit_hash == shared_commit)
            .unwrap()
            .deduplication_id
            .clone();
        assert_eq!(initial_metric_key, expanded_metric_key);
    }

    #[test]
    fn rebased_commits_replace_unreachable_ids_and_deduplicate_across_devices() {
        let temp = TempDir::new().unwrap();
        run_test_git(temp.path(), &["init", "-q"]);
        run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
        run_test_git(temp.path(), &["config", "user.name", "Test"]);
        fs::write(temp.path().join("base.rs"), "base\n").unwrap();
        run_test_git(temp.path(), &["add", "base.rs"]);
        run_test_git(temp.path(), &["commit", "-qm", "base"]);
        let main_branch = test_git_stdout(temp.path(), &["branch", "--show-current"]);
        run_test_git(temp.path(), &["checkout", "-qb", "feature"]);
        fs::write(temp.path().join("feature.rs"), "before rebase\n").unwrap();
        run_test_git(temp.path(), &["add", "feature.rs"]);
        run_test_git(temp.path(), &["commit", "-qm", "feature"]);
        let old_hash = test_git_stdout(temp.path(), &["rev-parse", "HEAD"]);

        run_test_git(temp.path(), &["checkout", "-q", &main_branch]);
        fs::write(temp.path().join("main.rs"), "main\n").unwrap();
        run_test_git(temp.path(), &["add", "main.rs"]);
        run_test_git(temp.path(), &["commit", "-qm", "main"]);
        run_test_git(temp.path(), &["checkout", "-q", "feature"]);
        run_test_git(temp.path(), &["rebase", &main_branch]);
        let new_hash = test_git_stdout(temp.path(), &["rev-parse", "HEAD"]);
        assert_ne!(old_hash, new_hash);

        let scan = scan_local_git_repository(temp.path(), None).unwrap();
        assert!(scan
            .commits
            .iter()
            .any(|commit| commit.commit_hash == new_hash));
        assert!(!scan
            .commits
            .iter()
            .any(|commit| commit.commit_hash == old_hash));
        let committed_metric_ids = scan
            .commits
            .iter()
            .enumerate()
            .map(|(index, commit)| {
                (
                    commit.deduplication_id.clone(),
                    format!("opaque-commit-{index}"),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let first = build_code_change_metrics(
            Utc::now(),
            "device-a",
            &[],
            std::slice::from_ref(&scan),
            &[],
            &committed_metric_ids,
            CoverageStatus::Unavailable,
        )
        .unwrap();
        let second = build_code_change_metrics(
            Utc::now(),
            "device-b",
            &[],
            &[scan],
            &[],
            &committed_metric_ids,
            CoverageStatus::Unavailable,
        )
        .unwrap();
        let first_ids = first
            .metrics
            .iter()
            .filter(|metric| metric.kind == CodeChangeMetricKind::Committed)
            .map(|metric| &metric.metric_id)
            .collect::<BTreeSet<_>>();
        let second_ids = second
            .metrics
            .iter()
            .filter(|metric| metric.kind == CodeChangeMetricKind::Committed)
            .map(|metric| &metric.metric_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(first_ids, second_ids);
    }

    #[test]
    fn attribution_is_exact_high_partial_medium_and_chronology_bounded() {
        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\nnew-d\nnew-e\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/repo"));
        let exact_file = GitFileChange {
            relative_path: PathBuf::from("src/lib.rs"),
            category: CodeCategory::Source,
            counts: CodeLineCounts::classified(CodeCategory::Source, 6, 2),
            added_line_fingerprints: trace
                .added_line_fingerprints
                .iter()
                .cloned()
                .chain([hash_text("human")])
                .collect(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let exact_scan = scan_with_file("/repo", "exact", "2026-08-05T10:00:00Z", exact_file);
        let exact = match_trace_edits_to_commits(&[trace.clone()], &[exact_scan]);
        assert_eq!(exact[0].confidence, AttributionConfidence::High);

        let mut partial_file = GitFileChange {
            relative_path: PathBuf::from("src/lib.rs"),
            category: CodeCategory::Source,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints[..4].to_vec(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let partial_scan = scan_with_file(
            "/repo",
            "partial",
            "2026-08-12T10:00:00Z",
            partial_file.clone(),
        );
        let partial = match_trace_edits_to_commits(&[trace.clone()], &[partial_scan]);
        assert_eq!(partial[0].confidence, AttributionConfidence::Medium);

        partial_file.relative_path = PathBuf::from("src/other.rs");
        let other_file_scan =
            scan_with_file("/repo", "other-file", "2026-08-12T10:00:00Z", partial_file);
        assert!(match_trace_edits_to_commits(&[trace.clone()], &[other_file_scan]).is_empty());

        let delayed_file = GitFileChange {
            relative_path: PathBuf::from("src/lib.rs"),
            category: CodeCategory::Source,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let delayed_scan = scan_with_file("/repo", "delayed", "2026-09-02T10:00:01Z", delayed_file);
        assert!(match_trace_edits_to_commits(&[trace], &[delayed_scan]).is_empty());
    }

    #[test]
    fn equally_strong_repeated_boilerplate_is_left_unattributed() {
        let source = SourceId("source".to_string());
        // Distinct enough to clear the attribution floor, so this exercises the
        // ambiguity guard rather than being dropped before it.
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "repeated-a\nrepeated-b\nrepeated-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/repo"));
        let file = GitFileChange {
            relative_path: trace.relative_path.clone(),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let first = scan_with_file("/repo", "first", "2026-08-02T10:00:00Z", file.clone());
        let second = scan_with_file("/repo", "second", "2026-08-03T10:00:00Z", file.clone());
        assert!(
            match_trace_edits_to_commits(&[trace.clone()], &[first.clone(), second]).is_empty(),
            "two equally strong candidates leave the edit unattributed"
        );
        assert_eq!(
            match_trace_edits_to_commits(&[trace], &[first]).len(),
            1,
            "the same edit does match when only one candidate exists"
        );
    }

    #[test]
    fn edits_too_trivial_to_identify_a_commit_are_not_attributed_at_any_confidence() {
        let source = SourceId("source".to_string());
        // A lone `}` reaches a perfect overlap against whichever commit happens
        // to touch the file, so it identifies no author and is withheld rather
        // than merely demoted to medium confidence.
        for (old_string, new_string) in [("", "}"), ("", "\n"), ("old", "new")] {
            let mut trace = parse_structured_edit(
                &context(&source),
                Path::new("src/lib.rs"),
                old_string,
                new_string,
                0,
            )
            .edits
            .remove(0);
            trace.repository_path = Some(PathBuf::from("/repo"));
            let file = GitFileChange {
                relative_path: trace.relative_path.clone(),
                category: trace.category,
                counts: trace.counts,
                added_line_fingerprints: trace.added_line_fingerprints.clone(),
                deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
            };
            let scan = scan_with_file("/repo", "trivial", "2026-08-02T10:00:00Z", file);

            assert!(
                match_trace_edits_to_commits(&[trace], &[scan]).is_empty(),
                "{old_string:?} -> {new_string:?} carries too little distinct content to attribute"
            );
        }
    }

    #[test]
    fn merge_replay_does_not_make_the_original_commit_match_ambiguous() {
        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/repo"));
        let file = GitFileChange {
            relative_path: trace.relative_path.clone(),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let original = scan_with_file(
            "/repo",
            "feature-commit",
            "2026-08-02T10:00:00Z",
            file.clone(),
        );
        let mut merge = scan_with_file("/repo", "merge-commit", "2026-08-03T10:00:00Z", file);
        merge.commits[0].parent_count = 2;
        let matched = match_trace_edits_to_commits(&[trace], &[original, merge]);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].commit_hash, "feature-commit");
    }

    #[test]
    fn attribution_normalizes_nested_project_paths_to_the_repository_root() {
        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/repo/packages/app"));
        let file = GitFileChange {
            relative_path: PathBuf::from("packages/app/src/lib.rs"),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let scan = scan_with_file("/repo", "nested", "2026-08-02T10:00:00Z", file);
        let matched = match_trace_edits_to_commits(&[trace.clone()], std::slice::from_ref(&scan));
        assert_eq!(matched.len(), 1);
        assert_eq!(
            trace_repository_hash(&trace, &[scan]),
            Some("repository".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn attribution_matches_a_symlinked_project_to_the_physical_repository_root() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new().unwrap();
        let repository_root = directory.path().join("repository");
        fs::create_dir_all(&repository_root).unwrap();
        let linked_root = directory.path().join("linked-repository");
        symlink(&repository_root, &linked_root).unwrap();

        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(linked_root);
        let file = GitFileChange {
            relative_path: PathBuf::from("src/lib.rs"),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let scan = scan_with_file(
            &repository_root.to_string_lossy(),
            "symlinked",
            "2026-08-02T10:00:00Z",
            file,
        );

        let matched = match_trace_edits_to_commits(&[trace.clone()], std::slice::from_ref(&scan));

        assert_eq!(matched.len(), 1);
        assert_eq!(
            trace_repository_hash(&trace, &[scan]),
            Some("repository".to_string())
        );
    }

    #[test]
    fn trace_repository_hash_uses_deepest_matching_repository_root() {
        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/repo/nested"));
        let mut parent = scan_with_file(
            "/repo",
            "parent",
            "2026-08-02T10:00:00Z",
            GitFileChange {
                relative_path: PathBuf::from("nested/src/lib.rs"),
                category: trace.category,
                counts: trace.counts,
                added_line_fingerprints: trace.added_line_fingerprints.clone(),
                deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
            },
        );
        parent.repository_hash = "parent-repository".to_string();
        let mut nested = scan_with_file(
            "/repo/nested",
            "nested",
            "2026-08-02T10:00:00Z",
            GitFileChange {
                relative_path: PathBuf::from("src/lib.rs"),
                category: trace.category,
                counts: trace.counts,
                added_line_fingerprints: trace.added_line_fingerprints.clone(),
                deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
            },
        );
        nested.repository_hash = "nested-repository".to_string();

        assert_eq!(
            trace_repository_hash(&trace, &[parent, nested]),
            Some("nested-repository".to_string())
        );
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

    #[test]
    fn a_hunk_before_any_file_header_is_not_charged_to_the_next_file() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "@@ -1 +1 @@\n-old\n+new\n\
             diff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -1 +1 @@\n-x\n+y\n",
        );

        assert_eq!(parsed.edits.len(), 1, "only the headed section is measured");
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/b.rs"));
        assert_eq!(parsed.edits[0].counts.source_additions, 1);
        assert_eq!(parsed.edits[0].counts.source_deletions, 1);
        assert_eq!(parsed.unsupported_sections, 1);
        assert_eq!(parsed.coverage, CoverageStatus::Partial);
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
    fn rebased_commits_keep_their_attribution_through_the_author_date() {
        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        // The edit is made a few hours before the work is first committed.
        trace.occurred_at = Some(
            DateTime::parse_from_rfc3339("2026-07-11T10:00:00Z")
                .unwrap()
                .into(),
        );
        trace.repository_path = Some(PathBuf::from("/repo"));
        let file = GitFileChange {
            relative_path: trace.relative_path.clone(),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        // The branch is rebased a month later, which resets the committer date
        // well past the matching window while the author date stays put.
        let mut scan = scan_with_file("/repo", "rebased", "2026-08-14T15:52:02Z", file);
        assert!(
            match_trace_edits_to_commits(&[trace.clone()], std::slice::from_ref(&scan)).is_empty(),
            "the committer date alone puts the commit outside the window"
        );

        scan.commits[0].authored_at = Some(
            DateTime::parse_from_rfc3339("2026-07-11T16:23:50Z")
                .unwrap()
                .into(),
        );
        let matched = match_trace_edits_to_commits(&[trace], std::slice::from_ref(&scan));

        assert_eq!(matched.len(), 1, "the author date rescues the attribution");
        assert_eq!(matched[0].confidence, AttributionConfidence::High);
    }

    #[test]
    fn edits_in_a_multi_repository_workspace_reach_the_nested_repository() {
        let source = SourceId("source".to_string());
        // The agent's project is a workspace folder; the Git repository is a
        // directory inside it, so the edit's path leads with that directory
        // while the commit records the path relative to the repository root.
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("AudioToolSwift/Parity/adapters/chatterbox.py"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/workspace"));
        let file = GitFileChange {
            relative_path: PathBuf::from("Parity/adapters/chatterbox.py"),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let scan = scan_with_file(
            "/workspace/AudioToolSwift",
            "nested",
            "2026-08-02T10:00:00Z",
            file,
        );

        let matched = match_trace_edits_to_commits(&[trace.clone()], std::slice::from_ref(&scan));

        assert_eq!(matched.len(), 1, "the nested repository owns this edit");
        assert_eq!(
            matched[0].relative_path,
            Path::new("Parity/adapters/chatterbox.py")
        );
        assert_eq!(
            trace_repository_hash(&trace, std::slice::from_ref(&scan)),
            Some("repository".to_string())
        );

        // A sibling repository in the same workspace must not claim it.
        let sibling = scan_with_file(
            "/workspace/ClearVoice",
            "sibling",
            "2026-08-02T10:00:00Z",
            GitFileChange {
                relative_path: PathBuf::from("Parity/adapters/chatterbox.py"),
                category: trace.category,
                counts: trace.counts,
                added_line_fingerprints: trace.added_line_fingerprints.clone(),
                deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
            },
        );
        assert_eq!(
            trace_repository_hash(&trace, std::slice::from_ref(&sibling)),
            None
        );
    }

    #[test]
    fn edits_outside_the_repository_do_not_inherit_its_identity() {
        let source = SourceId("source".to_string());
        let file = GitFileChange {
            relative_path: PathBuf::from("src/lib.rs"),
            category: CodeCategory::Source,
            counts: CodeLineCounts::classified(CodeCategory::Source, 3, 0),
            added_line_fingerprints: Vec::new(),
            deleted_line_fingerprints: Vec::new(),
        };
        let scan = scan_with_file("/repo", "commit", "2026-08-02T10:00:00Z", file);

        for outside in ["/etc/elsewhere.rs", "../outside/lib.rs", "../../lib.rs"] {
            let mut trace = parse_structured_edit(
                &context(&source),
                Path::new(outside),
                "old-a\nold-b\n",
                "new-a\nnew-b\nnew-c\n",
                0,
            )
            .edits
            .remove(0);
            trace.repository_path = Some(PathBuf::from("/repo"));

            assert_eq!(
                trace_repository_hash(&trace, std::slice::from_ref(&scan)),
                None,
                "{outside} is not inside the scanned repository"
            );
            assert!(
                match_trace_edits_to_commits(&[trace], std::slice::from_ref(&scan)).is_empty(),
                "{outside} must not be attributed to this repository"
            );
        }

        // A path genuinely inside the repository is still placed.
        let mut inside = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        inside.repository_path = Some(PathBuf::from("/repo"));
        assert_eq!(
            trace_repository_hash(&inside, std::slice::from_ref(&scan)),
            Some("repository".to_string())
        );
    }

    #[test]
    fn commitless_repositories_without_a_remote_do_not_share_one_identity() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        for repository in [&first, &second] {
            run_test_git(repository.path(), &["init", "-q"]);
            run_test_git(
                repository.path(),
                &["config", "user.email", "test@example.com"],
            );
            run_test_git(repository.path(), &["config", "user.name", "Test"]);
        }

        let first_scan = scan_local_git_repository(first.path(), None).unwrap();
        let second_scan = scan_local_git_repository(second.path(), None).unwrap();
        assert!(first_scan.commits.is_empty());
        assert_ne!(
            first_scan.repository_hash, second_scan.repository_hash,
            "unrelated repositories must not merge before their first commit"
        );

        // The first commit supplies a stable root, and the identity re-keys to
        // one that other devices can derive too.
        fs::write(first.path().join("main.rs"), "fn main() {}\n").unwrap();
        run_test_git(first.path(), &["add", "main.rs"]);
        run_test_git(first.path(), &["commit", "-qm", "initial"]);
        let committed = scan_local_git_repository(first.path(), None).unwrap();
        assert_ne!(committed.repository_hash, first_scan.repository_hash);
    }

    #[test]
    fn clock_skewed_commits_are_left_unmeasured_instead_of_poisoning_the_batch() {
        let temp = TempDir::new().unwrap();
        run_test_git(temp.path(), &["init", "-q"]);
        run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
        run_test_git(temp.path(), &["config", "user.name", "Test"]);
        commit_test_file(
            temp.path(),
            "now.rs",
            "now\n",
            "current",
            "test@example.com",
            Utc::now(),
        );
        commit_test_file(
            temp.path(),
            "skewed.rs",
            "skewed\n",
            "clock skewed",
            "test@example.com",
            Utc::now() + Duration::days(30),
        );

        let scan = scan_local_git_repository(temp.path(), None).unwrap();

        assert_eq!(scan.commits.len(), 1);
        assert!(scan
            .commits
            .iter()
            .flat_map(|commit| &commit.files)
            .all(|file| file.relative_path != Path::new("skewed.rs")));
        assert_eq!(scan.coverage, CoverageStatus::Partial);
    }

    #[test]
    fn agent_edits_are_published_per_day_project_and_repository() {
        let source = SourceId("source".to_string());
        let edit_at = |timestamp: &str, path: &str| {
            let mut context = context(&source);
            context.occurred_at = Some(DateTime::parse_from_rfc3339(timestamp).unwrap().into());
            parse_full_file_write(&context, Path::new(path), "one\ntwo\n", true)
                .edits
                .remove(0)
        };
        let edits = vec![
            edit_at("2026-08-01T10:00:00Z", "src/first.rs"),
            edit_at("2026-08-01T18:00:00Z", "src/second.rs"),
            edit_at("2026-08-02T10:00:00Z", "src/third.rs"),
        ];

        let metrics = build_code_change_metrics(
            DateTime::parse_from_rfc3339("2026-08-03T00:00:00Z")
                .unwrap()
                .into(),
            "device",
            &edits,
            &[],
            &[],
            &BTreeMap::new(),
            CoverageStatus::Complete,
        )
        .unwrap();

        let agent_edits = metrics
            .metrics
            .iter()
            .filter(|metric| metric.kind == CodeChangeMetricKind::AgentEdit)
            .collect::<Vec<_>>();
        assert_eq!(agent_edits.len(), 2, "one metric per observed day");
        let first_day = agent_edits
            .iter()
            .find(|metric| metric.day == NaiveDate::from_ymd_opt(2026, 8, 1).unwrap())
            .expect("first day");
        assert_eq!(first_day.counts.source_additions, 4);
        assert!(agent_edits
            .iter()
            .all(|metric| metric.metric_id.len() == 64));
    }

    #[test]
    fn future_dated_agent_edits_are_not_published() {
        let source = SourceId("source".to_string());
        let mut context = context(&source);
        context.occurred_at = Some(
            DateTime::parse_from_rfc3339("2027-01-01T00:00:00Z")
                .unwrap()
                .into(),
        );
        let edits = parse_full_file_write(&context, Path::new("src/lib.rs"), "one\n", true).edits;

        let metrics = build_code_change_metrics(
            DateTime::parse_from_rfc3339("2026-08-03T00:00:00Z")
                .unwrap()
                .into(),
            "device",
            &edits,
            &[],
            &[],
            &BTreeMap::new(),
            CoverageStatus::Complete,
        )
        .unwrap();

        assert!(metrics.metrics.is_empty());
        assert_eq!(
            metrics.trace_coverage,
            CoverageStatus::Partial,
            "an edit the build declined to publish is unmeasured churn"
        );
    }

    #[test]
    fn a_skipped_future_edit_downgrades_coverage_for_the_edits_that_are_published() {
        let source = SourceId("source".to_string());
        let edit_at = |timestamp: &str, path: &str| {
            let mut context = context(&source);
            context.occurred_at = Some(DateTime::parse_from_rfc3339(timestamp).unwrap().into());
            parse_full_file_write(&context, Path::new(path), "one\n", true)
                .edits
                .remove(0)
        };
        let edits = vec![
            edit_at("2026-08-01T10:00:00Z", "src/current.rs"),
            edit_at("2027-01-01T10:00:00Z", "src/skewed.rs"),
        ];

        let build = build_code_change_metrics(
            DateTime::parse_from_rfc3339("2026-08-03T00:00:00Z")
                .unwrap()
                .into(),
            "device",
            &edits,
            &[],
            &[],
            &BTreeMap::new(),
            CoverageStatus::Complete,
        )
        .unwrap();

        assert_eq!(build.metrics.len(), 1);
        assert_eq!(build.trace_coverage, CoverageStatus::Partial);
        assert!(
            build
                .metrics
                .iter()
                .all(|metric| metric.trace_coverage == CoverageStatus::Partial),
            "the surviving metrics must not claim they describe the period completely"
        );
    }

    /// Test view of repository attribution, resolving the canonical paths the
    /// production caller resolves once per refresh.
    fn trace_repository_hash(trace: &TraceEdit, scans: &[GitScan]) -> Option<String> {
        let scan_roots = canonical_scan_roots(scans);
        let canonical_repository_path = canonical_path(trace.repository_path.as_deref()?);
        repository_hash_for_trace(trace, scans, &scan_roots, &canonical_repository_path)
    }

    fn scan_with_file(
        repository_root: &str,
        commit_hash: &str,
        committed_at: &str,
        file: GitFileChange,
    ) -> GitScan {
        GitScan {
            repository_root: PathBuf::from(repository_root),
            repository_hash: "repository".to_string(),
            commits: vec![GitCommitChange {
                deduplication_id: format!("dedup-{commit_hash}"),
                repository_hash: "repository".to_string(),
                commit_hash: commit_hash.to_string(),
                committed_at: DateTime::parse_from_rfc3339(committed_at).unwrap().into(),
                authored_at: None,
                parent_count: 1,
                project_id: None,
                files: vec![file],
            }],
            coverage: CoverageStatus::Complete,
        }
    }

    fn run_test_git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn commit_test_file(
        path: &Path,
        relative_path: &str,
        contents: &str,
        message: &str,
        email: &str,
        committed_at: DateTime<Utc>,
    ) {
        fs::write(path.join(relative_path), contents).unwrap();
        run_test_git(path, &["add", relative_path]);
        let timestamp = committed_at.to_rfc3339();
        let status = Command::new("git")
            .current_dir(path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_AUTHOR_DATE", &timestamp)
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", email)
            .env("GIT_COMMITTER_DATE", timestamp)
            .args(["commit", "-qm", message])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn test_git_stdout(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }
}
