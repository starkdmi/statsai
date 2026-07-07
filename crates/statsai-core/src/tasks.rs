//! Task collection domain types and helpers for local rebuilding plus hosted sync snapshots.

use crate::{
    hash_text, Confidence, EventId, GitInfo, ProjectInfo, SourceId, SummaryId, UsageCounts,
};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeSet;

pub const TASK_SPAN_SCHEMA_VERSION: &str = "task_span.v1";
pub const WORK_ITEM_SCHEMA_VERSION: &str = "work_item.v1";
pub const TASK_VERIFICATION_SCHEMA_VERSION: &str = "task_verification.v2";

const GENERIC_PLACEHOLDER_EXACT: &[&str] = &[
    "no prompt",
    "single cell",
    "work item",
    "unresolved work item",
];

const PHATIC_TOKENS: &[&str] = &[
    "hi",
    "hello",
    "hey",
    "yes",
    "yeah",
    "yep",
    "ok",
    "okay",
    "thanks",
    "thank",
    "greetings",
    "morning",
    "afternoon",
    "lunch",
    "evening",
    "night",
];

const DIALOGUE_MANAGEMENT_TOKENS: &[&str] = &[
    "ask",
    "browser",
    "casual",
    "check",
    "continue",
    "current",
    "date",
    "details",
    "do",
    "go",
    "greet",
    "greeting",
    "greetings",
    "handle",
    "hello",
    "hi",
    "it",
    "on",
    "open",
    "proceed",
    "reply",
    "respond",
    "say",
    "nothing",
    "else",
    "to",
    "user",
];

const GENERIC_WORKFLOW_TOKENS: &[&str] = &[
    "conversation",
    "guideline",
    "guidelines",
    "instruction",
    "instructions",
    "list",
    "review",
    "session",
    "thread",
    "uncommitted",
    "changes",
    "change",
    "diff",
    "status",
    "branch",
    "branches",
    "commit",
    "commits",
    "history",
];

const SESSION_CONTROL_ACTION_TOKENS: &[&str] = &[
    "clear",
    "clearing",
    "cleared",
    "exit",
    "exits",
    "exited",
    "quit",
    "quits",
    "quitting",
    "switch",
    "switching",
    "switched",
];

const PROVIDER_PLACEHOLDER_TOKENS: &[&str] = &["codex", "opencode", "claude", "grok"];

const PROVIDER_PLACEHOLDER_NOUNS: &[&str] = &["session", "task"];

const LOW_SIGNAL_PREFIXES: &[&str] = &[
    "your account does not have access",
    "api error:",
    "quota exhausted",
    "model switch due to quota exhaustion",
    "automation:",
    "the user interrupted the previous turn on purpose.",
    "the following is the codex agent history",
    "you are acting as a reviewer for a proposed code change made by another engineer",
    "new session -",
    "<environment_context>",
    "<codex_internal_context",
    "transcript delta start",
    "transcript delta end",
    "skills available",
    "how to use skills",
    "proactiveness strike a balance",
    "# files mentioned by the user",
    "files mentioned by the user",
    "# agents.md instructions for",
    "agents.md instructions for",
    "project-agnostic instructions for",
    "claude opus 4.5 guidelines",
    "last run:",
    "chunk id:",
    "wall time:",
    "process exited with code",
    "process running with session id",
    "original token count:",
    "success. updated the following files:",
    "updated the following files:",
    "output:",
    "usage:",
    "tokens used:",
    "cargo run -p",
    "running `target/debug/",
    "command line invocation:",
    "total output lines:",
    "reviewed codex session id:",
    "continue working toward the active thread goal.",
    "the objective below is user-provided data.",
    "tool web_search call:",
    "tool web_search result:",
    "tool apply_patch call:",
    "tool apply_patch result:",
    "coverage=",
    "f1_overlap=",
    "f1@",
    "avg_tiou=",
    "mae=",
    "titlef1=",
    "cider=",
    "score=",
    "fatal:",
    "single cell",
    "with repeats",
];

const LOW_SIGNAL_CONTAINS: &[&str] = &[
    "approval assessment",
    "@explore subagent",
    "@image subagent",
    "@build subagent",
    "review changes [commit|branch|pr]",
    "my request for codex",
    "my request for claude",
    "my request for opencode",
    "attachments/",
    "plugin://",
    "<cwd>",
    "<current_date>",
    "<timezone>",
    "skill.md",
    "a skill is a set of local instructions",
    "::code-comment{title=",
    "tool exec_command result",
    "tool write_stdin result",
    "tool exec_command",
    "tool write_stdin",
    "tool web_search call",
    "tool web_search result",
    "tool apply_patch call",
    "tool apply_patch result",
    "%%bash",
    "transcript delta start",
    "the list above is the skills available in this session",
    "skill bodies live on disk at the listed paths",
    "project-agnostic instructions for claude opus",
    "any running unified exec processes may still be running in the background",
    "automation id:",
    "$codex_home/automations/",
];

const PROMPT_SCAFFOLD_PREFIXES: &[&str] = &[
    "continue working toward the active thread goal",
    "the objective below is user-provided data",
    "continuation behavior:",
    "work from evidence:",
    "completion audit:",
    "blocked audit:",
    "budget:",
];

const PROMPT_SCAFFOLD_CONTAINS: &[&str] = &[
    "your training data",
    "before writing code",
    "read the relevant guide",
    "follow the instructions",
    "running unified exec processes",
    "tools/commands were aborted",
    "partially executed",
    "active thread goal",
    "objective below",
    "treat it as the task to pursue",
    "continuation behavior",
    "completion audit",
    "blocked audit",
    "work from evidence",
];

const REQUEST_MARKERS: &[&str] = &[
    "My request for Codex:",
    "My request for Claude Code:",
    "My request for Claude:",
    "My request for OpenCode:",
];

const META_WRAPPER_TOKENS: &[&str] = &[
    "implement",
    "implementation",
    "plan",
    "summary",
    "request",
    "objective",
    "goal",
    "task",
    "please",
];

const WRAPPER_FILLER_TOKENS: &[&str] = &["following", "below", "above", "current", "actual"];

const ABSTRACT_TASK_OBJECT_TOKENS: &[&str] = &[
    "goal",
    "goals",
    "issue",
    "issues",
    "item",
    "items",
    "objective",
    "objectives",
    "problem",
    "problems",
    "request",
    "requests",
    "result",
    "results",
    "task",
    "tasks",
    "thing",
    "things",
    "work",
];

const ABSTRACT_TASK_MODIFIER_TOKENS: &[&str] = &[
    "again",
    "all",
    "better",
    "best",
    "correct",
    "correctly",
    "existing",
    "fully",
    "more",
    "needed",
    "necessary",
    "proper",
    "properly",
    "real",
    "satisfy",
];

const DEICTIC_FOLLOWUP_TOKENS: &[&str] = &[
    "all",
    "anything",
    "everything",
    "it",
    "same",
    "something",
    "that",
    "them",
    "these",
    "this",
    "those",
];

const SHELL_ACTION_TOKENS: &[&str] = &[
    "build", "check", "compile", "deploy", "dev", "fmt", "format", "install", "lint", "preview",
    "run", "serve", "start", "test", "tests",
];

const COMMAND_TOKENS: &[&str] = &[
    "bash",
    "cargo",
    "cmake",
    "docker",
    "eslint",
    "git",
    "kubectl",
    "make",
    "node",
    "npm",
    "pip",
    "pnpm",
    "python",
    "python3",
    "sh",
    "swift",
    "tsc",
    "wrangler",
    "xcodebuild",
    "yarn",
    "zsh",
];

const INSTRUCTIONAL_LEAD_TOKENS: &[&str] = &[
    "if", "that", "the", "these", "this", "those", "unless", "when", "your",
];

const INSTRUCTIONAL_MODAL_TOKENS: &[&str] = &[
    "choose", "follow", "must", "need", "needs", "read", "required", "requires", "should", "use",
];

const INSTRUCTIONAL_CONTEXT_TOKENS: &[&str] = &[
    "api",
    "apis",
    "audit",
    "completion",
    "conventions",
    "guide",
    "instruction",
    "instructions",
    "objective",
    "policy",
    "prompt",
    "skill",
    "skills",
    "training",
    "version",
    "workflow",
];

const BRANCH_PREFIXES: &[&str] = &[
    "feature", "feat", "fix", "bugfix", "hotfix", "chore", "task", "story", "ticket",
];

const TITLE_TOPIC_STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "any",
    "at",
    "are",
    "as",
    "be",
    "build",
    "but",
    "can",
    "change",
    "changes",
    "check",
    "could",
    "debug",
    "did",
    "do",
    "does",
    "for",
    "from",
    "get",
    "had",
    "has",
    "have",
    "how",
    "i",
    "in",
    "into",
    "investigate",
    "is",
    "it",
    "its",
    "just",
    "lets",
    "let",
    "look",
    "maybe",
    "me",
    "my",
    "need",
    "now",
    "of",
    "okay",
    "ok",
    "or",
    "our",
    "please",
    "really",
    "results",
    "of",
    "on",
    "show",
    "so",
    "tell",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "to",
    "too",
    "review",
    "run",
    "same",
    "show",
    "still",
    "run",
    "task",
    "tell",
    "than",
    "there",
    "try",
    "update",
    "us",
    "use",
    "want",
    "was",
    "we",
    "what",
    "when",
    "where",
    "which",
    "why",
    "with",
    "would",
    "yes",
    "you",
    "your",
];

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

#[must_use]
pub fn normalize_task_title(value: &str) -> String {
    let cleaned = clean_task_text(value).unwrap_or_else(|| value.trim().to_string());
    let cleaned = polish_task_title_candidate(&cleaned);
    let mut normalized = String::with_capacity(value.len());
    let mut previous_was_space = false;
    for character in cleaned.chars() {
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
            previous_was_space = false;
            continue;
        }
        if character.is_whitespace() || matches!(character, '-' | '_' | '/' | ':' | '.') {
            if !previous_was_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            previous_was_space = true;
        }
    }
    normalized.trim().to_string()
}

#[must_use]
pub fn summarize_task_text(value: Option<&str>, width: usize) -> Option<String> {
    truncate_task_text(clean_task_text(value?)?, width)
}

#[must_use]
pub fn task_title_from_prompt(value: Option<&str>) -> Option<String> {
    task_preview_from_prompt(value, 90)
}

