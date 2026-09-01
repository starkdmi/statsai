use super::*;

pub(crate) fn grok_session_stats(
    session_dir: &Path,
    invalid_rows: &mut u64,
) -> Result<GrokSessionStats> {
    let mut stats = GrokSessionStats::default();
    parse_grok_chat_history(
        &session_dir.join("chat_history.jsonl"),
        &mut stats,
        invalid_rows,
    )?;
    parse_grok_updates(&session_dir.join("updates.jsonl"), &mut stats, invalid_rows)?;
    parse_grok_events(&session_dir.join("events.jsonl"), &mut stats, invalid_rows)?;
    Ok(stats)
}

pub(crate) fn parse_grok_unified_log(root: &Path) -> Result<GrokUnifiedLogIndex> {
    Ok(parse_grok_unified_log_with_invalid_rows(root)?.0)
}

pub(crate) fn parse_grok_unified_log_with_invalid_rows(
    root: &Path,
) -> Result<(GrokUnifiedLogIndex, u64)> {
    let mut index = GrokUnifiedLogIndex::default();
    let parse_stats = for_grok_jsonl_record(&grok_unified_log_path(root), |line, value| {
        if value.get("msg").and_then(Value::as_str) != Some("shell.turn.inference_done") {
            return Ok(());
        }
        let Some(session_id) = value.get("sid").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(ctx) = value.get("ctx") else {
            return Ok(());
        };
        let prompt_tokens = ctx.get("prompt_tokens").and_then(value_as_u64).unwrap_or(0);
        let cached_prompt_tokens = ctx
            .get("cached_prompt_tokens")
            .and_then(value_as_u64)
            .unwrap_or(0)
            .min(prompt_tokens);
        let completion_tokens = ctx
            .get("completion_tokens")
            .and_then(value_as_u64)
            .unwrap_or(0);
        let reasoning_tokens = ctx
            .get("reasoning_tokens")
            .and_then(value_as_u64)
            .unwrap_or(0);
        if prompt_tokens == 0 && completion_tokens == 0 && reasoning_tokens == 0 {
            return Ok(());
        }
        let stats = index
            .session_stats
            .entry(session_id.to_string())
            .or_default();
        let input_tokens = prompt_tokens.saturating_sub(cached_prompt_tokens);
        stats.rows += 1;
        stats.input_tokens = stats.input_tokens.saturating_add(input_tokens);
        stats.cache_read_tokens = stats.cache_read_tokens.saturating_add(cached_prompt_tokens);
        stats.output_tokens = stats.output_tokens.saturating_add(completion_tokens);
        stats.reasoning_tokens = stats.reasoning_tokens.saturating_add(reasoning_tokens);
        stats.request_samples.push(GrokInferenceSample {
            usage: GrokInferenceStats::request_sample_usage(
                input_tokens,
                cached_prompt_tokens,
                completion_tokens,
                reasoning_tokens,
            ),
            observed_at: value.get("ts").and_then(timestamp_from_scalar),
        });
        if let Some(value) = ctx.get("model_elapsed_ms").and_then(value_as_u64) {
            stats.model_elapsed_ms.push(value);
        }
        if let Some(value) = ctx.get("ttft_ms").and_then(value_as_u64) {
            stats.time_to_first_token_ms.push(value);
        }
        let row_signature = hash_text(line);
        index
            .session_signatures
            .entry(session_id.to_string())
            .and_modify(|signature| *signature = hash_text(&format!("{signature}:{row_signature}")))
            .or_insert(row_signature);
        Ok(())
    })?;
    Ok((index, parse_stats.invalid_rows))
}

pub(crate) fn parse_grok_chat_history(
    path: &Path,
    stats: &mut GrokSessionStats,
    invalid_rows: &mut u64,
) -> Result<()> {
    *invalid_rows += for_grok_jsonl_value(path, |value| {
        stats.chat_rows += 1;
        match value.get("type").and_then(Value::as_str) {
            Some("user") => stats.user_messages += 1,
            Some("assistant") => stats.assistant_messages += 1,
            Some("reasoning") => stats.reasoning_messages += 1,
            Some("tool_result") => stats.tool_result_messages += 1,
            Some("system") => stats.system_messages += 1,
            _ => {}
        }
        Ok(())
    })?
    .invalid_rows;
    Ok(())
}

pub(crate) fn parse_grok_updates(
    path: &Path,
    stats: &mut GrokSessionStats,
    invalid_rows: &mut u64,
) -> Result<()> {
    let mut prompt_context_tokens = HashMap::<String, u64>::new();
    *invalid_rows += for_grok_jsonl_value(path, |value| {
        stats.update_rows += 1;
        update_max(
            &mut stats.max_total_tokens,
            value.pointer("/params/_meta/totalTokens"),
        );
        if let (Some(prompt_id), Some(tokens)) = (
            value
                .pointer("/params/_meta/promptId")
                .and_then(Value::as_str),
            value
                .pointer("/params/_meta/totalTokens")
                .and_then(value_as_u64),
        ) {
            prompt_context_tokens
                .entry(prompt_id.to_string())
                .and_modify(|current| *current = (*current).max(tokens))
                .or_insert(tokens);
        }
        if let Some(observation) = grok_prompt_model_observation(value) {
            stats.prompt_models.push(observation);
        }
        update_max(
            &mut stats.max_tokens_used,
            value.pointer("/params/update/tokens_used"),
        );
        update_max(
            &mut stats.max_tokens_after,
            value.pointer("/params/update/tokens_after"),
        );
        Ok(())
    })?
    .invalid_rows;
    stats.prompt_count = prompt_context_tokens.len() as u64;
    stats.prompt_context_tokens = prompt_context_tokens
        .values()
        .copied()
        .reduce(u64::saturating_add);
    Ok(())
}

pub(crate) fn parse_grok_events(
    path: &Path,
    stats: &mut GrokSessionStats,
    invalid_rows: &mut u64,
) -> Result<()> {
    *invalid_rows += for_grok_jsonl_value(path, |value| {
        stats.events_rows += 1;
        if value.get("type").and_then(Value::as_str) == Some("turn_started") {
            if let Some(observation) = grok_turn_model_observation(value) {
                stats.turn_models.push(observation);
            }
        }
        Ok(())
    })?
    .invalid_rows;
    Ok(())
}
