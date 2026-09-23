use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexTokenUsageRecord {
    pub(crate) usage: UsageCounts,
    pub(crate) turn_id: Option<String>,
    pub(crate) root_turn_id: Option<String>,
    pub(crate) response_id: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) session_id: Option<String>,
}

impl CodexTokenUsageRecord {
    /// Parent session for a sub-agent record. Main threads use the same id for
    /// `thread_id` and `session_id`, so they have no parent to roll up to.
    ///
    /// Callers that roll sub-agent cost onto the parent turn should read this
    /// with `root_turn_id`. Nothing in the store reads it yet.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn parent_session_id(&self) -> Option<&str> {
        match (self.session_id.as_deref(), self.thread_id.as_deref()) {
            (Some(session_id), Some(thread_id)) if session_id != thread_id => Some(session_id),
            _ => None,
        }
    }
}

pub(crate) fn codex_token_usage_record_from_value(value: &Value) -> Option<CodexTokenUsageRecord> {
    if value.get("type").and_then(Value::as_str) != Some("token_usage_record") {
        return None;
    }
    let payload = value.get("payload")?;
    let usage = payload.get("usage")?;
    let string_at = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    Some(CodexTokenUsageRecord {
        usage: codex_usage_counts_from_value(usage),
        turn_id: string_at("turn_id"),
        root_turn_id: string_at("root_turn_id"),
        response_id: string_at("response_id"),
        thread_id: string_at("thread_id"),
        session_id: string_at("session_id"),
    })
}

pub(crate) struct CodexCumulativeTotal {
    raw: Value,
    counts: UsageCounts,
}

/// Whether a `token_count` repeats the response a `token_usage_record` already
/// counted. Compares the normalized counts, so whether either side reported
/// `total_tokens` explicitly does not matter.
pub(crate) fn same_codex_response_usage(record: &UsageCounts, token_count: &UsageCounts) -> bool {
    let key = |usage: &UsageCounts| {
        (
            usage.input_tokens.unwrap_or(0),
            usage.output_tokens.unwrap_or(0),
            usage.cache_creation_tokens.unwrap_or(0),
            usage.cache_read_tokens.unwrap_or(0),
            usage.reasoning_tokens.unwrap_or(0),
        )
    };
    key(record) == key(token_count)
}

/// Usage to attribute for one `token_count` line.
///
/// When `total_token_usage` repeats the previous cumulative total for this
/// session, the line is a duplicate snapshot (or the post-compaction context
/// size) and contributes nothing. A smaller total is a fork or resume reset
/// and still counts, from zero. Lines with no cumulative total keep
/// `last_token_usage`.
pub(crate) fn codex_token_count_usage(
    info: Option<&Value>,
    previous: &mut Option<CodexCumulativeTotal>,
) -> Option<UsageCounts> {
    let info = info?;
    let last_usage = info
        .get("last_token_usage")
        .map(codex_usage_counts_from_value);
    let Some(total_value) = info
        .get("total_token_usage")
        .filter(|value| value.is_object())
    else {
        return last_usage;
    };
    let total_counts = codex_usage_counts_from_value(total_value);
    let unchanged = previous
        .as_ref()
        .is_some_and(|previous| cumulative_total_unchanged(previous, total_value));
    let reset = previous
        .as_ref()
        .is_some_and(|previous| cumulative_total_decreased(previous, total_value));
    let usage = if unchanged {
        None
    } else {
        last_usage.or_else(|| {
            // After a reset the counter restarted at zero, so the whole new
            // total is usage; subtracting the larger old total would clamp
            // every field to zero.
            let baseline = (!reset)
                .then(|| previous.as_ref().map(|previous| &previous.counts))
                .flatten();
            Some(crate::subtract_usage_counts(&total_counts, baseline))
        })
    };
    *previous = Some(CodexCumulativeTotal {
        raw: total_value.clone(),
        counts: total_counts,
    });
    usage
}

