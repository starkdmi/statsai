use anyhow::{Context, Result};
use statsai_adapters::{
    adapter_for_provider, default_adapters, retain_accounts_referenced_by_account_evidence,
    AccountEvidenceScan, ProviderAdapter, ScanCandidateFile, ScanDiagnostics, ScanOptions,
    VerifiedSourceObservation,
};
use statsai_core::{
    hash_text, EventId, QuotaObservationRecordV1, SourceId, SourceKind, SourceLocation,
    SourceVerificationMode, TaskSpan, TaskVerification, UsageEvent, UsageTotals,
};
use statsai_store::{
    derive_task_work_items, reconcile_verified_source_state, verified_source_observation_hash,
    ScanFileStateEntry, Store, TaskRebuildReport,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Instant;

use super::args::ScanCommand;
use super::format::{abbreviate_home, format_cost, format_u64};

use super::source::{
    apply_source_account_resolution, canonicalize_account_evidence,
    canonicalize_known_account_evidence, dedupe_overlapping_sources, location_origin_label,
    path_label_from_hashless_source, persist_source_after_preview, preview_path_label,
    provider_matches, source_verification_mode, sources_refer_to_same_location,
};

pub(crate) fn scan(command: ScanCommand, store: &Store, device_id: &str) -> Result<()> {
    let adapters: Vec<Box<dyn ProviderAdapter>> =
        if let Some(provider) = command.provider.as_deref() {
            vec![adapter_for_provider(provider)
                .with_context(|| format!("unsupported provider {provider}"))?]
        } else {
            default_adapters()
        };

    scan_with_adapters(command, store, device_id, adapters)
}

pub(crate) fn scan_with_adapters(
    command: ScanCommand,
    store: &Store,
    device_id: &str,
    adapters: Vec<Box<dyn ProviderAdapter>>,
) -> Result<()> {
    let scan_started_at = Instant::now();
    let mut preview_task_rebuild = PreviewTaskRebuild::default();
    let mut preview_work_item_rebuild_count = 0u64;
    let mut event_count = 0u64;
    let mut summary_count = 0u64;
    let mut task_span_count = 0u64;
    let mut quota_observation_count = 0u64;
    let mut inserted_count = 0u64;
    let mut summary_written_count = 0u64;
    let mut task_span_written_count = 0u64;
    let mut removed_event_count = 0u64;
    let mut removed_summary_count = 0u64;
    let mut removed_task_span_count = 0u64;
    let mut rebuilt_work_item_count = 0u64;
    let mut total_sources = 0u64;
    let mut total_log_rows = 0u64;
    let mut total_diagnostics = ScanDiagnostics::default();
    let mut total_usage = UsageTotals::default();
    let mut total_summary_usage = UsageTotals::default();
    let mut adapter_scan_duration_ms = 0u64;
    let mut preview_rebuild_duration_ms = 0u64;
    let mut delete_duration_ms = 0u64;
    let mut insert_events_duration_ms = 0u64;
    let mut upsert_summaries_duration_ms = 0u64;
    let mut upsert_task_spans_duration_ms = 0u64;
    let mut rebuild_work_items_duration_ms = 0u64;
    let mut rebuild_work_item_report = TaskRebuildReport::default();

    let configured_sources = store.list_sources()?;

    for adapter in adapters {
        let sources = scan_sources_for_adapter(adapter.as_ref(), &configured_sources);

        for mut source in sources {
            if source.path_label.is_none() {
                source.path_label = path_label_from_hashless_source(&source);
            }
            let verification_mode = source_verification_mode(&source);
            let account_evidence_enabled =
                matches!(verification_mode, SourceVerificationMode::Auto);
            let mut account_evidence = AccountEvidenceScan::default();
            if account_evidence_enabled {
                let account_evidence_checkpoints =
                    store.account_evidence_checkpoints(&source.source_id)?;
                account_evidence =
                    adapter.collect_account_evidence(&source, &account_evidence_checkpoints)?;
                let known_account_aliases = canonicalize_known_account_evidence(
                    store,
                    adapter.provider(),
                    &mut account_evidence,
                )?;
                store.retain_unseen_account_evidence(
                    &source.source_id,
                    &mut account_evidence.identity_observations,
                    &mut account_evidence.plan_observations,
                    &mut account_evidence.conversation_bindings,
                )?;
                retain_accounts_referenced_by_account_evidence(
                    adapter.provider(),
                    &known_account_aliases,
                    &mut account_evidence,
                );
            }
            let account_evidence_count = account_evidence.identity_observations.len()
                + account_evidence.plan_observations.len()
                + account_evidence.conversation_bindings.len();
            let account_evidence_checkpoint_count = account_evidence.checkpoints.len();
            let cache_candidates = adapter.scan_candidates(&source)?;
            let compatible_scan_signatures =
                scan_candidate_compatible_signatures(&cache_candidates);
            let file_cache_entries = scan_file_state_entries(&cache_candidates);
            let file_reconciliation = select_scan_file_reconciliation(
                store,
                &source.source_id,
                &file_cache_entries,
                &compatible_scan_signatures,
                command.replace,
                command.no_cache,
                command.include_tasks,
            )?;
            let pending_file_entries = file_reconciliation.pending_entries;
            let compatible_entries_to_upgrade = file_reconciliation.compatible_entries_to_upgrade;
            let removed_file_entries = file_reconciliation.removed_entries;
            let touched_files =
                !pending_file_entries.is_empty() || !removed_file_entries.is_empty();
            let has_cache_entry_upgrades = !compatible_entries_to_upgrade.is_empty();
            let scan_all_current_files = !file_cache_entries.is_empty()
                && pending_file_entries.len() == file_cache_entries.len();
            let needs_legacy_full_reconcile = !command.replace
                && !command.no_cache
                && touched_files
                && !scan_all_current_files
                && store.source_records_missing_scan_file_hashes(&source.source_id)?;
            let replace_source_records = should_replace_source_records_for_scan(
                command.replace,
                command.no_cache,
                file_cache_entries.len(),
                pending_file_entries.len(),
                needs_legacy_full_reconcile,
            );
            let replace_all_source_quota_records = should_replace_all_source_quota_records(
                command.replace,
                needs_legacy_full_reconcile,
            );
            let should_run_adapter_scan = if replace_source_records {
                !file_cache_entries.is_empty()
            } else {
                !pending_file_entries.is_empty()
            };
            let options = ScanOptions {
                device_id: device_id.to_string(),
                collect_tasks: command.include_tasks,
                selected_cache_keys: (should_run_adapter_scan
                    && !replace_source_records
                    && !command.no_cache)
                    .then(|| {
                        pending_file_entries
                            .iter()
                            .map(|entry| entry.cache_key.clone())
                            .collect()
                    }),
            };
            let probed_verified_source_state =
                if matches!(verification_mode, SourceVerificationMode::Disabled) {
                    VerifiedSourceObservation::Unavailable
                } else {
                    adapter.probe_verified_source_state(&source)?
                };
            let mut scan = if should_run_adapter_scan {
                let started_at = Instant::now();
                let scan = adapter.scan(&source, &options)?;
                adapter_scan_duration_ms += started_at.elapsed().as_millis() as u64;
                scan
            } else {
                statsai_adapters::AdapterScan {
                    diagnostics: ScanDiagnostics {
                        files_skipped_unchanged: (file_cache_entries
                            .len()
                            .saturating_sub(pending_file_entries.len()))
                            as u64,
                        ..ScanDiagnostics::default()
                    },
                    ..statsai_adapters::AdapterScan::default()
                }
            };
            if !command.include_tasks {
                scan.task_spans.clear();
            }
            let effective_verified_source_state =
                if matches!(verification_mode, SourceVerificationMode::Disabled) {
                    VerifiedSourceObservation::Unavailable
                } else if should_run_adapter_scan {
                    scan.verified_source_state
                        .take()
                        .map(Box::new)
                        .map(VerifiedSourceObservation::Verified)
                        .unwrap_or(probed_verified_source_state)
                } else {
                    probed_verified_source_state
                };
            // `Unavailable` means the local snapshot yielded no conclusive observation,
            // so preserve the last state. `AttributionBlocked` is an explicit signal
            // that the cached account must no longer own this source.
            let next_verified_state_hash =
                if matches!(verification_mode, SourceVerificationMode::Auto) {
                    match &effective_verified_source_state {
                        VerifiedSourceObservation::Unavailable => {
                            source.verified_state_hash.clone()
                        }
                        observation => verified_source_observation_hash(observation)?,
                    }
                } else {
                    None
                };
            let verified_state_changed = matches!(verification_mode, SourceVerificationMode::Auto)
                && source.verified_state_hash != next_verified_state_hash;
            let log_rows = scan.diagnostics.raw_rows;
            let mut source_usage = UsageTotals::default();
            for event in &scan.events {
                source_usage.add_event(event);
            }
            let mut source_summary_usage = UsageTotals::default();
            for summary in &scan.summaries {
                source_summary_usage.add_summary(summary);
            }
            let source_event_count = scan.events.len() as u64;
            let source_summary_count = scan.summaries.len() as u64;
            let source_task_span_count = scan.task_spans.len() as u64;
            let source_quota_observation_count = scan.quota_observations.len() as u64;
            let has_scan_activity = touched_files
                || (has_cache_entry_upgrades && !command.preview)
                || source_event_count > 0
                || source_summary_count > 0
                || source_task_span_count > 0
                || source_quota_observation_count > 0
                || scan.diagnostics.files_scanned > 0
                || scan.diagnostics.files_skipped_unchanged > 0
                || log_rows > 0
                || account_evidence_count > 0
                || account_evidence_checkpoint_count > 0
                || verified_state_changed;
            let suppress_source_processing = !command.verbose
                && !command.explain
                && source_event_count == 0
                && source_summary_count == 0
                && source_task_span_count == 0
                && source_quota_observation_count == 0
                && !touched_files
                && !has_cache_entry_upgrades
                && account_evidence_count == 0
                && account_evidence_checkpoint_count == 0
                && !verified_state_changed;

            if !has_scan_activity {
                continue;
            }

            total_sources += 1;
            total_log_rows += log_rows;
            event_count += source_event_count;
            summary_count += source_summary_count;
            task_span_count += source_task_span_count;
            quota_observation_count += source_quota_observation_count;
            total_usage.add_totals(&source_usage);
            total_summary_usage.add_totals(&source_summary_usage);
            add_diagnostics(&mut total_diagnostics, &scan.diagnostics);

            if suppress_source_processing {
                continue;
            }

            if command.preview {
                if command.include_tasks {
                    let rebuild_started_at = Instant::now();
                    preview_work_item_rebuild_count += preview_task_rebuild.apply_source_changes(
                        store,
                        SourceTaskChangeSet {
                            source_id: &source.source_id,
                            replace_source_records,
                            touched_files,
                            pending_file_entries: &pending_file_entries,
                            removed_file_entries: &removed_file_entries,
                            task_spans: &scan.task_spans,
                        },
                    )?;
                    preview_rebuild_duration_ms += rebuild_started_at.elapsed().as_millis() as u64;
                }
                print_scan_preview_line(ScanPreviewLine {
                    source: &source,
                    usage_events: source_event_count,
                    usage: &source_usage,
                    summaries: source_summary_count,
                    task_spans: source_task_span_count,
                    quota_observations: source_quota_observation_count,
                    summary_usage: &source_summary_usage,
                    diagnostics: &scan.diagnostics,
                    verbose: command.verbose || command.explain,
                });
                continue;
            }
            let source_rebuild_report = store.apply_scan_update(|store| {
                reconcile_verified_source_state(
                    store,
                    &mut source,
                    &effective_verified_source_state,
                    next_verified_state_hash,
                )?;
                persist_source_after_preview(store, &source)?;
                if account_evidence_enabled {
                    canonicalize_account_evidence(
                        store,
                        adapter.provider(),
                        &mut account_evidence,
                    )?;
                    store.upsert_account_identity_observations(
                        &account_evidence.identity_observations,
                    )?;
                    store.upsert_account_plan_observations(&account_evidence.plan_observations)?;
                    store.reconcile_source_account_evidence_assignments(&source.source_id)?;
                    store.upsert_conversation_account_bindings(
                        &account_evidence.conversation_bindings,
                    )?;
                    store.reattribute_conversation_bound_events(&source.source_id)?;
                }
                apply_source_account_resolution(
                    store,
                    &source,
                    &mut scan.events,
                    &mut scan.summaries,
                )?;
                if account_evidence_enabled {
                    store
                        .apply_conversation_account_bindings(&source.source_id, &mut scan.events)?;
                }
                let mut affected_project_buckets = if command.include_tasks {
                    scan.task_spans
                        .iter()
                        .map(|span| span.project_bucket.clone())
                        .collect::<BTreeSet<_>>()
                } else {
                    BTreeSet::new()
                };
                let mut deleted_task_spans = Vec::new();
                if replace_source_records {
                    let delete_started_at = Instant::now();
                    removed_event_count +=
                        store.delete_events_for_sources(std::slice::from_ref(&source.source_id))?;
                    removed_summary_count += store
                        .delete_summaries_for_sources(std::slice::from_ref(&source.source_id))?;
                    if replace_all_source_quota_records {
                        store.delete_quota_observations_for_sources(std::slice::from_ref(
                            &source.source_id,
                        ))?;
                    }
                    if command.include_tasks {
                        let deleted = store.delete_task_spans_for_sources(std::slice::from_ref(
                            &source.source_id,
                        ))?;
                        removed_task_span_count += deleted.deleted;
                        affected_project_buckets
                            .extend(deleted.affected_project_buckets.iter().cloned());
                        deleted_task_spans.extend(deleted.deleted_spans);
                    }
                    delete_duration_ms += delete_started_at.elapsed().as_millis() as u64;
                } else if touched_files {
                    let delete_started_at = Instant::now();
                    let reconciled_file_hashes = scan_file_hashes_for_reconciliation(
                        &pending_file_entries,
                        &removed_file_entries,
                    );
                    removed_event_count += store.delete_events_for_source_file_hashes(
                        &source.source_id,
                        &reconciled_file_hashes,
                    )?;
                    removed_summary_count += store.delete_summaries_for_source_file_hashes(
                        &source.source_id,
                        &reconciled_file_hashes,
                    )?;
                    store.delete_quota_observations_for_source_file_hashes(
                        &source.source_id,
                        &reconciled_file_hashes,
                    )?;
                    if command.include_tasks {
                        let deleted = store.delete_task_spans_for_source_file_hashes(
                            &source.source_id,
                            &reconciled_file_hashes,
                        )?;
                        removed_task_span_count += deleted.deleted;
                        affected_project_buckets
                            .extend(deleted.affected_project_buckets.iter().cloned());
                        deleted_task_spans.extend(deleted.deleted_spans);
                    }
                    delete_duration_ms += delete_started_at.elapsed().as_millis() as u64;
                }
                let insert_started_at = Instant::now();
                let insert_result = store.insert_events_with_resolution(&scan.events)?;
                inserted_count += insert_result.inserted;
                insert_events_duration_ms += insert_started_at.elapsed().as_millis() as u64;
                rewrite_quota_usage_event_ids(
                    &mut scan.quota_observations,
                    &insert_result.canonical_event_ids,
                );
                if replace_source_records && !replace_all_source_quota_records {
                    let reconciled_file_hashes = scan_file_hashes_for_reconciliation(
                        &file_cache_entries,
                        &removed_file_entries,
                    );
                    store.replace_quota_observations_for_source_files(
                        &source.source_id,
                        &reconciled_file_hashes,
                        &scan.quota_observations,
                    )?;
                    // Every file was rescanned, so anything still carrying a hash outside that set
                    // belongs to a file that is gone, or predates file hashes entirely. Nothing
                    // later in this scan revisits it, so it is retired here rather than surviving
                    // as a row no source file explains.
                    store.delete_quota_observations_for_source_outside_file_hashes(
                        &source.source_id,
                        &reconciled_file_hashes,
                    )?;
                } else {
                    store.upsert_quota_observations(&scan.quota_observations)?;
                }
                store.rebuild_quota_plan_observations_for_source(&source.source_id)?;
                store.clear_orphaned_quota_usage_links()?;
                if command.include_tasks {
                    rewrite_task_span_linked_event_ids(
                        &mut scan.task_spans,
                        &insert_result.canonical_event_ids,
                    );
                    populate_task_span_rollups(
                        &mut scan.task_spans,
                        &scan.events,
                        &insert_result.canonical_event_ids,
                    );
                }
                let upsert_summaries_started_at = Instant::now();
                summary_written_count += store.upsert_summaries(&scan.summaries)?;
                upsert_summaries_duration_ms +=
                    upsert_summaries_started_at.elapsed().as_millis() as u64;

                let mut rebuild_project_buckets = BTreeSet::new();
                let mut rebuild_span_ids = BTreeSet::new();
                if command.include_tasks {
                    let upsert_task_spans_started_at = Instant::now();
                    task_span_written_count += store.upsert_task_spans(&scan.task_spans)?;
                    upsert_task_spans_duration_ms +=
                        upsert_task_spans_started_at.elapsed().as_millis() as u64;
                    rebuild_project_buckets.extend(
                        scan.task_spans
                            .iter()
                            .map(|span| span.project_bucket.clone()),
                    );
                    rebuild_span_ids
                        .extend(scan.task_spans.iter().map(|span| span.span_id.0.clone()));
                    rebuild_project_buckets.extend(affected_project_buckets);
                }

                let cache_entries_to_record = if replace_source_records || command.no_cache {
                    &file_cache_entries
                } else {
                    &pending_file_entries
                };
                store.record_scan_file_entries_with_tasks_collected(
                    &source.source_id,
                    cache_entries_to_record,
                    command.include_tasks,
                )?;
                store
                    .upgrade_scan_file_entries(&source.source_id, &compatible_entries_to_upgrade)?;
                let removed_cache_keys = scan_file_cache_keys(&removed_file_entries);
                store.delete_scan_file_entries(&source.source_id, &removed_cache_keys)?;
                if account_evidence_enabled {
                    store.upsert_account_evidence_checkpoints(&account_evidence.checkpoints)?;
                }

                if command.include_tasks
                    && !rebuild_project_buckets.is_empty()
                    && (!rebuild_span_ids.is_empty() || !deleted_task_spans.is_empty())
                {
                    let rebuild_started_at = Instant::now();
                    let report = store.rebuild_task_work_items_for_changes_report(
                        &rebuild_project_buckets,
                        &rebuild_span_ids,
                        &deleted_task_spans,
                    )?;
                    rebuild_work_items_duration_ms +=
                        rebuild_started_at.elapsed().as_millis() as u64;
                    Ok(report)
                } else {
                    Ok(TaskRebuildReport::default())
                }
            })?;
            rebuilt_work_item_count += source_rebuild_report.work_items_rebuilt;
            add_task_rebuild_report(&mut rebuild_work_item_report, &source_rebuild_report);
        }
    }

    if command.preview {
        if command.verbose {
            println!(
                "preview total: sources={} usage_events={} summaries={} quota_observations={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} summary_est_cost={} log_rows={} written=0",
                format_u64(total_sources),
                format_u64(event_count),
                format_u64(summary_count),
                format_u64(quota_observation_count),
                format_u64(total_usage.input_tokens),
                format_u64(total_usage.cache_creation_tokens),
                format_u64(total_usage.cached_input_tokens),
                format_u64(total_usage.output_tokens),
                format_u64(total_usage.total_tokens),
                format_cost(total_usage.estimated_cost_usd),
                format_u64(total_summary_usage.total_tokens),
                format_cost(total_summary_usage.estimated_cost_usd),
                format_u64(total_log_rows)
            );
            println!(
                "preview tasks: spans={} work_items_rebuilt={}",
                format_u64(task_span_count),
                format_u64(preview_work_item_rebuild_count)
            );
            println!(
                "timings_ms: adapter_scan={} preview_rebuild={} total_wall={}",
                format_u64(adapter_scan_duration_ms),
                format_u64(preview_rebuild_duration_ms),
                format_u64(scan_started_at.elapsed().as_millis() as u64)
            );
            print_scan_diagnostics_total(&total_diagnostics);
        } else {
            println!(
                "preview total: sources={} usage_events={} summaries={} quota_observations={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} summary_est_cost={} written=0",
                format_u64(total_sources),
                format_u64(event_count),
                format_u64(summary_count),
                format_u64(quota_observation_count),
                format_u64(total_usage.input_tokens),
                format_u64(total_usage.cache_creation_tokens),
                format_u64(total_usage.cached_input_tokens),
                format_u64(total_usage.output_tokens),
                format_u64(total_usage.total_tokens),
                format_cost(total_usage.estimated_cost_usd),
                format_u64(total_summary_usage.total_tokens),
                format_cost(total_summary_usage.estimated_cost_usd)
            );
            println!(
                "preview tasks: spans={} work_items_rebuilt={}",
                format_u64(task_span_count),
                format_u64(preview_work_item_rebuild_count)
            );
        }
    } else {
        println!(
            "scan complete: sources={} usage_events={} inserted={} summaries={} summaries_written={} task_spans={} task_spans_written={} quota_observations={} work_items_rebuilt={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} summary_est_cost={} log_rows={}",
            format_u64(total_sources),
            format_u64(event_count),
            format_u64(inserted_count),
            format_u64(summary_count),
            format_u64(summary_written_count),
            format_u64(task_span_count),
            format_u64(task_span_written_count),
            format_u64(quota_observation_count),
            format_u64(rebuilt_work_item_count),
            format_u64(total_usage.input_tokens),
            format_u64(total_usage.cache_creation_tokens),
            format_u64(total_usage.cached_input_tokens),
            format_u64(total_usage.output_tokens),
            format_u64(total_usage.total_tokens),
            format_cost(total_usage.estimated_cost_usd),
            format_u64(total_summary_usage.total_tokens),
            format_cost(total_summary_usage.estimated_cost_usd),
            format_u64(total_log_rows)
        );
        if command.replace
            || removed_event_count > 0
            || removed_summary_count > 0
            || removed_task_span_count > 0
        {
            println!(
                "scan removed stale records: events={} summaries={} task_spans={}",
                format_u64(removed_event_count),
                format_u64(removed_summary_count),
                format_u64(removed_task_span_count)
            );
        }
        if command.verbose {
            println!(
                "timings_ms: adapter_scan={} delete={} insert_events={} upsert_summaries={} upsert_task_spans={} rebuild_work_items={} rebuild_delete={} rebuild_span_load={} rebuild_verifications={} rebuild_grouping={} rebuild_title_selection={} rebuild_insert={} total_wall={}",
                format_u64(adapter_scan_duration_ms),
                format_u64(delete_duration_ms),
                format_u64(insert_events_duration_ms),
                format_u64(upsert_summaries_duration_ms),
                format_u64(upsert_task_spans_duration_ms),
                format_u64(rebuild_work_items_duration_ms),
                format_u64(rebuild_work_item_report.timings.delete_ms),
                format_u64(rebuild_work_item_report.timings.span_load_ms),
                format_u64(rebuild_work_item_report.timings.verification_load_ms),
                format_u64(rebuild_work_item_report.timings.grouping_ms),
                format_u64(rebuild_work_item_report.timings.title_selection_ms),
                format_u64(rebuild_work_item_report.timings.insert_ms),
                format_u64(scan_started_at.elapsed().as_millis() as u64)
            );
        }
        print_scan_diagnostics_total(&total_diagnostics);
    }
    Ok(())
}

pub(crate) fn add_task_rebuild_report(total: &mut TaskRebuildReport, report: &TaskRebuildReport) {
    total.work_items_rebuilt = total
        .work_items_rebuilt
        .saturating_add(report.work_items_rebuilt);
    total.work_items_deleted = total
        .work_items_deleted
        .saturating_add(report.work_items_deleted);
    total.affected_bucket_count = total
        .affected_bucket_count
        .saturating_add(report.affected_bucket_count);
    total.affected_segment_count = total
        .affected_segment_count
        .saturating_add(report.affected_segment_count);
    total.touched_span_count = total
        .touched_span_count
        .saturating_add(report.touched_span_count);
    total.timings.delete_ms = total
        .timings
        .delete_ms
        .saturating_add(report.timings.delete_ms);
    total.timings.span_load_ms = total
        .timings
        .span_load_ms
        .saturating_add(report.timings.span_load_ms);
    total.timings.verification_load_ms = total
        .timings
        .verification_load_ms
        .saturating_add(report.timings.verification_load_ms);
    total.timings.grouping_ms = total
        .timings
        .grouping_ms
        .saturating_add(report.timings.grouping_ms);
    total.timings.title_selection_ms = total
        .timings
        .title_selection_ms
        .saturating_add(report.timings.title_selection_ms);
    total.timings.insert_ms = total
        .timings
        .insert_ms
        .saturating_add(report.timings.insert_ms);
}

#[derive(Debug, Default)]
pub(crate) struct PreviewTaskRebuild {
    pub(crate) projected_spans: Option<HashMap<String, TaskSpan>>,
    pub(crate) verifications: Option<Vec<TaskVerification>>,
}

pub(crate) struct SourceTaskChangeSet<'a> {
    pub(crate) source_id: &'a SourceId,
    pub(crate) replace_source_records: bool,
    pub(crate) touched_files: bool,
    pub(crate) pending_file_entries: &'a [ScanFileStateEntry],
    pub(crate) removed_file_entries: &'a [ScanFileStateEntry],
    pub(crate) task_spans: &'a [TaskSpan],
}

