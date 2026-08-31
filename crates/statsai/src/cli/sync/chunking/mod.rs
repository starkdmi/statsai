use chrono::Datelike;
use serde_json::{json, Value};
use statsai_core::{
    project_has_stable_identity, SyncAuthoritativeSnapshot, SyncBatch, TaskBucketSnapshot,
    TaskSpan, UsageSummary, WorkItem, SYNC_BATCH_SCHEMA_VERSION,
};
use std::collections::{BTreeSet, HashMap};

mod budget;
mod payloads;

pub(crate) use budget::*;
pub(crate) use payloads::*;

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
