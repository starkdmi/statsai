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
mod tests {
    use super::normalize::{
        bounded_task_preview_input, polish_task_title_candidate, prefix_at_char_boundary,
        TASK_PREVIEW_MAX_INPUT_BYTES,
    };
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
    fn normalize_task_title_preserves_unicode_letters_and_numbers() {
        assert_eq!(
            normalize_task_title("Исправить API / 修复错误 １２３"),
            "исправить api 修复错误 １２３"
        );
    }

    #[test]
    fn normalize_task_title_preserves_combining_marks_attached_to_letters() {
        assert_eq!(normalize_task_title("नमस्ते दुनिया"), "नमस्ते दुनिया");
        assert_eq!(normalize_task_title("বাংলা ভাষা"), "বাংলা ভাষা");
        assert_eq!(normalize_task_title("\u{301}abc"), "abc");
    }

    #[test]
    fn normalize_task_title_canonicalizes_equivalent_unicode_sequences() {
        let precomposed = normalize_task_title("Café");
        let decomposed = normalize_task_title("Cafe\u{301}");

        assert_eq!(precomposed, "café");
        assert_eq!(decomposed, precomposed);
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
