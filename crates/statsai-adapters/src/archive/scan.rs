use super::super::{
    canonical_display, codex_project_context_from_value, codex_quota_observation,
    codex_usage_counts_from_value, codex_usage_roots, collect_jsonl_files, expand_home_path,
    file_modified_timestamp, grok_sessions_root, model_from_nested_value, open_sqlite_readonly,
    read_bounded_jsonl_line, resolve_project_context, source_root_path, subtract_usage_counts,
    timestamp_from_nested_value, BoundedLineRead, ProjectContextCache, CLAUDE_CODE_PROVIDER,
    CODEX_PROVIDER, GROK_BUILD_PROVIDER, MAX_JSONL_RECORD_BYTES, OPENCODE_PROVIDER,
};
use super::mutations::{
    finish_original_mutation, mark_unresolved_mutations, record_unmeasurable_mutation,
    remember_original_mutation, MutationCompletion, MutationInvocation,
};
use super::*;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;
use serde_json::Value;
use statsai_core::{
    ArchiveItemKind, ArchiveRole, CoverageStatus, ModelInfo, ProjectInfo, QuotaObservationRecordV1,
    SourceLocation, UsageCounts,
};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

pub(crate) fn collect_codex(
    source: &SourceLocation,
    selected_cache_keys: Option<&HashSet<String>>,
) -> Result<ArchiveScan> {
    let Some(path_label) = source.path_label.as_deref() else {
        return Ok(ArchiveScan::default());
    };
    let source_path = PathBuf::from(path_label);
    let paths = selected_jsonl_paths(selected_cache_keys, || {
        codex_usage_roots(&source_path)
            .into_iter()
            .flat_map(|path| collect_jsonl_files(&path).unwrap_or_default())
            .collect()
    });
    let mut scan = ArchiveScan::measured();
    let mut artifact_dependencies = ArtifactDependencyMap::new();
    for path in paths {
        scan.diagnostics.files_scanned += 1;
        scan.conversations.push(collect_codex_file(
            source,
            &path,
            &mut scan.diagnostics,
            &mut artifact_dependencies,
            &mut scan.trace_edits,
            &mut scan.trace_coverage,
        )?);
        scan.quota_observations
            .extend(collect_codex_quota_observations(source, &path)?);
    }
    scan.artifact_dependencies = finish_artifact_dependencies(artifact_dependencies);
    finish_diagnostics(&mut scan);
    Ok(scan)
}

pub(crate) fn collect_codex_quota_observations(
    source: &SourceLocation,
    path: &Path,
) -> Result<Vec<QuotaObservationRecordV1>> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let fallback_timestamp = file_modified_timestamp(path).unwrap_or_else(Utc::now);
    let mut previous_totals: Option<UsageCounts> = None;
    let mut observations = Vec::new();
    let mut line_bytes = Vec::new();
    let mut line_number = 0usize;
    loop {
        let status = read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
        if status == BoundedLineRead::Eof {
            break;
        }
        line_number = line_number.saturating_add(1);
        if status == BoundedLineRead::Oversized {
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("event_msg")
            || value.pointer("/payload/type").and_then(Value::as_str) != Some("token_count")
        {
            continue;
        }
        let info = value.pointer("/payload/info");
        let total_usage = info
            .and_then(|info| info.get("total_token_usage"))
            .map(codex_usage_counts_from_value);
        let usage_sample = info
            .and_then(|info| info.get("last_token_usage"))
            .map(codex_usage_counts_from_value)
            .or_else(|| {
                total_usage
                    .as_ref()
                    .map(|total| subtract_usage_counts(total, previous_totals.as_ref()))
            });
        if let Some(total_usage) = total_usage {
            previous_totals = Some(total_usage);
        }
        let observed_at = timestamp_from_nested_value(&value).unwrap_or(fallback_timestamp);
        if let Some(observation) =
            codex_quota_observation(source, path, line_number, observed_at, usage_sample, &value)
        {
            observations.push(observation);
        }
    }
    Ok(observations)
}

