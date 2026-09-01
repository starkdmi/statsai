use crate::*;
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) use crate::tests::options;
#[cfg(test)]
pub(crate) use statsai_core::{display_path, path_hash, ReasoningLevel};

#[derive(Debug, Default)]
pub struct OpenCodeAdapter;

impl ProviderAdapter for OpenCodeAdapter {
    fn id(&self) -> &'static str {
        "opencode-local-sqlite"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn provider(&self) -> &'static str {
        OPENCODE_PROVIDER
    }

    fn discover(&self) -> Vec<SourceLocation> {
        discover_sources_from_env_or_defaults(
            self,
            &["OPENCODE_DATA_DIRS", "OPENCODE_DATA_DIR"],
            &[".local/share/opencode"],
            opencode_root_is_source,
        )
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        opencode_scan_candidates(source, self.version())
    }

    fn scan(&self, source: &SourceLocation, options: &ScanOptions) -> Result<AdapterScan> {
        scan_opencode_source(self, source, options)
    }
}

pub(crate) fn scan_opencode_source(
    adapter: &OpenCodeAdapter,
    source: &SourceLocation,
    options: &ScanOptions,
) -> Result<AdapterScan> {
    let mut scan = AdapterScan::default();
    let Some(root) = source_root_path(source) else {
        return Ok(scan);
    };
    let db_path = root.join("opencode.db");
    if !db_path.is_file() {
        return Ok(scan);
    }

    let connection = open_sqlite_readonly(&db_path)?;
    let todos_by_session = load_opencode_todos(&connection)?;
    let recovered_session_models = load_opencode_session_models(&connection)?;
    let reconstructed_session_ids = recovered_session_models
        .iter()
        .filter_map(|(session_id, summary)| {
            (summary.ambiguous || summary.has_variant).then_some(session_id.clone())
        })
        .collect::<HashSet<_>>();
    let mut reconstructed_session_rows = HashMap::<String, OpenCodeSessionAggregate>::new();
    let summary_diffs_sql = if sqlite_column_exists(&connection, "session", "summary_diffs")? {
        "summary_diffs"
    } else {
        "NULL AS summary_diffs"
    };
    let mut task_seeds = Vec::<OpenCodeTaskSeed>::new();
    let mut statement = connection.prepare(&format!(
        "SELECT id, title, model, cost, tokens_input, tokens_output, tokens_reasoning, \
         tokens_cache_read, tokens_cache_write, time_created, time_updated, directory, \
         {summary_diffs_sql} \
         FROM session"
    ))?;
    let mut rows = statement.query([])?;
    let mut seen = HashSet::new();
    while let Some(row) = rows.next()? {
        scan.diagnostics.raw_rows += 1;
        let session_id: String = row.get(0)?;
        let title: Option<String> = row.get(1).ok();
        let model_text: Option<String> = row.get(2).ok();
        let provider_cost: f64 = row.get::<_, Option<f64>>(3)?.unwrap_or(0.0);
        let usage = UsageCounts {
            input_tokens: sqlite_nonzero_u64(row.get::<_, i64>(4)?),
            output_tokens: sqlite_nonzero_u64(row.get::<_, i64>(5)?),
            reasoning_tokens: sqlite_nonzero_u64(row.get::<_, i64>(6)?),
            cache_read_tokens: sqlite_nonzero_u64(row.get::<_, i64>(7)?),
            cache_creation_tokens: sqlite_nonzero_u64(row.get::<_, i64>(8)?),
            cache_creation_5m_tokens: None,
            cache_creation_1h_tokens: None,
            total_tokens: None,
            requests: Some(1),
            local_prompt_eval_tokens: None,
            local_eval_tokens: None,
        };
        let started_at = timestamp_from_millis(row.get::<_, i64>(9)?).unwrap_or_else(Utc::now);
        let ended_at = timestamp_from_millis(row.get::<_, i64>(10)?).unwrap_or(started_at);
        let duration_seconds = ended_at
            .signed_duration_since(started_at)
            .num_seconds()
            .try_into()
            .ok();
        let directory: Option<String> = row.get::<_, Option<String>>(11).ok().flatten();
        let summary_diffs = row
            .get::<_, Option<String>>(12)
            .ok()
            .flatten()
            .and_then(|value| summarize_task_text(Some(&value), 220));
        let todos = todos_by_session
            .get(&session_id)
            .cloned()
            .unwrap_or_default();
        let todo_excerpt = summarize_task_text(
            Some(
                &todos
                    .iter()
                    .take(3)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" | "),
            ),
            220,
        );
        let project = directory
            .as_deref()
            .map(PathBuf::from)
            .and_then(|path| resolve_project_context(Some(path), None, None));
        let session_title = title
            .clone()
            .and_then(|value| summarize_task_text(Some(&value), 90));
        let preferred_session_title = session_title.clone().filter(|value| {
            !task_title_is_generic(Some(value.as_str()))
                && !task_title_is_weak_signal(Some(value.as_str()))
        });
        let has_preferred_session_title = preferred_session_title.is_some();
        let inferred_title = preferred_session_title
            .or_else(|| task_title_from_prompt(summary_diffs.as_deref()))
            .or_else(|| task_title_from_prompt(todo_excerpt.as_deref()));
        task_seeds.push(OpenCodeTaskSeed {
            session_id: session_id.clone(),
            title: inferred_title.or(session_title.clone()),
            title_source: if has_preferred_session_title {
                "session_title"
            } else if summary_diffs.is_some() {
                "summary_diffs"
            } else if todo_excerpt.is_some() {
                "todo_excerpt"
            } else if session_title.is_some() {
                "session_title_weak"
            } else {
                "default"
            },
            summary_preview: summary_diffs.clone().or_else(|| todo_excerpt.clone()),
            todo_excerpt,
            project: project.clone(),
            started_at,
            ended_at,
            duration_seconds,
            usage: usage.clone(),
            estimated_cost_usd: (provider_cost > 0.0)
                .then_some((provider_cost * 100.0).round() as i64),
            estimated_cost_micro_usd: (provider_cost > 0.0)
                .then(|| usd_to_micro_usd(provider_cost))
                .flatten(),
        });
        if reconstructed_session_ids.contains(&session_id) {
            reconstructed_session_rows.insert(
                session_id.clone(),
                OpenCodeSessionAggregate {
                    title,
                    model_text,
                    provider_cost,
                    usage,
                    started_at,
                    ended_at,
                    duration_seconds,
                    directory,
                },
            );
            continue;
        }
        if usage.computed_total() == 0 {
            scan.diagnostics.skipped_zero_events += 1;
            continue;
        }
        scan.diagnostics.candidate_usage_rows += 1;
        let model = model_text
            .as_deref()
            .and_then(opencode_model_info)
            .or_else(|| {
                recovered_session_models
                    .get(&session_id)
                    .and_then(|summary| summary.model.clone())
            });
        let model_inferred = model.is_none();
        if model_inferred {
            scan.diagnostics.model_fallbacks += 1;
        }
        let mut event = usage_event(
            adapter,
            source,
            options,
            ProviderEventParts {
                timestamp: ended_at,
                session_started_at: Some(started_at),
                session_ended_at: Some(ended_at),
                duration_seconds,
                model,
                usage,
                runtime: None,
                session_raw: session_id,
                project,
                event_kind: "opencode_session_usage",
                source_file: &db_path,
                source_line_number: None,
                source_type: "sqlite:session",
                model_inferred,
                timestamp_inferred: false,
                deduplication: EventDeduplication::PathIndependent,
                dedupe_salt: None,
            },
        );
        event.session.title = title.filter(|title| !title.trim().is_empty());
        if provider_cost > 0.0 {
            if let Some(provider_cost_micro_usd) = usd_to_micro_usd(provider_cost) {
                event
                    .cost
                    .set_provider_reported_micro_usd(provider_cost_micro_usd);
            }
            event.cost.pricing_source = Some("opencode.session.cost".to_string());
            event.cost.confidence = Confidence::High;
        }
        push_deduped(&mut scan, &mut seen, event);
    }
    if !reconstructed_session_ids.is_empty() {
        let reconstructed_usage = emit_opencode_message_events(
            &connection,
            &mut OpenCodeMessageEventContext {
                db_path: &db_path,
                reconstructed_session_ids: &reconstructed_session_ids,
                adapter,
                source,
                options,
                scan: &mut scan,
                seen: &mut seen,
            },
        )?;
        for (session_id, aggregate) in reconstructed_session_rows {
            let reconstructed = reconstructed_usage.get(&session_id);
            if opencode_usage_fully_reconstructed(
                &aggregate.usage,
                reconstructed.map(|value| &value.usage),
            ) {
                continue;
            }
            let residual_usage =
                subtract_usage_counts(&aggregate.usage, reconstructed.map(|value| &value.usage));
            if residual_usage.computed_total() == 0 {
                continue;
            }
            scan.diagnostics.candidate_usage_rows += 1;
            let project = aggregate
                .directory
                .as_deref()
                .map(PathBuf::from)
                .and_then(|path| resolve_project_context(Some(path), None, None));
            let model = recovered_session_models
                .get(&session_id)
                .and_then(|summary| {
                    if summary.model_conflict {
                        return None;
                    }
                    let session_model = aggregate
                        .model_text
                        .as_deref()
                        .and_then(opencode_model_info);
                    match (session_model, summary.model.clone()) {
                        (Some(mut session_model), Some(recovered))
                            if same_model_identity(Some(&session_model), &recovered) =>
                        {
                            apply_reasoning_state(
                                &mut session_model,
                                &reasoning_state_from_model(&recovered),
                            );
                            Some(session_model)
                        }
                        (Some(session_model), _) => Some(session_model),
                        (None, Some(recovered)) => Some(recovered),
                        (None, None) => None,
                    }
                });
            let model_inferred = model.is_none();
            if model_inferred {
                scan.diagnostics.model_fallbacks += 1;
            }
            let mut event = usage_event(
                adapter,
                source,
                options,
                ProviderEventParts {
                    timestamp: aggregate.ended_at,
                    session_started_at: Some(aggregate.started_at),
                    session_ended_at: Some(aggregate.ended_at),
                    duration_seconds: aggregate.duration_seconds,
                    model,
                    usage: residual_usage,
                    runtime: None,
                    session_raw: session_id,
                    project,
                    event_kind: "opencode_session_usage",
                    source_file: &db_path,
                    source_line_number: None,
                    source_type: "sqlite:session",
                    model_inferred,
                    timestamp_inferred: false,
                    deduplication: EventDeduplication::PathIndependent,
                    dedupe_salt: None,
                },
            );
            event.session.title = aggregate.title.filter(|title| !title.trim().is_empty());
            let aggregate_provider_cost_micro_usd =
                usd_to_micro_usd(aggregate.provider_cost).unwrap_or(0);
            let residual_provider_cost_micro_usd = aggregate_provider_cost_micro_usd
                .saturating_sub(
                    reconstructed
                        .map(|value| value.provider_reported_micro_usd)
                        .unwrap_or(0),
                );
            if residual_provider_cost_micro_usd > 0 {
                event
                    .cost
                    .set_provider_reported_micro_usd(residual_provider_cost_micro_usd);
                event.cost.pricing_source = Some("opencode.session.cost".to_string());
                event.cost.confidence = Confidence::High;
            }
            push_deduped(&mut scan, &mut seen, event);
        }
    }
    if options.should_collect_tasks() {
        let event_rollups = session_event_rollups(&scan.events);
        for seed in task_seeds {
            let session_hash = hash_text(&seed.session_id);
            let event_rollup = event_rollups.get(&session_hash);
            let title = seed
                .title
                .clone()
                .unwrap_or_else(|| "OpenCode session".to_string());
            let issue_keys = extract_issue_keys(&[
                title.as_str(),
                seed.summary_preview.as_deref().unwrap_or(""),
                seed.todo_excerpt.as_deref().unwrap_or(""),
                seed.project
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
                        "opencode_task_span.v1:{}:{}",
                        seed.session_id,
                        seed.ended_at.to_rfc3339()
                    ),
                ),
                provider: adapter.provider().to_string(),
                source_id: source.source_id.clone(),
                span_kind: "opencode_session".to_string(),
                source_record_id: Some(seed.session_id.clone()),
                source_file_path_hash: Some(hash_text(&canonical_display(&db_path))),
                summary_id: None,
                session_id: Some(seed.session_id.clone()),
                thread_id: None,
                title: title.clone(),
                normalized_title: normalize_task_title(&title),
                title_source: Some(seed.title_source.to_string()),
                summary_preview: seed.summary_preview.clone(),
                todo_excerpt: seed.todo_excerpt.clone(),
                issue_keys,
                branch_family: branch_family(
                    seed.project
                        .as_ref()
                        .and_then(|project| project.branch_label.as_deref()),
                ),
                project_bucket: project_bucket_key(seed.project.as_ref()),
                project: seed.project.clone(),
                git: None,
                usage: event_rollup
                    .map(|rollup| rollup.usage.clone())
                    .filter(|usage| usage.computed_total() > 0)
                    .unwrap_or_else(|| seed.usage.clone()),
                estimated_cost_usd: event_rollup
                    .and_then(|rollup| rollup.cost.cents_rounded())
                    .or(seed.estimated_cost_usd),
                estimated_cost_micro_usd: event_rollup
                    .and_then(|rollup| rollup.cost.micro_usd())
                    .or(seed.estimated_cost_micro_usd),
                event_count: event_rollup
                    .map(|rollup| rollup.event_ids.len() as u64)
                    .unwrap_or(0),
                has_usage_evidence: event_rollup.is_some_and(|rollup| !rollup.event_ids.is_empty()),
                total_messages: 0,
                user_messages: 0,
                assistant_messages: 0,
                developer_messages: 0,
                linked_event_ids: event_rollup
                    .map(|rollup| rollup.event_ids.clone())
                    .unwrap_or_default(),
                confidence: if seed.title_source == "session_title"
                    && !task_title_is_generic(Some(title.as_str()))
                {
                    Confidence::High
                } else if seed.summary_preview.is_some() || seed.todo_excerpt.is_some() {
                    Confidence::Medium
                } else {
                    Confidence::Low
                },
                is_meta: task_title_is_generic(Some(title.as_str())),
                started_at: seed.started_at,
                ended_at: Some(seed.ended_at),
                duration_seconds: seed.duration_seconds,
            });
        }
    }
    scan.diagnostics.files_scanned = 1;
    scan.diagnostics.accepted_events = scan.events.len() as u64;
    Ok(scan)
}

