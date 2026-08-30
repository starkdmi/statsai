use super::*;
use crate::*;

pub(crate) fn parse_codex_file(
    ctx: &mut FileParseContext<'_, CodexAdapter>,
    root: &Path,
    usage_root: &Path,
    thread_titles: &HashMap<String, String>,
    path: &Path,
) -> Result<()> {
    let collect_tasks = ctx.options.should_collect_tasks();
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let fallback_timestamp = file_modified_timestamp(path).unwrap_or_else(Utc::now);
    let file_fallback_project = project_context_from_path_fallback(root, path);
    let mut previous_totals: Option<UsageCounts> = None;
    let mut current_model: Option<String> = None;
    let mut current_reasoning = ModelReasoningState::default();
    let mut current_model_is_fallback = false;
    let mut current_project: Option<ProjectInfo> = None;
    let mut current_title: Option<String> = None;
    let mut current_thread_id: Option<String> = None;
    // Seeded from the file path, replaced by the session's own id as soon as
    // the `session_meta` line declares one. The embedded id is the same UUID
    // Codex reports as `conversation.id` in telemetry, and conversation-to-
    // account bindings hash that UUID — a path-derived identity can never meet
    // them, which left every binding unable to attribute a single event.
    let mut session_raw = codex_session_id(usage_root, path);
    let mut records = Vec::new();
    let mut quota_observation_indices = HashMap::new();
    let mut project_cache = ProjectContextCache::new();
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
        let line_kind = codex_line_kind(line);
        if line_kind == CodexLineKind::Irrelevant && !is_codex_quota_line_structurally(line) {
            continue;
        }
        if line_kind == CodexLineKind::ResponseItemMessage {
            let header = codex_line_header(line);
            let role = codex_json_string_prefix_after_marker(header, "\"role\":\"", 32);
            let preview_raw_text = (collect_tasks && role.as_deref() == Some("user"))
                .then(|| {
                    codex_response_item_user_preview_from_line(line, CODEX_TASK_PREVIEW_RAW_BYTES)
                })
                .flatten()
                .and_then(|text| codex_prompt_preview_input(Some(text.as_str())));
            let needs_full_fallback = role.is_none()
                || (collect_tasks
                    && role.as_deref() == Some("user")
                    && preview_raw_text
                        .as_deref()
                        .and_then(|raw| task_preview_from_prompt(Some(raw), 220))
                        .as_deref()
                        .is_none_or(|title| task_title_is_weak_signal(Some(title))));
            if !needs_full_fallback {
                let (timestamp, timestamp_inferred) = codex_timestamp_from_text(
                    codex_json_string_prefix_after_marker(header, "\"timestamp\":\"", 64)
                        .as_deref(),
                    fallback_timestamp,
                );
                if timestamp_inferred {
                    ctx.scan.diagnostics.timestamp_fallbacks += 1;
                }
                let mut model_inferred = false;
                let model = current_model
                    .as_deref()
                    .map(|model| model_info_with_reasoning(model, &current_reasoning))
                    .or_else(|| {
                        model_inferred = true;
                        Some(model_info_with_reasoning("gpt-5", &current_reasoning))
                    });
                if model_inferred {
                    ctx.scan.diagnostics.model_fallbacks += 1;
                }
                let user_message_preview =
                    preview_raw_text.map(|raw_text| CodexPromptPreviewCandidate {
                        raw_text,
                        source: CodexPromptPreviewSource::ResponseItemUser,
                    });
                records.push(CodexLineRecord {
                    line_number: index,
                    timestamp,
                    timestamp_inferred,
                    session_raw: codex_json_string_prefix_after_marker(
                        header,
                        "\"session_id\":\"",
                        128,
                    )
                    .or_else(|| {
                        codex_json_string_prefix_after_marker(header, "\"sessionId\":\"", 128)
                    })
                    .unwrap_or_else(|| session_raw.clone()),
                    model,
                    model_inferred,
                    model_explicit: false,
                    usage: None,
                    is_token_count_event: false,
                    is_task_started: false,
                    is_task_complete: false,
                    message_role: role,
                    user_message_preview,
                    session_title: current_title.clone(),
                    thread_id: current_thread_id.clone(),
                    project: current_project
                        .clone()
                        .or_else(|| file_fallback_project.clone()),
                    task_started_at: None,
                    task_completed_at: None,
                    task_duration_ms: None,
                    time_to_first_token_ms: None,
                });
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<CodexFastResponseMessageLine<'_>>(line) else {
                ctx.scan.diagnostics.invalid_rows += 1;
                continue;
            };
            let (timestamp, timestamp_inferred) =
                codex_timestamp_from_text(parsed.timestamp.as_deref(), fallback_timestamp);
            if timestamp_inferred {
                ctx.scan.diagnostics.timestamp_fallbacks += 1;
            }
            let mut model_inferred = false;
            let model = current_model
                .as_deref()
                .map(|model| model_info_with_reasoning(model, &current_reasoning))
                .or_else(|| {
                    model_inferred = true;
                    Some(model_info_with_reasoning("gpt-5", &current_reasoning))
                });
            if model_inferred {
                ctx.scan.diagnostics.model_fallbacks += 1;
            }
            let message_role = parsed.payload.role.as_deref().map(ToOwned::to_owned);
            let user_message_preview = (collect_tasks
                && parsed.payload.role.as_deref() == Some("user"))
            .then(|| {
                codex_preview_from_response_parts(
                    parsed.payload.content.as_deref().unwrap_or(&[]),
                    CODEX_TASK_PREVIEW_RAW_BYTES,
                )
            })
            .flatten()
            .and_then(|text| codex_prompt_preview_input(Some(text.as_str())))
            .map(|raw_text| CodexPromptPreviewCandidate {
                raw_text,
                source: CodexPromptPreviewSource::ResponseItemUser,
            });
            records.push(CodexLineRecord {
                line_number: index,
                timestamp,
                timestamp_inferred,
                session_raw: parsed
                    .session_id
                    .as_deref()
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| session_raw.clone()),
                model,
                model_inferred,
                model_explicit: false,
                usage: None,
                is_token_count_event: false,
                is_task_started: false,
                is_task_complete: false,
                message_role,
                user_message_preview,
                session_title: current_title.clone(),
                thread_id: current_thread_id.clone(),
                project: current_project
                    .clone()
                    .or_else(|| file_fallback_project.clone()),
                task_started_at: None,
                task_completed_at: None,
                task_duration_ms: None,
                time_to_first_token_ms: None,
            });
            continue;
        }
        if line_kind == CodexLineKind::EventUserMessage {
            if !collect_tasks {
                continue;
            }
            let header = codex_line_header(line);
            let (timestamp, timestamp_inferred) = codex_timestamp_from_text(
                codex_json_string_prefix_after_marker(header, "\"timestamp\":\"", 64).as_deref(),
                fallback_timestamp,
            );
            if timestamp_inferred {
                ctx.scan.diagnostics.timestamp_fallbacks += 1;
            }
            let mut model_inferred = false;
            let model = current_model
                .as_deref()
                .map(|model| model_info_with_reasoning(model, &current_reasoning))
                .or_else(|| {
                    model_inferred = true;
                    Some(model_info_with_reasoning("gpt-5", &current_reasoning))
                });
            if model_inferred {
                ctx.scan.diagnostics.model_fallbacks += 1;
            }
            let user_message_preview =
                codex_event_user_message_preview_from_line(line, CODEX_TASK_PREVIEW_RAW_BYTES)
                    .and_then(|text| codex_prompt_preview_input(Some(text.as_str())))
                    .map(|raw_text| CodexPromptPreviewCandidate {
                        raw_text,
                        source: CodexPromptPreviewSource::UserMessageEvent,
                    });
            records.push(CodexLineRecord {
                line_number: index,
                timestamp,
                timestamp_inferred,
                session_raw: codex_json_string_prefix_after_marker(
                    header,
                    "\"session_id\":\"",
                    128,
                )
                .or_else(|| codex_json_string_prefix_after_marker(header, "\"sessionId\":\"", 128))
                .unwrap_or_else(|| session_raw.clone()),
                model,
                model_inferred,
                model_explicit: false,
                usage: None,
                is_token_count_event: false,
                is_task_started: false,
                is_task_complete: false,
                message_role: None,
                user_message_preview,
                session_title: current_title.clone(),
                thread_id: current_thread_id.clone(),
                project: current_project
                    .clone()
                    .or_else(|| file_fallback_project.clone()),
                task_started_at: None,
                task_completed_at: None,
                task_duration_ms: None,
                time_to_first_token_ms: None,
            });
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            ctx.scan.diagnostics.invalid_rows += 1;
            continue;
        };

        if is_codex_session_meta(&value) {
            let declared_session_id = value
                .pointer("/payload/id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if let Some(declared_session_id) = declared_session_id.clone() {
                session_raw = declared_session_id;
            }
            if collect_tasks {
                current_thread_id = declared_session_id;
                let session_id = current_thread_id
                    .clone()
                    .or_else(|| Some(session_raw.clone()));
                current_title = value
                    .pointer("/payload/thread_name")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        session_id
                            .as_ref()
                            .and_then(|session_id| thread_titles.get(session_id))
                            .cloned()
                    })
                    .or_else(|| thread_titles.get(&session_raw).cloned());
            }
            current_project = codex_project_context_from_value(&value, &mut project_cache);
            continue;
        }

        if is_codex_turn_context(&value) {
            current_reasoning = codex_reasoning_state_from_value(&value);
            if let Some(model_name) = codex_model_from_value(&value, current_model.as_deref())
                .and_then(|model| model.normalized_name)
            {
                current_model = Some(model_name);
                current_model_is_fallback = false;
            }
            if let Some(project) = codex_project_context_from_value(&value, &mut project_cache) {
                current_project = Some(project);
            }
            continue;
        }

        let is_token_count_event = is_codex_token_count(&value);
        let is_task_started = is_codex_task_started(&value);
        let is_task_complete = is_codex_task_complete(&value);
        let task_started_at = is_task_started
            .then(|| codex_task_timestamp(&value, &["/payload/started_at"]))
            .flatten();
        let task_completed_at = is_task_complete
            .then(|| codex_task_timestamp(&value, &["/payload/completed_at"]))
            .flatten();
        let task_duration_ms = is_task_complete
            .then(|| codex_task_u64(&value, &["/payload/duration_ms", "/payload/durationMs"]))
            .flatten();
        let time_to_first_token_ms = is_task_complete
            .then(|| {
                codex_task_u64(
                    &value,
                    &[
                        "/payload/time_to_first_token_ms",
                        "/payload/timeToFirstTokenMs",
                    ],
                )
            })
            .flatten();
        let message_role = codex_visible_message_role(&value).map(ToOwned::to_owned);
        let user_message_preview = collect_tasks
            .then(|| codex_user_message_preview(&value))
            .flatten();
        let event_session_raw =
            session_raw_from_value(&value).unwrap_or_else(|| session_raw.clone());
        let usage = if is_token_count_event {
            let info = value.pointer("/payload/info");
            let total_usage = info
                .and_then(|info| info.get("total_token_usage"))
                .map(codex_usage_counts_from_value);
            let usage = info
                .and_then(|info| info.get("last_token_usage"))
                .map(codex_usage_counts_from_value)
                .or_else(|| {
                    total_usage
                        .as_ref()
                        .map(|total| subtract_usage_counts(total, previous_totals.as_ref()))
                });
            if let Some(total) = total_usage {
                previous_totals = Some(total);
            }
            usage
        } else {
            codex_headless_usage_value(&value).map(codex_usage_counts_from_value)
        };
        let quota_usage_sample = usage.clone();

        let (timestamp, timestamp_inferred) = timestamp_from_nested_value(&value)
            .map(|timestamp| (timestamp, false))
            .unwrap_or((fallback_timestamp, true));
        if timestamp_inferred {
            ctx.scan.diagnostics.timestamp_fallbacks += 1;
        }
        if let Some(quota) = codex_quota_observation(
            ctx.source,
            path,
            index,
            timestamp,
            quota_usage_sample,
            &value,
        ) {
            quota_observation_indices.insert(index, ctx.scan.quota_observations.len());
            ctx.scan.quota_observations.push(quota);
        }

        let explicit_model =
            with_reasoning_state(codex_model_from_value(&value, None), &current_reasoning);
        if let Some(model_name) = explicit_model
            .as_ref()
            .and_then(|model| {
                model
                    .provider_model_id
                    .as_ref()
                    .or(model.name.as_ref())
                    .or(model.normalized_name.as_ref())
            })
            .cloned()
        {
            current_model = Some(model_name);
            current_model_is_fallback = false;
        }
        let model_explicit = explicit_model.is_some();
        let mut model_inferred = false;
        let model = explicit_model.or_else(|| {
            current_model
                .as_deref()
                .map(|model| model_info_with_reasoning(model, &current_reasoning))
                .or_else(|| {
                    model_inferred = true;
                    current_model_is_fallback = true;
                    Some(model_info_with_reasoning("gpt-5", &current_reasoning))
                })
        });
        if current_model_is_fallback && !model_inferred {
            model_inferred = true;
        }
        if model_inferred {
            ctx.scan.diagnostics.model_fallbacks += 1;
        }

        let usage = usage.and_then(|usage| {
            ctx.scan.diagnostics.candidate_usage_rows += 1;
            if usage.computed_total() == 0 {
                ctx.scan.diagnostics.skipped_zero_events += 1;
                None
            } else {
                Some(usage)
            }
        });

        records.push(CodexLineRecord {
            line_number: index,
            timestamp,
            timestamp_inferred,
            session_raw: event_session_raw,
            model,
            model_inferred,
            model_explicit,
            usage,
            is_token_count_event,
            is_task_started,
            is_task_complete,
            message_role,
            user_message_preview,
            session_title: current_title.clone(),
            thread_id: current_thread_id.clone(),
            project: current_project
                .clone()
                .or_else(|| file_fallback_project.clone()),
            task_started_at,
            task_completed_at,
            task_duration_ms,
            time_to_first_token_ms,
        });
    }

    let mut active_turns: Vec<ActiveCodexTurn> = Vec::new();
    let mut consumed_usage_lines = HashSet::new();

    for record in &records {
        if record.is_task_started {
            let started_at = record.task_started_at.unwrap_or(record.timestamp);
            active_turns.push(ActiveCodexTurn {
                started_at,
                session_raw: record.session_raw.clone(),
                title: record.session_title.clone(),
                thread_id: record.thread_id.clone(),
                model: record.model.clone(),
                model_inferred: record.model_inferred,
                timestamp_inferred: record.timestamp_inferred,
                message_counts: CodexMessageCounts::default(),
                last_usage: record.usage.clone(),
                accumulated_usage: record.usage.clone(),
                prompt_previews: Vec::new(),
                last_activity_at: record.timestamp,
                usage_lines: record
                    .usage
                    .as_ref()
                    .map(|_| vec![record.line_number])
                    .unwrap_or_default(),
                project: record.project.clone(),
            });
            if record.usage.is_some() {
                consumed_usage_lines.insert(record.line_number);
            }
            continue;
        }

        if let Some(turn) = active_turns
            .iter_mut()
            .rfind(|turn| turn.session_raw == record.session_raw)
        {
            if record.model_explicit {
                turn.model = record.model.clone();
                turn.model_inferred = record.model_inferred;
            }
            turn.timestamp_inferred |= record.timestamp_inferred;
            turn.last_activity_at = record.timestamp;
            if record.project.is_some() {
                turn.project = record.project.clone();
            }
            if turn.title.is_none() && record.session_title.is_some() {
                turn.title = record.session_title.clone();
            }
            if let Some(role) = record.message_role.as_deref() {
                turn.message_counts.total = turn.message_counts.total.saturating_add(1);
                match role {
                    "user" => turn.message_counts.user = turn.message_counts.user.saturating_add(1),
                    "assistant" => {
                        turn.message_counts.assistant =
                            turn.message_counts.assistant.saturating_add(1)
                    }
                    "developer" => {
                        turn.message_counts.developer =
                            turn.message_counts.developer.saturating_add(1)
                    }
                    _ => {}
                }
            }
            if let Some(prompt_preview) = collect_tasks
                .then_some(record.user_message_preview.as_ref())
                .flatten()
            {
                let already_present = turn.prompt_previews.iter().any(|existing| {
                    existing.source == prompt_preview.source
                        && existing.raw_text == prompt_preview.raw_text
                });
                if !already_present {
                    match prompt_preview.source {
                        CodexPromptPreviewSource::ResponseItemUser => {
                            let has_provider_native_event =
                                turn.prompt_previews.iter().any(|existing| {
                                    existing.source == CodexPromptPreviewSource::UserMessageEvent
                                });
                            let response_item_count = turn
                                .prompt_previews
                                .iter()
                                .filter(|existing| {
                                    existing.source == CodexPromptPreviewSource::ResponseItemUser
                                })
                                .count();
                            if !has_provider_native_event
                                && response_item_count < 1
                                && turn.prompt_previews.len() < 3
                            {
                                turn.prompt_previews.push(prompt_preview.clone());
                            }
                        }
                        CodexPromptPreviewSource::UserMessageEvent => {
                            turn.prompt_previews.retain(|existing| {
                                existing.source == CodexPromptPreviewSource::UserMessageEvent
                            });
                            if turn.prompt_previews.len() < 3 {
                                turn.prompt_previews.push(prompt_preview.clone());
                            }
                        }
                    }
                }
            }
            if let Some(usage) = record.usage.clone() {
                if !record.is_task_complete {
                    turn.accumulated_usage = Some(
                        turn.accumulated_usage
                            .as_ref()
                            .map(|accumulated| sum_usage_counts(accumulated, &usage))
                            .unwrap_or_else(|| usage.clone()),
                    );
                    turn.last_usage = Some(usage);
                    turn.usage_lines.push(record.line_number);
                }
            }
        }

        if record.is_task_complete {
            let Some(turn_index) = active_turns
                .iter()
                .rposition(|turn| turn.session_raw == record.session_raw)
            else {
                continue;
            };
            let turn = active_turns.remove(turn_index);
            let completed_at = record.task_completed_at.unwrap_or(record.timestamp);
            let usage = record
                .usage
                .clone()
                .or(turn.accumulated_usage.clone())
                .or(turn.last_usage.clone());
            let Some(usage) = usage else {
                continue;
            };
            for line_number in &turn.usage_lines {
                consumed_usage_lines.insert(*line_number);
            }
            if record.usage.is_some() {
                consumed_usage_lines.insert(record.line_number);
            }
            let explicit_duration_ms = record.task_duration_ms;
            let duration_ms = explicit_duration_ms
                .or_else(|| codex_duration_from_turn_timestamps(turn.started_at, completed_at));
            let latency_source = explicit_duration_ms
                .map(|_| LatencySource::Explicit)
                .or_else(|| duration_ms.map(|_| LatencySource::Inferred));
            let time_to_first_token_ms = record.time_to_first_token_ms;
            let event = usage_event(
                ctx.adapter,
                ctx.source,
                ctx.options,
                ProviderEventParts {
                    timestamp: completed_at,
                    session_started_at: Some(turn.started_at),
                    session_ended_at: Some(completed_at),
                    duration_seconds: duration_ms.map(|value| value / 1000),
                    model: record.model.clone().or(turn.model.clone()),
                    usage,
                    runtime: Some(RuntimeInfo {
                        runtime_name: None,
                        host_id: None,
                        latency_ms: duration_ms,
                        latency_source,
                        time_to_first_token_ms,
                        prompt_eval_duration_ms: None,
                        eval_duration_ms: None,
                        total_messages: Some(turn.message_counts.total),
                        user_messages: Some(turn.message_counts.user),
                        assistant_messages: Some(turn.message_counts.assistant),
                        developer_messages: Some(turn.message_counts.developer),
                    }),
                    session_raw: turn.session_raw,
                    project: record
                        .project
                        .clone()
                        .or(turn.project.clone())
                        .or_else(|| file_fallback_project.clone()),
                    event_kind: "codex_turn_usage",
                    source_file: path,
                    source_line_number: Some(record.line_number),
                    source_type: "jsonl",
                    model_inferred: record.model_inferred || turn.model_inferred,
                    timestamp_inferred: record.timestamp_inferred || turn.timestamp_inferred,
                    deduplication: EventDeduplication::PathIndependent,
                    dedupe_salt: None,
                },
            );
            let mut linked_quota_lines = turn.usage_lines.clone();
            if record.usage.is_some() {
                linked_quota_lines.push(record.line_number);
            }
            link_quota_observations(
                ctx.scan,
                &quota_observation_indices,
                &linked_quota_lines,
                &event.event_id,
                QuotaUsageLinkKind::TurnEvent,
            );
            let task_span = if ctx.options.should_collect_tasks() {
                let event_id = event.event_id.clone();
                let event_cost = event.cost.estimated_api_equivalent_usd;
                let event_cost_micro_usd = event.cost.estimated_api_equivalent_micro_usd;
                let prompt_previews = materialize_codex_task_previews(&turn.prompt_previews);
                let prompt_preview = choose_best_task_preview(&prompt_previews);
                let has_prompt_preview = prompt_preview.is_some();
                let (title, title_source, is_meta) =
                    codex_task_title(turn.title.as_deref(), prompt_preview.as_deref());
                let normalized_title = normalize_task_title(&title);
                let project = record
                    .project
                    .clone()
                    .or(turn.project.clone())
                    .or_else(|| file_fallback_project.clone());
                let issue_keys = extract_issue_keys(&[
                    title.as_str(),
                    prompt_preview.as_deref().unwrap_or(""),
                    project
                        .as_ref()
                        .and_then(|project| project.branch_label.as_deref())
                        .unwrap_or(""),
                ]);
                let branch_family = branch_family(
                    project
                        .as_ref()
                        .and_then(|project| project.branch_label.as_deref()),
                );
                let project_bucket = project_bucket_key(project.as_ref());
                let usage_snapshot = event.usage.clone();
                Some(TaskSpan {
                    schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                    span_id: task_span_id(
                        ctx.adapter.provider(),
                        &ctx.source.source_id,
                        &format!(
                            "codex_task_span.v1:{}:{}:{}:{}",
                            record.session_raw,
                            turn.started_at.to_rfc3339(),
                            completed_at.to_rfc3339(),
                            record.line_number
                        ),
                    ),
                    provider: ctx.adapter.provider().to_string(),
                    source_id: ctx.source.source_id.clone(),
                    span_kind: "codex_task".to_string(),
                    source_record_id: Some(format!(
                        "codex_task_span.v1:{}:{}",
                        record.session_raw, record.line_number
                    )),
                    source_file_path_hash: Some(hash_text(&canonical_display(path))),
                    summary_id: None,
                    session_id: Some(record.session_raw.clone()),
                    thread_id: record.thread_id.clone().or(turn.thread_id.clone()),
                    title,
                    normalized_title,
                    title_source: Some(title_source.to_string()),
                    summary_preview: prompt_preview,
                    todo_excerpt: None,
                    issue_keys,
                    branch_family,
                    project_bucket,
                    project,
                    git: None,
                    usage: usage_snapshot,
                    estimated_cost_usd: event_cost,
                    estimated_cost_micro_usd: event_cost_micro_usd,
                    event_count: 1,
                    has_usage_evidence: true,
                    total_messages: turn.message_counts.total,
                    user_messages: turn.message_counts.user,
                    assistant_messages: turn.message_counts.assistant,
                    developer_messages: turn.message_counts.developer,
                    linked_event_ids: vec![event_id],
                    confidence: if turn.title.is_some() {
                        Confidence::High
                    } else if has_prompt_preview {
                        Confidence::Medium
                    } else {
                        Confidence::Low
                    },
                    is_meta,
                    started_at: turn.started_at,
                    ended_at: Some(completed_at),
                    duration_seconds: duration_ms.map(|value| value / 1000),
                })
            } else {
                None
            };
            push_deduped(ctx.scan, ctx.seen, event);
            if let Some(task_span) = task_span {
                ctx.scan.task_spans.push(task_span);
            }
        }
    }

    if ctx.options.should_collect_tasks() {
        for turn in active_turns {
            let prompt_previews = materialize_codex_task_previews(&turn.prompt_previews);
            let prompt_preview = choose_best_task_preview(&prompt_previews);
            let (title, title_source, is_meta) =
                codex_task_title(turn.title.as_deref(), prompt_preview.as_deref());
            let normalized_title = normalize_task_title(&title);
            let project = turn
                .project
                .clone()
                .or_else(|| file_fallback_project.clone());
            let issue_keys = extract_issue_keys(&[
                title.as_str(),
                prompt_preview.as_deref().unwrap_or(""),
                project
                    .as_ref()
                    .and_then(|project| project.branch_label.as_deref())
                    .unwrap_or(""),
            ]);
            ctx.scan.task_spans.push(TaskSpan {
                schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                span_id: task_span_id(
                    ctx.adapter.provider(),
                    &ctx.source.source_id,
                    &format!(
                        "codex_task_span.v1:{}:{}:open",
                        turn.session_raw,
                        turn.started_at.to_rfc3339()
                    ),
                ),
                provider: ctx.adapter.provider().to_string(),
                source_id: ctx.source.source_id.clone(),
                span_kind: "codex_task".to_string(),
                source_record_id: Some(format!(
                    "codex_task_span.v1:{}:{}:open",
                    turn.session_raw,
                    turn.started_at.to_rfc3339()
                )),
                source_file_path_hash: Some(hash_text(&canonical_display(path))),
                summary_id: None,
                session_id: Some(turn.session_raw.clone()),
                thread_id: turn.thread_id.clone(),
                title,
                normalized_title,
                title_source: Some(title_source.to_string()),
                summary_preview: prompt_preview,
                todo_excerpt: None,
                issue_keys,
                branch_family: branch_family(
                    project
                        .as_ref()
                        .and_then(|project| project.branch_label.as_deref()),
                ),
                project_bucket: project_bucket_key(project.as_ref()),
                project,
                git: None,
                usage: turn.accumulated_usage.unwrap_or_default(),
                estimated_cost_usd: None,
                estimated_cost_micro_usd: None,
                event_count: 0,
                has_usage_evidence: false,
                total_messages: turn.message_counts.total,
                user_messages: turn.message_counts.user,
                assistant_messages: turn.message_counts.assistant,
                developer_messages: turn.message_counts.developer,
                linked_event_ids: Vec::new(),
                confidence: if turn.title.is_some() {
                    Confidence::Medium
                } else if turn.prompt_previews.is_empty() {
                    Confidence::Low
                } else {
                    Confidence::Medium
                },
                is_meta,
                started_at: turn.started_at,
                ended_at: None,
                duration_seconds: None,
            });
        }
    }

    for record in records {
        let Some(usage) = record.usage else {
            continue;
        };
        if consumed_usage_lines.contains(&record.line_number) {
            continue;
        }
        let event = usage_event(
            ctx.adapter,
            ctx.source,
            ctx.options,
            ProviderEventParts {
                timestamp: record.timestamp,
                session_started_at: None,
                session_ended_at: None,
                duration_seconds: None,
                model: record.model,
                usage,
                runtime: None,
                session_raw: record.session_raw,
                project: record
                    .project
                    .or_else(|| project_context_from_path_fallback(root, path)),
                event_kind: if record.is_token_count_event {
                    "codex_token_count"
                } else {
                    "codex_headless_usage"
                },
                source_file: path,
                source_line_number: Some(record.line_number),
                source_type: "jsonl",
                model_inferred: record.model_inferred,
                timestamp_inferred: record.timestamp_inferred,
                deduplication: if record.is_token_count_event {
                    EventDeduplication::PathIndependent
                } else {
                    EventDeduplication::SessionScoped
                },
                dedupe_salt: None,
            },
        );
        link_quota_observations(
            ctx.scan,
            &quota_observation_indices,
            &[record.line_number],
            &event.event_id,
            QuotaUsageLinkKind::RecordEvent,
        );
        push_deduped(ctx.scan, ctx.seen, event);
    }

    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct CodexLineRecord {
    pub(crate) line_number: usize,
    pub(crate) timestamp: DateTime<Utc>,
    pub(crate) timestamp_inferred: bool,
    pub(crate) session_raw: String,
    pub(crate) model: Option<ModelInfo>,
    pub(crate) model_inferred: bool,
    pub(crate) model_explicit: bool,
    pub(crate) usage: Option<UsageCounts>,
    pub(crate) is_token_count_event: bool,
    pub(crate) is_task_started: bool,
    pub(crate) is_task_complete: bool,
    pub(crate) message_role: Option<String>,
    pub(crate) user_message_preview: Option<CodexPromptPreviewCandidate>,
    pub(crate) session_title: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) project: Option<ProjectInfo>,
    pub(crate) task_started_at: Option<DateTime<Utc>>,
    pub(crate) task_completed_at: Option<DateTime<Utc>>,
    pub(crate) task_duration_ms: Option<u64>,
    pub(crate) time_to_first_token_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CodexPromptPreviewSource {
    ResponseItemUser,
    UserMessageEvent,
}

