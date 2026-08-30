use chrono::Datelike;
use serde_json::{json, Value};
use statsai_core::{
    project_has_stable_identity, SyncAuthoritativeSnapshot, SyncBatch, TaskBucketSnapshot,
    TaskSpan, UsageSummary, WorkItem, SYNC_BATCH_SCHEMA_VERSION,
};
use std::collections::{BTreeSet, HashMap};

pub(crate) const HTTP_ROLLUP_SUMMARIES_PER_BATCH: usize = 25;

pub(crate) const HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH: usize = 20;

const HTTP_ROLLUP_CODE_CHANGE_METRICS_PER_BATCH: usize = 1_000;

const HTTP_ROLLUP_QUOTA_CYCLE_CONTRIBUTIONS_PER_BATCH: usize = 100;

pub(crate) const HTTP_ROLLUP_D1_QUERY_BUDGET: usize = 45;

const HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE: usize = 90;

const HTTP_ROLLUP_DAILY_ROLLUP_ROWS_PER_QUERY: usize = 7;

pub(crate) const HTTP_ROLLUP_SNAPSHOT_IDS_PER_BATCH: usize = 200;

const TASK_SYNC_SQL_MAX_ROWS_PER_CHUNK: usize = 200;

const TASK_SYNC_SQL_MAX_JSON_BYTES_PER_CHUNK: usize = 512 * 1024;

pub(crate) fn split_http_rollup_sync_batches(batch: &SyncBatch) -> Vec<SyncBatch> {
    let mut data_batch = batch.clone();
    let authoritative_snapshot = data_batch.authoritative_snapshot.take();
    let mut chunks = split_http_rollup_sync_batches_without_snapshot(&data_batch);
    if let Some(authoritative_snapshot) = authoritative_snapshot {
        for snapshot in split_authoritative_snapshot(
            authoritative_snapshot,
            &data_batch.batch_id,
            HTTP_ROLLUP_SNAPSHOT_IDS_PER_BATCH,
        ) {
            let mut snapshot_chunk = empty_http_rollup_chunk(
                &data_batch,
                &format!("snapshot_{}", snapshot.part_index + 1),
            );
            snapshot_chunk.authoritative_snapshot = Some(snapshot);
            chunks.push(snapshot_chunk);
        }
    }
    chunks
}

fn split_authoritative_snapshot(
    snapshot: SyncAuthoritativeSnapshot,
    batch_id: &str,
    max_ids: usize,
) -> Vec<SyncAuthoritativeSnapshot> {
    debug_assert!(max_ids > 0);
    let snapshot_id = if snapshot.snapshot_id.trim().is_empty() {
        format!("{batch_id}_authoritative")
    } else {
        snapshot.snapshot_id
    };
    let empty_part = || SyncAuthoritativeSnapshot {
        snapshot_id: snapshot_id.clone(),
        part_index: 0,
        part_count: 1,
        source_ids: Vec::new(),
        provider_account_ids: Vec::new(),
        source_account_assignment_ids: Vec::new(),
        subscription_ids: Vec::new(),
        account_plan_observation_ids: Vec::new(),
        account_evidence_summary_ids: Vec::new(),
        summary_ids: Vec::new(),
        code_change_metric_ids: Vec::new(),
        quota_cycle_contribution_ids: Vec::new(),
    };
    let mut parts = Vec::new();
    let mut current = empty_part();

    macro_rules! append_ids {
        ($ids:expr, $field:ident) => {
            for id in $ids {
                if authoritative_snapshot_id_count(&current) == max_ids {
                    parts.push(std::mem::replace(&mut current, empty_part()));
                }
                current.$field.push(id);
            }
        };
    }

    append_ids!(snapshot.source_ids, source_ids);
    append_ids!(snapshot.provider_account_ids, provider_account_ids);
    append_ids!(
        snapshot.source_account_assignment_ids,
        source_account_assignment_ids
    );
    append_ids!(snapshot.subscription_ids, subscription_ids);
    append_ids!(
        snapshot.account_plan_observation_ids,
        account_plan_observation_ids
    );
    append_ids!(
        snapshot.account_evidence_summary_ids,
        account_evidence_summary_ids
    );
    append_ids!(snapshot.summary_ids, summary_ids);
    append_ids!(snapshot.code_change_metric_ids, code_change_metric_ids);
    append_ids!(
        snapshot.quota_cycle_contribution_ids,
        quota_cycle_contribution_ids
    );
    if authoritative_snapshot_id_count(&current) > 0 || parts.is_empty() {
        parts.push(current);
    }
    let part_count = u32::try_from(parts.len()).expect("snapshot part count fits u32");
    for (index, part) in parts.iter_mut().enumerate() {
        part.part_index = u32::try_from(index).expect("snapshot part index fits u32");
        part.part_count = part_count;
    }
    parts
}

