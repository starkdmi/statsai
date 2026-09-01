use super::*;

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
