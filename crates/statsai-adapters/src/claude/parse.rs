use super::{ClaudeCodeAdapter, ClaudeSessionProjectMetadata};
use crate::model::{
    claude_reasoning_state_from_value, claude_speed_from_usage, model_from_nested_value,
    model_info, with_model_metadata, ModelReasoningState,
};
use crate::ProviderAdapter;
use crate::{
    fallback_session_id, file_modified_timestamp, infer_missing_output, metadata_only_privacy,
    number_at_any, project_context_from_path_fallback, push_deduped, read_bounded_jsonl_line,
    resolve_project_context, resolve_project_context_cached, stats_cache_date_end,
    timestamp_from_nested_value, timestamp_from_scalar, usage_event, usd_to_micro_usd,
    value_as_u64, AdapterScan, BoundedLineRead, DuplicateSelection, EventDeduplication,
    FileParseContext, ProjectContextCache, ProviderEventParts, ScanOptions, MAX_JSONL_RECORD_BYTES,
};
use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use statsai_core::{
    canonical_display, expand_home_path, hash_text, summary_id, Confidence, EventSource,
    IdentitySource, ParseEvidence, ProjectInfo, SourceKind, SourceLocation, SummaryMetadata,
    UsageCounts, UsageSummary, USAGE_SUMMARY_SCHEMA_VERSION,
};
use statsai_pricing::{estimate_cost_at, pricing_changes_between, unknown_cost};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub(crate) fn parse_claude_file(
    ctx: &mut FileParseContext<'_, ClaudeCodeAdapter>,
    projects: &Path,
    session_projects: &HashMap<String, ClaudeSessionProjectMetadata>,
    path: &Path,
) -> Result<()> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let fallback_timestamp = file_modified_timestamp(path).unwrap_or_else(Utc::now);
    let indexed_project_metadata = claude_session_metadata_for_file(session_projects, path);
    let fallback_project = claude_project_context_for_file(session_projects, projects, path);
    let mut project_cache = ProjectContextCache::new();
    let mut current_reasoning = ModelReasoningState::default();

    let mut line_bytes = Vec::new();
    let mut index = 0usize;
    loop {
        let line_status =
            read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
        if line_status == BoundedLineRead::Eof {
            break;
        }
        index = index.saturating_add(1);
        if line_status == BoundedLineRead::Oversized {
            ctx.scan.diagnostics.raw_rows += 1;
            ctx.scan.diagnostics.invalid_rows += 1;
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            ctx.scan.diagnostics.raw_rows += 1;
            ctx.scan.diagnostics.invalid_rows += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        ctx.scan.diagnostics.raw_rows += 1;
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            ctx.scan.diagnostics.invalid_rows += 1;
            continue;
        };
        let reasoning = claude_reasoning_state_from_value(&value);
        if reasoning.raw.is_some() {
            current_reasoning = reasoning;
        }
        let Some(usage_value) = value
            .pointer("/message/usage")
            .or_else(|| value.get("usage"))
        else {
            continue;
        };
        ctx.scan.diagnostics.candidate_usage_rows += 1;
        let usage = claude_usage_counts_from_value(usage_value);
        if usage.computed_total() == 0 {
            ctx.scan.diagnostics.skipped_zero_events += 1;
            continue;
        }
        let (timestamp, timestamp_inferred) = timestamp_from_nested_value(&value)
            .map(|timestamp| (timestamp, false))
            .unwrap_or((fallback_timestamp, true));
        if timestamp_inferred {
            ctx.scan.diagnostics.timestamp_fallbacks += 1;
        }
        let model = with_model_metadata(
            model_from_nested_value(&value, None),
            &current_reasoning,
            claude_speed_from_usage(usage_value),
        );
        let model_inferred = model.is_none();
        if model_inferred {
            ctx.scan.diagnostics.model_fallbacks += 1;
        }
        let project =
            claude_project_context_from_value(&value, indexed_project_metadata, &mut project_cache)
                .or_else(|| fallback_project.clone());
        let session_raw = value
            .get("sessionId")
            .or_else(|| value.get("session_id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| fallback_session_id(path));
        // Claude Code rewrites one streamed response as several records that
        // share a provider-record identity while cumulative usage grows, so the
        // last snapshot for that identity is the authoritative one. Records
        // without a provider-record identity keep first-wins selection.
        let (deduplication, duplicate_selection) = claude_provider_record_id(&value).map_or(
            (
                EventDeduplication::SessionScoped,
                DuplicateSelection::KeepFirst,
            ),
            |record_id| {
                (
                    EventDeduplication::ProviderRecord(record_id),
                    DuplicateSelection::KeepLatestSnapshot,
                )
            },
        );
        let event = usage_event(
            ctx.adapter,
            ctx.source,
            ctx.options,
            ProviderEventParts {
                timestamp,
                session_started_at: None,
                session_ended_at: None,
                duration_seconds: None,
                model,
                usage,
                runtime: None,
                session_raw,
                project,
                event_kind: "claude_message_usage",
                source_file: path,
                source_line_number: Some(index),
                source_type: "jsonl",
                model_inferred,
                timestamp_inferred,
                deduplication,
                dedupe_salt: None,
            },
        );
        push_deduped(ctx.scan, ctx.seen, event, duplicate_selection);
    }

    Ok(())
}

pub(crate) fn claude_provider_record_id(value: &Value) -> Option<String> {
    let message_id = value
        .pointer("/message/id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())?;
    let request_id = value
        .get("requestId")
        .or_else(|| value.get("request_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())?;
    Some(format!("{message_id}\0{request_id}"))
}

pub(crate) fn claude_project_context_from_value(
    value: &Value,
    indexed_metadata: Option<&ClaudeSessionProjectMetadata>,
    cache: &mut ProjectContextCache,
) -> Option<ProjectInfo> {
    let project_path = value
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .or_else(|| {
            value
                .get("projectPath")
                .and_then(Value::as_str)
                .filter(|path| !path.trim().is_empty())
        })
        .map(expand_home_path)?;
    let branch = value
        .get("gitBranch")
        .and_then(Value::as_str)
        .filter(|branch| !branch.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            indexed_metadata
                .filter(|metadata| {
                    metadata
                        .project_path
                        .as_deref()
                        .is_some_and(|indexed_path| {
                            canonical_display(indexed_path) == canonical_display(&project_path)
                        })
                })
                .and_then(|metadata| metadata.git_branch.clone())
        });

    resolve_project_context_cached(Some(project_path), None, branch, cache)
}

pub(crate) fn claude_project_context_for_file(
    session_projects: &HashMap<String, ClaudeSessionProjectMetadata>,
    projects_root: &Path,
    path: &Path,
) -> Option<ProjectInfo> {
    claude_session_metadata_for_file(session_projects, path)
        .and_then(|metadata| {
            resolve_project_context(
                metadata.project_path.clone(),
                None,
                metadata.git_branch.clone(),
            )
        })
        .or_else(|| project_context_from_path_fallback(projects_root, path))
}

pub(crate) fn claude_session_metadata_for_file<'a>(
    session_projects: &'a HashMap<String, ClaudeSessionProjectMetadata>,
    path: &Path,
) -> Option<&'a ClaudeSessionProjectMetadata> {
    let canonical_path = canonical_display(path);
    if let Some(metadata) = session_projects.get(&canonical_path) {
        return Some(metadata);
    }

    path.ancestors()
        .skip(1)
        .find_map(|ancestor| session_projects.get(&canonical_display(ancestor)))
}

pub(crate) fn parse_claude_stats_cache(
    adapter: &ClaudeCodeAdapter,
    source: &SourceLocation,
    options: &ScanOptions,
    path: &Path,
    scan: &mut AdapterScan,
) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let Some(model_usage) = value.get("modelUsage").and_then(Value::as_object) else {
        scan.diagnostics.invalid_rows += 1;
        return Ok(());
    };

    let period_start = value
        .get("firstSessionDate")
        .and_then(timestamp_from_scalar);
    let period_end = value.get("lastComputedDate").and_then(stats_cache_date_end);
    let observed_at = period_end
        .or_else(|| file_modified_timestamp(path))
        .unwrap_or_else(Utc::now);
    let metadata = SummaryMetadata {
        summary_format: "claude_stats_cache".to_string(),
        summary_version: value
            .get("version")
            .and_then(value_as_u64)
            .map(|value| value.to_string()),
        total_sessions: value.get("totalSessions").and_then(value_as_u64),
        total_messages: value.get("totalMessages").and_then(value_as_u64),
        last_computed_at: period_end,
    };
    let file_path_hash = hash_text(&canonical_display(path));

    for (model_name, usage_value) in model_usage {
        scan.diagnostics.candidate_usage_rows += 1;
        let usage = claude_usage_counts_from_value(usage_value);
        if usage.computed_total() == 0 {
            scan.diagnostics.skipped_zero_events += 1;
            continue;
        }
        let model = model_info(model_name);
        let semantic_key = format!(
            "claude_stats_cache.v1:{}:{}:{}:{}:{}:{}:{}:{}",
            model_name,
            period_start
                .map(|date| date.to_rfc3339())
                .unwrap_or_else(|| "unknown_start".to_string()),
            period_end
                .map(|date| date.to_rfc3339())
                .unwrap_or_else(|| "unknown_end".to_string()),
            usage.input_tokens.unwrap_or(0),
            usage.cache_read_tokens.unwrap_or(0),
            usage.cache_creation_tokens.unwrap_or(0),
            usage.output_tokens.unwrap_or(0),
            usage.computed_total(),
        );
        let pricing_date = period_end.unwrap_or(observed_at);
        let crosses_pricing_boundary = period_start
            .zip(period_end)
            .and_then(|(start, end)| {
                model
                    .normalized_name
                    .as_deref()
                    .or(model.name.as_deref())
                    .map(|model_name| {
                        pricing_changes_between(model_name, start.date_naive(), end.date_naive())
                    })
            })
            .unwrap_or(false);
        let mut cost = if crosses_pricing_boundary {
            unknown_cost()
        } else {
            estimate_cost_at(adapter.provider(), Some(&model), &usage, &pricing_date)
        };
        if let Some(provider_cost) = usage_value
            .get("costUSD")
            .and_then(Value::as_f64)
            .filter(|cost| *cost > 0.0)
        {
            if let Some(provider_cost_micro_usd) = usd_to_micro_usd(provider_cost) {
                cost.set_provider_reported_micro_usd(provider_cost_micro_usd);
            }
            cost.pricing_source = Some("claude_stats_cache:costUSD".to_string());
            cost.confidence = Confidence::Medium;
        }
        scan.summaries.push(UsageSummary {
            schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: summary_id(adapter.provider(), &source.source_id, &semantic_key),
            device_id: options.device_id.clone(),
            provider: adapter.provider().to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: None,
            source: EventSource {
                adapter_id: adapter.id().to_string(),
                adapter_version: adapter.version().to_string(),
                source_kind: SourceKind::LocalSummary,
                location_origin: Some(source.location_origin.clone()),
                source_type: "stats-cache.json".to_string(),
                source_path_hash: Some(file_path_hash.clone()),
                source_record_id: Some(format!("summary_key_{}", &hash_text(&semantic_key)[..32])),
                parse_confidence: Confidence::Medium,
            },
            model: Some(model),
            models: Vec::new(),
            usage,
            cost,
            parse_evidence: Some(ParseEvidence {
                event_key_version: "claude_stats_cache_summary.v1".to_string(),
                source_file_path_hash: Some(file_path_hash.clone()),
                source_line_number: None,
                source_record_id: Some(semantic_key),
                model_inferred: false,
                timestamp_inferred: period_start.is_none() || period_end.is_none(),
                account_identity_source: IdentitySource::Unresolved,
            }),
            project: None,
            privacy: metadata_only_privacy(),
            metrics: None,
            period_start,
            period_end,
            observed_at,
            metadata: metadata.clone(),
            imported_at: Utc::now(),
        });
    }

    Ok(())
}

