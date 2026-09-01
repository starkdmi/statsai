use super::*;

fn test_quota_cycle_contributions(
    now: DateTime<Utc>,
    count: usize,
) -> Vec<statsai_core::QuotaCycleContributionV1> {
    (0..count)
        .map(|index| {
            let reset = now + chrono::Duration::days(7 * index as i64);
            statsai_core::QuotaCycleContributionV1 {
                schema_version: statsai_core::QUOTA_CYCLE_CONTRIBUTION_SCHEMA_VERSION.to_string(),
                contribution_id: format!("quota_cycle_{index:032}"),
                provider: "codex".to_string(),
                provider_account_id: ProviderAccountId("acct".to_string()),
                limit_id: Some("weekly".to_string()),
                window_minutes: 10_080,
                representative_reset: reset,
                representative_reset_epoch_seconds: reset.timestamp(),
                has_schedule_overlap: false,
                daily_envelopes: Vec::new(),
                boundary_slices: Vec::new(),
            }
        })
        .collect()
}

fn test_quota_only_sync_batch(now: DateTime<Utc>, count: usize) -> SyncBatch {
    SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_quota_only".to_string(),
        device_id: "device".to_string(),
        sources: vec![],
        accounts: vec![],
        source_account_assignments: vec![],
        subscriptions: vec![],
        account_plan_observations: vec![],
        account_evidence_summaries: vec![],
        events: vec![],
        summaries: vec![],
        task_buckets: vec![],
        task_verifications: vec![],
        code_change_metrics: vec![],
        quota_cycle_contributions: test_quota_cycle_contributions(now, count),
        authoritative_snapshot: None,
        created_at: now,
    }
}

#[test]
fn a_quota_only_batch_splits_into_strictly_smaller_chunks() {
    // Quota cycles carry nothing else, so the split has to make progress on
    // the quota collection itself. Counting them as metadata made
    // `has_non_quota_cycle_payload` true for this batch, so the splitter
    // peeled the quota off "the rest" and handed back the identical chunk
    // beside an empty one — which `should_retry_http_rollup_chunk_after_error`
    // then retried and split the same way, forever.
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = test_quota_only_sync_batch(now, 4);

    let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);

    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.quota_cycle_contributions.len()
                < batch.quota_cycle_contributions.len())
    );
    assert!(chunks
        .iter()
        .all(|chunk| !chunk.quota_cycle_contributions.is_empty()));
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.quota_cycle_contributions.len())
            .sum::<usize>(),
        batch.quota_cycle_contributions.len()
    );
}

#[test]
fn splitting_sends_each_quota_cycle_exactly_once() {
    // Enough cycles to cross the metadata-per-batch limit once they were
    // wrongly counted as metadata. Past that point the metadata splitter
    // and the dedicated quota splitter both ran over the same batch, so
    // every contribution went out twice.
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let mut batch = test_quota_only_sync_batch(now, HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH + 10);
    batch.sources = vec![SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-quota-once"),
        LocationOrigin::Configured,
    )];

    let sent = split_http_rollup_sync_batches(&batch)
        .iter()
        .flat_map(|chunk| chunk.quota_cycle_contributions.clone())
        .map(|contribution| contribution.contribution_id)
        .collect::<Vec<_>>();

    let unique = sent.iter().collect::<BTreeSet<_>>();
    assert_eq!(sent.len(), unique.len(), "sent: {sent:?}");
    assert_eq!(unique.len(), batch.quota_cycle_contributions.len());
}

#[test]
fn http_rollup_retry_splits_mixed_task_payloads() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = test_task_only_sync_batch(now, 1, 1);

    assert!(should_retry_http_rollup_chunk_after_error(
        &batch,
        &anyhow::anyhow!(r#"sync endpoint returned HTTP 413: {{"error":"sync_batch_too_large"}}"#),
    ));

    let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);

    assert_eq!(chunks.len(), 2);
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.task_buckets.len())
            .sum::<usize>(),
        1
    );
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.task_verifications.len())
            .sum::<usize>(),
        1
    );
    assert!(chunks
        .iter()
        .all(|chunk| { chunk.task_buckets.is_empty() || chunk.task_verifications.is_empty() }));
}

#[test]
fn http_rollup_retry_preserves_metrics_when_splitting_mixed_payloads() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let mut batch = test_task_only_sync_batch(now, 1, 1);
    batch.code_change_metrics = vec![test_code_change_metric(0, now)];

    assert!(should_retry_http_rollup_chunk_after_error(
        &batch,
        &anyhow::anyhow!(r#"sync endpoint returned HTTP 413: {{"error":"sync_batch_too_large"}}"#),
    ));

    let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.code_change_metrics.len())
            .sum::<usize>(),
        1
    );
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.task_buckets.len())
            .sum::<usize>(),
        1
    );
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.task_verifications.len())
            .sum::<usize>(),
        1
    );
}

#[test]
fn http_rollup_retry_splits_code_change_only_payloads() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let mut batch = test_task_only_sync_batch(now, 0, 0);
    batch.code_change_metrics = (0..3)
        .map(|index| test_code_change_metric(index, now))
        .collect();

    assert!(should_retry_http_rollup_chunk_after_error(
        &batch,
        &anyhow::anyhow!(
            r#"sync endpoint returned HTTP 413: {{"error":"sync_batch_d1_query_budget_exceeded"}}"#
        ),
    ));

    let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);
    assert_eq!(chunks.len(), 2);
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.code_change_metrics.len())
            .sum::<usize>(),
        3
    );
}

#[test]
fn http_rollup_retry_halves_task_only_bucket_chunks() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = test_task_only_sync_batch(now, 3, 0);

    assert!(should_retry_http_rollup_chunk_after_error(
        &batch,
        &anyhow::anyhow!(
            r#"sync endpoint returned HTTP 413: {{"error":"sync_batch_d1_query_budget_exceeded"}}"#
        ),
    ));

    let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].task_buckets.len(), 2);
    assert_eq!(chunks[1].task_buckets.len(), 1);
    assert!(chunks
        .iter()
        .all(|chunk| chunk.task_verifications.is_empty()));
}

#[test]
fn http_rollup_sends_metadata_before_task_chunks() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-metadata-before-task"),
        LocationOrigin::Configured,
    );
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_metadata_before_task".to_string(),
        device_id: "device".to_string(),
        sources: vec![source],
        accounts: vec![],
        source_account_assignments: vec![],
        subscriptions: vec![],
        account_plan_observations: vec![],
        account_evidence_summaries: vec![],
        events: vec![],
        summaries: vec![],
        task_buckets: test_task_only_sync_batch(now, 1, 0).task_buckets,
        task_verifications: vec![],
        code_change_metrics: vec![],
        quota_cycle_contributions: vec![],
        authoritative_snapshot: None,
        created_at: now,
    };

    let chunks = split_http_rollup_sync_batches(&batch);

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].sources.len(), 1);
    assert!(chunks[0].task_buckets.is_empty());
    assert!(chunks[1].sources.is_empty());
    assert_eq!(chunks[1].task_buckets.len(), 1);
}
