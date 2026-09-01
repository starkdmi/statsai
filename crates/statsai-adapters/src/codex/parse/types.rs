use super::*;

#[derive(Debug, Clone)]
pub(crate) struct CodexLineRecord {
    pub(crate) line_number: usize,
    pub(crate) timestamp: DateTime<Utc>,
    pub(crate) timestamp_inferred: bool,
    pub(crate) session_raw: String,
    pub(crate) model: Option<ModelInfo>,
    pub(crate) model_inferred: bool,
    pub(crate) model_explicit: bool,
    pub(crate) usage: Option<UsageCounts>,
    pub(crate) is_token_count_event: bool,
    pub(crate) is_task_started: bool,
    pub(crate) is_task_complete: bool,
    pub(crate) message_role: Option<String>,
    pub(crate) user_message_preview: Option<CodexPromptPreviewCandidate>,
    pub(crate) session_title: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) project: Option<ProjectInfo>,
    pub(crate) task_started_at: Option<DateTime<Utc>>,
    pub(crate) task_completed_at: Option<DateTime<Utc>>,
    pub(crate) task_duration_ms: Option<u64>,
    pub(crate) time_to_first_token_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CodexPromptPreviewSource {
    ResponseItemUser,
    UserMessageEvent,
}

impl CodexPromptPreviewSource {
    pub(crate) const fn priority(self) -> i32 {
        match self {
            Self::ResponseItemUser => 0,
            Self::UserMessageEvent => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexPromptPreview {
    pub(crate) text: String,
    pub(crate) source: CodexPromptPreviewSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexPromptPreviewCandidate {
    pub(crate) raw_text: String,
    pub(crate) source: CodexPromptPreviewSource,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexFastResponseMessageLine<'a> {
    #[serde(default, borrow)]
    pub(crate) timestamp: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    pub(crate) session_id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    pub(crate) payload: CodexFastResponseMessagePayload<'a>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexFastResponseMessagePayload<'a> {
    #[serde(default, borrow)]
    pub(crate) role: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    pub(crate) content: Option<Vec<CodexFastContentPart<'a>>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexFastContentPart<'a> {
    #[serde(default, borrow)]
    pub(crate) text: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    pub(crate) content: Option<CodexFastNestedText<'a>>,
    #[serde(default, borrow)]
    pub(crate) input: Option<CodexFastNestedText<'a>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexFastNestedText<'a> {
    #[serde(default, borrow)]
    pub(crate) text: Option<Cow<'a, str>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CodexMessageCounts {
    pub(crate) total: u64,
    pub(crate) user: u64,
    pub(crate) assistant: u64,
    pub(crate) developer: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ActiveCodexTurn {
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) session_raw: String,
    pub(crate) title: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) model: Option<ModelInfo>,
    pub(crate) model_inferred: bool,
    pub(crate) timestamp_inferred: bool,
    pub(crate) message_counts: CodexMessageCounts,
    pub(crate) last_usage: Option<UsageCounts>,
    pub(crate) accumulated_usage: Option<UsageCounts>,
    pub(crate) prompt_previews: Vec<CodexPromptPreviewCandidate>,
    pub(crate) last_activity_at: DateTime<Utc>,
    pub(crate) usage_lines: Vec<usize>,
    pub(crate) project: Option<ProjectInfo>,
}