pub(crate) fn claude_usage_counts_from_value(value: &Value) -> UsageCounts {
    let input = number_at_any(
        value,
        &[
            "input_tokens",
            "inputTokens",
            "prompt_tokens",
            "promptTokens",
            "input",
        ],
    );
    let output = number_at_any(
        value,
        &[
            "output_tokens",
            "outputTokens",
            "completion_tokens",
            "completionTokens",
            "output",
        ],
    );
    let reported_cache_creation = number_at_any(
        value,
        &[
            "cache_creation_input_tokens",
            "cacheCreationInputTokens",
            "cacheCreationTokens",
            "cache_creation_tokens",
        ],
    );
    let cache_creation_5m = value
        .pointer("/cache_creation/ephemeral_5m_input_tokens")
        .and_then(value_as_u64)
        .or_else(|| {
            value
                .pointer("/cacheCreation/ephemeral5mInputTokens")
                .and_then(value_as_u64)
        });
    let cache_creation_1h = value
        .pointer("/cache_creation/ephemeral_1h_input_tokens")
        .and_then(value_as_u64)
        .or_else(|| {
            value
                .pointer("/cacheCreation/ephemeral1hInputTokens")
                .and_then(value_as_u64)
        });
    let cache_creation = reported_cache_creation.or_else(|| {
        (cache_creation_5m.is_some() || cache_creation_1h.is_some()).then_some(
            cache_creation_5m
                .unwrap_or(0)
                .saturating_add(cache_creation_1h.unwrap_or(0)),
        )
    });
    let cache_read = number_at_any(
        value,
        &[
            "cache_read_input_tokens",
            "cacheReadInputTokens",
            "cacheReadTokens",
            "cache_read_tokens",
            "cached_input_tokens",
        ],
    );
    let reasoning = number_at_any(
        value,
        &[
            "reasoning_tokens",
            "reasoningTokens",
            "reasoning_output_tokens",
            "reasoningOutputTokens",
        ],
    );
    let total = number_at_any(value, &["total_tokens", "totalTokens", "total"]);
    let output = output
        .or_else(|| infer_missing_output(total, input, cache_creation, cache_read, reasoning));

    UsageCounts {
        input_tokens: input,
        output_tokens: output,
        cache_creation_tokens: cache_creation,
        cache_creation_5m_tokens: cache_creation_5m,
        cache_creation_1h_tokens: cache_creation_1h,
        cache_read_tokens: cache_read,
        reasoning_tokens: reasoning,
        total_tokens: total,
        requests: Some(1),
        local_prompt_eval_tokens: None,
        local_eval_tokens: None,
    }
}
