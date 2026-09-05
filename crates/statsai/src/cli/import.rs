use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use statsai_adapters::{
    cursor_csv_paths, cursor_import_source, parse_cursor_usage_csv, unpriced_model_tokens,
    ScanOptions,
};
use statsai_core::{
    hash_text, normalize_email, normalize_provider_user_id, source_account_assignment_id,
    source_id as statsai_source_id, SourceAccountAssignment, SourceId, SourceKind, SourceLocation,
    UsageEvent, UsageSummary, UsageTotals, SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION,
};
use statsai_sdk::{
    build_reported_usage_summary, ReportedUsageSummaryInput, ReportedUsageSummaryRecord,
    REPORTED_USAGE_IMPORT_ADAPTER_ID,
};
use statsai_store::{apply_current_estimated_pricing, Store};
use std::collections::{btree_map, BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::args::{ImportCommand, ImportSubcommand};
use super::format::{abbreviate_home, format_cost, format_u64};
use super::source::apply_source_account_resolution;

pub(crate) fn import(command: ImportCommand, store: &Store, device_id: &str) -> Result<()> {
    match command.command {
        ImportSubcommand::Summary {
            path,
            replace,
            dry_run,
            verbose,
        } => {
            let inputs = read_reported_summary_inputs(&path)?;
            let records = inputs
                .into_iter()
                .map(|input| build_reported_import_record(input, device_id))
                .collect::<Result<Vec<_>>>()?;
            import_reported_summary_records(
                store,
                &[ReportedImportReport {
                    path,
                    records,
                    warnings: Vec::new(),
                }],
                dry_run,
                verbose,
                replace,
            )?;
        }
        ImportSubcommand::Cursor {
            path,
            dry_run,
            verbose,
        } => import_cursor_usage_events(&path, store, device_id, dry_run, verbose)?,
    }
    Ok(())
}

/// Collapses repeated snapshots of one event down to the one the store keeps.
///
/// A Cursor row accumulates over a session, so overlapping exports carry the
/// same event at different sizes. The largest wins, matching what the store
/// does on insert, so reported totals equal what is actually persisted.
pub(crate) fn dedupe_cursor_snapshots(events: Vec<UsageEvent>) -> Vec<UsageEvent> {
    let mut winners: BTreeMap<String, UsageEvent> = BTreeMap::new();
    for event in events {
        match winners.entry(event.event_id.0.clone()) {
            btree_map::Entry::Occupied(mut slot) => {
                if event.usage.computed_total() > slot.get().usage.computed_total() {
                    slot.insert(event);
                }
            }
            btree_map::Entry::Vacant(slot) => {
                slot.insert(event);
            }
        }
    }
    winners.into_values().collect()
}

/// Imports Cursor dashboard CSV exports.
///
/// Every export writes to one stable manual source, and event identity omits
/// token counts, so re-importing an overlapping range is idempotent and order
/// does not matter: a session's row grows over time, and the store keeps the
/// larger snapshot. See `statsai_adapters::parse_cursor_usage_csv`.
pub(crate) fn import_cursor_usage_events(
    paths: &[PathBuf],
    store: &Store,
    device_id: &str,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    let options = ScanOptions {
        device_id: device_id.to_string(),
        collect_tasks: false,
        selected_cache_keys: None,
    };

    let mut files = Vec::new();
    for path in paths {
        files.extend(cursor_csv_paths(path)?);
    }
    files.sort();
    files.dedup();

    let mut reports = Vec::with_capacity(files.len());
    for file in &files {
        reports.push(parse_cursor_usage_csv(file, &options)?);
    }

    let mut events = Vec::new();
    let mut skipped = 0u64;
    for report in &reports {
        if verbose || dry_run {
            println!(
                "cursor export path={} rows={} events={} skipped={} warnings={}",
                abbreviate_home(report.path.to_string_lossy().as_ref()),
                format_u64(report.rows_read),
                format_u64(report.events.len() as u64),
                format_u64(report.rows_skipped),
                format_u64(report.warnings.len() as u64)
            );
            for warning in &report.warnings {
                println!("  warning: {warning}");
            }
        }
        skipped += report.rows_skipped;
        events.extend(report.events.iter().cloned());
    }

    // Totals are computed after collapsing snapshots, not per file: overlapping
    // exports repeat a session's row, and reporting the sum would double-count
    // every repeat against what the store actually keeps.
    let mut events = dedupe_cursor_snapshots(events);
    let mut total_usage = UsageTotals::default();
    for event in &events {
        total_usage.add_event(event);
    }
    let unpriced = unpriced_model_tokens(&events);

    let unpriced_tokens: u64 = unpriced.values().copied().sum();
    if (verbose || dry_run) && !unpriced.is_empty() {
        println!("models without catalog pricing (tokens counted, cost omitted):");
        for (model, tokens) in unpriced.iter().collect::<Vec<_>>().iter().rev() {
            println!("  {model} tokens={}", format_u64(**tokens));
        }
    }

    if dry_run {
        println!(
            "import preview: files={} events={} skipped_rows={} input={} cache_create={} cache_read={} output={} total={} unpriced_tokens={} cost={} written=0",
            format_u64(files.len() as u64),
            format_u64(events.len() as u64),
            format_u64(skipped),
            format_u64(total_usage.input_tokens),
            format_u64(total_usage.cache_creation_tokens),
            format_u64(total_usage.cached_input_tokens),
            format_u64(total_usage.output_tokens),
            format_u64(total_usage.total_tokens),
            format_u64(unpriced_tokens),
            format_cost(total_usage.estimated_cost_usd)
        );
        return Ok(());
    }

    let source = cursor_import_source();
    store.upsert_source(&source)?;
    // Cursor's export carries no account identity, so attribution comes from
    // whatever the user connected this source to. Resolving it before insert
    // keeps a re-import from unlinking events that were already attributed.
    apply_source_account_resolution(store, &source, &mut events, &mut [])?;
    let inserted = store.insert_events(&events)?;
    // Anything not inserted matched an existing event and refreshed it in place.
    let updated = (events.len() as u64).saturating_sub(inserted);
    println!(
        "import complete: files={} events={} inserted={} updated={} skipped_rows={} input={} cache_create={} cache_read={} output={} total={} unpriced_tokens={} cost={}",
        format_u64(files.len() as u64),
        format_u64(events.len() as u64),
        format_u64(inserted),
        format_u64(updated),
        format_u64(skipped),
        format_u64(total_usage.input_tokens),
        format_u64(total_usage.cache_creation_tokens),
        format_u64(total_usage.cached_input_tokens),
        format_u64(total_usage.output_tokens),
        format_u64(total_usage.total_tokens),
        format_u64(unpriced_tokens),
        format_cost(total_usage.estimated_cost_usd)
    );
    Ok(())
}

pub(crate) fn import_reported_summary_records(
    store: &Store,
    reports: &[ReportedImportReport],
    dry_run: bool,
    verbose: bool,
    replace: bool,
) -> Result<()> {
    let total_summaries: usize = reports.iter().map(|report| report.records.len()).sum();
    let mut total_usage = UsageTotals::default();
    for report in reports {
        for record in &report.records {
            total_usage.add_summary(&record.record.summary);
        }
    }

    if verbose || dry_run {
        for report in reports {
            println!(
                "reported source path={} summaries={} warnings={}",
                abbreviate_home(report.path.to_string_lossy().as_ref()),
                report.records.len(),
                report.warnings.len()
            );
            for warning in &report.warnings {
                println!("  warning: {warning}");
            }
        }
    }

    if dry_run {
        let replace_count = if replace {
            matching_reported_summary_ids(store, reports)?.len() as u64
        } else {
            0
        };
        println!(
            "import preview: sources={} summaries={} replace_existing={} input={} cache_create={} cache_read={} output={} total={} cost={} written=0",
            format_u64(reports.len() as u64),
            format_u64(total_summaries as u64),
            format_u64(replace_count),
            format_u64(total_usage.input_tokens),
            format_u64(total_usage.cache_creation_tokens),
            format_u64(total_usage.cached_input_tokens),
            format_u64(total_usage.output_tokens),
            format_u64(total_usage.total_tokens),
            format_cost(total_usage.estimated_cost_usd)
        );
        return Ok(());
    }

    let replaced = if replace {
        let summary_ids = matching_reported_summary_ids(store, reports)?;
        store.delete_summaries(&summary_ids)?
    } else {
        0
    };
    let mut written = 0u64;
    for report in reports {
        for record in &report.records {
            store.upsert_source(&record.record.source)?;
            written += store.upsert_summaries(std::slice::from_ref(&record.record.summary))?;
        }
    }
    migrate_legacy_reported_source_assignments(store, reports)?;
    if replace {
        delete_orphaned_legacy_reported_sources(store, reports)?;
    } else {
        delete_legacy_reported_alias_summaries(store, reports)?;
    }
    println!(
        "import complete: sources={} summaries={} replaced={} summaries_written={} input={} cache_create={} cache_read={} output={} total={} cost={}",
        format_u64(reports.len() as u64),
        format_u64(total_summaries as u64),
        format_u64(replaced),
        format_u64(written),
        format_u64(total_usage.input_tokens),
        format_u64(total_usage.cache_creation_tokens),
        format_u64(total_usage.cached_input_tokens),
        format_u64(total_usage.output_tokens),
        format_u64(total_usage.total_tokens),
        format_cost(total_usage.estimated_cost_usd)
    );
    Ok(())
}

pub(crate) fn matching_reported_summary_ids(
    store: &Store,
    reports: &[ReportedImportReport],
) -> Result<Vec<statsai_core::SummaryId>> {
    let incoming_keys: BTreeSet<_> = reports
        .iter()
        .flat_map(|report| report.records.iter())
        .flat_map(reported_replace_keys)
        .collect();
    matching_reported_summary_ids_for_keys(store, &incoming_keys)
}

pub(crate) fn matching_legacy_reported_summary_ids(
    store: &Store,
    reports: &[ReportedImportReport],
) -> Result<Vec<statsai_core::SummaryId>> {
    let incoming_keys: BTreeSet<_> = reports
        .iter()
        .flat_map(|report| report.records.iter())
        .flat_map(reported_replace_keys)
        .collect();
    let summary_ids = store
        .summaries()?
        .into_iter()
        .filter(|summary| is_legacy_reported_summary_format(&summary.metadata.summary_format))
        .filter(|summary| {
            matches!(
                summary.source.source_kind,
                SourceKind::ExternalReport | SourceKind::Manual
            )
        })
        .filter(|summary| {
            reported_replace_keys_from_summary(summary)
                .iter()
                .any(|key| incoming_keys.contains(key))
        })
        .map(|summary| summary.summary_id)
        .collect();
    Ok(summary_ids)
}

pub(crate) fn matching_reported_summary_ids_for_keys(
    store: &Store,
    incoming_keys: &BTreeSet<ReportedReplaceKey>,
) -> Result<Vec<statsai_core::SummaryId>> {
    let summary_ids = store
        .summaries()?
        .into_iter()
        .filter(|summary| {
            matches!(
                summary.source.source_kind,
                SourceKind::ExternalReport | SourceKind::Manual
            )
        })
        .filter(|summary| {
            reported_replace_keys_from_summary(summary)
                .iter()
                .any(|key| incoming_keys.contains(key))
        })
        .map(|summary| summary.summary_id)
        .collect();
    Ok(summary_ids)
}

pub(crate) fn delete_legacy_reported_alias_summaries(
    store: &Store,
    reports: &[ReportedImportReport],
) -> Result<u64> {
    let summary_ids = matching_legacy_reported_summary_ids(store, reports)?;
    let deleted = store.delete_summaries(&summary_ids)?;
    delete_orphaned_legacy_reported_sources(store, reports)?;
    Ok(deleted)
}

pub(crate) fn migrate_legacy_reported_source_assignments(
    store: &Store,
    reports: &[ReportedImportReport],
) -> Result<u64> {
    let mut migrated = 0;
    let now = Utc::now();
    for record in reports.iter().flat_map(|report| report.records.iter()) {
        let canonical_source = &record.record.source;
        for legacy_source_id in &record.legacy_replacement_source_ids {
            let Some(legacy_source) = store.source(legacy_source_id)? else {
                continue;
            };
            if !is_reported_usage_source(&legacy_source) {
                continue;
            }
            for assignment in store.list_source_account_assignments_for_source(legacy_source_id)? {
                let migrated_assignment = SourceAccountAssignment {
                    schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                    assignment_id: source_account_assignment_id(
                        &canonical_source.source_id,
                        &assignment.provider_account_id,
                        assignment.started_at,
                    ),
                    source_id: canonical_source.source_id.clone(),
                    provider: canonical_source.provider.clone(),
                    provider_account_id: assignment.provider_account_id.clone(),
                    started_at: assignment.started_at,
                    ended_at: assignment.ended_at,
                    record_source: assignment.record_source.clone(),
                    verified_at: assignment.verified_at,
                    created_at: assignment.created_at,
                    updated_at: now,
                };
                let already_exists = store
                    .source_account_assignment(&migrated_assignment.assignment_id)?
                    .is_some();
                if already_exists {
                    continue;
                }
                store.upsert_source_account_assignment(&migrated_assignment)?;
                migrated += 1;
            }
        }
    }
    Ok(migrated)
}

pub(crate) fn delete_orphaned_legacy_reported_sources(
    store: &Store,
    reports: &[ReportedImportReport],
) -> Result<u64> {
    let mut deleted = 0;
    for source_id in legacy_reported_source_ids(reports) {
        let Some(source) = store.source(&source_id)? else {
            continue;
        };
        if !is_reported_usage_source(&source) {
            continue;
        }
        if !store.events_for_source(&source_id)?.is_empty()
            || !store.summaries_for_source(&source_id)?.is_empty()
        {
            continue;
        }
        if store.delete_source(&source_id)? {
            deleted += 1;
        }
    }
    Ok(deleted)
}

pub(crate) fn is_reported_usage_source(source: &SourceLocation) -> bool {
    matches!(
        source.source_kind,
        SourceKind::ExternalReport | SourceKind::Manual
    ) && source.adapter_id.as_deref() == Some(REPORTED_USAGE_IMPORT_ADAPTER_ID)
}

pub(crate) fn legacy_reported_source_ids(reports: &[ReportedImportReport]) -> Vec<SourceId> {
    let source_ids: BTreeSet<_> = reports
        .iter()
        .flat_map(|report| report.records.iter())
        .flat_map(|record| {
            record
                .legacy_replacement_source_ids
                .iter()
                .map(|source_id| source_id.0.clone())
        })
        .collect();
    source_ids.into_iter().map(SourceId).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ReportedReplaceKey {
    pub(crate) provider: String,
    pub(crate) provider_account_id: Option<String>,
    pub(crate) summary_format: String,
    pub(crate) source_id: String,
    pub(crate) period_start: Option<DateTime<Utc>>,
    pub(crate) period_end: Option<DateTime<Utc>>,
    pub(crate) source_record_id: Option<String>,
}

pub(crate) fn reported_replace_keys(record: &ReportedImportRecord) -> Vec<ReportedReplaceKey> {
    let mut keys = vec![reported_replace_key_from_summary(&record.record.summary)];
    for source_id in &record.legacy_replacement_source_ids {
        let mut key = keys[0].clone();
        key.source_id = source_id.0.clone();
        keys.push(key);
    }
    keys
}

pub(crate) fn canonical_reported_summary_format(value: &str) -> &str {
    match value {
        "ccusage_daily" | "custom_daily" => "manual_daily",
        "custom_period_summary" => "manual_period_summary",
        _ => value,
    }
}

pub(crate) fn is_legacy_reported_summary_format(value: &str) -> bool {
    canonical_reported_summary_format(value) != value
}

fn reported_replace_keys_from_summary(summary: &UsageSummary) -> [ReportedReplaceKey; 1] {
    [reported_replace_key_from_summary(summary)]
}

pub(crate) fn reported_replace_key_from_summary(summary: &UsageSummary) -> ReportedReplaceKey {
    let summary_format = canonical_reported_summary_format(&summary.metadata.summary_format);
    ReportedReplaceKey {
        provider: summary.provider.clone(),
        provider_account_id: summary.provider_account_id.as_ref().map(|id| id.0.clone()),
        summary_format: summary_format.to_string(),
        source_id: summary.source_id.0.clone(),
        period_start: summary.period_start,
        period_end: summary.period_end,
        source_record_id: stable_reported_record_id(summary),
    }
}

pub(crate) fn stable_reported_record_id(summary: &UsageSummary) -> Option<String> {
    summary
        .source
        .source_record_id
        .as_deref()
        .filter(|record_id| !record_id.starts_with("summary_key_"))
        .map(ToOwned::to_owned)
}

pub(crate) fn build_reported_import_record(
    input: ReportedUsageSummaryInput,
    device_id: &str,
) -> Result<ReportedImportRecord> {
    let legacy_replacement_source_ids = legacy_alias_replacement_source_ids(&input);
    let mut record = build_reported_usage_summary(input, device_id)?;
    // Overlay before upsert so a store already at this ruleset cannot keep an
    // imported estimated-only figure from an older catalog. A later
    // ensure_current_pricing pass would no-op.
    record.summary = apply_current_estimated_pricing(record.summary);
    Ok(ReportedImportRecord {
        record,
        legacy_replacement_source_ids,
    })
}

fn legacy_alias_replacement_source_ids(input: &ReportedUsageSummaryInput) -> Vec<SourceId> {
    if input.evidence_path.is_some() || input.evidence_id.is_some() {
        return Vec::new();
    }
    let canonical_format = canonical_reported_summary_format(&input.report_format);
    let legacy_formats: &[&str] = match canonical_format {
        "manual_daily" => &["ccusage_daily", "custom_daily"],
        "manual_period_summary" => &["custom_period_summary"],
        _ => &[],
    };
    if legacy_formats.is_empty() {
        return Vec::new();
    }

    let identity_key = reported_summary_identity_key(input);
    legacy_formats
        .iter()
        .map(|format| {
            let evidence_key = format!(
                "{}:{}:{}:{}",
                input.provider, input.source_name, identity_key, format
            );
            let source_path_hash = hash_text(&evidence_key);
            statsai_source_id(
                &input.provider,
                input.source_kind.clone(),
                &source_path_hash,
            )
        })
        .collect()
}

pub(crate) fn reported_summary_identity_key(input: &ReportedUsageSummaryInput) -> String {
    input
        .provider_account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            input
                .email
                .as_deref()
                .map(normalize_email)
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            input
                .provider_user_id
                .as_deref()
                .map(normalize_provider_user_id)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "unassigned".to_string())
}

#[derive(Debug, Clone)]
pub(crate) struct ReportedImportRecord {
    pub(crate) record: ReportedUsageSummaryRecord,
    pub(crate) legacy_replacement_source_ids: Vec<SourceId>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReportedImportReport {
    pub(crate) path: PathBuf,
    pub(crate) records: Vec<ReportedImportRecord>,
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn read_reported_summary_inputs(path: &Path) -> Result<Vec<ReportedUsageSummaryInput>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if let Ok(input) = serde_json::from_str::<ReportedUsageSummaryInput>(&text) {
        return Ok(vec![input]);
    }
    let inputs = serde_json::from_str::<Vec<ReportedUsageSummaryInput>>(&text)
        .with_context(|| format!("parse reported usage summary JSON {}", path.display()))?;
    Ok(inputs)
}
