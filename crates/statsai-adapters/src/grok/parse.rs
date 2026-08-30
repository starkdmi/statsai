use super::*;
use crate::*;

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

pub(crate) fn grok_prompt_model_observation(value: &Value) -> Option<GrokModelObservation> {
    let meta = value.pointer("/params/update/_meta")?;
    // User-prompt rows carry both modelId and promptIndex. Later stream/tool
    // chunks share promptId but omit modelId, so only this pair is a stable
    // per-prompt identity.
    let model_id = grok_nonempty_model_id(meta.get("modelId"))?;
    meta.get("promptIndex").and_then(value_as_u64)?;
    Some(GrokModelObservation {
        model_id,
        observed_at: grok_update_timestamp(value),
    })
}

pub(crate) fn grok_turn_model_observation(value: &Value) -> Option<GrokModelObservation> {
    Some(GrokModelObservation {
        model_id: grok_nonempty_model_id(value.get("model_id"))?,
        observed_at: value.get("ts").and_then(timestamp_from_scalar),
    })
}

pub(crate) fn grok_update_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .pointer("/params/_meta/agentTimestampMs")
        .and_then(timestamp_from_scalar)
        .or_else(|| value.get("timestamp").and_then(timestamp_from_scalar))
}

pub(crate) fn grok_nonempty_model_id(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model_id| !model_id.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn grok_signals_models_used(signals: Option<&Value>) -> Vec<String> {
    signals
        .and_then(|signals| signals.get("modelsUsed"))
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|value| grok_nonempty_model_id(Some(value)))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn grok_normalized_model_id(model_id: &str) -> String {
    normalize_model_name(model_id)
}

pub(crate) fn grok_models_equivalent(left: &str, right: &str) -> bool {
    grok_normalized_model_id(left) == grok_normalized_model_id(right)
}

pub(crate) fn unique_grok_normalized_models<'a>(
    ids: impl IntoIterator<Item = &'a str>,
) -> HashSet<String> {
    ids.into_iter()
        .map(str::trim)
        .filter(|model_id| !model_id.is_empty())
        .map(grok_normalized_model_id)
        .collect()
}

pub(crate) fn grok_current_model_id(model: Option<&ModelInfo>) -> Option<&str> {
    model.and_then(|model| {
        model
            .name
            .as_deref()
            .or(model.provider_model_id.as_deref())
            .map(str::trim)
            .filter(|model_id| !model_id.is_empty())
    })
}

pub(crate) fn last_grok_model_at_or_before(
    observations: &[GrokModelObservation],
    at: DateTime<Utc>,
) -> Option<&str> {
    observations
        .iter()
        .enumerate()
        .filter(|(_, observation)| {
            observation
                .observed_at
                .is_some_and(|observed_at| observed_at <= at)
        })
        .max_by_key(|(index, observation)| (observation.observed_at, *index))
        .map(|(_, observation)| observation.model_id.as_str())
}

pub(crate) fn resolve_grok_inference_sample_model(
    sample: &GrokInferenceSample,
    prompt_models: &[GrokModelObservation],
    turn_models: &[GrokModelObservation],
    session_models_used: &[String],
    current_model: Option<&ModelInfo>,
) -> Option<ModelInfo> {
    let assignable_ids = prompt_models
        .iter()
        .map(|observation| observation.model_id.as_str())
        .chain(
            turn_models
                .iter()
                .map(|observation| observation.model_id.as_str()),
        );
    let assignable = unique_grok_normalized_models(assignable_ids);
    if assignable.len() == 1 {
        let models_used =
            unique_grok_normalized_models(session_models_used.iter().map(String::as_str));
        // A lone prompt/turn observation cannot cover every inference when
        // modelsUsed reports another model: request-level attribution is
        // incomplete, so do not silently price the missing model as this one.
        if !models_used.is_empty() && models_used != assignable {
            return None;
        }
        let model_id = prompt_models
            .iter()
            .map(|observation| observation.model_id.as_str())
            .chain(
                turn_models
                    .iter()
                    .map(|observation| observation.model_id.as_str()),
            )
            .next()?;
        return Some(model_info(model_id));
    }
    if assignable.len() >= 2 {
        let observed_at = sample.observed_at?;
        let from_prompt = last_grok_model_at_or_before(prompt_models, observed_at);
        let from_turn = last_grok_model_at_or_before(turn_models, observed_at);
        return match (from_prompt, from_turn) {
            (Some(prompt), Some(turn)) if grok_models_equivalent(prompt, turn) => {
                Some(model_info(prompt))
            }
            (Some(_prompt), Some(_turn)) => None,
            (Some(prompt), None) => Some(model_info(prompt)),
            (None, Some(turn)) => Some(model_info(turn)),
            (None, None) => None,
        };
    }

    let session_ids = session_models_used
        .iter()
        .map(String::as_str)
        .chain(grok_current_model_id(current_model));
    if unique_grok_normalized_models(session_ids).len() == 1 {
        return current_model.cloned().or_else(|| {
            session_models_used
                .first()
                .map(|model_id| model_info(model_id))
        });
    }
    None
}

