use super::*;

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