fn authoritative_snapshot_id_count(snapshot: &SyncAuthoritativeSnapshot) -> usize {
    snapshot.source_ids.len()
        + snapshot.provider_account_ids.len()
        + snapshot.source_account_assignment_ids.len()
        + snapshot.subscription_ids.len()
        + snapshot.account_plan_observation_ids.len()
        + snapshot.account_evidence_summary_ids.len()
        + snapshot.summary_ids.len()
        + snapshot.code_change_metric_ids.len()
        + snapshot.quota_cycle_contribution_ids.len()
}

pub(crate) fn split_http_rollup_sync_batches_without_snapshot(batch: &SyncBatch) -> Vec<SyncBatch> {
    let task_chunks = split_http_rollup_task_chunks(
        batch,
        HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH,
        HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH,
    );
    let has_task_payload = !task_chunks.is_empty();
    let metadata_count = http_rollup_metadata_count(batch);
    let has_rollup_payload = metadata_count > 0
        || !batch.summaries.is_empty()
        || !batch.code_change_metrics.is_empty()
        || !batch.quota_cycle_contributions.is_empty();
    if !has_task_payload
        && batch.summaries.len() <= HTTP_ROLLUP_SUMMARIES_PER_BATCH
        && batch.code_change_metrics.len() <= HTTP_ROLLUP_CODE_CHANGE_METRICS_PER_BATCH
        && batch.quota_cycle_contributions.len() <= HTTP_ROLLUP_QUOTA_CYCLE_CONTRIBUTIONS_PER_BATCH
        && metadata_count <= HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH
    {
        return fit_http_rollup_batches_to_d1_budget(vec![batch.clone()]);
    }
    if has_task_payload && !has_rollup_payload {
        return fit_http_rollup_batches_to_d1_budget(task_chunks);
    }

    let total_chunks = batch
        .summaries
        .len()
        .div_ceil(HTTP_ROLLUP_SUMMARIES_PER_BATCH);
    let metadata_chunks = metadata_count.div_ceil(HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH);
    let code_change_chunks = batch
        .code_change_metrics
        .len()
        .div_ceil(HTTP_ROLLUP_CODE_CHANGE_METRICS_PER_BATCH);
    let quota_cycle_chunks = batch
        .quota_cycle_contributions
        .len()
        .div_ceil(HTTP_ROLLUP_QUOTA_CYCLE_CONTRIBUTIONS_PER_BATCH);
    let mut chunks = Vec::with_capacity(
        total_chunks
            + metadata_chunks
            + code_change_chunks
            + quota_cycle_chunks
            + task_chunks.len(),
    );

    chunks.extend(split_http_rollup_metadata_chunks(
        batch,
        HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH,
    ));
    chunks.extend(task_chunks);
    chunks.extend(split_http_code_change_metric_chunks(
        batch,
        HTTP_ROLLUP_CODE_CHANGE_METRICS_PER_BATCH,
    ));
    chunks.extend(split_http_quota_cycle_contribution_chunks(
        batch,
        HTTP_ROLLUP_QUOTA_CYCLE_CONTRIBUTIONS_PER_BATCH,
    ));
    chunks.extend(split_http_rollup_summary_chunks(
        batch,
        HTTP_ROLLUP_SUMMARIES_PER_BATCH,
    ));

    fit_http_rollup_batches_to_d1_budget(chunks)
}