pub(crate) fn for_grok_jsonl_record(
    path: &Path,
    mut visit: impl FnMut(&str, &Value) -> Result<()>,
) -> Result<GrokJsonlParseStats> {
    if !path.is_file() {
        return Ok(GrokJsonlParseStats::default());
    }
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut parse_stats = GrokJsonlParseStats::default();
    let mut line_bytes = Vec::new();
    let mut index = 0usize;
    loop {
        let line_status =
            read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)
                .with_context(|| format!("read {} line {}", path.display(), index + 1))?;
        if line_status == BoundedLineRead::Eof {
            break;
        }
        index = index.saturating_add(1);
        if line_status == BoundedLineRead::Oversized {
            parse_stats.invalid_rows += 1;
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            parse_stats.invalid_rows += 1;
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                parse_stats.invalid_rows += 1;
                continue;
            }
        };
        parse_stats.rows += 1;
        visit(trimmed, &value)?;
    }
    Ok(parse_stats)
}

pub(crate) fn for_grok_jsonl_value(
    path: &Path,
    mut visit: impl FnMut(&Value) -> Result<()>,
) -> Result<GrokJsonlParseStats> {
    for_grok_jsonl_record(path, |_line, value| visit(value))
}

pub(crate) fn grok_session_id_from_summary_path(summary_path: &Path) -> Option<String> {
    read_json_file(summary_path)
        .as_ref()
        .and_then(|value| grok_session_id_from_summary_value(value, summary_path))
        .or_else(|| {
            summary_path
                .parent()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        })
}

pub(crate) fn grok_session_id_from_summary_value(
    value: &Value,
    summary_path: &Path,
) -> Option<String> {
    value
        .pointer("/info/id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            summary_path
                .parent()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        })
}

pub(crate) fn update_max(target: &mut Option<u64>, value: Option<&Value>) {
    if let Some(value) = value.and_then(value_as_u64) {
        *target = Some(target.unwrap_or(0).max(value));
    }
}

pub(crate) fn estimate_grok_inference_sample_costs(
    provider: &str,
    current_model: Option<&ModelInfo>,
    samples: &[GrokInferenceSample],
    prompt_models: &[GrokModelObservation],
    turn_models: &[GrokModelObservation],
    session_models_used: &[String],
    fallback_observed_at: &DateTime<Utc>,
) -> CostInfo {
    if samples.is_empty() {
        return unknown_cost();
    }
    let mut total = CostAccumulator::default();
    let mut representative = None;
    let mut pricing_sources = HashSet::new();
    for sample in samples {
        let Some(model) = resolve_grok_inference_sample_model(
            sample,
            prompt_models,
            turn_models,
            session_models_used,
            current_model,
        ) else {
            return unknown_cost();
        };
        let occurred_at = sample.observed_at.as_ref().unwrap_or(fallback_observed_at);
        let cost = estimate_cost_at(provider, Some(&model), &sample.usage, occurred_at);
        if cost.estimated_micro_usd().is_none() {
            return unknown_cost();
        }
        if let Some(source) = &cost.pricing_source {
            pricing_sources.insert(source.clone());
        }
        if representative.is_none() {
            representative = Some(cost.clone());
        }
        total.add_estimated(&cost);
    }
    let Some(mut cost) = representative else {
        return unknown_cost();
    };
    let Some(micro_usd) = total.micro_usd() else {
        return unknown_cost();
    };
    cost.set_estimated_micro_usd(micro_usd);
    if pricing_sources.len() > 1 {
        cost.pricing_source = Some("xai_api_pricing:mixed".to_string());
    }
    cost
}

