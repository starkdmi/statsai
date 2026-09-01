use super::*;

pub(crate) fn scan_file_state_entries(candidates: &[ScanCandidateFile]) -> Vec<ScanFileStateEntry> {
    candidates
        .iter()
        .map(|candidate| ScanFileStateEntry {
            cache_key: candidate.cache_key.clone(),
            cache_signature: candidate.cache_signature.clone(),
        })
        .collect()
}

pub(crate) fn scan_candidate_compatible_signatures(
    candidates: &[ScanCandidateFile],
) -> HashMap<String, Vec<String>> {
    candidates
        .iter()
        .filter(|candidate| !candidate.compatible_cache_signatures.is_empty())
        .map(|candidate| {
            (
                candidate.cache_key.clone(),
                candidate.compatible_cache_signatures.clone(),
            )
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanFileReconciliation {
    pub(crate) pending_entries: Vec<ScanFileStateEntry>,
    pub(crate) compatible_entries_to_upgrade: Vec<ScanFileStateEntry>,
    pub(crate) removed_entries: Vec<ScanFileStateEntry>,
}

pub(crate) fn select_scan_file_reconciliation(
    store: &Store,
    source_id: &statsai_core::SourceId,
    file_cache_entries: &[ScanFileStateEntry],
    compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    replace: bool,
    no_cache: bool,
    require_tasks_collected: bool,
) -> Result<ScanFileReconciliation> {
    let selection = select_scan_file_state_entries_with_task_requirement_and_compatibility(
        store,
        source_id,
        file_cache_entries,
        compatible_signatures_by_key,
        replace,
        no_cache,
        require_tasks_collected,
    )?;
    let tracked_entries = store.scan_file_entries(source_id)?;
    let current_cache_keys: BTreeSet<_> = file_cache_entries
        .iter()
        .map(|entry| entry.cache_key.as_str())
        .collect();
    let removed_entries = tracked_entries
        .into_iter()
        .filter(|entry| !current_cache_keys.contains(entry.cache_key.as_str()))
        .collect();
    Ok(ScanFileReconciliation {
        pending_entries: selection.pending_entries,
        compatible_entries_to_upgrade: selection.compatible_entries_to_upgrade,
        removed_entries,
    })
}

pub(crate) fn select_scan_file_state_entries_with_task_requirement_and_compatibility(
    store: &Store,
    source_id: &statsai_core::SourceId,
    file_cache_entries: &[ScanFileStateEntry],
    compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    replace: bool,
    no_cache: bool,
    require_tasks_collected: bool,
) -> Result<statsai_store::ScanFileStateSelection> {
    if replace || no_cache {
        return Ok(statsai_store::ScanFileStateSelection {
            pending_entries: file_cache_entries.to_vec(),
            compatible_entries_to_upgrade: Vec::new(),
        });
    }
    store.select_scan_file_state_entries_with_task_requirement_and_compatibility(
        source_id,
        file_cache_entries,
        require_tasks_collected,
        compatible_signatures_by_key,
    )
}

#[cfg(test)]
pub(crate) fn select_scan_file_entries(
    store: &Store,
    source_id: &statsai_core::SourceId,
    file_cache_entries: &[ScanFileStateEntry],
    compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    replace: bool,
    no_cache: bool,
    require_tasks_collected: bool,
) -> Result<Vec<ScanFileStateEntry>> {
    Ok(
        select_scan_file_state_entries_with_task_requirement_and_compatibility(
            store,
            source_id,
            file_cache_entries,
            compatible_signatures_by_key,
            replace,
            no_cache,
            require_tasks_collected,
        )?
        .pending_entries,
    )
}

pub(crate) fn should_replace_source_records_for_scan(
    explicit_replace: bool,
    no_cache: bool,
    candidate_count: usize,
    pending_count: usize,
    legacy_full_reconcile: bool,
) -> bool {
    explicit_replace
        || no_cache
        || legacy_full_reconcile
        || (candidate_count > 0 && pending_count == candidate_count)
}

/// Whether this scan must delete every quota row the source owns before reinserting.
///
/// `--no-cache` used to be here. It reparses every file, so file-level reconciliation already
/// covers everything it rewrites, and the only rows the file path could miss are those no current
/// file accounts for -- which `delete_quota_observations_for_source_outside_file_hashes` retires
/// directly. Deleting the whole source to reach them walked every observation, every window, and
/// the payload table, and on a multi-gigabyte store that ran for tens of minutes. `--replace` keeps
/// the blanket delete: it is documented as a destructive rebuild, and a caller asking for one is
/// asking for exactly that.
pub(crate) fn should_replace_all_source_quota_records(
    explicit_replace: bool,
    legacy_full_reconcile: bool,
) -> bool {
    explicit_replace || legacy_full_reconcile
}

pub(crate) fn scan_file_hashes_for_reconciliation(
    pending_entries: &[ScanFileStateEntry],
    removed_entries: &[ScanFileStateEntry],
) -> Vec<String> {
    pending_entries
        .iter()
        .chain(removed_entries.iter())
        .map(|entry| hash_text(&entry.cache_key))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn scan_file_cache_keys(entries: &[ScanFileStateEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.cache_key.clone())
        .collect()
}

pub(crate) fn rewrite_task_span_linked_event_ids(
    task_spans: &mut [TaskSpan],
    canonical_event_ids: &HashMap<EventId, EventId>,
) {
    for span in task_spans {
        if span.linked_event_ids.is_empty() {
            continue;
        }
        let mut rewritten = Vec::with_capacity(span.linked_event_ids.len());
        let mut seen = HashSet::new();
        for event_id in &span.linked_event_ids {
            let canonical = canonical_event_ids
                .get(event_id)
                .cloned()
                .unwrap_or_else(|| event_id.clone());
            if seen.insert(canonical.clone()) {
                rewritten.push(canonical);
            }
        }
        span.linked_event_ids = rewritten;
    }
}

pub(crate) fn rewrite_quota_usage_event_ids(
    observations: &mut [QuotaObservationRecordV1],
    canonical_event_ids: &HashMap<EventId, EventId>,
) {
    for record in observations {
        let Some(event_id) = record.observation.usage_event_id.as_ref() else {
            continue;
        };
        if let Some(canonical) = canonical_event_ids.get(event_id) {
            record.observation.usage_event_id = Some(canonical.clone());
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TaskSpanRuntimeRollup {
    pub(crate) total_messages: u64,
    pub(crate) user_messages: u64,
    pub(crate) assistant_messages: u64,
    pub(crate) developer_messages: u64,
}

pub(crate) fn populate_task_span_rollups(
    task_spans: &mut [TaskSpan],
    events: &[UsageEvent],
    canonical_event_ids: &HashMap<EventId, EventId>,
) {
    let mut event_rollups = HashMap::<String, TaskSpanRuntimeRollup>::new();
    for event in events {
        let canonical_event_id = canonical_event_ids
            .get(&event.event_id)
            .unwrap_or(&event.event_id)
            .0
            .clone();
        event_rollups
            .entry(canonical_event_id)
            .or_insert_with(|| task_span_runtime_rollup(event));
    }

    for span in task_spans {
        let mut total_messages = 0u64;
        let mut user_messages = 0u64;
        let mut assistant_messages = 0u64;
        let mut developer_messages = 0u64;
        let mut seen_event_ids = HashSet::<String>::new();
        for event_id in &span.linked_event_ids {
            if !seen_event_ids.insert(event_id.0.clone()) {
                continue;
            }
            let Some(rollup) = event_rollups.get(&event_id.0) else {
                continue;
            };
            total_messages = total_messages.saturating_add(rollup.total_messages);
            user_messages = user_messages.saturating_add(rollup.user_messages);
            assistant_messages = assistant_messages.saturating_add(rollup.assistant_messages);
            developer_messages = developer_messages.saturating_add(rollup.developer_messages);
        }
        span.event_count = span.event_count.max(seen_event_ids.len() as u64);
        span.has_usage_evidence = span.has_usage_evidence || span.event_count > 0;
        span.total_messages = span.total_messages.max(total_messages);
        span.user_messages = span.user_messages.max(user_messages);
        span.assistant_messages = span.assistant_messages.max(assistant_messages);
        span.developer_messages = span.developer_messages.max(developer_messages);
    }
}

pub(crate) fn task_span_runtime_rollup(event: &UsageEvent) -> TaskSpanRuntimeRollup {
    let Some(runtime) = event.runtime.as_ref() else {
        return TaskSpanRuntimeRollup::default();
    };
    let user_messages = runtime.user_messages.unwrap_or(0);
    let assistant_messages = runtime.assistant_messages.unwrap_or(0);
    let developer_messages = runtime.developer_messages.unwrap_or(0);
    let total_messages = runtime.total_messages.unwrap_or_else(|| {
        user_messages
            .saturating_add(assistant_messages)
            .saturating_add(developer_messages)
    });
    TaskSpanRuntimeRollup {
        total_messages,
        user_messages,
        assistant_messages,
        developer_messages,
    }
}

pub(crate) fn format_cache_key_sample<'a>(keys: impl IntoIterator<Item = &'a str>) -> String {
    let values: Vec<_> = keys.into_iter().map(abbreviate_home).collect();
    if values.is_empty() {
        return "none".to_string();
    }
    let sample: Vec<_> = values.iter().take(3).cloned().collect();
    let remaining = values.len().saturating_sub(sample.len());
    if remaining == 0 {
        sample.join(", ")
    } else {
        format!("{} (+{} more)", sample.join(", "), remaining)
    }
}

pub(crate) fn scan_sources_for_adapter(
    adapter: &dyn ProviderAdapter,
    configured_sources: &[SourceLocation],
) -> Vec<SourceLocation> {
    let configured_sources = configured_sources
        .iter()
        .filter(|source| {
            provider_matches(&source.provider, adapter.provider())
                && source.source_kind == SourceKind::LocalAdapter
        })
        .cloned()
        .map(|mut source| {
            if source.path_label.is_none() {
                source.path_label = path_label_from_hashless_source(&source);
            }
            source
        })
        .collect::<Vec<_>>();
    let mut sources = BTreeMap::new();
    for mut source in adapter.discover() {
        if source.path_label.is_none() {
            source.path_label = path_label_from_hashless_source(&source);
        }
        if configured_sources
            .iter()
            .any(|configured| sources_refer_to_same_location(&source, configured))
        {
            continue;
        }
        sources.insert(source.source_id.0.clone(), source);
    }
    for source in configured_sources
        .into_iter()
        .filter(|source| source.enabled)
    {
        sources.insert(source.source_id.0.clone(), source);
    }
    dedupe_overlapping_sources(
        sources
            .into_values()
            .filter(|source| source.enabled)
            .collect(),
    )
}