const TASK_PREVIEW_MAX_INPUT_BYTES: usize = 24 * 1024;
const TASK_PREVIEW_MAX_INPUT_LINES: usize = 128;
const TASK_PREVIEW_FAST_SCAN_BYTES: usize = 16 * 1024;
const TASK_PREVIEW_FAST_SCAN_LINES: usize = 128;

#[must_use]
pub fn task_preview_from_prompt(value: Option<&str>, width: usize) -> Option<String> {
    let raw = value?;
    if let Some(candidate) = fast_structured_task_preview_candidate(raw) {
        return truncate_task_text(candidate, width);
    }
    let bounded = bounded_task_preview_input(raw);
    select_task_prompt_candidate(bounded.as_ref())
        .and_then(|candidate| truncate_task_text(candidate, width))
}

fn bounded_task_preview_input(raw: &str) -> Cow<'_, str> {
    if raw.len() <= TASK_PREVIEW_MAX_INPUT_BYTES {
        return Cow::Borrowed(raw);
    }

    let mut excerpt = String::new();
    let mut used_bytes = 0usize;
    let mut used_lines = 0usize;

    for line in raw.lines() {
        if used_lines >= TASK_PREVIEW_MAX_INPUT_LINES || used_bytes >= TASK_PREVIEW_MAX_INPUT_BYTES
        {
            break;
        }

        let line_bytes = line.len();
        let remaining_bytes = TASK_PREVIEW_MAX_INPUT_BYTES.saturating_sub(used_bytes);
        let fits_with_newline = line_bytes.saturating_add(1) <= remaining_bytes;
        if !fits_with_newline {
            if remaining_bytes == 0 {
                break;
            }
            excerpt.push_str(prefix_at_char_boundary(line, remaining_bytes));
            break;
        }

        excerpt.push_str(line);
        excerpt.push('\n');
        used_bytes = used_bytes.saturating_add(line_bytes).saturating_add(1);
        used_lines = used_lines.saturating_add(1);
    }

    if excerpt.is_empty() {
        return Cow::Borrowed(prefix_at_char_boundary(raw, TASK_PREVIEW_MAX_INPUT_BYTES));
    }

    Cow::Owned(excerpt)
}

fn prefix_at_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn fast_structured_task_preview_candidate(raw: &str) -> Option<String> {
    let window = prefix_at_char_boundary(raw, TASK_PREVIEW_FAST_SCAN_BYTES);
    for line in window.lines().take(TASK_PREVIEW_FAST_SCAN_LINES) {
        let sentence_breaks = [". ", "? ", "! "]
            .into_iter()
            .map(|marker| line.matches(marker).count())
            .sum::<usize>();
        if sentence_breaks > 1 && !line.trim_start().starts_with('#') {
            continue;
        }
        let Some(candidate) = clean_task_line(line) else {
            continue;
        };
        let mut polished = polish_task_title_candidate(&candidate);
        if let Some(stripped) = strip_plain_role_prefix(&polished) {
            polished = polish_task_title_candidate(&stripped);
        }
        let normalized = basic_normalize_phrase(&polished);
        let token_count = normalized.split_whitespace().count();
        if polished.is_empty()
            || polished.len() > 160
            || token_count > 18
            || task_title_is_generic(Some(polished.as_str()))
            || task_title_is_weak_signal(Some(polished.as_str()))
            || task_scaffolding_line(&polished)
            || looks_like_metric_result_stub(line, &polished)
            || (looks_like_statemental_heading(&polished) && !has_explicit_task_intent(&polished))
            || normalized.starts_with("transcript start")
            || normalized.starts_with("transcript end")
        {
            continue;
        }
        return Some(polished);
    }

    None
}

#[must_use]
pub fn task_title_signal_score(value: Option<&str>) -> i32 {
    let Some(raw) = value else {
        return -100;
    };
    if looks_like_sensitive_locator_dump(raw) {
        return -100;
    }
    let Some(value) = clean_task_text(raw) else {
        return -100;
    };
    let value = polish_task_title_candidate(&value);
    if value.is_empty() {
        return -100;
    }
    if looks_like_metric_result_stub(raw, &value) {
        return -24;
    }

    let normalized = basic_normalize_phrase(&value);
    if normalized.is_empty() {
        return -100;
    }

    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    let content_token_count = tokens
        .iter()
        .filter(|token| !TITLE_TOPIC_STOP_WORDS.contains(token))
        .count();
    let topic_token_count = title_topic_tokens(&value).len();
    let pathlike_token_count = value
        .split_whitespace()
        .filter(|token| token.contains('/') || token.contains('\\'))
        .count();
    let flag_count = value
        .split_whitespace()
        .filter(|token| token.starts_with("--"))
        .count();
    let mut score = 0;

    if task_title_is_generic(Some(value.as_str())) {
        score -= 12;
    } else {
        score += 5;
    }
    if has_explicit_task_intent(&value) {
        score += 6;
    }
    if starts_with_task_verb(&normalized) {
        score += 4;
    }
    if looks_like_command_or_output_title(&value) {
        score -= 12;
    }
    if looks_like_instructional_preamble_title(&value) {
        score -= 10;
    }
    score += match tokens.len() {
        3..=10 => 6,
        2..=14 => 3,
        15..=22 => -2,
        0..=1 => -8,
        _ => -5,
    };
    score += match content_token_count {
        2..=8 => 4,
        9..=14 => 2,
        0..=1 => -5,
        _ => 0,
    };
    score += match topic_token_count {
        2..=6 => 4,
        7..=10 => 2,
        0..=1 => -4,
        _ => 0,
    };
    if pathlike_token_count >= 2 {
        score -= 8;
    } else if pathlike_token_count == 1 && flag_count >= 1 {
        score -= 5;
    }
    if value.ends_with('?') {
        score -= 2;
    }

    score
}

#[must_use]
pub fn task_title_is_generic(value: Option<&str>) -> bool {
    let Some(raw) = value else {
        return true;
    };
    if looks_like_sensitive_locator_dump(raw) {
        return true;
    }
    let Some(value) = clean_task_text(raw) else {
        return true;
    };
    let value = polish_task_title_candidate(&value);
    if value.is_empty() {
        return true;
    }
    if looks_like_metric_result_stub(raw, &value) {
        return true;
    }
    let lowercase = value.to_ascii_lowercase();
    if looks_like_sensitive_locator_dump(&lowercase) {
        return true;
    }
    let normalized = basic_normalize_phrase(&value);
    if GENERIC_PLACEHOLDER_EXACT.contains(&normalized.as_str()) {
        return true;
    }
    if looks_like_short_dialogue_management_title(&value) {
        return true;
    }
    if looks_like_provider_placeholder_title(&value) {
        return true;
    }
    if looks_like_meta_wrapper_title(&value) {
        return true;
    }
    if looks_like_presentational_wrapper_title(&value) {
        return true;
    }
    if looks_like_review_guidance_title(&value) {
        return true;
    }
    if looks_like_generic_workflow_title(&value) {
        return true;
    }
    if looks_like_meta_conversation_title(&value) {
        return true;
    }
    if looks_like_abstract_followup_title(&value) {
        return true;
    }
    if looks_like_abstract_objective_title(&value) {
        return true;
    }
    if looks_like_structured_key_value_title(&value) {
        return true;
    }
    if looks_like_path_stub_title(&value) {
        return true;
    }
    if normalized_matches_prefixes(&normalized, LOW_SIGNAL_PREFIXES) {
        return true;
    }
    if normalized_contains_fragments(&normalized, LOW_SIGNAL_CONTAINS) {
        return true;
    }
    if looks_like_command_or_output_title(&value) {
        return true;
    }
    looks_like_instructional_preamble_title(&value)
}

#[must_use]
pub fn task_title_is_weak_signal(value: Option<&str>) -> bool {
    let Some(value) = value.and_then(clean_task_text) else {
        return true;
    };
    let value = polish_task_title_candidate(&value);
    if value.is_empty() {
        return true;
    }
    if task_title_is_generic(Some(value.as_str())) {
        return true;
    }
    let normalized = normalize_task_title(&value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    (tokens.len() == 1
        && tokens[0].len() <= 12
        && tokens[0]
            .chars()
            .all(|character| character.is_ascii_lowercase()))
        || looks_like_locator_stub(&normalized)
        || looks_like_short_meta_instruction(&value)
        || task_title_signal_score(Some(value.as_str())) < 5
}

#[must_use]
pub fn task_title_is_session_meta(value: Option<&str>) -> bool {
    let Some(value) = value.and_then(clean_task_text) else {
        return false;
    };
    let value = polish_task_title_candidate(&value);
    if value.is_empty() {
        return false;
    }
    looks_like_session_control_meta_title(&value)
}

fn normalized_title_tokens(value: &str) -> Vec<String> {
    normalize_task_title(value)
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect()
}

fn basic_normalize_phrase(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut previous_was_space = false;
    for character in value.chars() {
        if character.is_alphanumeric() {
            for lowercase in character.to_lowercase() {
                normalized.push(lowercase);
            }
            previous_was_space = false;
            continue;
        }
        if !previous_was_space && !normalized.is_empty() {
            normalized.push(' ');
        }
        previous_was_space = true;
    }
    normalized.trim().to_string()
}

fn normalized_matches_prefixes(value: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| {
        let normalized_phrase = basic_normalize_phrase(phrase);
        !normalized_phrase.is_empty() && value.starts_with(&normalized_phrase)
    })
}

fn normalized_contains_fragments(value: &str, fragments: &[&str]) -> bool {
    fragments.iter().any(|fragment| {
        let normalized_fragment = basic_normalize_phrase(fragment);
        !normalized_fragment.is_empty() && value.contains(&normalized_fragment)
    })
}

fn starts_with_task_verb(normalized: &str) -> bool {
    normalized
        .split_whitespace()
        .next()
        .is_some_and(has_explicit_task_intent)
}

fn looks_like_short_dialogue_management_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.is_empty() || tokens.len() > 4 {
        return false;
    }
    let mut saw_signal = false;
    for token in tokens {
        if TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()) {
            continue;
        }
        if PHATIC_TOKENS.contains(&token.as_str())
            || DIALOGUE_MANAGEMENT_TOKENS.contains(&token.as_str())
        {
            saw_signal = true;
            continue;
        }
        return false;
    }
    saw_signal
}

fn looks_like_short_meta_instruction(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.is_empty() || tokens.len() > 4 {
        return false;
    }
    let mut saw_signal = false;
    for token in tokens {
        if TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()) {
            continue;
        }
        if DIALOGUE_MANAGEMENT_TOKENS.contains(&token.as_str()) {
            saw_signal = true;
            continue;
        }
        return false;
    }
    saw_signal
}