impl PreviewTaskRebuild {
    pub(crate) fn apply_source_changes(
        &mut self,
        store: &Store,
        changes: SourceTaskChangeSet<'_>,
    ) -> Result<u64> {
        if self.projected_spans.is_none() {
            self.projected_spans = Some(
                store
                    .task_spans()?
                    .into_iter()
                    .map(|span| (span.span_id.0.clone(), span))
                    .collect(),
            );
        }
        if self.verifications.is_none() {
            self.verifications = Some(store.task_verifications()?);
        }

        let projected_spans = self
            .projected_spans
            .as_mut()
            .expect("projected spans initialized");
        let verifications = self
            .verifications
            .as_ref()
            .expect("task verifications initialized");
        let mut affected_project_buckets = BTreeSet::new();
        if changes.replace_source_records {
            let removed_span_ids = projected_spans
                .iter()
                .filter(|(_, span)| span.source_id == *changes.source_id)
                .map(|(span_id, span)| {
                    affected_project_buckets.insert(span.project_bucket.clone());
                    span_id.clone()
                })
                .collect::<Vec<_>>();
            for span_id in removed_span_ids {
                projected_spans.remove(span_id.as_str());
            }
        } else if changes.touched_files {
            let reconciled_hashes = scan_file_hashes_for_reconciliation(
                changes.pending_file_entries,
                changes.removed_file_entries,
            )
            .into_iter()
            .collect::<HashSet<_>>();
            if !reconciled_hashes.is_empty() {
                let removed_span_ids = projected_spans
                    .iter()
                    .filter(|(_, span)| span.source_id == *changes.source_id)
                    .filter(|(_, span)| {
                        span.source_file_path_hash
                            .as_deref()
                            .is_some_and(|hash| reconciled_hashes.contains(hash))
                    })
                    .map(|(span_id, span)| {
                        affected_project_buckets.insert(span.project_bucket.clone());
                        span_id.clone()
                    })
                    .collect::<Vec<_>>();
                for span_id in removed_span_ids {
                    projected_spans.remove(span_id.as_str());
                }
            }
        }
        for span in changes.task_spans {
            if let Some(previous) = projected_spans.insert(span.span_id.0.clone(), span.clone()) {
                affected_project_buckets.insert(previous.project_bucket);
            }
            affected_project_buckets.insert(span.project_bucket.clone());
        }
        if affected_project_buckets.is_empty() {
            return Ok(0);
        }

        let preview_spans = projected_spans
            .values()
            .filter(|span| affected_project_buckets.contains(&span.project_bucket))
            .cloned()
            .collect::<Vec<_>>();
        let (work_items, _) = derive_task_work_items(preview_spans, verifications);
        Ok(work_items.len() as u64)
    }
}

