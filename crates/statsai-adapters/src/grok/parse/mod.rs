use super::*;
use crate::*;

mod jsonl;
mod models;
mod session;

pub(crate) use jsonl::*;
pub(crate) use models::*;
pub(crate) use session::*;

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