fn looks_like_provider_placeholder_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    matches!(
        tokens.as_slice(),
        [provider, noun]
            if PROVIDER_PLACEHOLDER_TOKENS.contains(&provider.as_str())
                && PROVIDER_PLACEHOLDER_NOUNS.contains(&noun.as_str())
    )
}

fn looks_like_meta_wrapper_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.is_empty() || tokens.len() > 5 {
        return false;
    }
    let mut saw_signal = false;
    for token in tokens {
        if TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()) {
            continue;
        }
        if META_WRAPPER_TOKENS.contains(&token.as_str())
            || WRAPPER_FILLER_TOKENS.contains(&token.as_str())
        {
            saw_signal = true;
            continue;
        }
        return false;
    }
    saw_signal
}

fn looks_like_review_guidance_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.len() < 2 || tokens.len() > 4 {
        return false;
    }
    let mut has_review = false;
    let mut has_guidance = false;
    for token in tokens {
        match token.as_str() {
            "code" => {}
            "review" => has_review = true,
            "guideline" | "guidelines" => has_guidance = true,
            token if TITLE_TOPIC_STOP_WORDS.contains(&token) => {}
            _ => return false,
        }
    }
    has_review && has_guidance
}

fn looks_like_presentational_wrapper_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    looks_like_presentational_wrapper_clause(&normalized)
        || normalized
            .split(" and ")
            .collect::<Vec<_>>()
            .as_slice()
            .iter()
            .copied()
            .all(looks_like_presentational_wrapper_clause)
}

fn looks_like_presentational_wrapper_clause(normalized: &str) -> bool {
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    let tail = match tokens.as_slice() {
        ["here", "is", tail @ ..]
        | ["here", "are", tail @ ..]
        | ["there", "is", tail @ ..]
        | ["there", "are", tail @ ..]
        | ["this", "is", tail @ ..]
        | ["these", "are", tail @ ..] => tail,
        _ => return false,
    };
    if tail.is_empty() || tail.len() > 6 {
        return false;
    }
    let topic_token_count = title_topic_tokens(&tail.join(" ")).len();
    let artifact_token_count = tail
        .iter()
        .filter(|token| {
            GENERIC_WORKFLOW_TOKENS.contains(token)
                || META_WRAPPER_TOKENS.contains(token)
                || matches!(
                    **token,
                    "code"
                        | "diff"
                        | "output"
                        | "report"
                        | "reports"
                        | "result"
                        | "results"
                        | "test"
                        | "tests"
                        | "case"
                        | "cases"
                )
        })
        .count();
    topic_token_count <= 1 || artifact_token_count >= tail.len().saturating_sub(1)
}

fn looks_like_abstract_followup_title(value: &str) -> bool {
    let content_tokens = normalized_content_tokens(value);
    if content_tokens.len() < 2 || content_tokens.len() > 6 {
        return false;
    }
    if !content_tokens
        .first()
        .is_some_and(|token| is_task_verb_token(token))
    {
        return false;
    }
    let concrete_count = content_tokens
        .iter()
        .filter(|token| is_concrete_task_topic_token(token))
        .count();
    let abstract_followup_count = content_tokens
        .iter()
        .skip(1)
        .filter(|token| {
            DEICTIC_FOLLOWUP_TOKENS.contains(&token.as_str())
                || ABSTRACT_TASK_OBJECT_TOKENS.contains(&token.as_str())
                || ABSTRACT_TASK_MODIFIER_TOKENS.contains(&token.as_str())
                || WRAPPER_FILLER_TOKENS.contains(&token.as_str())
        })
        .count();
    concrete_count == 0 && abstract_followup_count >= content_tokens.len().saturating_sub(1)
}

fn looks_like_abstract_objective_title(value: &str) -> bool {
    let content_tokens = normalized_content_tokens(value);
    if content_tokens.len() < 4 || content_tokens.len() > 14 {
        return false;
    }
    let verb_count = content_tokens
        .iter()
        .filter(|token| is_task_verb_token(token))
        .count();
    let abstract_count = content_tokens
        .iter()
        .filter(|token| {
            ABSTRACT_TASK_OBJECT_TOKENS.contains(&token.as_str())
                || ABSTRACT_TASK_MODIFIER_TOKENS.contains(&token.as_str())
                || WRAPPER_FILLER_TOKENS.contains(&token.as_str())
        })
        .count();
    let concrete_count = content_tokens
        .iter()
        .filter(|token| is_concrete_task_topic_token(token))
        .count();
    verb_count >= 2 && abstract_count >= 2 && concrete_count == 0
}

fn looks_like_generic_workflow_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.len() < 2 || tokens.len() > 4 {
        return false;
    }
    let mut saw_signal = false;
    for token in tokens {
        if TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()) {
            continue;
        }
        if GENERIC_WORKFLOW_TOKENS.contains(&token.as_str()) {
            saw_signal = true;
            continue;
        }
        return false;
    }
    saw_signal
}

fn looks_like_meta_conversation_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.is_empty() || tokens.len() > 8 {
        return false;
    }
    let has_continue = tokens
        .iter()
        .any(|token| matches!(token.as_str(), "continue" | "resume"));
    let has_conversation_boundary = tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "conversation" | "session" | "thread" | "review"
        )
    });
    has_continue && has_conversation_boundary
}

fn looks_like_session_control_meta_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.is_empty() || tokens.len() > 6 {
        return false;
    }
    let content_tokens = tokens
        .iter()
        .filter(|token| !TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()))
        .collect::<Vec<_>>();
    let has_action = content_tokens
        .iter()
        .any(|token| SESSION_CONTROL_ACTION_TOKENS.contains(&token.as_str()));
    let has_boundary_object = content_tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "conversation" | "history" | "session" | "cli"
        )
    });
    if has_action && has_boundary_object {
        return true;
    }
    content_tokens.iter().any(|token| token.as_str() == "model")
        && content_tokens
            .iter()
            .any(|token| matches!(token.as_str(), "switch" | "switching" | "switched"))
        && content_tokens.iter().all(|token| {
            matches!(
                token.as_str(),
                "model"
                    | "switch"
                    | "switching"
                    | "switched"
                    | "quick"
                    | "exit"
                    | "exits"
                    | "exited"
                    | "quit"
                    | "quits"
                    | "quitting"
            )
        })
}

fn looks_like_command_or_output_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    if normalized.is_empty() {
        return false;
    }
    if looks_like_test_harness_output_title(value)
        || looks_like_bracketed_log_prefix_title(value)
        || looks_like_package_version_banner_title(value)
        || looks_like_progress_measurement_title(value)
        || looks_like_settings_banner_title(value)
        || looks_like_git_ref_review_title(value)
        || looks_like_shell_invocation_title(value)
        || looks_like_build_status_title(value)
        || looks_like_warning_banner_title(value)
        || looks_like_structured_key_value_title(value)
    {
        return true;
    }
    if normalized_matches_prefixes(
        &normalized,
        &[
            "command line invocation",
            "fatal",
            "total output lines",
            "process exited with code",
            "process running with session id",
            "reviewed codex session id",
            "running target debug",
            "blocking waiting for file lock",
        ],
    ) {
        return true;
    }

    let command_token_count = normalized
        .split_whitespace()
        .filter(|token| COMMAND_TOKENS.contains(token))
        .count();
    let flag_count = value
        .split_whitespace()
        .filter(|token| token.starts_with("--"))
        .count();
    let pathlike_token_count = value
        .split_whitespace()
        .filter(|token| token.contains('/') || token.contains('\\'))
        .count();
    let filelike_token_count = value
        .split_whitespace()
        .filter(|token| token.contains('.') && token.len() > 4)
        .count();
    let banner_char_count = value
        .chars()
        .filter(|character| matches!(character, '─' | '│' | '┌' | '┐' | '└' | '┘' | '⛅'))
        .count();

    banner_char_count >= 3
        || normalized.contains("update available")
        || (command_token_count >= 2 && (flag_count >= 1 || pathlike_token_count >= 1))
        || (pathlike_token_count >= 2 && command_token_count >= 1)
        || (filelike_token_count >= 2 && command_token_count >= 1)
        || flag_count >= 3
}

fn looks_like_bracketed_log_prefix_title(value: &str) -> bool {
    let trimmed = value.trim();
    let Some(rest) = trimmed.strip_prefix('[') else {
        return false;
    };
    let Some((label, remainder)) = rest.split_once(']') else {
        return false;
    };
    let normalized_label = basic_normalize_phrase(label);
    let label_tokens = normalized_label.split_whitespace().collect::<Vec<_>>();
    if label_tokens.is_empty() || label_tokens.len() > 2 {
        return false;
    }
    let is_log_label = label_tokens.iter().all(|token| {
        matches!(
            *token,
            "debug" | "info" | "warn" | "warning" | "error" | "trace" | "notice"
        )
    });
    is_log_label && !remainder.trim().is_empty()
}

fn looks_like_progress_measurement_title(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || basic_has_explicit_task_intent(trimmed) {
        return false;
    }
    let lowercase = trimmed.to_ascii_lowercase();
    let normalized = basic_normalize_phrase(trimmed);
    let has_timer = lowercase.contains("[00:")
        || lowercase.contains("[0:")
        || lowercase.contains("runtime:")
        || normalized.starts_with("total runtime")
        || normalized.starts_with("elapsed time");
    let has_rate = [
        "examples/s",
        "example/s",
        "steps/s",
        "step/s",
        "it/s",
        "tok/s",
        "tokens/s",
        "items/s",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker));
    has_timer && (has_rate || normalized.starts_with("total runtime"))
}

fn looks_like_shell_invocation_title(value: &str) -> bool {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() || tokens.len() > 5 {
        return false;
    }
    let first = normalize_shell_token(tokens[0]);
    let first_is_command =
        COMMAND_TOKENS.contains(&first.as_str()) || looks_like_package_script_token(&first);
    if !first_is_command {
        return false;
    }
    tokens.iter().skip(1).all(|token| {
        let normalized = normalize_shell_token(token);
        !normalized.is_empty()
            && (SHELL_ACTION_TOKENS.contains(&normalized.as_str())
                || COMMAND_TOKENS.contains(&normalized.as_str())
                || normalized.starts_with('-')
                || normalized
                    .chars()
                    .all(|character| character.is_ascii_digit()))
    })
}

fn normalize_shell_token(value: &str) -> String {
    value
        .trim_matches(|character: char| {
            matches!(
                character,
                ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']'
            )
        })
        .to_ascii_lowercase()
}

