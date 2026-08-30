use anyhow::{bail, Result};
use statsai_core::{
    SyncAck, SyncBatch, SyncEntityCounts, SyncRejectedRecord, SYNC_ACK_V1_SCHEMA_VERSION,
    SYNC_ACK_V2_SCHEMA_VERSION, SYNC_ACK_V3_SCHEMA_VERSION, SYNC_ACK_V4_SCHEMA_VERSION,
    SYNC_ACK_V5_SCHEMA_VERSION, SYNC_BATCH_V1_SCHEMA_VERSION, SYNC_BATCH_V2_SCHEMA_VERSION,
    SYNC_BATCH_V3_SCHEMA_VERSION, SYNC_BATCH_V4_SCHEMA_VERSION, SYNC_BATCH_V5_SCHEMA_VERSION,
};
use statsai_store::Store;

/// Acknowledgement schema owed to a batch schema.
///
/// Every arm is spelled out so that adding a batch version without deciding its
/// acknowledgement fails to compile, rather than silently claiming v3. Callers
/// reject unknown schemas before reaching this, so the error is unreachable
/// today; it exists to keep that true.
fn sync_ack_schema_version(batch_schema_version: &str) -> Result<&'static str> {
    match batch_schema_version {
        SYNC_BATCH_V1_SCHEMA_VERSION => Ok(SYNC_ACK_V1_SCHEMA_VERSION),
        SYNC_BATCH_V2_SCHEMA_VERSION => Ok(SYNC_ACK_V2_SCHEMA_VERSION),
        SYNC_BATCH_V3_SCHEMA_VERSION => Ok(SYNC_ACK_V3_SCHEMA_VERSION),
        SYNC_BATCH_V4_SCHEMA_VERSION => Ok(SYNC_ACK_V4_SCHEMA_VERSION),
        SYNC_BATCH_V5_SCHEMA_VERSION => Ok(SYNC_ACK_V5_SCHEMA_VERSION),
        other => bail!("unsupported sync batch schema {other}"),
    }
}

pub fn ingest_sync_batch(store: &Store, batch: &SyncBatch) -> Result<SyncAck> {
    if batch.schema_version != SYNC_BATCH_V1_SCHEMA_VERSION
        && batch.schema_version != SYNC_BATCH_V2_SCHEMA_VERSION
        && batch.schema_version != SYNC_BATCH_V3_SCHEMA_VERSION
        && batch.schema_version != SYNC_BATCH_V4_SCHEMA_VERSION
        && batch.schema_version != SYNC_BATCH_V5_SCHEMA_VERSION
    {
        bail!("unsupported sync batch schema {}", batch.schema_version);
    }
    if !matches!(
        batch.schema_version.as_str(),
        SYNC_BATCH_V3_SCHEMA_VERSION | SYNC_BATCH_V4_SCHEMA_VERSION | SYNC_BATCH_V5_SCHEMA_VERSION
    ) && !batch.code_change_metrics.is_empty()
    {
        bail!("code-change metrics require sync_batch.v3");
    }
    if !matches!(
        batch.schema_version.as_str(),
        SYNC_BATCH_V4_SCHEMA_VERSION | SYNC_BATCH_V5_SCHEMA_VERSION
    ) && !batch.quota_cycle_contributions.is_empty()
    {
        bail!("quota cycle contributions require sync_batch.v4");
    }
    if batch.schema_version != SYNC_BATCH_V5_SCHEMA_VERSION
        && (!batch.account_plan_observations.is_empty()
            || !batch.account_evidence_summaries.is_empty())
    {
        bail!("account-plan evidence requires sync_batch.v5");
    }
    if batch
        .code_change_metrics
        .iter()
        .any(|metric| metric.device_id != batch.device_id)
    {
        bail!("code-change metric device_id must match batch device_id");
    }
    if batch.authoritative_snapshot.is_some() {
        bail!("authoritative snapshots are not supported by the loopback daemon");
    }
    // A local store holds quota observations and account-plan evidence it
    // derives from its own scans; there is no table for another device's
    // contributions. Acknowledging them as accepted told the sender to record
    // them synced against this target and stop offering them, certifying a
    // write that never happened. Refusing follows the authoritative-snapshot
    // precedent above: this endpoint is for loopback diagnostics, and
    // `/api/sync/batches` is the contract that stores these collections.
    if !batch.quota_cycle_contributions.is_empty() {
        bail!("quota cycle contributions are not supported by the loopback daemon");
    }
    if !batch.account_plan_observations.is_empty() || !batch.account_evidence_summaries.is_empty() {
        bail!("account-plan evidence is not supported by the loopback daemon");
    }

    let result = store.ingest_sync_batch(batch)?;

    Ok(SyncAck {
        schema_version: sync_ack_schema_version(&batch.schema_version)?.to_string(),
        batch_id: batch.batch_id.clone(),
        accepted: SyncEntityCounts {
            sources: batch.sources.len() as u64,
            accounts: batch.accounts.len() as u64,
            source_account_assignments: batch.source_account_assignments.len() as u64,
            subscriptions: batch.subscriptions.len() as u64,
            events: result.inserted_events,
            summaries: result.written_summaries,
            task_buckets: batch.task_buckets.len() as u64,
            task_verifications: result.merged_task_verifications,
            code_change_metrics: batch.code_change_metrics.len() as u64,
            quota_cycle_contributions: batch.quota_cycle_contributions.len() as u64,
            account_plan_observations: batch.account_plan_observations.len() as u64,
            account_evidence_summaries: batch.account_evidence_summaries.len() as u64,
        },
        duplicates: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: (batch.events.len() as u64).saturating_sub(result.inserted_events),
            summaries: 0,
            task_buckets: 0,
            task_verifications: (batch.task_verifications.len() as u64)
                .saturating_sub(result.merged_task_verifications),
            code_change_metrics: 0,
            quota_cycle_contributions: 0,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        rejected: Vec::<SyncRejectedRecord>::new(),
    })
}