pub(crate) struct ScanPreviewLine<'a> {
    pub(crate) source: &'a SourceLocation,
    pub(crate) usage_events: u64,
    pub(crate) usage: &'a UsageTotals,
    pub(crate) summaries: u64,
    pub(crate) task_spans: u64,
    pub(crate) quota_observations: u64,
    pub(crate) summary_usage: &'a UsageTotals,
    pub(crate) diagnostics: &'a ScanDiagnostics,
    pub(crate) verbose: bool,
}

pub(crate) fn print_scan_preview_line(line: ScanPreviewLine<'_>) {
    if line.verbose {
        println!(
            "{} path={} usage_events={} summaries={} task_spans={} quota_observations={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} summary_est_cost={} raw_rows={} candidates={} duplicates={} skipped_zero={} invalid={} files={} cached={} timestamp_fallbacks={} model_fallbacks={} origin={} source={}",
            line.source.provider,
            preview_path_label(line.source),
            line.usage_events,
            line.summaries,
            line.task_spans,
            line.quota_observations,
            format_u64(line.usage.input_tokens),
            format_u64(line.usage.cache_creation_tokens),
            format_u64(line.usage.cached_input_tokens),
            format_u64(line.usage.output_tokens),
            format_u64(line.usage.total_tokens),
            format_cost(line.usage.estimated_cost_usd),
            format_u64(line.summary_usage.total_tokens),
            format_cost(line.summary_usage.estimated_cost_usd),
            format_u64(line.diagnostics.raw_rows),
            format_u64(line.diagnostics.candidate_usage_rows),
            format_u64(line.diagnostics.duplicate_events),
            format_u64(line.diagnostics.skipped_zero_events),
            format_u64(line.diagnostics.invalid_rows),
            format_u64(line.diagnostics.files_scanned),
            format_u64(line.diagnostics.files_skipped_unchanged),
            format_u64(line.diagnostics.timestamp_fallbacks),
            format_u64(line.diagnostics.model_fallbacks),
            location_origin_label(&line.source.location_origin),
            line.source.source_id.0
        );
    } else {
        println!(
            "{} path={} usage_events={} summaries={} task_spans={} quota_observations={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} summary_est_cost={}",
            line.source.provider,
            preview_path_label(line.source),
            line.usage_events,
            line.summaries,
            line.task_spans,
            line.quota_observations,
            format_u64(line.usage.input_tokens),
            format_u64(line.usage.cache_creation_tokens),
            format_u64(line.usage.cached_input_tokens),
            format_u64(line.usage.output_tokens),
            format_u64(line.usage.total_tokens),
            format_cost(line.usage.estimated_cost_usd),
            format_u64(line.summary_usage.total_tokens),
            format_cost(line.summary_usage.estimated_cost_usd)
        );
    }
}