fn cumulative_total_unchanged(previous: &CodexCumulativeTotal, raw: &Value) -> bool {
    match (previous.raw.get("total_tokens"), raw.get("total_tokens")) {
        // Compare the provider's cumulative counter, not the normalized sum.
        // An equal counter is a repeated snapshot. A smaller one is a reset.
        (Some(before), Some(after)) => before == after,
        _ => &previous.raw == raw,
    }
}

fn cumulative_total_decreased(previous: &CodexCumulativeTotal, raw: &Value) -> bool {
    match (
        previous.raw.get("total_tokens").and_then(Value::as_u64),
        raw.get("total_tokens").and_then(Value::as_u64),
    ) {
        (Some(before), Some(after)) => after < before,
        _ => false,
    }
}

pub(crate) fn codex_usage_counts_from_value(value: &Value) -> UsageCounts {
    let raw_input = number_at_any(value, &["input_tokens", "prompt_tokens", "input"]);
    let raw_output = number_at_any(value, &["output_tokens", "completion_tokens", "output"]);
    let raw_cache_creation = number_at_any(
        value,
        &[
            "cache_creation_input_tokens",
            "cacheCreationInputTokens",
            "cache_creation_tokens",
            "cacheCreationTokens",
            "cache_write_input_tokens",
            "cacheWriteInputTokens",
        ],
    );
    let raw_cache_read = number_at_any(
        value,
        &[
            "cached_input_tokens",
            "cache_read_input_tokens",
            "cached_tokens",
        ],
    );
    let raw_reasoning = number_at_any(value, &["reasoning_output_tokens", "reasoning_tokens"]);
    let total = number_at_any(value, &["total_tokens", "total"]);

    normalize_codex_usage_counts(
        raw_input,
        raw_output,
        raw_cache_creation,
        raw_cache_read,
        raw_reasoning,
        total,
    )
}

// Codex reports cached input, cache writes, and reasoning output as subsets of
// the top-level input/output counters. Normalize that inclusive provider shape
// into the additive contract used everywhere else in statsai.
pub(crate) fn normalize_codex_usage_counts(
    raw_input: Option<u64>,
    raw_output: Option<u64>,
    raw_cache_creation: Option<u64>,
    raw_cache_read: Option<u64>,
    raw_reasoning: Option<u64>,
    total: Option<u64>,
) -> UsageCounts {
    let cache_creation = match (raw_input, raw_cache_creation) {
        (Some(input), Some(cache_creation)) => Some(cache_creation.min(input)),
        _ => raw_cache_creation,
    };
    let cache_read = match (raw_input, raw_cache_read) {
        (Some(input), Some(cache_read)) => Some(cache_read.min(input)),
        _ => raw_cache_read,
    };
    let reasoning = match (raw_output, raw_reasoning) {
        (Some(output), Some(reasoning)) => Some(reasoning.min(output)),
        _ => raw_reasoning,
    };
    let input = raw_input.map(|input| {
        input
            .saturating_sub(cache_creation.unwrap_or(0))
            .saturating_sub(cache_read.unwrap_or(0))
    });
    let output = raw_output
        .map(|output| output.saturating_sub(reasoning.unwrap_or(0)))
        .or_else(|| infer_missing_output(total, input, cache_creation, cache_read, reasoning));
    let total = total.or_else(|| {
        (input.is_some()
            || output.is_some()
            || cache_creation.is_some()
            || cache_read.is_some()
            || reasoning.is_some())
        .then_some(
            input
                .unwrap_or(0)
                .saturating_add(output.unwrap_or(0))
                .saturating_add(cache_creation.unwrap_or(0))
                .saturating_add(cache_read.unwrap_or(0))
                .saturating_add(reasoning.unwrap_or(0)),
        )
    });

    UsageCounts {
        input_tokens: input,
        output_tokens: output,
        cache_creation_tokens: cache_creation,
        cache_creation_5m_tokens: None,
        cache_creation_1h_tokens: None,
        cache_read_tokens: cache_read,
        reasoning_tokens: reasoning,
        total_tokens: total,
        requests: Some(1),
        local_prompt_eval_tokens: None,
        local_eval_tokens: None,
    }
}
