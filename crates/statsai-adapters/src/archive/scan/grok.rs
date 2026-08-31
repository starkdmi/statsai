use super::*;

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
