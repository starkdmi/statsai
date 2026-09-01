use super::*;

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

pub(crate) fn split_http_rollup_task_chunks(
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

pub(crate) fn split_http_code_change_metric_chunks(
    batch: &SyncBatch,
    chunk_size: usize,
) -> Vec<SyncBatch> {
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

pub(crate) fn split_http_quota_cycle_contribution_chunks(
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

pub(crate) fn split_http_rollup_metadata_chunks(
    batch: &SyncBatch,
    chunk_size: usize,
) -> Vec<SyncBatch> {
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

pub(crate) fn split_http_rollup_single_metadata_kind(
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

pub(crate) fn split_http_rollup_summary_chunks(
    batch: &SyncBatch,
    chunk_size: usize,
) -> Vec<SyncBatch> {
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