fn looks_like_package_script_token(value: &str) -> bool {
    let Some((name, suffix)) = value.rsplit_once('@') else {
        return false;
    };
    !name.is_empty()
        && suffix.contains('.')
        && suffix.chars().any(|character| character.is_ascii_digit())
}

fn looks_like_build_status_title(value: &str) -> bool {
    let trimmed = value.trim();
    let lowercase = trimmed.to_ascii_lowercase();
    lowercase.starts_with("blocking waiting for file lock")
        || lowercase.starts_with("compiling ")
        || lowercase.starts_with("finished ")
        || lowercase.starts_with("running `target/")
        || lowercase.starts_with("running target/")
}

fn looks_like_warning_banner_title(value: &str) -> bool {
    let trimmed = value.trim_start();
    if trimmed.is_empty() || basic_has_explicit_task_intent(trimmed) {
        return false;
    }
    let normalized = basic_normalize_phrase(trimmed);
    let token_count = normalized.split_whitespace().count();
    token_count <= 18
        && (trimmed.starts_with('⚠')
            || normalized.starts_with("warning")
            || normalized.starts_with("error"))
}

fn looks_like_test_harness_output_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    normalized.starts_with("test case ")
        && (normalized.contains(" failed ")
            || normalized.contains(" passed ")
            || value.contains("-["))
}

fn looks_like_package_version_banner_title(value: &str) -> bool {
    let mut tokens = value.split_whitespace();
    let Some(first_token) = tokens.next() else {
        return false;
    };
    if !first_token.starts_with('@')
        || first_token.matches('@').count() < 2
        || !first_token
            .chars()
            .any(|character| character.is_ascii_digit())
    {
        return false;
    }
    let remaining = tokens
        .map(|token| {
            token
                .trim_matches(|character: char| {
                    matches!(
                        character,
                        ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']'
                    )
                })
                .to_ascii_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    !remaining.is_empty()
        && remaining.len() <= 3
        && remaining.iter().all(|token| {
            COMMAND_TOKENS.contains(&token.as_str())
                || matches!(
                    token.as_str(),
                    "build" | "deploy" | "dev" | "run" | "start" | "test"
                )
        })
}

fn looks_like_settings_banner_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    tokens.len() <= 8
        && normalized.contains("command line")
        && tokens.iter().any(|token| {
            matches!(
                *token,
                "setting" | "settings" | "configuration" | "configurations"
            )
        })
}

fn looks_like_git_ref_review_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    if tokens.len() < 4 || tokens.len() > 10 {
        return false;
    }
    let workflow_token_count = tokens
        .iter()
        .filter(|token| {
            GENERIC_WORKFLOW_TOKENS.contains(token)
                || matches!(**token, "against" | "between" | "compare")
        })
        .count();
    let git_ref_token_count = tokens
        .iter()
        .filter(|token| looks_like_git_ref_token(token))
        .count();
    workflow_token_count >= 2
        && git_ref_token_count >= 2
        && !tokens.iter().any(|token| {
            matches!(
                token,
                &"fix" | &"implement" | &"track" | &"debug" | &"investigate"
            )
        })
}

fn looks_like_git_ref_token(token: &str) -> bool {
    matches!(
        token,
        "head" | "main" | "master" | "origin" | "upstream" | "develop" | "development" | "dev"
    ) || (token.contains('/')
        && token.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '/')
        }))
}

fn looks_like_instructional_preamble_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    if tokens.len() < 6 {
        return false;
    }

    let starts_with_instructional_lead = tokens
        .first()
        .is_some_and(|token| INSTRUCTIONAL_LEAD_TOKENS.contains(token));
    let has_modal = tokens
        .iter()
        .any(|token| INSTRUCTIONAL_MODAL_TOKENS.contains(token));
    let has_context = tokens
        .iter()
        .any(|token| INSTRUCTIONAL_CONTEXT_TOKENS.contains(token));

    (starts_with_instructional_lead && (has_modal || has_context))
        || normalized_contains_fragments(
            &normalized,
            &[
                "your training data",
                "read the relevant guide",
                "follow the instructions",
                "breaking changes",
            ],
        )
}

fn looks_like_structured_key_value_title(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let first = trimmed.chars().next();
    if !matches!(first, Some('"') | Some('{') | Some('[')) {
        return false;
    }
    let normalized = basic_normalize_phrase(trimmed);
    let token_count = normalized.split_whitespace().count();
    token_count <= 12 && trimmed.matches("\":").count() >= 1 && trimmed.matches('"').count() >= 4
}

fn looks_like_path_stub_title(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || basic_has_explicit_task_intent(trimmed) {
        return false;
    }
    let tokens = trimmed.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() || tokens.len() > 3 {
        return false;
    }
    tokens.iter().all(|token| looks_like_pathish_token(token))
}

fn looks_like_pathish_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|character: char| {
        matches!(
            character,
            ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']'
        )
    });
    if trimmed.len() < 4 {
        return false;
    }
    let has_separator = trimmed.contains('/') || trimmed.contains('\\');
    let has_extension = trimmed.rsplit_once('.').is_some_and(|(_, suffix)| {
        !suffix.is_empty()
            && suffix.len() <= 12
            && suffix
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
    });
    (has_separator || trimmed.ends_with('/') || has_extension)
        && trimmed.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '/' | '\\' | '.' | '_' | '-' | '~')
        })
}

fn clean_task_text(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let mut candidate = strip_request_wrapper(value);
    if let Some(extracted) = extract_structured_task_signal(&candidate) {
        candidate = extracted;
    }
    candidate = expand_inline_markdown_headings(&candidate);
    candidate = candidate.replace("```", " ");

    let mut cleaned_lines = Vec::<String>::new();
    let mut in_code_fence = false;
    for raw_line in candidate.lines() {
        let Some(line) = clean_task_line(raw_line) else {
            continue;
        };
        if line.starts_with("```") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence || line.is_empty() {
            continue;
        }
        if task_scaffolding_line(&line) {
            continue;
        }
        cleaned_lines.push(line);
        if cleaned_lines.len() >= 6 {
            break;
        }
    }

    let compact = cleaned_lines.join(" ");
    let compact = compact.split_whitespace().collect::<Vec<_>>().join(" ");
    (!compact.is_empty()).then_some(compact)
}

fn truncate_task_text(compact: String, width: usize) -> Option<String> {
    let compact_len = compact.chars().count();
    if compact_len <= width {
        return Some(compact);
    }
    if width <= 3 {
        return Some(".".repeat(width));
    }
    let shortened = compact
        .chars()
        .take(width.saturating_sub(3))
        .collect::<String>();
    Some(format!("{}...", shortened.trim_end()))
}

fn polish_task_title_candidate(value: &str) -> String {
    let mut title = value.trim().to_string();
    if title.is_empty() {
        return String::new();
    }

    if let Some(stripped) = strip_meta_wrapper_prefix(&title) {
        title = stripped;
    }

    while let Some(stripped) = strip_conversational_prefix(&title) {
        if stripped == title || stripped.is_empty() {
            break;
        }
        title = stripped;
    }

    title = strip_inline_image_references(&title);
    title = strip_metric_dump_suffix(&title);
    title = strip_artifact_tokens(&title);
    title = strip_trailing_heading_wrapper_suffix(&title);
    title = title
        .replace('?', " ")
        .trim_start_matches([',', ':', ';', '-', '.'])
        .trim()
        .trim_end_matches([',', ':', ';', '?', '.'])
        .trim()
        .to_string();
    title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        value.trim().to_string()
    } else {
        title
    }
}