pub(crate) fn collect_codex_file(
    source: &SourceLocation,
    path: &Path,
    diagnostics: &mut ArchiveScanDiagnostics,
    artifact_dependencies: &mut ArtifactDependencyMap,
    trace_edits: &mut Vec<TraceEdit>,
    trace_coverage: &mut CoverageStatus,
) -> Result<ArchiveConversation> {
    let fallback_id = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string();
    let (native_id, title, project) = codex_archive_header(path, fallback_id)?;
    let mut builder = ConversationBuilder::new(CODEX_PROVIDER, source, native_id.clone(), path);
    builder.title = title;
    builder.project = project.clone();
    let mut current_model = None::<ModelInfo>;
    let mut structured_user_fingerprints = HashSet::new();
    let mut fallback_user_items = Vec::new();
    let mut pending_mutations = HashMap::new();
    let source_record_path = canonical_display(path);

    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line_bytes = Vec::new();
    let mut line_number = 0usize;
    loop {
        let status = read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
        if status == BoundedLineRead::Eof {
            break;
        }
        line_number = line_number.saturating_add(1);
        if status == BoundedLineRead::Oversized {
            diagnostics.records_scanned += 1;
            diagnostics.invalid_records += 1;
            record_unmeasurable_mutation(&mut diagnostics.truncated_mutations, trace_coverage);
            builder.missing_content += 1;
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            diagnostics.records_scanned += 1;
            diagnostics.invalid_records += 1;
            builder.missing_content += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        diagnostics.records_scanned += 1;
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                diagnostics.invalid_records += 1;
                builder.missing_content += 1;
                continue;
            }
        };
        if value.get("type").and_then(Value::as_str) == Some("turn_context") {
            current_model = model_from_nested_value(&value, None);
            continue;
        }
        let timestamp = timestamp_from_nested_value(&value);
        let record_id = format!("{source_record_path}:{line_number}");
        let top_type = value.get("type").and_then(Value::as_str);
        if top_type == Some("response_item") {
            let payload = value.get("payload").unwrap_or(&Value::Null);
            let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
            match payload_type {
                "message" => {
                    let role = payload
                        .get("role")
                        .and_then(Value::as_str)
                        .map(ArchiveRole::parse);
                    let native_item_id = native_id_from_value(payload)
                        .unwrap_or_else(|| format!("line:{line_number}"));
                    let content = payload.get("content").unwrap_or(&Value::Null);
                    if local_artifacts_allowed(ArchiveItemKind::Message, role) {
                        collect_artifact_dependencies(content, path, artifact_dependencies);
                    }
                    let (item, missing) = item_from_value(ItemInput {
                        provider: CODEX_PROVIDER,
                        conversation_native_id: &native_id,
                        native_item_id: &native_item_id,
                        source_record_id: &record_id,
                        ordinal: line_number as u64,
                        kind: ArchiveItemKind::Message,
                        role,
                        created_at: timestamp,
                        model: current_model.clone(),
                        tool_name: None,
                        tool_call_id: None,
                        status: None,
                        usage: None,
                        content,
                    });
                    builder.missing_content += missing;
                    if role == Some(ArchiveRole::User) {
                        structured_user_fingerprints.insert(item_fingerprint(&item));
                    }
                    builder.push(item);
                }
                "reasoning" => {
                    let summary = payload.get("summary").unwrap_or(&Value::Null);
                    if value_has_readable_content(summary) {
                        let native_item_id = native_id_from_value(payload)
                            .unwrap_or_else(|| format!("reasoning:{line_number}"));
                        let (item, missing) = item_from_value(ItemInput {
                            provider: CODEX_PROVIDER,
                            conversation_native_id: &native_id,
                            native_item_id: &native_item_id,
                            source_record_id: &record_id,
                            ordinal: line_number as u64,
                            kind: ArchiveItemKind::ReasoningSummary,
                            role: Some(ArchiveRole::Assistant),
                            created_at: timestamp,
                            model: current_model.clone(),
                            tool_name: None,
                            tool_call_id: None,
                            status: None,
                            usage: None,
                            content: summary,
                        });
                        builder.missing_content += missing;
                        builder.push(item);
                    }
                }
                "function_call" | "tool_call" | "custom_tool_call" => {
                    let native_item_id = native_id_from_value(payload)
                        .or_else(|| {
                            payload
                                .get("call_id")
                                .and_then(Value::as_str)
                                .map(|call_id| format!("tool-call:{call_id}"))
                        })
                        .unwrap_or_else(|| format!("tool-call:{line_number}"));
                    let content = payload
                        .get("arguments")
                        .or_else(|| payload.get("input"))
                        .unwrap_or(&Value::Null);
                    let call_key = payload
                        .get("call_id")
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or(&native_item_id)
                        .to_string();
                    remember_original_mutation(
                        &mut pending_mutations,
                        diagnostics,
                        trace_coverage,
                        MutationInvocation {
                            call_key,
                            tool_name: payload.get("name").and_then(Value::as_str),
                            arguments: content,
                            source_record_id: &record_id,
                            occurred_at: timestamp,
                        },
                    );
                    let (item, missing) = item_from_value(ItemInput {
                        provider: CODEX_PROVIDER,
                        conversation_native_id: &native_id,
                        native_item_id: &native_item_id,
                        source_record_id: &record_id,
                        ordinal: line_number as u64,
                        kind: ArchiveItemKind::ToolCall,
                        role: Some(ArchiveRole::Assistant),
                        created_at: timestamp,
                        model: current_model.clone(),
                        tool_name: payload.get("name").and_then(Value::as_str),
                        tool_call_id: payload.get("call_id").and_then(Value::as_str),
                        status: payload.get("status").and_then(Value::as_str),
                        usage: None,
                        content,
                    });
                    builder.missing_content += missing;
                    builder.push(item);
                }
                "function_call_output" | "tool_result" | "custom_tool_call_output" => {
                    let native_item_id = native_id_from_value(payload)
                        .or_else(|| {
                            payload
                                .get("call_id")
                                .and_then(Value::as_str)
                                .map(|call_id| format!("tool-result:{call_id}"))
                        })
                        .unwrap_or_else(|| format!("tool-result:{line_number}"));
                    let content = payload
                        .get("output")
                        .or_else(|| payload.get("content"))
                        .unwrap_or(&Value::Null);
                    let call_key = payload
                        .get("call_id")
                        .or_else(|| payload.get("tool_use_id"))
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or(&native_item_id);
                    finish_original_mutation(
                        &mut pending_mutations,
                        diagnostics,
                        trace_edits,
                        trace_coverage,
                        MutationCompletion {
                            call_key,
                            cache_key: &source_record_path,
                            result: payload,
                            status: payload.get("status").and_then(Value::as_str),
                            provider: CODEX_PROVIDER,
                            source,
                            conversation_id: &archive_conversation_id(CODEX_PROVIDER, &native_id),
                            project: project.as_ref(),
                        },
                    );
                    let (item, missing) = item_from_value(ItemInput {
                        provider: CODEX_PROVIDER,
                        conversation_native_id: &native_id,
                        native_item_id: &native_item_id,
                        source_record_id: &record_id,
                        ordinal: line_number as u64,
                        kind: ArchiveItemKind::ToolResult,
                        role: Some(ArchiveRole::Tool),
                        created_at: timestamp,
                        model: current_model.clone(),
                        tool_name: payload.get("name").and_then(Value::as_str),
                        tool_call_id: payload.get("call_id").and_then(Value::as_str),
                        status: payload.get("status").and_then(Value::as_str),
                        usage: None,
                        content,
                    });
                    builder.missing_content += missing;
                    builder.push(item);
                }
                _ => {}
            }
        } else if top_type == Some("event_msg")
            && value.pointer("/payload/type").and_then(Value::as_str) == Some("user_message")
        {
            let payload = value.get("payload").unwrap_or(&Value::Null);
            let content = payload
                .get("message")
                .or_else(|| payload.get("text"))
                .unwrap_or(&Value::Null);
            collect_artifact_dependencies(content, path, artifact_dependencies);
            let native_item_id = format!("user-event:{line_number}");
            let (item, missing) = item_from_value(ItemInput {
                provider: CODEX_PROVIDER,
                conversation_native_id: &native_id,
                native_item_id: &native_item_id,
                source_record_id: &record_id,
                ordinal: line_number as u64,
                kind: ArchiveItemKind::Message,
                role: Some(ArchiveRole::User),
                created_at: timestamp,
                model: current_model.clone(),
                tool_name: None,
                tool_call_id: None,
                status: None,
                usage: None,
                content,
            });
            fallback_user_items.push((item, missing));
        }
    }
    for (item, missing) in fallback_user_items {
        if !structured_user_fingerprints.contains(&item_fingerprint(&item)) {
            builder.missing_content += missing;
            builder.push(item);
        }
    }
    mark_unresolved_mutations(&pending_mutations, diagnostics, trace_coverage);
    Ok(builder.finish())
}

