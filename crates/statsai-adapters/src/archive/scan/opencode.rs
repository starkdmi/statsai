use super::*;

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