fn strip_trailing_heading_wrapper_suffix(value: &str) -> String {
    for suffix in [" implementation plan", " plan"] {
        let Some(cutoff) = value.len().checked_sub(suffix.len()) else {
            continue;
        };
        if value.len() > suffix.len()
            && value
                .get(cutoff..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
        {
            return value[..cutoff]
                .trim()
                .trim_end_matches([':', ';', '-', ','])
                .trim()
                .to_string();
        }
    }
    value.trim().to_string()
}

fn looks_like_metric_result_stub(original: &str, polished: &str) -> bool {
    let polished_lowercase = polished.to_ascii_lowercase();
    if polished_lowercase.is_empty() {
        return true;
    }
    if [
        "coverage=",
        "f1_overlap=",
        "f1@",
        "avg_tiou=",
        "mae=",
        "titlef1=",
        "cider=",
        "score=",
    ]
    .iter()
    .any(|prefix| polished_lowercase.starts_with(prefix))
    {
        return true;
    }

    let original_lowercase = original.to_ascii_lowercase();
    if !contains_metric_report_marker(&original_lowercase)
        || has_explicit_task_intent(&polished_lowercase)
    {
        return false;
    }

    let normalized = normalize_task_title(polished);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    tokens.len() <= 8
        && tokens.iter().any(|token| {
            token.contains("ckpt")
                || token.contains("checkpoint")
                || token.contains("adapter")
                || token.contains("4bit")
                || token.contains("8bit")
                || token.contains("16bit")
                || token.contains("bf16")
                || token.contains("fp16")
                || token.contains("mlx")
                || token.contains("lora")
        })
}

fn strip_metric_dump_suffix(value: &str) -> String {
    let lowercase = value.to_ascii_lowercase();
    let markers = [
        " coverage=",
        " f1@",
        " avg_tiou",
        " avg_tiou=",
        " mae=",
        " titlef1=",
        " cider=",
        " pred=",
        " gold=",
        " gen=",
    ];
    let cutoff = markers
        .iter()
        .filter_map(|marker| lowercase.find(marker))
        .min();
    cutoff
        .map(|index| {
            value[..index]
                .trim()
                .trim_end_matches(':')
                .trim()
                .to_string()
        })
        .unwrap_or_else(|| value.trim().to_string())
}

fn contains_metric_report_marker(value: &str) -> bool {
    [
        "coverage=",
        "f1_overlap=",
        "f1@",
        "avg_tiou",
        "mae=",
        "titlef1=",
        "cider=",
        "score=",
        "ueo(",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}

fn has_explicit_task_intent(value: &str) -> bool {
    let normalized = normalize_task_title(value);
    if normalized.is_empty() {
        return false;
    }
    if [
        "i want ",
        "i want to ",
        "i need ",
        "i need to ",
        "need to ",
        "please ",
        "lets ",
        "let s ",
    ]
    .iter()
    .any(|prefix| normalized.starts_with(prefix))
    {
        return true;
    }
    normalized.split_whitespace().any(explicit_task_verb_token)
}

fn strip_inline_image_references(value: &str) -> String {
    let mut remaining = value;
    let mut cleaned = String::new();
    while let Some(start) = remaining.find("[Image") {
        cleaned.push_str(&remaining[..start]);
        let tail = &remaining[start..];
        let Some(end) = tail.find(']') else {
            remaining = tail;
            break;
        };
        remaining = &tail[end + 1..];
    }
    cleaned.push_str(remaining);
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_artifact_tokens(value: &str) -> String {
    value
        .split_whitespace()
        .filter(|token| !should_drop_artifact_token(token))
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_plain_role_prefix(value: &str) -> Option<String> {
    let lowercase = value.to_ascii_lowercase();
    for prefix in ["assistant:", "user:", "developer:", "system:", "d user:"] {
        if lowercase.starts_with(prefix) {
            let stripped = value[prefix.len()..].trim();
            return (!stripped.is_empty()).then_some(stripped.to_string());
        }
    }
    None
}

fn strip_meta_wrapper_prefix(value: &str) -> Option<String> {
    let (prefix, suffix) = value.split_once(':')?;
    let prefix_tokens = basic_normalize_phrase(prefix)
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if prefix_tokens.is_empty() || prefix_tokens.len() > 8 {
        return None;
    }
    let content_tokens = prefix_tokens
        .iter()
        .filter(|token| !TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()))
        .collect::<Vec<_>>();
    if content_tokens.is_empty() {
        return None;
    }
    let meta_token_count = content_tokens
        .iter()
        .filter(|token| {
            META_WRAPPER_TOKENS.contains(&token.as_str())
                || WRAPPER_FILLER_TOKENS.contains(&token.as_str())
        })
        .count();
    if meta_token_count < content_tokens.len() {
        return None;
    }
    let suffix = suffix.trim();
    (!suffix.is_empty()).then_some(suffix.to_string())
}

fn should_drop_artifact_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|character: char| {
        matches!(
            character,
            ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\''
        )
    });
    if trimmed.is_empty() {
        return false;
    }
    let lowercase = trimmed.to_ascii_lowercase();
    lowercase.starts_with("http://")
        || lowercase.starts_with("https://")
        || lowercase.starts_with("/users/")
        || lowercase.starts_with("/kaggle/")
        || lowercase.contains("jupyter-proxy.kaggle.net")
        || lowercase.starts_with("token=eyj")
        || lowercase.starts_with("<image")
        || lowercase.starts_with("name=")
        || lowercase.starts_with("name=[image")
        || lowercase.starts_with("</image")
        || lowercase.contains("[image")
        || lowercase.ends_with("]>")
        || lowercase == "[image"
        || looks_like_opaque_token(trimmed)
}

fn looks_like_sensitive_locator_dump(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    lowercase.contains("$codex_home/automations/")
        || lowercase.contains("token=eyj")
        || ((lowercase.contains("http://") || lowercase.contains("https://"))
            && ["token=", "apikey=", "api_key=", "auth=", "signature="]
                .iter()
                .any(|marker| lowercase.contains(marker)))
}

fn looks_like_locator_stub(value: &str) -> bool {
    matches!(value.trim(), "colab" | "kaggle" | "automation")
}

fn looks_like_opaque_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|character: char| {
        matches!(
            character,
            ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']'
        )
    });
    if trimmed.len() < 16 {
        return false;
    }
    let lowercase = trimmed.to_ascii_lowercase();
    if lowercase.starts_with("eyj") {
        return true;
    }
    let has_alpha = trimmed
        .chars()
        .any(|character| character.is_ascii_alphabetic());
    let has_digit = trimmed.chars().any(|character| character.is_ascii_digit());
    let safe_chars = trimmed.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '=')
    });
    has_alpha && has_digit && safe_chars
}

fn strip_conversational_prefix(value: &str) -> Option<String> {
    let lowercase = value.to_ascii_lowercase();
    for prefix in [
        "could you please ",
        "could you ",
        "can you please ",
        "can you ",
        "please ",
        "show me ",
        "tell me ",
        "how to ",
        "what are ",
        "what is ",
        "what about ",
        "do we ",
        "does this ",
        "did we ",
        "is there ",
        "are these ",
        "are they ",
        "would this ",
        "would it ",
        "there they say ",
        "they say ",
        "let's ",
        "lets ",
        "i mean ",
        "i meant ",
        "okay, ",
        "okay. ",
        "ok, ",
        "ok. ",
        "again. ",
        "hm, ",
        "hmm, ",
        "aha, ",
        "interesting, ",
        "now, ",
        "now. ",
        "so ",
        "so, ",
        "but ",
    ] {
        if lowercase.starts_with(prefix) {
            let stripped = value[prefix.len()..].trim();
            return (!stripped.is_empty()).then_some(stripped.to_string());
        }
    }
    None
}

fn select_task_prompt_candidate(raw: &str) -> Option<String> {
    if looks_like_sensitive_locator_dump(raw) {
        return None;
    }

    if let Some(candidate) = leading_markdown_heading_candidate(raw) {
        return Some(candidate);
    }

    let mut best = None::<(i32, String)>;
    for candidate in prompt_candidate_fragments(raw) {
        let polished = polish_task_title_candidate(&candidate);
        if polished.is_empty() || looks_like_metric_result_stub(&candidate, &polished) {
            continue;
        }
        let score = prompt_candidate_score(&polished);
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, polished));
        }
    }
    if let Some((score, candidate)) = best {
        if score > 0 {
            return Some(candidate);
        }
    }

    let compact = clean_task_text(raw)?;
    let polished = polish_task_title_candidate(&compact);
    if polished.is_empty()
        || looks_like_metric_result_stub(&compact, &polished)
        || task_scaffolding_line(&polished)
        || looks_like_prompt_scaffolding_line(&polished)
    {
        return None;
    }
    Some(polished)
}

fn leading_markdown_heading_candidate(raw: &str) -> Option<String> {
    let expanded = expand_inline_markdown_headings(raw);
    let supporting_topic_sets = prompt_supporting_topic_sets(raw);
    let mut best = None::<(i32, String)>;
    for line in expanded.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            continue;
        }
        let mut heading = trimmed.trim_start_matches('#').trim().to_string();
        if heading.is_empty() {
            continue;
        }
        if let Some(stripped) = strip_meta_wrapper_prefix(&heading) {
            heading = stripped;
        }
        let polished = polish_task_title_candidate(&heading);
        if polished.is_empty()
            || looks_like_document_section_heading(&polished)
            || starts_with_document_section_label(&polished)
            || looks_like_prompt_scaffolding_line(&polished)
        {
            continue;
        }
        let score = prompt_candidate_score(&polished)
            + markdown_heading_support_score(&polished, &supporting_topic_sets);
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, polished));
        }
    }
    best.filter(|(score, _)| *score > 0).map(|(_, title)| title)
}

fn prompt_candidate_fragments(raw: &str) -> Vec<String> {
    let mut seen = BTreeSet::<String>::new();
    let mut candidates = Vec::<String>::new();
    let fragments = split_prompt_fragments(raw);

    for fragment in std::iter::once(raw)
        .chain(raw.lines())
        .chain(fragments.iter().map(String::as_str))
    {
        if let Some(stripped) = strip_conversational_prefix(fragment) {
            push_prompt_candidate(stripped.as_str(), &mut candidates, &mut seen);
        }
        push_prompt_candidate(fragment, &mut candidates, &mut seen);
    }

    candidates
}

fn push_prompt_candidate(raw: &str, candidates: &mut Vec<String>, seen: &mut BTreeSet<String>) {
    let Some(candidate) = clean_task_text(raw) else {
        return;
    };
    if candidate.is_empty() || !seen.insert(candidate.clone()) {
        return;
    }
    candidates.push(candidate);
}

fn split_prompt_fragments(value: &str) -> Vec<String> {
    expand_inline_markdown_headings(value)
        .replace("\r\n", "\n")
        .replace(['\r', '|'], "\n")
        .replace("? ", "?\n")
        .replace("! ", "!\n")
        .replace(". ", ".\n")
        .lines()
        .map(str::trim)
        .filter(|fragment| !fragment.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn prompt_supporting_topic_sets(raw: &str) -> Vec<BTreeSet<String>> {
    let mut topic_sets = Vec::<BTreeSet<String>>::new();
    let mut seen = BTreeSet::<String>::new();
    for fragment in raw
        .lines()
        .chain(split_prompt_fragments(raw).iter().map(String::as_str))
    {
        let trimmed = fragment.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(candidate) = clean_task_text(fragment) else {
            continue;
        };
        if candidate.is_empty() || !seen.insert(candidate.clone()) {
            continue;
        }
        let topic_tokens = title_topic_tokens(&candidate);
        if !topic_tokens.is_empty() {
            topic_sets.push(topic_tokens);
        }
    }
    topic_sets
}

fn markdown_heading_support_score(
    heading: &str,
    supporting_topic_sets: &[BTreeSet<String>],
) -> i32 {
    let heading_tokens = title_topic_tokens(heading);
    if heading_tokens.is_empty() {
        return -6;
    }
    let overlap_score = supporting_topic_sets
        .iter()
        .map(|topic_set| heading_tokens.intersection(topic_set).count())
        .sum::<usize>();
    if overlap_score >= 4 {
        6
    } else if overlap_score >= 2 {
        3
    } else if overlap_score == 1 && has_explicit_task_intent(heading) {
        0
    } else if has_explicit_task_intent(heading) {
        -2
    } else if heading_tokens.len() <= 4 && !looks_like_statemental_heading(heading) {
        0
    } else if overlap_score == 1 {
        -8
    } else {
        -20
    }
}

fn looks_like_statemental_heading(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.len() < 4 || tokens.len() > 10 {
        return false;
    }
    let has_subject = tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "this" | "that" | "these" | "those" | "it" | "you" | "your"
        )
    });
    let has_copula = tokens
        .iter()
        .any(|token| matches!(token.as_str(), "is" | "are" | "was" | "were"));
    has_subject && has_copula
}

fn expand_inline_markdown_headings(value: &str) -> String {
    let mut expanded = String::with_capacity(value.len() + 16);
    let characters = value.chars().collect::<Vec<_>>();
    for (index, character) in characters.iter().enumerate() {
        if *character == '#' {
            let previous = index.checked_sub(1).and_then(|idx| characters.get(idx));
            let next = characters.get(index + 1);
            let starts_heading = previous
                .is_some_and(|previous| previous.is_whitespace() || matches!(previous, ':' | ';'))
                && next.is_some_and(|next| next.is_whitespace() || *next == '#');
            if starts_heading && !expanded.ends_with('\n') {
                expanded.push('\n');
            }
        }
        expanded.push(*character);
    }
    expanded
}