pub(crate) fn parse_grok_summary(
    adapter: &GrokBuildAdapter,
    source: &SourceLocation,
    options: &ScanOptions,
    summary_path: &Path,
    unified_session_stats: &HashMap<String, GrokInferenceStats>,
    scan: &mut AdapterScan,
) -> Result<()> {
    let text = std::fs::read_to_string(summary_path)
        .with_context(|| format!("read {}", summary_path.display()))?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", summary_path.display()))?;
    scan.diagnostics.raw_rows += 1;
    let session_id = grok_session_id_from_summary_value(&value, summary_path)
        .unwrap_or_else(|| "unknown".to_string());
    let observed_at = value
        .get("updated_at")
        .and_then(timestamp_from_scalar)
        .or_else(|| file_modified_timestamp(summary_path))
        .unwrap_or_else(Utc::now);
    let model = string_at_any(&value, &["current_model_id"]).map(|model| model_info(&model));
    let session_dir = summary_path.parent();
    let signals = session_dir
        .map(|parent| parent.join("signals.json"))
        .and_then(|path| read_json_file(&path).map(|value| (path, value)));
    let stats = session_dir
        .map(|path| grok_session_stats(path, &mut scan.diagnostics.invalid_rows))
        .transpose()?
        .unwrap_or_default();
    let inference_stats = unified_session_stats
        .get(session_id.as_str())
        .cloned()
        .unwrap_or_default();
    let signal_value = signals.as_ref().map(|(_, signals)| signals);
    let session_models_used = grok_signals_models_used(signal_value);
    let total_messages = value
        .get("num_messages")
        .and_then(value_as_u64)
        .or_else(|| {
            let total = stats.total_chat_messages();
            (total > 0).then_some(total)
        });
    let user_messages = signal_value
        .and_then(|signals| signals.get("userMessageCount"))
        .and_then(value_as_u64)
        .or_else(|| (stats.user_messages > 0).then_some(stats.user_messages));
    let assistant_messages = signal_value
        .and_then(|signals| signals.get("assistantMessageCount"))
        .and_then(value_as_u64)
        .or_else(|| (stats.assistant_messages > 0).then_some(stats.assistant_messages));
    let usage = if inference_stats.has_usage() {
        inference_stats.usage_counts()
    } else {
        UsageCounts {
            input_tokens: stats.usage_context_tokens(signal_value),
            output_tokens: None,
            cache_creation_tokens: None,
            cache_creation_5m_tokens: None,
            cache_creation_1h_tokens: None,
            cache_read_tokens: None,
            reasoning_tokens: None,
            total_tokens: stats.usage_context_tokens(signal_value),
            requests: signal_value
                .and_then(|signals| signals.get("turnCount"))
                .and_then(value_as_u64)
                .or_else(|| (stats.prompt_count > 0).then_some(stats.prompt_count)),
            local_prompt_eval_tokens: None,
            local_eval_tokens: None,
        }
    };
    let runtime = signals.as_ref().map(|(_, signals)| RuntimeInfo {
        runtime_name: Some("grok-build".to_string()),
        host_id: None,
        latency_ms: signals
            .get("sessionDurationSeconds")
            .and_then(value_as_u64)
            .map(|seconds| seconds * 1000),
        latency_source: signals
            .get("sessionDurationSeconds")
            .and_then(value_as_u64)
            .map(|_| LatencySource::Explicit),
        time_to_first_token_ms: signals.get("avgTimeToFirstTokenMs").and_then(value_as_u64),
        prompt_eval_duration_ms: None,
        eval_duration_ms: None,
        total_messages,
        user_messages,
        assistant_messages,
        developer_messages: None,
    });
    let project = value
        .pointer("/info/cwd")
        .and_then(Value::as_str)
        .map(expand_home_path)
        .and_then(|path| {
            resolve_project_context(
                Some(path),
                value
                    .get("git_remotes")
                    .and_then(Value::as_array)
                    .and_then(|remotes| remotes.first())
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                value
                    .get("head_branch")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            )
        });
    let summary_version = value
        .get("chat_format_version")
        .and_then(value_as_u64)
        .map(|value| {
            format!(
                "{value};chat_rows={};updates={};events={};reasoning={};tool_results={};system={};token_footprint={}",
                stats.chat_rows,
                stats.update_rows,
                stats.events_rows,
                stats.reasoning_messages,
                stats.tool_result_messages,
                stats.system_messages,
                stats.token_footprint(signal_value).unwrap_or(0)
            )
        });
    let summary_version = summary_version.map(|version| {
        format!(
            "{version};prompts={};prompt_context_tokens={};inference_rows={};usage_source={}",
            stats.prompt_count,
            stats.prompt_context_tokens.unwrap_or(0),
            inference_stats.rows,
            if inference_stats.has_usage() {
                "unified_log"
            } else {
                "session_context"
            }
        )
    });
    let mut summary = metadata_summary(
        adapter,
        source,
        options,
        MetadataSummaryParts {
            source_file: summary_path,
            summary_format: "grok_build_session_summary",
            semantic_key: &format!("grok_build_session_summary.v1:{session_id}"),
            observed_at,
            metadata: SummaryMetadata {
                summary_format: "grok_build_session_summary".to_string(),
                summary_version,
                total_sessions: Some(1),
                total_messages,
                last_computed_at: Some(observed_at),
            },
            model,
            runtime,
            project,
        },
    );
    summary.usage = usage;
    summary.cost = if inference_stats.has_usage() && !inference_stats.request_samples.is_empty() {
        estimate_grok_inference_sample_costs(
            adapter.provider(),
            summary.model.as_ref(),
            &inference_stats.request_samples,
            &stats.prompt_models,
            &stats.turn_models,
            &session_models_used,
            &summary.observed_at,
        )
    } else if unique_grok_normalized_models(
        session_models_used
            .iter()
            .map(String::as_str)
            .chain(grok_current_model_id(summary.model.as_ref())),
    )
    .len()
        > 1
    {
        unknown_cost()
    } else {
        estimate_cost_at(
            adapter.provider(),
            summary.model.as_ref(),
            &summary.usage,
            &summary.observed_at,
        )
    };
    if summary.cost.estimated_api_equivalent_usd.is_some() {
        if inference_stats.has_usage() {
            summary.cost.confidence = Confidence::Medium;
            summary.cost.pricing_source = summary
                .cost
                .pricing_source
                .map(|source| format!("{source}:unified_log_inference_usage"));
        } else {
            summary.cost.confidence = Confidence::Low;
            summary.cost.pricing_source = summary
                .cost
                .pricing_source
                .map(|source| format!("{source}:prompt_context_token_footprint"));
        }
    }
    if let Some(metrics) = summary.metrics.as_mut() {
        metrics.tracked_requests = metrics.tracked_requests.or(summary.usage.requests);
        metrics.total_messages = metrics.total_messages.or(total_messages);
        metrics.user_messages = metrics.user_messages.or(user_messages);
        metrics.assistant_messages = metrics.assistant_messages.or(assistant_messages);
        metrics.tracked_output_tokens = metrics
            .tracked_output_tokens
            .or(summary.usage.output_tokens);
        metrics.tracked_reasoning_tokens = metrics
            .tracked_reasoning_tokens
            .or(summary.usage.reasoning_tokens);
        if inference_stats.has_usage() {
            metrics.latency_ms = metric_from_samples(&inference_stats.model_elapsed_ms);
            metrics.time_to_first_token_ms =
                metric_from_samples(&inference_stats.time_to_first_token_ms);
        }
        if metrics.latency_ms.is_none() {
            metrics.latency_ms = signal_value
                .and_then(|signals| signals.get("avgResponseTimeMs"))
                .and_then(value_as_u64)
                .map(metric_single_sample);
        }
    } else {
        summary.metrics = Some(SummaryMetrics {
            active_seconds: signal_value
                .and_then(|signals| signals.get("sessionDurationSeconds"))
                .and_then(value_as_u64)
                .map(|value| value as f64),
            tracked_requests: summary.usage.requests,
            tracked_output_tokens: summary.usage.output_tokens,
            tracked_reasoning_tokens: summary.usage.reasoning_tokens,
            latency_ms: metric_from_samples(&inference_stats.model_elapsed_ms).or_else(|| {
                signal_value
                    .and_then(|signals| signals.get("avgResponseTimeMs"))
                    .and_then(value_as_u64)
                    .map(metric_single_sample)
            }),
            time_to_first_token_ms: metric_from_samples(&inference_stats.time_to_first_token_ms)
                .or_else(|| {
                    signal_value
                        .and_then(|signals| signals.get("avgTimeToFirstTokenMs"))
                        .and_then(value_as_u64)
                        .map(metric_single_sample)
                }),
            generated_tps: None,
            visible_tps: None,
            overall_generated_tps: None,
            overall_visible_tps: None,
            cache_hit_ratio: None,
            reasoning_share: None,
            total_messages,
            user_messages,
            assistant_messages,
            developer_messages: None,
        });
    }
    scan.diagnostics.raw_rows += stats
        .chat_rows
        .saturating_add(stats.update_rows)
        .saturating_add(stats.events_rows)
        .saturating_add(inference_stats.rows);
    scan.diagnostics.candidate_usage_rows += summary.usage.requests.unwrap_or(0);
    if options.should_collect_tasks() {
        let generated_title = value
            .get("generated_title")
            .and_then(Value::as_str)
            .and_then(|value| summarize_task_text(Some(value), 90));
        let session_summary = value
            .get("session_summary")
            .and_then(Value::as_str)
            .and_then(|value| summarize_task_text(Some(value), 220));
        let title = generated_title
            .clone()
            .or_else(|| task_title_from_prompt(session_summary.as_deref()))
            .unwrap_or_else(|| format!("Grok session {session_id}"));
        let issue_keys = extract_issue_keys(&[
            title.as_str(),
            session_summary.as_deref().unwrap_or(""),
            summary
                .project
                .as_ref()
                .and_then(|project| project.branch_label.as_deref())
                .unwrap_or(""),
        ]);
        scan.task_spans.push(TaskSpan {
            schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
            span_id: task_span_id(
                adapter.provider(),
                &source.source_id,
                &format!(
                    "grok_task_span.v1:{session_id}:{}",
                    observed_at.to_rfc3339()
                ),
            ),
            provider: adapter.provider().to_string(),
            source_id: source.source_id.clone(),
            span_kind: "grok_session_summary".to_string(),
            source_record_id: Some(session_id.clone()),
            source_file_path_hash: Some(hash_text(&canonical_display(summary_path))),
            summary_id: Some(summary.summary_id.clone()),
            session_id: Some(session_id.clone()),
            thread_id: None,
            title: title.clone(),
            normalized_title: normalize_task_title(&title),
            title_source: Some(
                if generated_title.is_some() {
                    "generated_title"
                } else if session_summary.is_some() {
                    "session_summary"
                } else {
                    "default"
                }
                .to_string(),
            ),
            summary_preview: session_summary.clone(),
            todo_excerpt: None,
            issue_keys,
            branch_family: branch_family(
                summary
                    .project
                    .as_ref()
                    .and_then(|project| project.branch_label.as_deref()),
            ),
            project_bucket: project_bucket_key(summary.project.as_ref()),
            project: summary.project.clone(),
            git: None,
            usage: summary.usage.clone(),
            estimated_cost_usd: summary.cost.estimated_api_equivalent_usd,
            estimated_cost_micro_usd: summary.cost.estimated_api_equivalent_micro_usd,
            event_count: 0,
            has_usage_evidence: false,
            total_messages: summary
                .metrics
                .as_ref()
                .and_then(|metrics| metrics.total_messages)
                .unwrap_or(0),
            user_messages: summary
                .metrics
                .as_ref()
                .and_then(|metrics| metrics.user_messages)
                .unwrap_or(0),
            assistant_messages: summary
                .metrics
                .as_ref()
                .and_then(|metrics| metrics.assistant_messages)
                .unwrap_or(0),
            developer_messages: summary
                .metrics
                .as_ref()
                .and_then(|metrics| metrics.developer_messages)
                .unwrap_or(0),
            linked_event_ids: Vec::new(),
            confidence: if generated_title.is_some() {
                Confidence::High
            } else if session_summary.is_some() {
                Confidence::Medium
            } else {
                Confidence::Low
            },
            is_meta: task_title_is_generic(Some(title.as_str())),
            started_at: value
                .get("created_at")
                .and_then(timestamp_from_scalar)
                .unwrap_or(observed_at),
            ended_at: Some(observed_at),
            duration_seconds: summary
                .metrics
                .as_ref()
                .and_then(|metrics| metrics.active_seconds)
                .map(|seconds| seconds as u64),
        });
    }
    scan.summaries.push(summary);
    Ok(())
}
