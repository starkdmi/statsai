pub(crate) use super::*;

mod budget;
mod payloads;

#[test]
fn http_rollup_sync_splits_large_summary_batches() {
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-chunks"),
        LocationOrigin::Configured,
    );
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let summaries: Vec<_> = (0..(HTTP_ROLLUP_SUMMARIES_PER_BATCH * 2 + 4))
        .map(|index| {
            let mut summary = test_summary(
                "codex",
                &source,
                now + Duration::days(index as i64),
                10,
                None,
            );
            summary.summary_id = statsai_core::SummaryId(format!("summary-{index}"));
            summary
        })
        .collect();
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_large".to_string(),
        device_id: "device".to_string(),
        sources: vec![source.clone()],
        accounts: vec![],
        source_account_assignments: vec![],
        subscriptions: vec![],
        account_plan_observations: vec![],
        account_evidence_summaries: vec![],
        events: vec![],
        summaries,
        task_buckets: vec![],
        task_verifications: vec![],
        code_change_metrics: vec![],
        quota_cycle_contributions: vec![],
        authoritative_snapshot: None,
        created_at: now,
    };

    let chunks = split_http_rollup_sync_batches(&batch);

    assert_eq!(chunks.len(), 4);
    assert_eq!(chunks[0].batch_id, "batch_large_sources_1");
    assert_eq!(chunks[1].batch_id, "batch_large_part_1_of_3");
    assert_eq!(chunks[2].batch_id, "batch_large_part_2_of_3");
    assert_eq!(chunks[3].batch_id, "batch_large_part_3_of_3");
    assert!(chunks[0].summaries.is_empty());
    assert_eq!(chunks[1].summaries.len(), HTTP_ROLLUP_SUMMARIES_PER_BATCH);
    assert_eq!(chunks[2].summaries.len(), HTTP_ROLLUP_SUMMARIES_PER_BATCH);
    assert_eq!(chunks[3].summaries.len(), 4);
    assert_eq!(chunks[0].sources.len(), 1);
    assert!(chunks[1].sources.is_empty());
    assert!(chunks[2].sources.is_empty());
    assert!(chunks[3].sources.is_empty());
    assert!(chunks.iter().all(|chunk| chunk.events.is_empty()));
}

#[test]
fn http_rollup_sync_sends_authoritative_snapshot_after_data_chunks() {
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-snapshot"),
        LocationOrigin::Configured,
    );
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_snapshot".to_string(),
        device_id: "device".to_string(),
        sources: vec![source.clone()],
        accounts: Vec::new(),
        source_account_assignments: Vec::new(),
        subscriptions: Vec::new(),
        account_plan_observations: Vec::new(),
        account_evidence_summaries: Vec::new(),
        events: Vec::new(),
        summaries: Vec::new(),
        task_buckets: Vec::new(),
        task_verifications: Vec::new(),
        code_change_metrics: Vec::new(),
        quota_cycle_contributions: Vec::new(),
        authoritative_snapshot: Some(SyncAuthoritativeSnapshot {
            source_ids: vec![source.source_id.clone()],
            ..SyncAuthoritativeSnapshot::default()
        }),
        created_at: now,
    };

    let chunks = split_http_rollup_sync_batches(&batch);

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].batch_id, "batch_snapshot");
    assert_eq!(chunks[0].sources, vec![source.clone()]);
    assert!(chunks[0].authoritative_snapshot.is_none());
    assert_eq!(chunks[1].batch_id, "batch_snapshot_snapshot_1");
    assert!(chunks[1].sources.is_empty());
    let snapshot = chunks[1]
        .authoritative_snapshot
        .as_ref()
        .expect("snapshot chunk");
    assert_eq!(snapshot.snapshot_id, "batch_snapshot_authoritative");
    assert_eq!(snapshot.part_index, 0);
    assert_eq!(snapshot.part_count, 1);
    assert_eq!(snapshot.source_ids, vec![source.source_id]);
    assert_eq!(
        logical_http_rollup_batch_id(&chunks[1].batch_id),
        "batch_snapshot"
    );
}

#[test]
fn http_rollup_sync_bounds_authoritative_snapshot_chunks() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let summary_ids = (0..(HTTP_ROLLUP_SNAPSHOT_IDS_PER_BATCH * 2 + 1))
        .map(|index| statsai_core::SummaryId(format!("summary-{index}")))
        .collect::<Vec<_>>();
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_large_snapshot".to_string(),
        device_id: "device".to_string(),
        sources: Vec::new(),
        accounts: Vec::new(),
        source_account_assignments: Vec::new(),
        subscriptions: Vec::new(),
        account_plan_observations: Vec::new(),
        account_evidence_summaries: Vec::new(),
        events: Vec::new(),
        summaries: Vec::new(),
        task_buckets: Vec::new(),
        task_verifications: Vec::new(),
        code_change_metrics: Vec::new(),
        quota_cycle_contributions: Vec::new(),
        authoritative_snapshot: Some(SyncAuthoritativeSnapshot {
            summary_ids,
            ..SyncAuthoritativeSnapshot::default()
        }),
        created_at: now,
    };

    let chunks = split_http_rollup_sync_batches(&batch);
    let snapshot_chunks = chunks
        .iter()
        .filter_map(|chunk| chunk.authoritative_snapshot.as_ref())
        .collect::<Vec<_>>();

    assert_eq!(snapshot_chunks.len(), 3);
    assert!(snapshot_chunks.iter().all(|snapshot| {
        snapshot.source_ids.len()
            + snapshot.provider_account_ids.len()
            + snapshot.source_account_assignment_ids.len()
            + snapshot.subscription_ids.len()
            + snapshot.summary_ids.len()
            <= HTTP_ROLLUP_SNAPSHOT_IDS_PER_BATCH
    }));
}