pub(crate) fn load_opencode_session_models(
    connection: &Connection,
) -> Result<HashMap<String, OpenCodeSessionModelSummary>> {
    let mut statement = match connection.prepare(
        "SELECT session_id, data, \
                coalesce(json_extract(data, '$.tokens.input'), 0), \
                coalesce(json_extract(data, '$.tokens.output'), 0), \
                coalesce(json_extract(data, '$.tokens.reasoning'), 0), \
                coalesce(json_extract(data, '$.tokens.cache.read'), 0), \
                coalesce(json_extract(data, '$.tokens.cache.write'), 0) \
         FROM message \
         WHERE json_extract(data, '$.providerID') IS NOT NULL \
            OR json_extract(data, '$.provider_id') IS NOT NULL \
            OR json_extract(data, '$.modelID') IS NOT NULL \
            OR json_extract(data, '$.id') IS NOT NULL \
            OR json_extract(data, '$.model') IS NOT NULL \
            OR json_extract(data, '$.variant') IS NOT NULL \
            OR json_extract(data, '$.model.variant') IS NOT NULL \
            OR coalesce(json_extract(data, '$.tokens.input'), 0) > 0 \
            OR coalesce(json_extract(data, '$.tokens.output'), 0) > 0 \
            OR coalesce(json_extract(data, '$.tokens.reasoning'), 0) > 0 \
            OR coalesce(json_extract(data, '$.tokens.cache.read'), 0) > 0 \
            OR coalesce(json_extract(data, '$.tokens.cache.write'), 0) > 0",
    ) {
        Ok(statement) => statement,
        Err(error) if error.to_string().contains("no such table: message") => {
            return Ok(HashMap::new());
        }
        Err(error) => return Err(error.into()),
    };
    let mut rows = statement.query([])?;
    let mut models = HashMap::<String, OpenCodeSessionModelSummary>::new();
    while let Some(row) = rows.next()? {
        let session_id: String = row.get(0)?;
        let data_text: String = row.get(1)?;
        let value = match serde_json::from_str::<Value>(&data_text) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let usage = UsageCounts {
            input_tokens: sqlite_nonzero_u64(row.get::<_, i64>(2)?),
            output_tokens: sqlite_nonzero_u64(row.get::<_, i64>(3)?),
            reasoning_tokens: sqlite_nonzero_u64(row.get::<_, i64>(4)?),
            cache_read_tokens: sqlite_nonzero_u64(row.get::<_, i64>(5)?),
            cache_creation_tokens: sqlite_nonzero_u64(row.get::<_, i64>(6)?),
            cache_creation_5m_tokens: None,
            cache_creation_1h_tokens: None,
            total_tokens: None,
            requests: None,
            local_prompt_eval_tokens: None,
            local_eval_tokens: None,
        };
        let model = opencode_message_model_info(&value);
        let entry = models.entry(session_id).or_default();
        entry.has_variant |= opencode_message_has_variant(&value);
        if usage.computed_total() > 0 && model.is_none() {
            entry.ambiguous = true;
            continue;
        }
        let Some(model) = model else {
            continue;
        };
        // Ambiguous usage rows can still be followed by explicit model context that
        // reveals whether residual aggregate usage is safe to label or must stay model-less.
        match entry.model.as_ref() {
            None => entry.model = Some(model),
            Some(existing) if same_model_identity(Some(existing), &model) => {
                let existing_reasoning = reasoning_state_from_model(existing);
                let model_reasoning = reasoning_state_from_model(&model);
                let existing_has_reasoning =
                    existing_reasoning.level.is_some() || existing_reasoning.raw.is_some();
                let model_has_reasoning =
                    model_reasoning.level.is_some() || model_reasoning.raw.is_some();
                if !existing_has_reasoning && model_has_reasoning {
                    entry.model = Some(model);
                    continue;
                }
                if existing_has_reasoning
                    && model_has_reasoning
                    && existing_reasoning != model_reasoning
                {
                    entry.model = None;
                    entry.ambiguous = true;
                    entry.model_conflict = true;
                }
            }
            Some(_) => {
                entry.model = None;
                entry.ambiguous = true;
                entry.model_conflict = true;
            }
        }
    }
    Ok(models)
}