pub(crate) fn codex_archive_header(
    path: &Path,
    fallback_id: String,
) -> Result<(String, Option<String>, Option<ProjectInfo>)> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut project_cache = ProjectContextCache::new();
    let mut reader = BufReader::new(file);
    let mut line_bytes = Vec::new();
    loop {
        let status = read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
        if status == BoundedLineRead::Eof {
            break;
        }
        if status == BoundedLineRead::Oversized {
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            continue;
        };
        if !line.contains("session_meta") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let native_id = value
            .pointer("/payload/id")
            .and_then(Value::as_str)
            .unwrap_or(&fallback_id)
            .to_string();
        let title = value
            .pointer("/payload/thread_name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let project = codex_project_context_from_value(&value, &mut project_cache);
        return Ok((native_id, title, project));
    }
    Ok((fallback_id, None, None))
}

pub(crate) fn collect_claude(
    source: &SourceLocation,
    selected_cache_keys: Option<&HashSet<String>>,
) -> Result<ArchiveScan> {
    let Some(root) = source_root_path(source) else {
        return Ok(ArchiveScan::default());
    };
    let projects = root.join("projects");
    let paths = selected_jsonl_paths(selected_cache_keys, || {
        collect_jsonl_files(&projects).unwrap_or_default()
    });
    let mut scan = ArchiveScan::measured();
    let mut artifact_dependencies = ArtifactDependencyMap::new();
    for path in paths {
        if path.file_name().and_then(|name| name.to_str()) == Some("sessions-index.json") {
            continue;
        }
        scan.diagnostics.files_scanned += 1;
        scan.conversations.push(collect_claude_file(
            source,
            &path,
            &mut scan.diagnostics,
            &mut artifact_dependencies,
            &mut scan.trace_edits,
            &mut scan.trace_coverage,
        )?);
    }
    scan.artifact_dependencies = finish_artifact_dependencies(artifact_dependencies);
    finish_diagnostics(&mut scan);
    Ok(scan)
}