pub(crate) fn add_diagnostics(target: &mut ScanDiagnostics, source: &ScanDiagnostics) {
    target.files_scanned += source.files_scanned;
    target.files_skipped_unchanged += source.files_skipped_unchanged;
    target.raw_rows += source.raw_rows;
    target.candidate_usage_rows += source.candidate_usage_rows;
    target.accepted_events += source.accepted_events;
    target.duplicate_events += source.duplicate_events;
    target.skipped_zero_events += source.skipped_zero_events;
    target.invalid_rows += source.invalid_rows;
    target.timestamp_fallbacks += source.timestamp_fallbacks;
    target.model_fallbacks += source.model_fallbacks;
}

pub(crate) fn print_scan_diagnostics_total(diagnostics: &ScanDiagnostics) {
    println!(
        "diagnostics: files={} cached={} raw_rows={} candidates={} duplicates={} skipped_zero={} invalid={} timestamp_fallbacks={} model_fallbacks={}",
        format_u64(diagnostics.files_scanned),
        format_u64(diagnostics.files_skipped_unchanged),
        format_u64(diagnostics.raw_rows),
        format_u64(diagnostics.candidate_usage_rows),
        format_u64(diagnostics.duplicate_events),
        format_u64(diagnostics.skipped_zero_events),
        format_u64(diagnostics.invalid_rows),
        format_u64(diagnostics.timestamp_fallbacks),
        format_u64(diagnostics.model_fallbacks)
    );
}

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