pub(crate) fn load_opencode_todos(connection: &Connection) -> Result<HashMap<String, Vec<String>>> {
    let mut statement = match connection
        .prepare("SELECT session_id, content FROM todo ORDER BY session_id, position")
    {
        Ok(statement) => statement,
        Err(error) if error.to_string().contains("no such table: todo") => {
            return Ok(HashMap::new());
        }
        Err(error) => return Err(error.into()),
    };
    let mut rows = statement.query([])?;
    let mut todos = HashMap::<String, Vec<String>>::new();
    while let Some(row) = rows.next()? {
        let session_id: String = row.get(0)?;
        let content: Option<String> = row.get(1).ok();
        let Some(content) = content
            .as_deref()
            .and_then(|value| summarize_task_text(Some(value), 220))
        else {
            continue;
        };
        todos.entry(session_id).or_default().push(content);
    }
    Ok(todos)
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OpenCodeSessionModelSummary {
    pub(crate) model: Option<ModelInfo>,
    pub(crate) ambiguous: bool,
    pub(crate) has_variant: bool,
    pub(crate) model_conflict: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct OpenCodeSessionAggregate {
    pub(crate) title: Option<String>,
    pub(crate) model_text: Option<String>,
    pub(crate) provider_cost: f64,
    pub(crate) usage: UsageCounts,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) ended_at: DateTime<Utc>,
    pub(crate) duration_seconds: Option<u64>,
    pub(crate) directory: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct OpenCodeTaskSeed {
    pub(crate) session_id: String,
    pub(crate) title: Option<String>,
    pub(crate) title_source: &'static str,
    pub(crate) summary_preview: Option<String>,
    pub(crate) todo_excerpt: Option<String>,
    pub(crate) project: Option<ProjectInfo>,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) ended_at: DateTime<Utc>,
    pub(crate) duration_seconds: Option<u64>,
    pub(crate) usage: UsageCounts,
    pub(crate) estimated_cost_usd: Option<i64>,
    pub(crate) estimated_cost_micro_usd: Option<i64>,
}

pub(crate) struct OpenCodeMessageEventContext<'a> {
    pub(crate) db_path: &'a Path,
    pub(crate) reconstructed_session_ids: &'a HashSet<String>,
    pub(crate) adapter: &'a OpenCodeAdapter,
    pub(crate) source: &'a SourceLocation,
    pub(crate) options: &'a ScanOptions,
    pub(crate) scan: &'a mut AdapterScan,
    pub(crate) seen: &'a mut HashSet<String>,
}