#[test]
fn http_rollup_sync_splits_metadata_away_from_summaries() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let sources: Vec<_> = (0..17)
        .map(|index| {
            SourceLocation::local_adapter(
                "codex",
                format!("test-{index}"),
                "0",
                Path::new("/tmp/codex-http-metadata"),
                LocationOrigin::Configured,
            )
        })
        .collect();
    let accounts: Vec<_> = (0..7)
        .map(|index| {
            test_account(
                "codex",
                Some(&format!("account-{index}")),
                None,
                None,
                Some("Pro"),
                now,
            )
        })
        .collect();
    let assignments: Vec<_> = (0..16)
        .map(|index| {
            test_assignment(
                &sources[index],
                &accounts[index % accounts.len()].provider_account_id,
                now + Duration::days(index as i64),
                None,
                now,
            )
        })
        .collect();
    let subscriptions: Vec<_> = accounts
        .iter()
        .take(3)
        .enumerate()
        .map(|(index, account)| Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: subscription_id(
                "codex",
                &account.provider_account_id,
                &format!("pro-{index}"),
                now,
            ),
            provider: "codex".to_string(),
            provider_account_id: account.provider_account_id.clone(),
            plan_name: "Pro".to_string(),
            price: 2000,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: None,
            renewal_day: None,
            started_at: now,
            ended_at: None,
            current_period_ends_at: None,
            status: SubscriptionStatus::Active,
            record_source: IdentitySource::UserConfigured,
            verified_at: None,
            notes: None,
        })
        .collect();
    let summaries: Vec<_> = (0..HTTP_ROLLUP_SUMMARIES_PER_BATCH)
        .map(|index| {
            let mut summary = test_summary(
                "codex",
                &sources[index % sources.len()],
                now + Duration::days(index as i64),
                10,
                Some(accounts[index % accounts.len()].provider_account_id.clone()),
            );
            summary.summary_id = statsai_core::SummaryId(format!("summary-{index}"));
            summary
        })
        .collect();
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_metadata".to_string(),
        device_id: "device".to_string(),
        sources,
        accounts,
        source_account_assignments: assignments,
        subscriptions,
        account_plan_observations: vec![],
        account_evidence_summaries: vec![],
        events: vec![],
        summaries,
        task_buckets: vec![],
        task_verifications: vec![],
        code_change_metrics: vec![],
        quota_cycle_contributions: vec![],
        authoritative_snapshot: None,
        created_at: now,
    };

    let chunks = split_http_rollup_sync_batches(&batch);

    assert_eq!(chunks.len(), 5);
    assert_eq!(chunks[0].batch_id, "batch_metadata_sources_1");
    assert_eq!(chunks[1].batch_id, "batch_metadata_accounts_1");
    assert_eq!(chunks[2].batch_id, "batch_metadata_assignments_1");
    assert_eq!(chunks[3].batch_id, "batch_metadata_subscriptions_1");
    assert_eq!(chunks[4].batch_id, "batch_metadata_part_1_of_1");
    assert_eq!(chunks[0].sources.len(), 17);
    assert_eq!(chunks[1].accounts.len(), 7);
    assert_eq!(chunks[2].source_account_assignments.len(), 16);
    assert_eq!(chunks[3].subscriptions.len(), 3);
    assert_eq!(chunks[4].summaries.len(), HTTP_ROLLUP_SUMMARIES_PER_BATCH);
    assert!(chunks[..4].iter().all(|chunk| chunk.summaries.is_empty()));
    assert!(chunks[4].sources.is_empty());
    assert!(chunks[4].accounts.is_empty());
    assert!(chunks[4].source_account_assignments.is_empty());
    assert!(chunks[4].subscriptions.is_empty());
    assert!(chunks.iter().all(|chunk| chunk.events.is_empty()));
}

#[test]
fn http_rollup_metadata_budget_retries_preserve_all_metadata_kinds() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let sources: Vec<_> = (0..4)
        .map(|index| {
            SourceLocation::local_adapter(
                "codex",
                format!("retry-source-{index}"),
                "0",
                Path::new("/tmp/codex-http-metadata-retry"),
                LocationOrigin::Configured,
            )
        })
        .collect();
    let accounts: Vec<_> = (0..3)
        .map(|index| {
            test_account(
                "codex",
                Some(&format!("retry-account-{index}")),
                None,
                None,
                Some("Pro"),
                now,
            )
        })
        .collect();
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_metadata_retry".to_string(),
        device_id: "device".to_string(),
        sources: sources.clone(),
        accounts: accounts.clone(),
        source_account_assignments: vec![],
        subscriptions: vec![],
        account_plan_observations: vec![],
        account_evidence_summaries: vec![],
        events: vec![],
        summaries: vec![],
        task_buckets: vec![],
        task_verifications: vec![],
        code_change_metrics: vec![],
        quota_cycle_contributions: vec![],
        authoritative_snapshot: None,
        created_at: now,
    };

    let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);

    assert_eq!(chunks.len(), 4);
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.sources.len())
            .sum::<usize>(),
        sources.len()
    );
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.accounts.len())
            .sum::<usize>(),
        accounts.len()
    );
    assert!(chunks
        .iter()
        .all(|chunk| chunk.source_account_assignments.is_empty()));
    assert!(chunks.iter().all(|chunk| chunk.subscriptions.is_empty()));
    assert!(chunks.iter().all(|chunk| chunk.summaries.is_empty()));
    assert!(chunks.iter().all(|chunk| chunk.events.is_empty()));
    assert!(chunks.iter().any(|chunk| !chunk.sources.is_empty()));
    assert!(chunks.iter().any(|chunk| !chunk.accounts.is_empty()));
}