pub(crate) fn collect_claude_file(
    source: &SourceLocation,
    path: &Path,
    diagnostics: &mut ArchiveScanDiagnostics,
    artifact_dependencies: &mut ArtifactDependencyMap,
    trace_edits: &mut Vec<TraceEdit>,
    trace_coverage: &mut CoverageStatus,
) -> Result<ArchiveConversation> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let fallback_id = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string();
    let is_subagent_file = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        == Some("subagents");
    let fallback_agent_id = fallback_id
        .strip_prefix("agent-")
        .unwrap_or(&fallback_id)
        .to_string();
    let mut builder =
        ConversationBuilder::new(CLAUDE_CODE_PROVIDER, source, fallback_id.clone(), path);
    let mut reader = BufReader::new(file);
    let mut line_bytes = Vec::new();
    let mut line_number = 0usize;
    let mut pending_mutations = HashMap::new();
    let source_record_path = canonical_display(path);
    // Claude records the conversation identity per line, and a resumed session
    // carries its parent's identifier before its own. Edits reconstructed
    // before the last identity is known are re-bound to it below, so they
    // always reference the conversation this file is finally written as.
    let trace_edits_start = trace_edits.len();
    loop {
        let status = read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
        if status == BoundedLineRead::Eof {
            break;
        }
        line_number = line_number.saturating_add(1);
        if status == BoundedLineRead::Oversized {
            diagnostics.records_scanned += 1;
            diagnostics.invalid_records += 1;
            record_unmeasurable_mutation(&mut diagnostics.truncated_mutations, trace_coverage);
            builder.missing_content += 1;
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            diagnostics.records_scanned += 1;
            diagnostics.invalid_records += 1;
            builder.missing_content += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        diagnostics.records_scanned += 1;
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                diagnostics.invalid_records += 1;
                builder.missing_content += 1;
                continue;
            }
        };
        if let Some(session_id) = value
            .get("sessionId")
            .or_else(|| value.get("session_id"))
            .and_then(Value::as_str)
        {
            let native_id =
                claude_archive_native_id(&value, session_id, &fallback_agent_id, is_subagent_file);
            if native_id != session_id {
                let superseded_id = archive_conversation_id(CLAUDE_CODE_PROVIDER, session_id);
                if !builder.superseded_conversation_ids.contains(&superseded_id) {
                    builder.superseded_conversation_ids.push(superseded_id);
                }
            }
            builder.native_id = native_id;
        }
        builder.title = builder.title.or_else(|| {
            value
                .get("summary")
                .or_else(|| value.get("title"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
        if builder.project.is_none() {
            builder.project = value
                .get("cwd")
                .and_then(Value::as_str)
                .map(expand_home_path)
                .and_then(|path| resolve_project_context(Some(path), None, None));
        }
        let message = value.get("message").unwrap_or(&value);
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .or_else(|| value.get("type").and_then(Value::as_str))
            .map(ArchiveRole::parse);
        let Some(content) = message.get("content").or_else(|| value.get("content")) else {
            continue;
        };
        let native_item_id = native_id_from_value(&value)
            .or_else(|| native_id_from_value(message))
            .unwrap_or_else(|| format!("line:{line_number}"));
        let created_at = timestamp_from_nested_value(&value);
        let model = model_from_nested_value(&value, None);
        let usage = message
            .get("usage")
            .map(super::claude_usage_counts_from_value);
        let content_blocks = content
            .as_array()
            .map_or_else(|| vec![content], |blocks| blocks.iter().collect());
        for (part_index, block) in content_blocks.into_iter().enumerate() {
            let content_type = block.get("type").and_then(Value::as_str).unwrap_or("");
            let source_record_id = format!("{source_record_path}:{line_number}:{part_index}");
            if claude_block_is_opaque_reasoning(block, content_type) {
                builder.discarded_source_record_ids.push(source_record_id);
                continue;
            }
            let (kind, item_role) = match content_type {
                "tool_use" | "tool_call" => {
                    (ArchiveItemKind::ToolCall, Some(ArchiveRole::Assistant))
                }
                "tool_result" => (ArchiveItemKind::ToolResult, Some(ArchiveRole::Tool)),
                "thinking" | "reasoning" | "reasoning_summary" => (
                    ArchiveItemKind::ReasoningSummary,
                    Some(ArchiveRole::Assistant),
                ),
                _ => (ArchiveItemKind::Message, role),
            };
            let block_native_item_id = native_id_from_value(block)
                .unwrap_or_else(|| format!("{native_item_id}:part:{part_index}:{}", kind.as_str()));
            let tool_call_id = block
                .get("tool_use_id")
                .or_else(|| block.get("call_id"))
                .or_else(|| block.get("id"))
                .and_then(Value::as_str);
            let archive_content = match kind {
                ArchiveItemKind::ToolCall => block.get("input").unwrap_or(block),
                ArchiveItemKind::ToolResult => block
                    .get("content")
                    .or_else(|| block.get("output"))
                    .unwrap_or(block),
                ArchiveItemKind::Message
                | ArchiveItemKind::ReasoningSummary
                | ArchiveItemKind::Artifact => block,
            };
            if kind == ArchiveItemKind::ToolCall {
                remember_original_mutation(
                    &mut pending_mutations,
                    diagnostics,
                    trace_coverage,
                    MutationInvocation {
                        call_key: tool_call_id.unwrap_or(&block_native_item_id).to_string(),
                        tool_name: block
                            .get("name")
                            .or_else(|| block.get("tool_name"))
                            .and_then(Value::as_str),
                        arguments: archive_content,
                        source_record_id: &source_record_id,
                        occurred_at: created_at,
                    },
                );
            } else if kind == ArchiveItemKind::ToolResult {
                finish_original_mutation(
                    &mut pending_mutations,
                    diagnostics,
                    trace_edits,
                    trace_coverage,
                    MutationCompletion {
                        call_key: tool_call_id.unwrap_or(&block_native_item_id),
                        cache_key: &source_record_path,
                        result: block,
                        status: block.get("status").and_then(Value::as_str),
                        provider: CLAUDE_CODE_PROVIDER,
                        source,
                        conversation_id: &archive_conversation_id(
                            CLAUDE_CODE_PROVIDER,
                            &builder.native_id,
                        ),
                        project: builder.project.as_ref(),
                    },
                );
            }
            if local_artifacts_allowed(kind, item_role) {
                collect_artifact_dependencies(archive_content, path, artifact_dependencies);
            }
            let (item, missing) = item_from_value(ItemInput {
                provider: CLAUDE_CODE_PROVIDER,
                conversation_native_id: &builder.native_id,
                native_item_id: &block_native_item_id,
                source_record_id: &source_record_id,
                ordinal: (line_number as u64) << 32 | part_index as u64,
                kind,
                role: item_role,
                created_at,
                model: model.clone(),
                tool_name: block
                    .get("name")
                    .or_else(|| block.get("tool_name"))
                    .and_then(Value::as_str),
                tool_call_id,
                status: block.get("status").and_then(Value::as_str),
                usage: (part_index == 0).then(|| usage.clone()).flatten(),
                content: archive_content,
            });
            builder.missing_content += missing;
            builder.push(item);
        }
    }
    mark_unresolved_mutations(&pending_mutations, diagnostics, trace_coverage);
    let conversation = builder.finish();
    for edit in &mut trace_edits[trace_edits_start..] {
        edit.rebind_conversation(&conversation.conversation_id);
    }
    Ok(conversation)
}

pub(crate) fn claude_archive_native_id(
    value: &Value,
    session_id: &str,
    fallback_agent_id: &str,
    is_subagent_file: bool,
) -> String {
    let is_sidechain = value
        .get("isSidechain")
        .or_else(|| value.get("is_sidechain"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let agent_id = value
        .get("agentId")
        .or_else(|| value.get("agent_id"))
        .and_then(Value::as_str);
    if is_subagent_file {
        format!("{session_id}:agent:{fallback_agent_id}")
    } else if is_sidechain || agent_id.is_some() {
        format!(
            "{session_id}:agent:{}",
            agent_id.unwrap_or(fallback_agent_id)
        )
    } else {
        session_id.to_string()
    }
}

pub(crate) fn claude_block_is_opaque_reasoning(block: &Value, content_type: &str) -> bool {
    if matches!(content_type, "redacted_thinking" | "encrypted_thinking") {
        return true;
    }
    matches!(content_type, "thinking" | "reasoning" | "reasoning_summary")
        && !["text", "thinking", "summary", "content"]
            .into_iter()
            .filter_map(|key| block.get(key))
            .any(value_has_readable_content)
}

pub(crate) fn collect_grok(
    source: &SourceLocation,
    selected_cache_keys: Option<&HashSet<String>>,
) -> Result<ArchiveScan> {
    // Grok sessions record no tool mutations this module can reconstruct.
    let mut scan = ArchiveScan::default();
    let Some(root) = source_root_path(source) else {
        return Ok(scan);
    };
    let mut session_dirs = Vec::new();
    if let Some(selected) = selected_cache_keys {
        session_dirs.extend(
            selected
                .iter()
                .filter_map(|key| Path::new(key).parent().map(Path::to_path_buf)),
        );
    } else {
        let sessions = grok_sessions_root(&root);
        if sessions.is_dir() {
            session_dirs.extend(
                std::fs::read_dir(sessions)?
                    .filter_map(std::result::Result::ok)
                    .filter(|entry| entry.path().is_dir())
                    .map(|entry| entry.path()),
            );
        }
    }
    session_dirs.sort();
    session_dirs.dedup();
    let mut artifact_dependencies = ArtifactDependencyMap::new();
    for session_dir in session_dirs {
        let chat_path = session_dir.join("chat_history.jsonl");
        if !chat_path.is_file() {
            continue;
        }
        scan.diagnostics.files_scanned += 1;
        let native_id = session_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();
        let mut builder =
            ConversationBuilder::new(GROK_BUILD_PROVIDER, source, native_id.clone(), &chat_path);
        if let Ok(summary) = std::fs::read_to_string(session_dir.join("summary.json")) {
            if let Ok(value) = serde_json::from_str::<Value>(&summary) {
                builder.title = value
                    .get("title")
                    .or_else(|| value.pointer("/info/title"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                builder.project = value
                    .pointer("/info/cwd")
                    .and_then(Value::as_str)
                    .map(expand_home_path)
                    .and_then(|path| resolve_project_context(Some(path), None, None));
            }
        }
        let file = File::open(&chat_path)?;
        let mut reader = BufReader::new(file);
        let mut line_bytes = Vec::new();
        let mut line_number = 0usize;
        loop {
            let status =
                read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
            if status == BoundedLineRead::Eof {
                break;
            }
            line_number = line_number.saturating_add(1);
            if status == BoundedLineRead::Oversized {
                scan.diagnostics.records_scanned += 1;
                scan.diagnostics.invalid_records += 1;
                scan.diagnostics.truncated_mutations =
                    scan.diagnostics.truncated_mutations.saturating_add(1);
                builder.missing_content += 1;
                continue;
            }
            let Ok(line) = std::str::from_utf8(&line_bytes) else {
                scan.diagnostics.records_scanned += 1;
                scan.diagnostics.invalid_records += 1;
                builder.missing_content += 1;
                continue;
            };
            if line.trim().is_empty() {
                continue;
            }
            scan.diagnostics.records_scanned += 1;
            let value = match serde_json::from_str::<Value>(line) {
                Ok(value) => value,
                Err(_) => {
                    scan.diagnostics.invalid_records += 1;
                    builder.missing_content += 1;
                    continue;
                }
            };
            let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
            let (kind, role) = match record_type {
                "user" => (ArchiveItemKind::Message, ArchiveRole::User),
                "assistant" => (ArchiveItemKind::Message, ArchiveRole::Assistant),
                "reasoning" => (ArchiveItemKind::ReasoningSummary, ArchiveRole::Assistant),
                "tool_result" => (ArchiveItemKind::ToolResult, ArchiveRole::Tool),
                "system" => (ArchiveItemKind::Message, ArchiveRole::System),
                _ => continue,
            };
            let content = if kind == ArchiveItemKind::ToolResult {
                &value
            } else {
                value
                    .get("content")
                    .or_else(|| value.get("text"))
                    .or_else(|| value.get("message"))
                    .or_else(|| value.get("summary"))
                    .unwrap_or(&Value::Null)
            };
            if !value_has_readable_content(content) {
                continue;
            }
            if local_artifacts_allowed(kind, Some(role)) {
                collect_artifact_dependencies(content, &chat_path, &mut artifact_dependencies);
            }
            let native_item_id =
                native_id_from_value(&value).unwrap_or_else(|| format!("line:{line_number}"));
            let source_record_id = format!("{}:{line_number}", chat_path.display());
            let (item, missing) = item_from_value(ItemInput {
                provider: GROK_BUILD_PROVIDER,
                conversation_native_id: &native_id,
                native_item_id: &native_item_id,
                source_record_id: &source_record_id,
                ordinal: line_number as u64,
                kind,
                role: Some(role),
                created_at: timestamp_from_nested_value(&value),
                model: model_from_nested_value(&value, None),
                tool_name: value.get("name").and_then(Value::as_str),
                tool_call_id: value.get("tool_call_id").and_then(Value::as_str),
                status: value.get("status").and_then(Value::as_str),
                usage: None,
                content,
            });
            builder.missing_content += missing;
            builder.push(item);
        }
        scan.conversations.push(builder.finish());
    }
    scan.artifact_dependencies = finish_artifact_dependencies(artifact_dependencies);
    finish_diagnostics(&mut scan);
    Ok(scan)
}

pub(crate) fn collect_opencode(
    source: &SourceLocation,
    selected_cache_keys: Option<&HashSet<String>>,
) -> Result<ArchiveScan> {
    let Some(root) = source_root_path(source) else {
        return Ok(ArchiveScan::default());
    };
    let db_path = root.join("opencode.db");
    if !db_path.is_file()
        || selected_cache_keys.is_some_and(|keys| !keys.contains(&canonical_display(&db_path)))
    {
        return Ok(ArchiveScan::default());
    }
    let connection = open_sqlite_readonly(&db_path)?;
    let mut builders = BTreeMap::<String, ConversationBuilder>::new();
    let mut artifact_dependencies = ArtifactDependencyMap::new();
    let mut session_statement = connection.prepare(
        "SELECT id, title, time_created, time_updated, directory FROM session ORDER BY time_created, id",
    )?;
    let sessions = session_statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    for row in sessions {
        let (id, title, created, updated, directory) = row?;
        let mut builder = ConversationBuilder::new(OPENCODE_PROVIDER, source, id.clone(), &db_path);
        builder.title = title;
        builder.started_at = timestamp_from_epoch(created);
        builder.updated_at = timestamp_from_epoch(updated);
        builder.project = directory
            .map(PathBuf::from)
            .and_then(|path| resolve_project_context(Some(path), None, None));
        builders.insert(id, builder);
    }

    let mut message_context = HashMap::<
        String,
        (
            String,
            ArchiveRole,
            Option<DateTime<Utc>>,
            Option<ModelInfo>,
        ),
    >::new();
    let mut records_scanned = 0u64;
    let mut invalid_records = 0u64;
    let mut message_statement = connection.prepare(
        "SELECT id, session_id, time_created, data FROM message ORDER BY session_id, time_created, id",
    )?;
    let messages = message_statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in messages {
        let (message_id, session_id, created, data) = row?;
        records_scanned += 1;
        let value = match serde_json::from_str::<Value>(&data) {
            Ok(value) => value,
            Err(_) => {
                invalid_records += 1;
                if let Some(builder) = builders.get_mut(&session_id) {
                    builder.missing_content += 1;
                }
                continue;
            }
        };
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .map(ArchiveRole::parse)
            .unwrap_or(ArchiveRole::Unknown);
        let model = super::opencode_message_model_info(&value);
        message_context.insert(
            message_id.clone(),
            (
                session_id.clone(),
                role,
                timestamp_from_epoch(created),
                model.clone(),
            ),
        );
        if let Some(content) = value
            .get("content")
            .filter(|content| value_has_readable_content(content))
        {
            if let Some(builder) = builders.get_mut(&session_id) {
                if local_artifacts_allowed(ArchiveItemKind::Message, Some(role)) {
                    collect_artifact_dependencies(content, &db_path, &mut artifact_dependencies);
                }
                let (item, missing) = item_from_value(ItemInput {
                    provider: OPENCODE_PROVIDER,
                    conversation_native_id: &session_id,
                    native_item_id: &message_id,
                    source_record_id: &format!("message:{message_id}"),
                    ordinal: created.max(0) as u64,
                    kind: ArchiveItemKind::Message,
                    role: Some(role),
                    created_at: timestamp_from_epoch(created),
                    model,
                    tool_name: None,
                    tool_call_id: None,
                    status: None,
                    usage: Some(super::opencode_message_usage_counts(&value)),
                    content,
                });
                builder.missing_content += missing;
                builder.push(item);
            }
        }
    }

    let part_table_exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'part'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if part_table_exists {
        let mut part_statement = connection.prepare(
            "SELECT id, message_id, session_id, time_created, data FROM part ORDER BY session_id, time_created, id",
        )?;
        let parts = part_statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        for row in parts {
            let (part_id, message_id, session_id, created, data) = row?;
            records_scanned += 1;
            let value = match serde_json::from_str::<Value>(&data) {
                Ok(value) => value,
                Err(_) => {
                    invalid_records += 1;
                    if let Some(builder) = builders.get_mut(&session_id) {
                        builder.missing_content += 1;
                    }
                    continue;
                }
            };
            let part_type = value.get("type").and_then(Value::as_str).unwrap_or("");
            let Some(builder) = builders.get_mut(&session_id) else {
                continue;
            };
            if part_type == "tool" {
                let state = value.get("state").unwrap_or(&Value::Null);
                let input = state
                    .get("input")
                    .or_else(|| value.get("input"))
                    .unwrap_or(&Value::Null);
                let output = state
                    .get("output")
                    .filter(|value| !value.is_null())
                    .or_else(|| value.get("output"))
                    .filter(|value| !value.is_null())
                    .or_else(|| state.get("error"))
                    .or_else(|| value.get("error"));
                let tool_name = value
                    .get("tool")
                    .or_else(|| value.get("name"))
                    .and_then(Value::as_str);
                let tool_call_id = value
                    .get("callID")
                    .or_else(|| value.get("call_id"))
                    .and_then(Value::as_str);
                let status = state
                    .get("status")
                    .or_else(|| value.get("status"))
                    .and_then(Value::as_str);
                let model = message_context
                    .get(&message_id)
                    .and_then(|value| value.3.clone());
                let source_record_id = format!("part:{part_id}");
                let (call, call_missing) = item_from_value(ItemInput {
                    provider: OPENCODE_PROVIDER,
                    conversation_native_id: &session_id,
                    native_item_id: &part_id,
                    source_record_id: &source_record_id,
                    ordinal: created.max(0) as u64,
                    kind: ArchiveItemKind::ToolCall,
                    role: Some(ArchiveRole::Assistant),
                    created_at: timestamp_from_epoch(created),
                    model: model.clone(),
                    tool_name,
                    tool_call_id,
                    status,
                    usage: None,
                    content: input,
                });
                builder.missing_content += call_missing;
                builder.push(call);
                if let Some(output) = output {
                    let result_native_id = format!("{part_id}:result");
                    let (result, result_missing) = item_from_value(ItemInput {
                        provider: OPENCODE_PROVIDER,
                        conversation_native_id: &session_id,
                        native_item_id: &result_native_id,
                        source_record_id: &format!("{source_record_id}:result"),
                        ordinal: created.max(0) as u64,
                        kind: ArchiveItemKind::ToolResult,
                        role: Some(ArchiveRole::Tool),
                        created_at: timestamp_from_epoch(created),
                        model,
                        tool_name,
                        tool_call_id,
                        status,
                        usage: None,
                        content: output,
                    });
                    builder.missing_content += result_missing;
                    builder.push(result);
                }
                continue;
            }
            let (kind, role) = match part_type {
                "reasoning" => (
                    ArchiveItemKind::ReasoningSummary,
                    Some(ArchiveRole::Assistant),
                ),
                "tool_call" => (ArchiveItemKind::ToolCall, Some(ArchiveRole::Assistant)),
                "tool_result" => (ArchiveItemKind::ToolResult, Some(ArchiveRole::Tool)),
                "file" | "image" => (
                    ArchiveItemKind::Artifact,
                    message_context.get(&message_id).map(|value| value.1),
                ),
                "text" => (
                    ArchiveItemKind::Message,
                    message_context.get(&message_id).map(|value| value.1),
                ),
                _ => continue,
            };
            let content = if matches!(
                kind,
                ArchiveItemKind::ToolCall | ArchiveItemKind::ToolResult
            ) {
                &value
            } else {
                value
                    .get("text")
                    .or_else(|| value.get("content"))
                    .or_else(|| value.get("output"))
                    .unwrap_or(&value)
            };
            if local_artifacts_allowed(kind, role) {
                collect_artifact_dependencies(content, &db_path, &mut artifact_dependencies);
            }
            let (item, missing) = item_from_value(ItemInput {
                provider: OPENCODE_PROVIDER,
                conversation_native_id: &session_id,
                native_item_id: &part_id,
                source_record_id: &format!("part:{part_id}"),
                ordinal: created.max(0) as u64,
                kind,
                role,
                created_at: timestamp_from_epoch(created),
                model: message_context
                    .get(&message_id)
                    .and_then(|value| value.3.clone()),
                tool_name: value
                    .get("tool")
                    .or_else(|| value.get("name"))
                    .and_then(Value::as_str),
                tool_call_id: value
                    .get("callID")
                    .or_else(|| value.get("call_id"))
                    .and_then(Value::as_str),
                status: value
                    .pointer("/state/status")
                    .or_else(|| value.get("status"))
                    .and_then(Value::as_str),
                usage: None,
                content,
            });
            builder.missing_content += missing;
            builder.push(item);
        }
    }
    let mut scan = ArchiveScan {
        conversations: builders
            .into_values()
            .map(ConversationBuilder::finish)
            .collect(),
        artifact_dependencies: finish_artifact_dependencies(artifact_dependencies),
        trace_edits: Vec::new(),
        quota_observations: Vec::new(),
        trace_coverage: CoverageStatus::Unavailable,
        diagnostics: ArchiveScanDiagnostics {
            files_scanned: 1,
            records_scanned,
            invalid_records,
            ..ArchiveScanDiagnostics::default()
        },
    };
    finish_diagnostics(&mut scan);
    Ok(scan)
}