pub(crate) fn opencode_usage_fully_reconstructed(
    aggregate: &UsageCounts,
    reconstructed: Option<&UsageCounts>,
) -> bool {
    let Some(reconstructed) = reconstructed else {
        return false;
    };
    aggregate.input_tokens == reconstructed.input_tokens
        && aggregate.output_tokens == reconstructed.output_tokens
        && aggregate.reasoning_tokens == reconstructed.reasoning_tokens
        && aggregate.cache_read_tokens == reconstructed.cache_read_tokens
        && aggregate.cache_creation_tokens == reconstructed.cache_creation_tokens
}

pub(crate) fn emit_opencode_message_events(
    connection: &Connection,
    ctx: &mut OpenCodeMessageEventContext<'_>,
) -> Result<HashMap<String, OpenCodeReconstructedUsage>> {
    let mut statement = connection.prepare(
        "SELECT m.id, m.session_id, m.time_created, m.time_updated, m.data, s.title, s.directory \
         FROM message m \
         JOIN session s ON s.id = m.session_id \
         ORDER BY m.session_id, m.time_created, m.id",
    )?;
    let mut rows = statement.query([])?;
    let mut reconstructed_usage = HashMap::<String, OpenCodeReconstructedUsage>::new();
    let mut session_models = HashMap::<String, ModelInfo>::new();
    while let Some(row) = rows.next()? {
        let session_id: String = row.get(1)?;
        if !ctx.reconstructed_session_ids.contains(&session_id) {
            continue;
        }
        ctx.scan.diagnostics.raw_rows += 1;
        let message_id: String = row.get(0)?;
        let created_at_raw: i64 = row.get(2)?;
        let updated_at_raw: i64 = row.get(3)?;
        let data_text: String = row.get(4)?;
        let title: Option<String> = row.get(5).ok();
        let directory: Option<String> = row.get(6).ok();
        let value: Value = match serde_json::from_str(&data_text) {
            Ok(value) => value,
            Err(_) => {
                ctx.scan.diagnostics.invalid_rows += 1;
                continue;
            }
        };
        if let Some(model) = opencode_message_model_info(&value) {
            session_models.insert(session_id.clone(), model);
        }
        let usage = opencode_message_usage_counts(&value);
        if usage.computed_total() == 0 {
            ctx.scan.diagnostics.skipped_zero_events += 1;
            continue;
        }
        ctx.scan.diagnostics.candidate_usage_rows += 1;
        let Some(model) = session_models.get(&session_id).cloned() else {
            ctx.scan.diagnostics.model_fallbacks += 1;
            continue;
        };
        let started_at = value
            .pointer("/time/created")
            .and_then(value_as_u64)
            .and_then(|value| timestamp_from_millis(value as i64))
            .or_else(|| timestamp_from_millis(created_at_raw))
            .unwrap_or_else(Utc::now);
        let ended_at = value
            .pointer("/time/completed")
            .and_then(value_as_u64)
            .and_then(|value| timestamp_from_millis(value as i64))
            .or_else(|| timestamp_from_millis(updated_at_raw))
            .unwrap_or(started_at);
        let duration_seconds = ended_at
            .signed_duration_since(started_at)
            .num_seconds()
            .try_into()
            .ok();
        let project = directory
            .as_deref()
            .map(PathBuf::from)
            .and_then(|path| resolve_project_context(Some(path), None, None));
        let mut event = usage_event(
            ctx.adapter,
            ctx.source,
            ctx.options,
            ProviderEventParts {
                timestamp: ended_at,
                session_started_at: Some(started_at),
                session_ended_at: Some(ended_at),
                duration_seconds,
                model: Some(model),
                usage,
                runtime: None,
                session_raw: session_id.clone(),
                project,
                event_kind: "opencode_message_usage",
                source_file: ctx.db_path,
                source_line_number: None,
                source_type: "sqlite:message",
                model_inferred: false,
                timestamp_inferred: false,
                deduplication: EventDeduplication::SessionScoped,
                dedupe_salt: Some(message_id),
            },
        );
        event.session.title = title.filter(|title| !title.trim().is_empty());
        if let Some(provider_cost) = value
            .get("cost")
            .and_then(Value::as_f64)
            .filter(|cost| *cost > 0.0)
        {
            if let Some(provider_cost_micro_usd) = usd_to_micro_usd(provider_cost) {
                event
                    .cost
                    .set_provider_reported_micro_usd(provider_cost_micro_usd);
            }
            event.cost.pricing_source = Some("opencode.message.cost".to_string());
            event.cost.confidence = Confidence::High;
        }
        reconstructed_usage
            .entry(session_id)
            .and_modify(|current| {
                current.usage = sum_usage_counts(&current.usage, &event.usage);
                current.provider_reported_micro_usd = current
                    .provider_reported_micro_usd
                    .saturating_add(event.cost.provider_reported_micro_usd_value().unwrap_or(0));
            })
            .or_insert_with(|| OpenCodeReconstructedUsage {
                usage: event.usage.clone(),
                provider_reported_micro_usd: event
                    .cost
                    .provider_reported_micro_usd_value()
                    .unwrap_or(0),
            });
        push_deduped(ctx.scan, ctx.seen, event);
    }
    Ok(reconstructed_usage)
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OpenCodeReconstructedUsage {
    pub(crate) usage: UsageCounts,
    pub(crate) provider_reported_micro_usd: i64,
}

mod scan;

pub(crate) use scan::*;

pub(crate) fn opencode_message_usage_counts(value: &Value) -> UsageCounts {
    UsageCounts {
        input_tokens: value.pointer("/tokens/input").and_then(value_as_u64),
        output_tokens: value.pointer("/tokens/output").and_then(value_as_u64),
        reasoning_tokens: value.pointer("/tokens/reasoning").and_then(value_as_u64),
        cache_read_tokens: value.pointer("/tokens/cache/read").and_then(value_as_u64),
        cache_creation_tokens: value.pointer("/tokens/cache/write").and_then(value_as_u64),
        cache_creation_5m_tokens: None,
        cache_creation_1h_tokens: None,
        total_tokens: None,
        requests: Some(1),
        local_prompt_eval_tokens: None,
        local_eval_tokens: None,
    }
}

#[cfg(test)]
mod tests;