fn prompt_candidate_score(value: &str) -> i32 {
    if task_scaffolding_line(value) || looks_like_prompt_scaffolding_line(value) {
        return -100;
    }
    if task_title_is_session_meta(Some(value)) {
        return -80;
    }

    let normalized = normalize_task_title(value);
    if normalized.is_empty() {
        return -100;
    }

    let token_count = normalized.split_whitespace().count();
    let mut score = 0;
    if has_explicit_task_intent(value) {
        score += 6;
    }
    if task_title_is_generic(Some(value)) {
        score -= 6;
    } else {
        score += 4;
    }
    if task_title_is_weak_signal(Some(value)) {
        score -= 3;
    } else {
        score += 2;
    }
    if (2..=14).contains(&token_count) {
        score += 1;
    } else if token_count > 20 {
        score -= 2;
    }
    score + task_title_signal_score(Some(value)) / 3
}

fn looks_like_prompt_scaffolding_line(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    normalized_matches_prefixes(&normalized, PROMPT_SCAFFOLD_PREFIXES)
        || normalized_contains_fragments(&normalized, PROMPT_SCAFFOLD_CONTAINS)
}

fn looks_like_document_section_heading(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() || tokens.len() > 4 {
        return false;
    }
    let section_token_count = tokens
        .iter()
        .filter(|token| {
            matches!(
                **token,
                "approach"
                    | "background"
                    | "context"
                    | "current"
                    | "details"
                    | "implementation"
                    | "issue"
                    | "issues"
                    | "overview"
                    | "problem"
                    | "state"
                    | "steps"
                    | "summary"
            )
        })
        .count();
    section_token_count >= 1 && section_token_count == tokens.len()
}

fn starts_with_document_section_label(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    if tokens.len() < 2 {
        return false;
    }
    matches!(
        tokens.first().copied(),
        Some(
            "approach"
                | "background"
                | "context"
                | "current"
                | "details"
                | "implementation"
                | "issue"
                | "issues"
                | "overview"
                | "problem"
                | "state"
                | "steps"
                | "summary"
        )
    )
}

fn strip_request_wrapper(value: &str) -> String {
    let mut best_index: Option<usize> = None;
    let lowercase = value.to_ascii_lowercase();
    for marker in REQUEST_MARKERS {
        let marker_lower = marker.to_ascii_lowercase();
        if let Some(index) = lowercase.rfind(&marker_lower) {
            best_index = Some(best_index.map_or(index, |current| current.max(index)));
        }
    }
    best_index
        .map(|index| value[index..].trim())
        .and_then(|suffix| {
            suffix
                .split_once(':')
                .map(|(_, value)| value.trim().to_string())
        })
        .unwrap_or_else(|| value.trim().to_string())
}

fn clean_task_line(raw_line: &str) -> Option<String> {
    let stripped = strip_request_wrapper(raw_line);
    let stripped = stripped.trim();
    if stripped.is_empty() {
        return None;
    }

    let mut line = stripped.trim_start_matches('#').trim().to_string();
    if line.is_empty() {
        return None;
    }

    if let Some(stripped) = strip_meta_wrapper_prefix(&line) {
        line = stripped;
    }
    line = line.trim_start_matches('#').trim().to_string();

    if let Some(extracted) = extract_structured_task_signal(&line) {
        line = extracted;
    }
    if let Some(stripped) = strip_plain_role_prefix(&line) {
        line = stripped;
    }
    line = strip_trailing_subagent_marker(&line);
    line = line.trim_start_matches('>').trim().to_string();
    line = line.trim_start_matches('-').trim().to_string();

    while let Some(rest) = strip_bracketed_counter(&line) {
        line = rest.trim().to_string();
    }

    let lowercase = line.to_ascii_lowercase();
    if lowercase.starts_with("transcript delta start")
        || lowercase.starts_with("transcript delta end")
    {
        let mut remainder = line
            .split_once(':')
            .map(|(_, value)| value.trim().to_string())
            .unwrap_or_default();
        remainder = remainder.trim_start_matches('>').trim().to_string();
        if remainder.is_empty() {
            return None;
        }
        line = remainder;
    }

    let compact = line.split_whitespace().collect::<Vec<_>>().join(" ");
    (!compact.is_empty()).then_some(compact)
}

fn task_scaffolding_line(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    normalized.is_empty()
        || normalized_matches_prefixes(&normalized, LOW_SIGNAL_PREFIXES)
        || normalized_contains_fragments(&normalized, LOW_SIGNAL_CONTAINS)
        || looks_like_prompt_scaffolding_line(value)
        || looks_like_structured_key_value_title(value)
        || looks_like_path_stub_title(value)
        || looks_like_command_or_output_title(value)
        || looks_like_context_markup(value)
        || looks_like_file_reference(value)
}

fn extract_structured_task_signal(value: &str) -> Option<String> {
    let compact = value.replace("```", " ");
    let lowercase = compact.to_ascii_lowercase();
    let likely_wrapped = lowercase.contains("transcript delta")
        || lowercase.contains("::code-comment{")
        || lowercase.contains("tool exec_command result")
        || lowercase.contains("tool write_stdin result")
        || lowercase.contains("found one actionable issue");
    if !likely_wrapped {
        return None;
    }

    if let Some(user_content) = extract_role_segment(&compact, "user:") {
        return Some(user_content);
    }

    if let Some(comment_title) = extract_code_comment_title(&compact) {
        let comment_title = strip_code_comment_severity(&comment_title);
        if lowercase.contains("code review") || lowercase.contains("actionable issue") {
            return Some(format!("Code review: {comment_title}"));
        }
        return Some(comment_title);
    }

    if lowercase.contains("code review") {
        return Some("Code review".to_string());
    }

    None
}

fn extract_role_segment(value: &str, role_marker: &str) -> Option<String> {
    let lowercase = value.to_ascii_lowercase();
    let start = lowercase.find(&role_marker.to_ascii_lowercase())?;
    let after_marker = value[start + role_marker.len()..].trim();
    if after_marker.is_empty() {
        return None;
    }

    let after_lower = after_marker.to_ascii_lowercase();
    let boundaries = [
        " assistant:",
        " developer:",
        " system:",
        " tool ",
        " chunk id:",
        " wall time:",
        " process exited with code",
        " process running with session id",
        " original token count:",
        " output:",
        " found one actionable issue:",
        " ::code-comment{",
    ];
    let end = boundaries
        .iter()
        .filter_map(|boundary| after_lower.find(boundary))
        .min()
        .unwrap_or(after_marker.len());
    let content = after_marker[..end].trim();
    (!content.is_empty()).then_some(content.to_string())
}

fn extract_code_comment_title(value: &str) -> Option<String> {
    let marker = "::code-comment{title=\"";
    let start = value.find(marker)?;
    let rest = &value[start + marker.len()..];
    let end = rest.find('"')?;
    let title = rest[..end].trim();
    (!title.is_empty()).then_some(title.to_string())
}

fn strip_code_comment_severity(value: &str) -> String {
    let trimmed = value.trim();
    if let Some(stripped) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
    {
        return stripped.1.trim().to_string();
    }
    trimmed.to_string()
}

fn strip_bracketed_counter(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if !trimmed.starts_with('[') {
        return None;
    }
    let end = trimmed.find(']')?;
    trimmed[1..end]
        .chars()
        .all(|character| character.is_ascii_digit())
        .then_some(trimmed[end + 1..].trim())
}

fn normalized_content_tokens(value: &str) -> Vec<String> {
    normalized_title_tokens(value)
        .into_iter()
        .filter(|token| !TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()))
        .collect()
}

fn is_task_verb_token(token: &str) -> bool {
    explicit_task_verb_token(token)
}

fn explicit_task_verb_token(token: &str) -> bool {
    matches!(
        token,
        "add"
            | "analyze"
            | "audit"
            | "benchmark"
            | "build"
            | "choose"
            | "compare"
            | "create"
            | "debug"
            | "deploy"
            | "evaluate"
            | "explain"
            | "export"
            | "fix"
            | "implement"
            | "improve"
            | "investigate"
            | "locate"
            | "merge"
            | "remove"
            | "refactor"
            | "rename"
            | "replace"
            | "rescore"
            | "review"
            | "rebuild"
            | "split"
            | "summarize"
            | "test"
            | "track"
            | "train"
            | "verify"
    )
}

fn basic_has_explicit_task_intent(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    normalized
        .split_whitespace()
        .next()
        .is_some_and(explicit_task_verb_token)
}

fn is_concrete_task_topic_token(token: &str) -> bool {
    !TITLE_TOPIC_STOP_WORDS.contains(&token)
        && !META_WRAPPER_TOKENS.contains(&token)
        && !WRAPPER_FILLER_TOKENS.contains(&token)
        && !ABSTRACT_TASK_OBJECT_TOKENS.contains(&token)
        && !ABSTRACT_TASK_MODIFIER_TOKENS.contains(&token)
        && !DEICTIC_FOLLOWUP_TOKENS.contains(&token)
        && !is_task_verb_token(token)
}

fn strip_trailing_subagent_marker(value: &str) -> String {
    let trimmed = value.trim();
    if let Some(prefix) = trimmed
        .strip_suffix(')')
        .and_then(|prefix| prefix.rsplit_once("(@"))
        .and_then(|(head, tail)| tail.trim().ends_with("subagent").then_some(head))
    {
        return prefix.trim_end().to_string();
    }
    trimmed.to_string()
}

fn looks_like_context_markup(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with('<')
        && (trimmed.ends_with('>')
            || trimmed.contains("<cwd>")
            || trimmed.contains("<shell>")
            || trimmed.contains("<current_date>")
            || trimmed.contains("<timezone>"))
}

fn looks_like_file_reference(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with("/Users/")
        || trimmed.starts_with("~/")
        || trimmed.starts_with("/var/")
        || trimmed.contains(": /Users/")
        || trimmed.contains(": /var/")
        || trimmed.contains(".png:")
        || trimmed.contains(".jpg:")
        || trimmed.contains(".jpeg:")
        || trimmed.contains(".md:")
        || trimmed.contains(".json:")
}