pub(crate) fn split_http_rollup_sync_batch_after_budget_error(batch: &SyncBatch) -> Vec<SyncBatch> {
    if !batch.quota_cycle_contributions.is_empty() && has_non_quota_cycle_payload(batch) {
        let mut without_quota = batch.clone();
        without_quota.quota_cycle_contributions.clear();
        let mut chunks = split_http_quota_cycle_contribution_chunks(
            batch,
            batch.quota_cycle_contributions.len(),
        );
        chunks.extend(split_http_rollup_sync_batch_after_budget_error(
            &without_quota,
        ));
        return chunks;
    }
    if batch.quota_cycle_contributions.len() > 1 {
        return split_http_quota_cycle_contribution_chunks(
            batch,
            batch.quota_cycle_contributions.len().div_ceil(2),
        );
    }
    if !batch.code_change_metrics.is_empty() && has_non_code_change_payload(batch) {
        let mut without_code_changes = batch.clone();
        without_code_changes.code_change_metrics.clear();
        let mut chunks =
            split_http_code_change_metric_chunks(batch, batch.code_change_metrics.len());
        chunks.extend(split_http_rollup_sync_batch_after_budget_error(
            &without_code_changes,
        ));
        return chunks;
    }
    if batch.code_change_metrics.len() > 1 {
        return split_http_code_change_metric_chunks(
            batch,
            batch.code_change_metrics.len().div_ceil(2),
        );
    }

    if !batch.task_buckets.is_empty() || !batch.task_verifications.is_empty() {
        if !batch.task_buckets.is_empty() && !batch.task_verifications.is_empty() {
            return split_http_rollup_task_chunks(
                batch,
                batch.task_buckets.len(),
                batch.task_verifications.len(),
            );
        }
        if batch.task_buckets.len() > 1 {
            return split_http_rollup_task_chunks(
                batch,
                batch.task_buckets.len().div_ceil(2),
                batch.task_verifications.len().max(1),
            );
        }
        if batch.task_verifications.len() > 1 {
            return split_http_rollup_task_chunks(
                batch,
                batch.task_buckets.len().max(1),
                batch.task_verifications.len().div_ceil(2),
            );
        }
    }

    if http_rollup_metadata_count(batch) > 0 && !batch.summaries.is_empty() {
        let mut chunks =
            split_http_rollup_metadata_chunks(batch, HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH);
        chunks.extend(split_http_rollup_summary_chunks(
            batch,
            batch.summaries.len(),
        ));
        return chunks;
    }

    if batch.summaries.len() > 1 {
        return split_http_rollup_summary_chunks(batch, batch.summaries.len().div_ceil(2));
    }

    if batch.sources.len() > 1 {
        return split_http_rollup_metadata_chunks(batch, batch.sources.len().div_ceil(2));
    }
    if batch.accounts.len() > 1 {
        return split_http_rollup_metadata_chunks(batch, batch.accounts.len().div_ceil(2));
    }
    if batch.source_account_assignments.len() > 1 {
        return split_http_rollup_metadata_chunks(
            batch,
            batch.source_account_assignments.len().div_ceil(2),
        );
    }
    if batch.subscriptions.len() > 1 {
        return split_http_rollup_metadata_chunks(batch, batch.subscriptions.len().div_ceil(2));
    }
    if batch.account_plan_observations.len() > 1 {
        return split_http_rollup_metadata_chunks(
            batch,
            batch.account_plan_observations.len().div_ceil(2),
        );
    }
    if batch.account_evidence_summaries.len() > 1 {
        return split_http_rollup_metadata_chunks(
            batch,
            batch.account_evidence_summaries.len().div_ceil(2),
        );
    }
    vec![batch.clone()]
}

pub(crate) fn has_non_code_change_payload(batch: &SyncBatch) -> bool {
    http_rollup_metadata_count(batch) > 0
        || !batch.events.is_empty()
        || !batch.summaries.is_empty()
        || !batch.task_buckets.is_empty()
        || !batch.task_verifications.is_empty()
        || !batch.quota_cycle_contributions.is_empty()
}

pub(crate) fn has_non_quota_cycle_payload(batch: &SyncBatch) -> bool {
    http_rollup_metadata_count(batch) > 0
        || !batch.events.is_empty()
        || !batch.summaries.is_empty()
        || !batch.task_buckets.is_empty()
        || !batch.task_verifications.is_empty()
        || !batch.code_change_metrics.is_empty()
}

