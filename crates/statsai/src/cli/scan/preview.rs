use super::*;

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