#[must_use]
pub fn choose_best_task_title<'a>(
    primary: Option<&'a str>,
    fallback: Option<&'a str>,
    default_title: &'a str,
) -> (String, &'static str) {
    let primary = primary.and_then(|value| summarize_task_text(Some(value), 90));
    if primary
        .as_deref()
        .is_some_and(|value| !task_title_is_generic(Some(value)))
    {
        return (primary.expect("primary title exists"), "primary");
    }
    let fallback = fallback.and_then(|value| task_title_from_prompt(Some(value)));
    if let Some(title) = primary.or(fallback) {
        return (title, "fallback");
    }
    (default_title.to_string(), "default")
}

#[must_use]
pub fn extract_issue_keys(values: &[&str]) -> Vec<String> {
    let mut keys = BTreeSet::new();
    for value in values {
        for raw_token in value.split(|character: char| {
            !(character.is_ascii_alphanumeric() || character == '-' || character == '#')
        }) {
            let token = raw_token.trim_matches(|character: char| {
                !(character.is_ascii_alphanumeric() || character == '-' || character == '#')
            });
            if token.is_empty() {
                continue;
            }
            if token.starts_with('#')
                && token.len() > 1
                && token[1..]
                    .chars()
                    .all(|character| character.is_ascii_digit())
            {
                keys.insert(token.to_string());
                continue;
            }
            let mut parts = token.split('-');
            let Some(left) = parts.next() else {
                continue;
            };
            let Some(right) = parts.next() else {
                continue;
            };
            if left.is_empty()
                || right.is_empty()
                || !left
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_uppercase())
                || !left
                    .chars()
                    .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
                || !right.chars().all(|character| character.is_ascii_digit())
            {
                continue;
            }
            keys.insert(format!("{left}-{right}"));
        }
    }
    keys.into_iter().collect()
}

#[must_use]
pub fn branch_family(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let issue_keys = extract_issue_keys(&[value]);
    if let Some(issue_key) = issue_keys.first() {
        return Some(issue_key.to_ascii_lowercase());
    }

    let tail = value
        .rsplit('/')
        .next()
        .unwrap_or(value)
        .trim_matches(|character: char| character == '-' || character == '_' || character == '.');
    let normalized = normalize_task_title(tail);
    if normalized.is_empty() {
        return None;
    }
    let words = normalized.split_whitespace().collect::<Vec<_>>();
    let stripped = if words
        .first()
        .is_some_and(|word| BRANCH_PREFIXES.contains(word))
    {
        words.into_iter().skip(1).collect::<Vec<_>>().join(" ")
    } else {
        normalized
    };
    (!stripped.is_empty()).then_some(stripped)
}