/// Records the backend writes one statement per row for.
///
/// Quota cycles are deliberately absent. They travel on their own splitting
/// path and the backend upserts the whole collection in a single statement, so
/// counting them here both charged them twice against the budget and made
/// `has_non_quota_cycle_payload` true for a batch that holds nothing else —
/// which returned the same quota chunk from every split and retried it forever.
pub(crate) fn http_rollup_metadata_count(batch: &SyncBatch) -> usize {
    batch.sources.len()
        + batch.accounts.len()
        + batch.source_account_assignments.len()
        + batch.subscriptions.len()
        + batch.account_plan_observations.len()
        + batch.account_evidence_summaries.len()
}

fn split_http_rollup_task_chunks(
    batch: &SyncBatch,
    task_bucket_chunk_size: usize,
    task_verification_chunk_size: usize,
) -> Vec<SyncBatch> {
    let mut chunks = Vec::new();
    let task_bucket_chunk_size = task_bucket_chunk_size.max(1);
    let task_verification_chunk_size = task_verification_chunk_size.max(1);

    chunks.extend(
        batch
            .task_buckets
            .chunks(task_bucket_chunk_size)
            .enumerate()
            .map(|(index, buckets)| {
                let mut chunk =
                    empty_http_rollup_chunk(batch, &format!("task_buckets_{}", index + 1));
                chunk.task_buckets = buckets.to_vec();
                chunk
            }),
    );
    chunks.extend(
        batch
            .task_verifications
            .chunks(task_verification_chunk_size)
            .enumerate()
            .map(|(index, verifications)| {
                let mut chunk =
                    empty_http_rollup_chunk(batch, &format!("task_verifications_{}", index + 1));
                chunk.task_verifications = verifications.to_vec();
                chunk
            }),
    );
    chunks
}

fn split_http_code_change_metric_chunks(batch: &SyncBatch, chunk_size: usize) -> Vec<SyncBatch> {
    batch
        .code_change_metrics
        .chunks(chunk_size.max(1))
        .enumerate()
        .map(|(index, metrics)| {
            let mut chunk = empty_http_rollup_chunk(batch, &format!("code_changes_{}", index + 1));
            chunk.code_change_metrics = metrics.to_vec();
            chunk
        })
        .collect()
}

fn split_http_quota_cycle_contribution_chunks(
    batch: &SyncBatch,
    chunk_size: usize,
) -> Vec<SyncBatch> {
    batch
        .quota_cycle_contributions
        .chunks(chunk_size.max(1))
        .enumerate()
        .map(|(index, contributions)| {
            let mut chunk = empty_http_rollup_chunk(batch, &format!("quota_cycles_{}", index + 1));
            chunk.quota_cycle_contributions = contributions.to_vec();
            chunk
        })
        .collect()
}

fn fit_http_rollup_batches_to_d1_budget(chunks: Vec<SyncBatch>) -> Vec<SyncBatch> {
    let mut fitted = Vec::new();
    for chunk in chunks {
        fitted.extend(fit_http_rollup_batch_to_d1_budget(&chunk));
    }
    fitted
}

fn fit_http_rollup_batch_to_d1_budget(batch: &SyncBatch) -> Vec<SyncBatch> {
    if estimate_http_rollup_d1_queries(batch) <= HTTP_ROLLUP_D1_QUERY_BUDGET {
        return vec![batch.clone()];
    }

    let smaller_chunks = split_http_rollup_sync_batch_after_budget_error(batch);
    if smaller_chunks.len() <= 1 {
        return vec![batch.clone()];
    }

    fit_http_rollup_batches_to_d1_budget(smaller_chunks)
}

