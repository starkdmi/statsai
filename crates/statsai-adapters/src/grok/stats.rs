use crate::*;

#[derive(Debug, Clone, Default)]
pub(crate) struct GrokSessionStats {
    pub(crate) chat_rows: u64,
    pub(crate) user_messages: u64,
    pub(crate) assistant_messages: u64,
    pub(crate) reasoning_messages: u64,
    pub(crate) tool_result_messages: u64,
    pub(crate) system_messages: u64,
    pub(crate) events_rows: u64,
    pub(crate) update_rows: u64,
    pub(crate) prompt_count: u64,
    pub(crate) prompt_context_tokens: Option<u64>,
    pub(crate) max_total_tokens: Option<u64>,
    pub(crate) max_tokens_used: Option<u64>,
    pub(crate) max_tokens_after: Option<u64>,
    pub(crate) prompt_models: Vec<GrokModelObservation>,
    pub(crate) turn_models: Vec<GrokModelObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrokModelObservation {
    pub(crate) model_id: String,
    pub(crate) observed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub(crate) struct GrokInferenceSample {
    pub(crate) usage: UsageCounts,
    pub(crate) observed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GrokInferenceStats {
    pub(crate) rows: u64,
    pub(crate) input_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_tokens: u64,
    pub(crate) model_elapsed_ms: Vec<u64>,
    pub(crate) time_to_first_token_ms: Vec<u64>,
    pub(crate) request_samples: Vec<GrokInferenceSample>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GrokUnifiedLogIndex {
    pub(crate) session_stats: HashMap<String, GrokInferenceStats>,
    pub(crate) session_signatures: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GrokJsonlParseStats {
    pub(crate) rows: u64,
    pub(crate) invalid_rows: u64,
}

impl GrokInferenceStats {
    pub(crate) fn has_usage(&self) -> bool {
        self.rows > 0
            && self
                .input_tokens
                .saturating_add(self.cache_read_tokens)
                .saturating_add(self.output_tokens)
                .saturating_add(self.reasoning_tokens)
                > 0
    }

    pub(crate) fn usage_counts(&self) -> UsageCounts {
        UsageCounts {
            input_tokens: nonzero_u64(self.input_tokens),
            output_tokens: nonzero_u64(self.output_tokens),
            cache_creation_tokens: None,
            cache_creation_5m_tokens: None,
            cache_creation_1h_tokens: None,
            cache_read_tokens: nonzero_u64(self.cache_read_tokens),
            reasoning_tokens: nonzero_u64(self.reasoning_tokens),
            total_tokens: None,
            requests: nonzero_u64(self.rows),
            local_prompt_eval_tokens: None,
            local_eval_tokens: None,
        }
    }

    pub(crate) fn request_sample_usage(
        input_tokens: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
    ) -> UsageCounts {
        UsageCounts {
            input_tokens: nonzero_u64(input_tokens),
            output_tokens: nonzero_u64(output_tokens),
            cache_creation_tokens: None,
            cache_creation_5m_tokens: None,
            cache_creation_1h_tokens: None,
            cache_read_tokens: nonzero_u64(cache_read_tokens),
            reasoning_tokens: nonzero_u64(reasoning_tokens),
            total_tokens: None,
            requests: Some(1),
            local_prompt_eval_tokens: None,
            local_eval_tokens: None,
        }
    }
}

impl GrokSessionStats {
    pub(crate) fn total_chat_messages(&self) -> u64 {
        self.user_messages
            .saturating_add(self.assistant_messages)
            .saturating_add(self.reasoning_messages)
            .saturating_add(self.tool_result_messages)
            .saturating_add(self.system_messages)
    }

    pub(crate) fn token_footprint(&self, signals: Option<&Value>) -> Option<u64> {
        [
            signals
                .and_then(|signals| signals.get("contextTokensUsed"))
                .and_then(value_as_u64),
            signals
                .and_then(|signals| signals.get("totalTokensBeforeCompaction"))
                .and_then(value_as_u64),
            self.max_total_tokens,
            self.max_tokens_used,
            self.max_tokens_after,
        ]
        .into_iter()
        .flatten()
        .max()
        .filter(|value| *value > 0)
    }

    pub(crate) fn usage_context_tokens(&self, signals: Option<&Value>) -> Option<u64> {
        self.prompt_context_tokens
            .filter(|value| *value > 0)
            .or_else(|| self.token_footprint(signals))
    }
}

pub(crate) fn nonzero_u64(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}