#[must_use]
pub fn title_topic_tokens(value: &str) -> BTreeSet<String> {
    normalize_task_title(value)
        .split_whitespace()
        .filter(|token| token.len() >= 3 && !TITLE_TOPIC_STOP_WORDS.contains(token))
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_issue_keys_from_titles_and_branches() {
        assert_eq!(
            extract_issue_keys(&["ABC-123 fix caching", "feature/ABC-123-fix-caching"]),
            vec!["ABC-123".to_string()]
        );
        assert_eq!(extract_issue_keys(&["see #42"]), vec!["#42".to_string()]);
    }

    #[test]
    fn detects_generic_titles() {
        assert!(task_title_is_generic(Some("hello")));
        assert!(task_title_is_generic(Some(
            "approval assessment for repo access"
        )));
        assert!(task_title_is_generic(Some("Review uncommitted changes")));
        assert!(task_title_is_generic(Some("OpenCode session")));
        assert!(task_title_is_generic(Some(
            "<environment_context> <cwd>/tmp/project</cwd> <shell>zsh</shell>"
        )));
        assert!(task_title_is_generic(Some(
            "Skills Available skills How to use skills - Discovery: The list above is the skills available in this session"
        )));
        assert!(task_title_is_generic(Some(
            "Claude Opus 4.5 Guidelines Project-agnostic instructions for Claude Opus 4.5 in OpenCode."
        )));
        assert!(task_title_is_generic(Some(
            "--- Proactiveness Strike a balance between doing the right thing and not surprising the user."
        )));
        assert!(task_title_is_generic(Some(
            "Automation: Morning Automation ID: morning Automation memory: $CODEX_HOME/automations/morning/memory.md"
        )));
        assert!(task_title_is_generic(Some(
            "tool web_search call: {\"type\":\"open_page\",\"url\":\"https://base-ui.com/react/overview/releases/v1-4-0\"}"
        )));
        assert!(task_title_is_generic(Some(
            "notebook https://proxy.example.invalid/session?token=eyJhbGciOiJIUzI1NiJ9"
        )));
        assert!(task_title_is_generic(Some(
            "Last run: 2026-05-06T04:36:49.224Z (1778042209224) Say hi, nothing else"
        )));
        assert!(task_title_is_generic(Some(
            "Success. Updated the following files: M /Users/example/workspace/project/ui/components/ui/sheet.tsx"
        )));
        assert!(task_title_is_generic(Some(
            "coverage=1.000 (100/100) F1@0.5=67.52 MAE=2.000"
        )));
        assert!(task_title_is_generic(Some(
            "Command line invocation: /Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild -scheme SampleApp"
        )));
        assert!(task_title_is_generic(Some("IMPLEMENT THIS PLAN")));
        assert!(task_title_is_generic(Some("Code review guidelines")));
        assert!(task_title_is_generic(Some(
            "fatal: Unable to create Operation not permitted"
        )));
        assert!(task_title_is_generic(Some(
            "This version has breaking changes and APIs may differ from your training data"
        )));
        assert!(task_title_is_generic(Some(
            "Continue the same review conversation"
        )));
        assert!(task_title_is_generic(Some("Here is code review")));
        assert!(task_title_is_generic(Some(
            "Test Case '-[SampleAppTests.MediaExportTests testWriteStablePreviewWhenRequested]' failed (218.100 seconds)"
        )));
        assert!(task_title_is_generic(Some(
            "Build settings from command line"
        )));
        assert!(task_title_is_generic(Some("@statsai/api@0.0.1 deploy")));
        assert!(task_title_is_generic(Some(
            "review changes on main against origin/main"
        )));
        assert!(task_title_is_generic(Some(
            "You are acting as a reviewer for a proposed code change made by another engineer"
        )));
        assert!(task_title_is_generic(Some("Tokens used: 2631368")));
        assert!(!task_title_is_generic(Some(
            "Implement task verification workflow"
        )));
    }

    #[test]
    fn detects_weak_titles() {
        assert!(task_title_is_weak_signal(Some("banana")));
        assert!(task_title_is_weak_signal(Some("colab")));
        assert!(!task_title_is_weak_signal(Some("Paywall UI review")));
        assert!(!task_title_is_weak_signal(Some("Gemma4 TPU finetuning")));
    }

    #[test]
    fn detects_session_control_meta_titles() {
        assert!(task_title_is_session_meta(Some(
            "Clearing Conversation History"
        )));
        assert!(task_title_is_session_meta(Some(
            "User exits conversation session"
        )));
        assert!(task_title_is_session_meta(Some(
            "Model Switch and Quick Exit"
        )));
        assert!(!task_title_is_session_meta(Some("Switch model loading UI")));
        assert!(!task_title_is_session_meta(Some(
            "SwiftUI Paywall Sheet Race Condition Fix"
        )));
    }

    #[test]
    fn detects_short_dialogue_management_titles_without_exact_history_match() {
        assert!(task_title_is_generic(Some("Hi")));
        assert!(task_title_is_generic(Some("say hello")));
        assert!(task_title_is_generic(Some("ask user for details")));
        assert!(task_title_is_generic(Some("Open browser")));
        assert!(task_title_is_generic(Some("Morning Greetings")));
        assert!(task_title_is_generic(Some("Lunch Greetings")));
        assert!(task_title_is_generic(Some("Handle greeting")));
        assert!(!task_title_is_generic(Some("Implement browser auth flow")));
    }

    #[test]
    fn derives_branch_family_from_issue_key_or_tail() {
        assert_eq!(
            branch_family(Some("feature/ABC-123-task-builder")),
            Some("abc-123".to_string())
        );
        assert_eq!(
            branch_family(Some("chore/rebuild-task-index")),
            Some("rebuild task index".to_string())
        );
    }

    #[test]
    fn summarize_task_text_truncates_unicode_without_panicking() {
        assert_eq!(
            summarize_task_text(Some("hello🙂 world"), 8),
            Some("hello...".to_string())
        );
        assert_eq!(
            summarize_task_text(Some("éééé"), 3),
            Some("...".to_string())
        );
    }

    #[test]
    fn summarize_task_text_removes_wrapper_scaffolding() {
        let wrapped = r#"
        # Files mentioned by the user:
        ## Screenshot.png: /Users/example/tmp/Screenshot.png

        ## My request for Codex:
        Add public leaderboard
        "#;
        assert_eq!(
            summarize_task_text(Some(wrapped), 90),
            Some("Add public leaderboard".to_string())
        );
    }

    #[test]
    fn summarize_task_text_skips_environment_context_lines() {
        let wrapped = r#"
        <environment_context>
          <cwd>/Users/example/workspace/project</cwd>
          <shell>zsh</shell>
        </environment_context>

        Investigate leaderboard ranking mismatch
        "#;
        assert_eq!(
            summarize_task_text(Some(wrapped), 90),
            Some("Investigate leaderboard ranking mismatch".to_string())
        );
    }

    #[test]
    fn summarize_task_text_extracts_request_from_inline_file_wrapper() {
        let wrapped = "# Files mentioned by the user: ## screenshot.png: /Users/example/tmp/screenshot.png ## My request for Codex: Add public leaderboard";
        assert_eq!(
            summarize_task_text(Some(wrapped), 90),
            Some("Add public leaderboard".to_string())
        );
    }

    #[test]
    fn summarize_task_text_extracts_user_request_from_transcript_delta() {
        let wrapped = r#">>> TRANSCRIPT DELTA START [167] user: Code review Found one actionable issue: ::code-comment{title="[P2] Concurrent filter changes can overwrite each other" body="Each update derives from the last rendered searchParams"}"#;
        assert_eq!(
            summarize_task_text(Some(wrapped), 90),
            Some("Code review".to_string())
        );
    }

    #[test]
    fn task_preview_from_prompt_bounds_large_transcript_wrappers() {
        let mut wrapped = String::from(
            "The following is the Codex agent history whose request action you are assessing.\n\
             >>> TRANSCRIPT START\n\
             [1] user: Deploy apps/api and ui to production.\n",
        );
        wrapped.push_str(&"tool exec_command result\n".repeat(200_000));

        assert_eq!(
            task_preview_from_prompt(Some(&wrapped), 90),
            Some("Deploy apps/api and ui to production".to_string())
        );
    }

    #[test]
    fn bounded_task_preview_input_truncates_first_oversized_line() {
        let raw = format!("{} done", "é".repeat(TASK_PREVIEW_MAX_INPUT_BYTES));
        let bounded = bounded_task_preview_input(&raw);

        assert!(bounded.len() <= TASK_PREVIEW_MAX_INPUT_BYTES);
        assert_eq!(
            bounded.as_ref(),
            prefix_at_char_boundary(raw.as_str(), TASK_PREVIEW_MAX_INPUT_BYTES)
        );
    }

    #[test]
    fn summarize_task_text_reduces_code_review_result_to_issue_title() {
        let wrapped = r#"Here is code review: ``` Found one actionable issue: ::code-comment{title="[P2] Concurrent filter changes can overwrite each other" body="Each update derives from the last rendered searchParams"} ```"#;
        assert_eq!(
            summarize_task_text(Some(wrapped), 90),
            Some("Code review: Concurrent filter changes can overwrite each other".to_string())
        );
    }

    #[test]
    fn transcript_delta_tool_result_is_generic() {
        let wrapped = ">>> TRANSCRIPT DELTA START [288] tool exec_command result: Chunk ID: 84e62e Wall time: 1.0006 seconds Process running with session ID 32988 Original token count: 30 Output:";
        assert!(task_title_is_generic(Some(wrapped)));
        assert_eq!(summarize_task_text(Some(wrapped), 90), None);
    }

    #[test]
    fn summarize_task_text_strips_subagent_suffix() {
        assert_eq!(
            summarize_task_text(
                Some("Audit code quality and test coverage (@general subagent)"),
                90
            ),
            Some("Audit code quality and test coverage".to_string())
        );
    }

    #[test]
    fn task_title_from_prompt_strips_polite_prompt_wrapper() {
        assert_eq!(
            task_title_from_prompt(Some("Could you show improved titles?")),
            Some("show improved titles".to_string())
        );
        assert_eq!(
            task_title_from_prompt(Some("there they say 4B bf16 10gb ram training")),
            Some("4B bf16 10gb ram training".to_string())
        );
    }

    #[test]
    fn task_title_from_prompt_strips_urls_and_tokens() {
        assert_eq!(
            task_title_from_prompt(Some(
                "notebook https://proxy.example.invalid/session?token=eyJhbGciOiJIUzI1NiJ9"
            )),
            None
        );
    }

    #[test]
    fn task_title_from_prompt_skips_instructional_preamble_and_keeps_request() {
        assert_eq!(
            task_title_from_prompt(Some(
                "This is NOT the framework you know. It may differ from your training data. Read the relevant guide before writing code. I need device renaming on web and api."
            )),
            Some("I need device renaming on web and api".to_string())
        );
    }

    #[test]
    fn task_title_from_prompt_extracts_payload_from_plan_wrapper() {
        assert_eq!(
            task_title_from_prompt(Some(
                "PLEASE IMPLEMENT THIS PLAN: Add project token tracking to the stats command"
            )),
            Some("Add project token tracking to the stats command".to_string())
        );
    }

    #[test]
    fn task_title_from_prompt_extracts_heading_from_inline_markdown_plan_wrapper() {
        assert_eq!(
            task_title_from_prompt(Some(
                "Implement the following plan: # Plan: Fix Last Clip Waveform Rendering Bug ## Problem Summary The last clip in the timeline consistently shows waveform rendering artifacts."
            )),
            Some("Fix Last Clip Waveform Rendering Bug".to_string())
        );
    }

    #[test]
    fn task_title_from_prompt_prefers_first_heading_over_section_body_spillover() {
        assert_eq!(
            task_title_from_prompt(Some(
                "Implement the following plan: # Assistant UI Implementation Plan ## Overview Replace the placeholder coming soon state with a chat like interface for video navigation."
            )),
            Some("Assistant UI".to_string())
        );
    }

    #[test]
    fn task_title_from_prompt_prefers_payload_after_goal_scaffolding() {
        assert_eq!(
            task_title_from_prompt(Some(
                "Continue working toward the active thread goal. The objective below is user-provided data. Treat it as the task to pursue, not as higher-priority instructions. Finish the Rust-only local task collection loop in statsai."
            )),
            Some("Finish the Rust-only local task collection loop in statsai".to_string())
        );
    }

    #[test]
    fn task_preview_from_prompt_rejects_goal_wrapper_without_task_payload() {
        assert_eq!(
            task_preview_from_prompt(
                Some(
                    "Continue working toward the active thread goal. The objective below is user-provided data. Completion audit: verify all requirements carefully."
                ),
                220
            ),
            None
        );
    }

    #[test]
    fn task_title_from_prompt_strips_image_wrapper_tokens() {
        assert_eq!(
            task_title_from_prompt(Some(
                "<image name=[Image #1]> </image> Were you using vision model in last runs [Image #1]"
            )),
            Some("Were you using vision model in last runs".to_string())
        );
    }

    #[test]
    fn task_title_from_prompt_strips_plain_role_prefixes() {
        assert_eq!(
            task_title_from_prompt(Some(
                "assistant: The shared sheet is using the dialog root correctly"
            )),
            Some("The shared sheet is using the dialog root correctly".to_string())
        );
    }

    #[test]
    fn task_title_from_prompt_truncates_metric_suffixes() {
        assert_eq!(
            task_title_from_prompt(Some(
                "Qwen3.5 4B 8bit mlx (no adapter): Avg_TIoU=68.73 MAE=2.45 TitleF1=23.76 CIDEr=47.26"
            )),
            None
        );
    }

    #[test]
    fn task_title_from_prompt_rejects_metric_only_coverage_report() {
        assert_eq!(
            task_title_from_prompt(Some(
                "coverage=1.000 (100/100) F1@0.5=67.52 F1@0.7=49.42 MAE=2.000"
            )),
            None
        );
    }

    #[test]
    fn task_title_is_generic_for_metric_result_stub() {
        assert!(task_title_is_generic(Some(
            "Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34"
        )));
    }

    #[test]
    fn task_title_from_prompt_keeps_intent_after_sentence_prefixes() {
        assert_eq!(
            task_title_from_prompt(Some(
                "Okay. Again. I want quick overfit test. Fast path off just because of 1200s compilation time draining gpu quota."
            )),
            Some("I want quick overfit test".to_string())
        );
    }

    #[test]
    fn task_title_from_prompt_keeps_metric_comparison_request_with_explicit_intent() {
        assert_eq!(
            task_title_from_prompt(Some(
                "Compare qwen ckpt2400 against final adapter using F1_overlap, Avg_TIoU and MAE."
            )),
            Some("Compare qwen ckpt2400 against final adapter using F1_overlap".to_string())
        );
    }

    #[test]
    fn interruption_system_notice_is_generic() {
        let interruption = "The user interrupted the previous turn on purpose. Any running unified exec processes may still be running in the background. If any tools/commands were aborted, they may have partially executed.";
        assert!(task_title_is_generic(Some(interruption)));
        assert_eq!(task_title_from_prompt(Some(interruption)), None);
    }

    #[test]
    fn task_title_signal_score_penalizes_logs_and_wrappers() {
        assert!(
            task_title_signal_score(Some(
                "Command line invocation: /Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild"
            )) < 0
        );
        assert!(
            task_title_signal_score(Some(
                "Continue working toward the active thread goal. The objective below is user-provided data."
            )) < 0
        );
        assert!(task_title_signal_score(Some("Add project token tracking")) > 0);
        assert!(
            task_title_signal_score(Some(
                "[DEBUG] ChapterLlamaBoundaryFinder: Wrote stage1 transcript to /tmp/stage1.txt"
            )) < 0
        );
        assert!(
            task_title_signal_score(Some(
                "Generating train split: 10 examples [00:00, 674.63 examples/s]"
            )) < 0
        );
        assert!(
            task_title_signal_score(Some(
                "Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34"
            )) < 0
        );
    }

    #[test]
    fn task_title_from_prompt_skips_unsupported_markdown_heading_banner() {
        assert_eq!(
            task_title_from_prompt(Some(
                "# This is NOT the framework you know\n\
                 Read the relevant guide before writing code.\n\
                 I need device renaming on web and api."
            )),
            Some("I need device renaming on web and api".to_string())
        );
    }

    #[test]
    fn polish_task_title_candidate_handles_non_ascii_without_panicking() {
        assert_eq!(
            polish_task_title_candidate("ёжик playback drift"),
            "ёжик playback drift"
        );
    }

    #[test]
    fn task_title_genericity_rejects_structural_artifacts() {
        for candidate in [
            "Improve, replace, or remove existing work as needed to satisfy the actual objective",
            "now fix them all properly",
            "\"tool_title\": \"Get Test List\"",
            "runs/codex_images_audit/",
            "ui@0.1.0 test",
            "Blocking waiting for file lock on build directory",
            "Here is conversation and here is code review",
        ] {
            assert!(task_title_is_generic(Some(candidate)), "{candidate}");
        }
    }

    #[test]
    fn anchor_level_verification_actions_keep_status_and_rename_keys_distinct() {
        let work_item_id = WorkItemId("work-test".to_string());
        let anchor_span_id = TaskSpanId("span-anchor".to_string());
        let accept = TaskVerificationAction::Accept {
            work_item_id: work_item_id.clone(),
            anchor_span_id: anchor_span_id.clone(),
        };
        let reject = TaskVerificationAction::Reject {
            work_item_id: work_item_id.clone(),
            anchor_span_id: anchor_span_id.clone(),
            reason: TaskVerdict::Meta,
        };
        let rename = TaskVerificationAction::Rename {
            work_item_id,
            anchor_span_id,
            title: "Verified task".to_string(),
        };

        assert_eq!(accept.action_key(), "status:span-anchor");
        assert_eq!(reject.action_key(), "status:span-anchor");
        assert_eq!(rename.action_key(), "rename:span-anchor");
    }

    #[test]
    fn split_verification_action_key_and_span_ids_include_explicit_right_boundary() {
        let action = TaskVerificationAction::Split {
            after_span_id: TaskSpanId("span-left".to_string()),
            before_span_id: Some(TaskSpanId("span-right".to_string())),
            left_title: None,
            right_title: None,
        };

        assert_eq!(action.action_key(), "split:span-left:span-right");
        assert_eq!(
            action
                .span_ids()
                .into_iter()
                .map(|span_id| span_id.0.as_str())
                .collect::<Vec<_>>(),
            vec!["span-left", "span-right"]
        );
    }
}
