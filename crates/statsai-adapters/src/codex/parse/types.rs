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
    pub(crate) is_usage_record: bool,
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
    /// For usage lines: when the request was sent, if the rollout says.
    pub(crate) requested_at: Option<DateTime<Utc>>,
    /// For usage lines: the conversation was compacted before this call.
    pub(crate) after_compaction: bool,
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
    /// The turn's last record before its completion. Codex writes a turn's
    /// completion when the thread next runs, which can be days after the work.
    pub(crate) last_work_at: DateTime<Utc>,
    pub(crate) usage_lines: Vec<usize>,
    /// `token_count` lines inside the turn that carry no usage, because they
    /// repeat a total or pair with a `token_usage_record`. Their quota
    /// observations still belong to this turn's event.
    pub(crate) quota_lines: Vec<usize>,
    pub(crate) project: Option<ProjectInfo>,
    /// The model calls whose usage the turn accumulates, in order.
    pub(crate) calls: Vec<ModelCall>,
}

/// When each session's model calls were requested, for the prompt-cache
/// report.
///
/// A `token_usage_record` is written as the response completes, before any
/// tool runs, so the latest prompt, tool output, or turn start after it is
/// when the next request went out. A `token_count` alone is written after the
/// response's tools have run, so it cannot place a request start. Rollouts can
/// carry copied history with older timestamps, so this follows file order.
#[derive(Default)]
pub(crate) struct CodexCallClock {
    last_input_at: HashMap<String, DateTime<Utc>>,
    last_call_at: HashMap<String, DateTime<Utc>>,
    pending_compaction: HashSet<String>,
}

impl CodexCallClock {
    pub(crate) fn input(&mut self, session_raw: &str, at: DateTime<Utc>) {
        self.last_input_at.insert(session_raw.to_string(), at);
    }

    pub(crate) fn compaction(&mut self, session_raw: &str) {
        self.pending_compaction.insert(session_raw.to_string());
    }

    /// Records a call completing and returns its request start, when the
    /// rollout places one, and whether it follows a compaction.
    pub(crate) fn call(
        &mut self,
        session_raw: &str,
        completed_at: DateTime<Utc>,
        record_placed: bool,
    ) -> (Option<DateTime<Utc>>, bool) {
        let previous_call_at = self
            .last_call_at
            .insert(session_raw.to_string(), completed_at);
        let requested_at = record_placed
            .then(|| self.last_input_at.get(session_raw).copied())
            .flatten()
            .filter(|at| *at <= completed_at)
            .filter(|at| previous_call_at.is_none_or(|previous| *at >= previous));
        (requested_at, self.pending_compaction.remove(session_raw))
    }
}