impl CodexPromptPreviewSource {
    pub(crate) const fn priority(self) -> i32 {
        match self {
            Self::ResponseItemUser => 0,
            Self::UserMessageEvent => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexPromptPreview {
    pub(crate) text: String,
    pub(crate) source: CodexPromptPreviewSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexPromptPreviewCandidate {
    pub(crate) raw_text: String,
    pub(crate) source: CodexPromptPreviewSource,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexFastResponseMessageLine<'a> {
    #[serde(default, borrow)]
    pub(crate) timestamp: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    pub(crate) session_id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    pub(crate) payload: CodexFastResponseMessagePayload<'a>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexFastResponseMessagePayload<'a> {
    #[serde(default, borrow)]
    pub(crate) role: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    pub(crate) content: Option<Vec<CodexFastContentPart<'a>>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexFastContentPart<'a> {
    #[serde(default, borrow)]
    pub(crate) text: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    pub(crate) content: Option<CodexFastNestedText<'a>>,
    #[serde(default, borrow)]
    pub(crate) input: Option<CodexFastNestedText<'a>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexFastNestedText<'a> {
    #[serde(default, borrow)]
    pub(crate) text: Option<Cow<'a, str>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CodexMessageCounts {
    pub(crate) total: u64,
    pub(crate) user: u64,
    pub(crate) assistant: u64,
    pub(crate) developer: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ActiveCodexTurn {
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) session_raw: String,
    pub(crate) title: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) model: Option<ModelInfo>,
    pub(crate) model_inferred: bool,
    pub(crate) timestamp_inferred: bool,
    pub(crate) message_counts: CodexMessageCounts,
    pub(crate) last_usage: Option<UsageCounts>,
    pub(crate) accumulated_usage: Option<UsageCounts>,
    pub(crate) prompt_previews: Vec<CodexPromptPreviewCandidate>,
    pub(crate) last_activity_at: DateTime<Utc>,
    pub(crate) usage_lines: Vec<usize>,
    pub(crate) project: Option<ProjectInfo>,
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

// Codex reports cached input and reasoning output as subsets of the top-level
// input/output counters. Normalize that inclusive provider shape into the
// additive contract used everywhere else in statsai.
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

pub(crate) fn is_codex_session_meta(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("session_meta")
}

pub(crate) fn codex_model_from_value(value: &Value, fallback: Option<&str>) -> Option<ModelInfo> {
    model_from_nested_value(value, fallback)
}

pub(crate) fn is_codex_turn_context(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("turn_context")
}

pub(crate) fn is_codex_token_count(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("event_msg")
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("token_count")
}

pub(crate) fn is_codex_task_started(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("event_msg")
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("task_started")
}

pub(crate) fn is_codex_task_complete(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("event_msg")
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("task_complete")
}

pub(crate) fn codex_visible_message_role(value: &Value) -> Option<&str> {
    (value.get("type").and_then(Value::as_str) == Some("response_item")
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("message"))
    .then(|| value.pointer("/payload/role").and_then(Value::as_str))
    .flatten()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexLineKind {
    Irrelevant,
    SessionMeta,
    TurnContext,
    ResponseItemMessage,
    EventUserMessage,
    TokenCount,
    TaskStarted,
    TaskComplete,
    HeadlessUsage,
}

#[derive(Deserialize)]
pub(crate) struct CodexQuotaLineProbe {
    #[serde(rename = "type")]
    pub(crate) line_type: Option<String>,
    pub(crate) payload: Option<CodexQuotaPayloadProbe>,
}

#[derive(Deserialize)]
pub(crate) struct CodexQuotaPayloadProbe {
    #[serde(rename = "type")]
    pub(crate) payload_type: Option<String>,
    pub(crate) rate_limits: Option<serde::de::IgnoredAny>,
}

pub(crate) fn is_codex_quota_line_structurally(line: &str) -> bool {
    if !line.contains("\"event_msg\"")
        || !line.contains("\"token_count\"")
        || !line.contains("\"rate_limits\"")
    {
        return false;
    }
    serde_json::from_str::<CodexQuotaLineProbe>(line)
        .ok()
        .is_some_and(|probe| {
            probe.line_type.as_deref() == Some("event_msg")
                && probe.payload.is_some_and(|payload| {
                    payload.payload_type.as_deref() == Some("token_count")
                        && payload.rate_limits.is_some()
                })
        })
}

pub(crate) fn codex_line_header(line: &str) -> &str {
    codex_prefix_at_char_boundary(line, 256)
}

pub(crate) fn codex_line_kind(line: &str) -> CodexLineKind {
    let header = codex_line_header(line);
    if header.contains("\"type\":\"session_meta\"") {
        return CodexLineKind::SessionMeta;
    }
    if header.contains("\"type\":\"turn_context\"") {
        return CodexLineKind::TurnContext;
    }
    if header.contains("\"type\":\"response_item\"") {
        return if header.contains("\"payload\":{\"type\":\"message\"") {
            CodexLineKind::ResponseItemMessage
        } else {
            CodexLineKind::Irrelevant
        };
    }
    if header.contains("\"type\":\"event_msg\"") {
        if header.contains("\"payload\":{\"type\":\"user_message\"") {
            return CodexLineKind::EventUserMessage;
        }
        if header.contains("\"payload\":{\"type\":\"token_count\"") {
            return CodexLineKind::TokenCount;
        }
        if header.contains("\"payload\":{\"type\":\"task_started\"") {
            return CodexLineKind::TaskStarted;
        }
        if header.contains("\"payload\":{\"type\":\"task_complete\"") {
            return CodexLineKind::TaskComplete;
        }
        return CodexLineKind::Irrelevant;
    }
    if header.contains("\"usage\":")
        || header.contains("\"token_count\":")
        || header.contains("\"message\":{\"usage\":")
        || header.contains("\"data\":{\"usage\":")
        || header.contains("\"result\":{\"usage\":")
        || header.contains("\"response\":{\"usage\":")
    {
        return CodexLineKind::HeadlessUsage;
    }
    CodexLineKind::Irrelevant
}

pub(crate) fn load_codex_thread_titles(root: &Path) -> HashMap<String, String> {
    let index_path = root.join("session_index.jsonl");
    let Ok(file) = File::open(&index_path) else {
        return HashMap::new();
    };
    let mut reader = BufReader::new(file);
    let mut titles = HashMap::new();
    let mut line_bytes = Vec::new();
    while let Ok(line_status) =
        read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)
    {
        if line_status == BoundedLineRead::Eof {
            break;
        }
        if line_status == BoundedLineRead::Oversized {
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(session_id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(title) = value.get("thread_name").and_then(Value::as_str) else {
            continue;
        };
        if let Some(title) = summarize_task_text(Some(title), 90) {
            titles.insert(session_id.to_string(), title);
        }
    }
    titles
}

pub(crate) fn codex_project_context_from_value(
    value: &Value,
    cache: &mut ProjectContextCache,
) -> Option<ProjectInfo> {
    let payload = value.get("payload");
    let project_path = payload
        .and_then(|payload| payload.get("cwd"))
        .and_then(Value::as_str)
        .map(expand_home_path);
    let repository_url = payload
        .and_then(|payload| payload.get("git"))
        .and_then(|git| git.get("repository_url"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let branch = payload
        .and_then(|payload| payload.get("git"))
        .and_then(|git| git.get("branch"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    resolve_project_context_cached(project_path, repository_url, branch, cache)
}

pub(crate) fn codex_headless_usage_value(value: &Value) -> Option<&Value> {
    [
        value.get("usage"),
        value.pointer("/data/usage"),
        value.pointer("/result/usage"),
        value.pointer("/response/usage"),
        value.get("token_count"),
        value.pointer("/event_msg/token_count"),
    ]
    .into_iter()
    .flatten()
    .next()
}

pub(crate) fn session_raw_from_value(value: &Value) -> Option<String> {
    [
        value.get("session_id"),
        value.get("sessionId"),
        value.pointer("/message/sessionId"),
        value.pointer("/message/session_id"),
        value.pointer("/data/session_id"),
        value.pointer("/result/session_id"),
        value.pointer("/response/session_id"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .map(ToOwned::to_owned)
}