pub(crate) fn estimate_http_rollup_d1_queries(batch: &SyncBatch) -> usize {
    let authenticated_device_queries = 2;
    let existing_batch_lookup_queries = 1;
    let final_sync_bookkeeping_queries = 2;
    let account_alias_lookup_queries = usize::from(!batch.accounts.is_empty());
    // The backend reads existing ownership for every non-empty plan batch and
    // may spend one more query deleting a fingerprint the device repointed.
    // The client cannot know whether that cleanup will be needed until the
    // server reads its ownership state, so reserve the worst case here.
    let account_plan_ownership_queries =
        usize::from(!batch.account_plan_observations.is_empty()) * 2;
    let semantic_lookup_queries =
        http_rollup_query_chunks(
            unique_non_empty_provider_account_ids(
                batch
                    .source_account_assignments
                    .iter()
                    .map(|assignment| assignment.provider_account_id.0.as_str()),
            )
            .len(),
            HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
        ) + http_rollup_query_chunks(
            unique_non_empty_provider_account_ids(
                batch
                    .subscriptions
                    .iter()
                    .map(|subscription| subscription.provider_account_id.0.as_str()),
            )
            .len(),
            HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
        ) + http_rollup_query_chunks(
            unique_non_empty_provider_account_ids(batch.summaries.iter().filter_map(|summary| {
                summary.provider_account_id.as_ref().map(|id| id.0.as_str())
            }))
            .len(),
            HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
        ) + http_rollup_query_chunks(
            unique_non_empty_provider_account_ids(
                batch
                    .account_plan_observations
                    .iter()
                    .map(|observation| observation.provider_account_id.0.as_str())
                    .chain(
                        batch
                            .account_evidence_summaries
                            .iter()
                            .map(|summary| summary.provider_account_id.0.as_str()),
                    ),
            )
            .len(),
            HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
        );
    let existing_summary_state_queries =
        http_rollup_query_chunks(batch.summaries.len(), HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE);
    let project_location_lookup_queries = http_rollup_query_chunks(
        http_rollup_project_location_count(batch),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let metadata_write_queries = batch.sources.len()
        + batch.accounts.len()
        + batch.source_account_assignments.len()
        + batch.subscriptions.len()
        + batch.account_plan_observations.len()
        + batch.account_evidence_summaries.len();
    let project_write_queries =
        http_rollup_project_count(batch) + http_rollup_project_location_count(batch);
    let daily_rollup_write_queries = http_rollup_query_chunks(
        batch.summaries.len(),
        HTTP_ROLLUP_DAILY_ROLLUP_ROWS_PER_QUERY,
    );
    let monthly_rollup_queries = http_rollup_summary_month_count(batch);
    let dashboard_snapshot_queries = usize::from(!batch.summaries.is_empty());
    // The backend batches metrics into one json_each upsert and one ownership
    // statement regardless of metric count.
    let code_change_metric_queries = usize::from(!batch.code_change_metrics.is_empty()) * 2;
    let quota_cycle_contribution_queries =
        usize::from(!batch.quota_cycle_contributions.is_empty()) * 2;
    let code_change_owner_metadata_refresh_queries = usize::from(
        batch.schema_version == SYNC_BATCH_SCHEMA_VERSION
            && batch
                .authoritative_snapshot
                .as_ref()
                .is_some_and(|snapshot| {
                    snapshot.part_index.saturating_add(1) == snapshot.part_count
                }),
    );

    authenticated_device_queries
        + existing_batch_lookup_queries
        + account_alias_lookup_queries
        + account_plan_ownership_queries
        + semantic_lookup_queries
        + existing_summary_state_queries
        + project_location_lookup_queries
        + metadata_write_queries
        + project_write_queries
        + daily_rollup_write_queries
        + monthly_rollup_queries
        + dashboard_snapshot_queries
        + code_change_metric_queries
        + quota_cycle_contribution_queries
        + code_change_owner_metadata_refresh_queries
        + estimate_http_rollup_task_queries(batch)
        + final_sync_bookkeeping_queries
}

fn estimate_http_rollup_task_queries(batch: &SyncBatch) -> usize {
    let source_lookup_queries = http_rollup_query_chunks(
        batch
            .task_buckets
            .iter()
            .flat_map(|bucket| bucket.spans.iter())
            .filter_map(|span| {
                let source_id = span.source_id.0.trim();
                (!source_id.is_empty()).then_some(source_id)
            })
            .collect::<BTreeSet<_>>()
            .len(),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let project_lookup_queries = http_rollup_query_chunks(
        batch
            .task_buckets
            .iter()
            .flat_map(|bucket| bucket.spans.iter())
            .filter_map(|span| {
                let project = span.project.as_ref()?;
                Some(
                    [
                        Some(project.project_id.as_str()),
                        project.path_hash.as_deref(),
                        project.repo_remote_hash.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
                    .join(":"),
                )
            })
            .filter(|descriptor| !descriptor.is_empty())
            .collect::<BTreeSet<_>>()
            .len(),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let verification_span_lookup_queries = http_rollup_query_chunks(
        batch
            .task_verifications
            .iter()
            .flat_map(|verification| verification.action.span_ids())
            .filter_map(|span_id| {
                let value = span_id.0.trim();
                (!value.is_empty()).then_some(value)
            })
            .collect::<BTreeSet<_>>()
            .len(),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let write_queries = batch
        .task_buckets
        .iter()
        .map(estimate_task_bucket_write_queries)
        .sum::<usize>()
        + batch.task_verifications.len();

    source_lookup_queries
        + project_lookup_queries
        + verification_span_lookup_queries
        + write_queries
}

fn estimate_task_bucket_write_queries(bucket: &TaskBucketSnapshot) -> usize {
    let member_work_item_ids_by_span_id = bucket
        .members
        .iter()
        .map(|member| (member.span_id.0.as_str(), member.work_item_id.0.as_str()))
        .collect::<HashMap<_, _>>();
    let spans_by_id = bucket
        .spans
        .iter()
        .map(|span| (span.span_id.0.as_str(), span))
        .collect::<HashMap<_, _>>();
    4 + count_task_sync_json_chunks(&bucket.spans, |span| {
        estimate_task_sync_span_insert_row_json(
            span,
            member_work_item_ids_by_span_id
                .get(span.span_id.0.as_str())
                .copied(),
        )
    }) + count_task_sync_json_chunks(&bucket.work_items, |work_item| {
        estimate_task_sync_work_item_insert_row_json(
            work_item,
            spans_by_id
                .get(work_item.anchor_span_id.0.as_str())
                .copied(),
        )
    }) + count_task_sync_json_chunks(&bucket.members, estimate_task_sync_member_insert_row_json)
}

fn count_task_sync_json_chunks<T>(rows: &[T], serialize_row: impl Fn(&T) -> String) -> usize {
    if rows.is_empty() {
        return 0;
    }
    let mut chunk_count = 0usize;
    let mut current_row_count = 0usize;
    let mut current_bytes = 2usize;
    for row in rows {
        let serialized = serialize_row(row);
        let row_bytes = serialized.len();
        let next_bytes = if current_row_count == 0 {
            2 + row_bytes
        } else {
            current_bytes + 1 + row_bytes
        };
        if current_row_count > 0
            && (current_row_count >= TASK_SYNC_SQL_MAX_ROWS_PER_CHUNK
                || next_bytes > TASK_SYNC_SQL_MAX_JSON_BYTES_PER_CHUNK)
        {
            chunk_count += 1;
            current_row_count = 0;
            current_bytes = 2;
        }
        current_row_count += 1;
        current_bytes = if current_row_count == 1 {
            2 + row_bytes
        } else {
            current_bytes + 1 + row_bytes
        };
    }
    chunk_count + usize::from(current_row_count > 0)
}

fn estimate_task_sync_project_fields(span: &TaskSpan) -> (Option<&str>, Option<String>) {
    let Some(project) = span.project.as_ref() else {
        return (None, None);
    };
    let project_location_id = [
        project.path_hash.as_deref(),
        project.repo_remote_hash.as_deref(),
        Some(project.project_id.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>()
    .join(":");
    (
        Some(project.project_id.as_str()),
        (!project_location_id.is_empty()).then_some(project_location_id),
    )
}

fn estimate_task_sync_span_insert_row_json(span: &TaskSpan, work_item_id: Option<&str>) -> String {
    let (project_id, project_location_id) = estimate_task_sync_project_fields(span);
    json!({
        "span_id": span.span_id.0,
        "work_item_id": work_item_id,
        "provider": span.provider,
        "provider_account_id": Value::Null,
        "project_id": project_id,
        "project_location_id": project_location_id,
        "started_at": span.started_at.to_rfc3339(),
        "ended_at": span.ended_at.map(|timestamp| timestamp.to_rfc3339()),
        "payload_json": serde_json::to_string(span).unwrap_or_default(),
    })
    .to_string()
}

fn estimate_task_sync_work_item_insert_row_json(
    work_item: &WorkItem,
    anchor_span: Option<&TaskSpan>,
) -> String {
    let (project_id, project_location_id) = anchor_span
        .map(estimate_task_sync_project_fields)
        .unwrap_or((None, None));
    json!({
        "work_item_id": work_item.work_item_id.0,
        "anchor_span_id": work_item.anchor_span_id.0,
        "status": work_item.status,
        "confidence": work_item.confidence,
        "started_at": work_item.started_at.to_rfc3339(),
        "ended_at": work_item.ended_at.to_rfc3339(),
        "project_id": project_id,
        "project_location_id": project_location_id,
        "payload_json": serde_json::to_string(work_item).unwrap_or_default(),
    })
    .to_string()
}

fn estimate_task_sync_member_insert_row_json(member: &statsai_core::WorkItemMember) -> String {
    json!({
        "work_item_id": member.work_item_id.0,
        "span_id": member.span_id.0,
        "ordinal": member.ordinal,
    })
    .to_string()
}

fn http_rollup_query_chunks(item_count: usize, chunk_size: usize) -> usize {
    if item_count == 0 {
        0
    } else {
        item_count.div_ceil(chunk_size.max(1))
    }
}

fn unique_non_empty_provider_account_ids<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<&'a str> {
    values
        .into_iter()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect()
}

fn http_rollup_summary_month_count(batch: &SyncBatch) -> usize {
    batch
        .summaries
        .iter()
        .map(http_rollup_summary_month_key)
        .collect::<BTreeSet<_>>()
        .len()
}

fn http_rollup_summary_month_key(summary: &UsageSummary) -> String {
    let anchor = summary
        .period_start
        .as_ref()
        .or(summary.period_end.as_ref())
        .unwrap_or(&summary.observed_at);
    format!("{:04}-{:02}", anchor.year(), anchor.month())
}

pub(crate) fn http_rollup_project_count(batch: &SyncBatch) -> usize {
    batch
        .summaries
        .iter()
        .filter_map(http_rollup_summary_project_key)
        .collect::<BTreeSet<_>>()
        .len()
}

fn http_rollup_summary_project_key(summary: &UsageSummary) -> Option<String> {
    let project = summary.project.as_ref()?;
    if !project_has_stable_identity(project) {
        return None;
    }
    if let Some(repo_remote_hash) = project.repo_remote_hash.as_deref() {
        return Some(format!("repo:{repo_remote_hash}"));
    }
    if let Some(path_hash) = project.path_hash.as_deref() {
        return Some(format!("path:{path_hash}"));
    }
    Some(format!("project:{}", project.project_id))
}

pub(crate) fn http_rollup_project_location_count(batch: &SyncBatch) -> usize {
    batch
        .summaries
        .iter()
        .filter_map(http_rollup_summary_project_location_key)
        .collect::<BTreeSet<_>>()
        .len()
}

fn http_rollup_summary_project_location_key(summary: &UsageSummary) -> Option<String> {
    let project = summary.project.as_ref()?;
    if !project_has_stable_identity(project) {
        return None;
    }
    if let Some(path_hash) = project.path_hash.as_deref() {
        return Some(format!("path:{path_hash}"));
    }
    if let Some(repo_remote_hash) = project.repo_remote_hash.as_deref() {
        return Some(format!("repo:{repo_remote_hash}:{}", project.project_id));
    }
    Some(format!("project:{}", project.project_id))
}

fn split_http_rollup_metadata_chunks(batch: &SyncBatch, chunk_size: usize) -> Vec<SyncBatch> {
    let mut chunks = Vec::new();
    chunks.extend(split_http_rollup_single_metadata_kind(
        batch, "sources", chunk_size,
    ));
    chunks.extend(split_http_rollup_single_metadata_kind(
        batch, "accounts", chunk_size,
    ));
    chunks.extend(split_http_rollup_single_metadata_kind(
        batch,
        "assignments",
        chunk_size,
    ));
    chunks.extend(split_http_rollup_single_metadata_kind(
        batch,
        "subscriptions",
        chunk_size,
    ));
    chunks.extend(split_http_rollup_single_metadata_kind(
        batch,
        "account_plans",
        chunk_size,
    ));
    chunks.extend(split_http_rollup_single_metadata_kind(
        batch,
        "account_evidence",
        chunk_size,
    ));
    chunks
}

fn split_http_rollup_single_metadata_kind(
    batch: &SyncBatch,
    kind: &str,
    chunk_size: usize,
) -> Vec<SyncBatch> {
    let chunk_size = chunk_size.max(1);
    match kind {
        "sources" => batch
            .sources
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, records)| {
                let mut chunk = empty_http_rollup_chunk(batch, &format!("sources_{}", index + 1));
                chunk.sources = records.to_vec();
                chunk
            })
            .collect(),
        "accounts" => batch
            .accounts
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, records)| {
                let mut chunk = empty_http_rollup_chunk(batch, &format!("accounts_{}", index + 1));
                chunk.accounts = records.to_vec();
                chunk
            })
            .collect(),
        "assignments" => batch
            .source_account_assignments
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, records)| {
                let mut chunk =
                    empty_http_rollup_chunk(batch, &format!("assignments_{}", index + 1));
                chunk.source_account_assignments = records.to_vec();
                chunk
            })
            .collect(),
        "subscriptions" => batch
            .subscriptions
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, records)| {
                let mut chunk =
                    empty_http_rollup_chunk(batch, &format!("subscriptions_{}", index + 1));
                chunk.subscriptions = records.to_vec();
                chunk
            })
            .collect(),
        "account_plans" => batch
            .account_plan_observations
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, records)| {
                let mut chunk =
                    empty_http_rollup_chunk(batch, &format!("account_plans_{}", index + 1));
                chunk.account_plan_observations = records.to_vec();
                chunk
            })
            .collect(),
        "account_evidence" => batch
            .account_evidence_summaries
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, records)| {
                let mut chunk =
                    empty_http_rollup_chunk(batch, &format!("account_evidence_{}", index + 1));
                chunk.account_evidence_summaries = records.to_vec();
                chunk
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn split_http_rollup_summary_chunks(batch: &SyncBatch, chunk_size: usize) -> Vec<SyncBatch> {
    let chunk_size = chunk_size.max(1);
    let total_chunks = batch.summaries.len().div_ceil(chunk_size);
    batch
        .summaries
        .chunks(chunk_size)
        .enumerate()
        .map(|(index, summaries)| {
            let mut chunk = batch.clone();
            chunk.batch_id = format!("{}_part_{}_of_{}", batch.batch_id, index + 1, total_chunks);
            chunk.sources.clear();
            chunk.accounts.clear();
            chunk.source_account_assignments.clear();
            chunk.subscriptions.clear();
            chunk.account_plan_observations.clear();
            chunk.account_evidence_summaries.clear();
            chunk.events.clear();
            chunk.summaries = summaries.to_vec();
            chunk.task_buckets.clear();
            chunk.task_verifications.clear();
            chunk.code_change_metrics.clear();
            chunk.quota_cycle_contributions.clear();
            chunk.authoritative_snapshot = None;
            chunk
        })
        .collect()
}

fn empty_http_rollup_chunk(batch: &SyncBatch, suffix: &str) -> SyncBatch {
    let mut chunk = batch.clone();
    chunk.batch_id = format!("{}_{}", batch.batch_id, suffix);
    chunk.sources.clear();
    chunk.accounts.clear();
    chunk.source_account_assignments.clear();
    chunk.subscriptions.clear();
    chunk.account_plan_observations.clear();
    chunk.account_evidence_summaries.clear();
    chunk.events.clear();
    chunk.summaries.clear();
    chunk.task_buckets.clear();
    chunk.task_verifications.clear();
    chunk.code_change_metrics.clear();
    chunk.quota_cycle_contributions.clear();
    chunk.authoritative_snapshot = None;
    chunk
}
