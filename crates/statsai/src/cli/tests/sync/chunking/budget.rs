use super::*;

#[test]
fn http_rollup_sync_proactively_splits_batches_to_fit_d1_budget() {
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollup-budget"),
        LocationOrigin::Configured,
    );
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let summaries: Vec<_> = (0..HTTP_ROLLUP_SUMMARIES_PER_BATCH)
        .map(|index| {
            let mut summary = test_summary(
                "codex",
                &source,
                now + Duration::days((index * 31) as i64),
                10,
                None,
            );
            summary.summary_id = statsai_core::SummaryId(format!("summary-budget-{index}"));
            summary.project = Some(ProjectInfo {
                project_id: format!("project-budget-{index}"),
                project_label: Some(format!("Project {index}")),
                repo_remote_hash: Some(format!("repo-hash-{index}")),
                repo_label: Some(format!("owner/repo-{index}")),
                branch_hash: None,
                branch_label: None,
                path_hash: Some(format!("path-hash-{index}")),
                path_label: Some(format!("/tmp/project-{index}")),
            });
            summary
        })
        .collect();
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_budget".to_string(),
        device_id: "device".to_string(),
        sources: vec![],
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
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.summaries.len())
            .sum::<usize>(),
        25
    );
    assert!(chunks.iter().all(|chunk| chunk.sources.is_empty()));
    assert!(chunks
        .iter()
        .all(|chunk| estimate_http_rollup_d1_queries(chunk) <= HTTP_ROLLUP_D1_QUERY_BUDGET));
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.summaries.len())
            .collect::<Vec<_>>(),
        vec![7, 6, 6, 6]
    );
}

#[test]
fn code_change_metric_d1_estimate_matches_batched_backend_writes() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let mut one_metric = test_task_only_sync_batch(now, 0, 0);
    one_metric.code_change_metrics = vec![test_code_change_metric(0, now)];
    let mut many_metrics = one_metric.clone();
    many_metrics.code_change_metrics = (0..10_000)
        .map(|index| test_code_change_metric(index, now))
        .collect();

    assert_eq!(estimate_http_rollup_d1_queries(&one_metric), 7);
    assert_eq!(
        estimate_http_rollup_d1_queries(&many_metrics),
        estimate_http_rollup_d1_queries(&one_metric)
    );
}

#[test]
fn v4_account_evidence_d1_estimate_includes_alias_lookup() {
    let now = Utc
        .with_ymd_and_hms(2026, 8, 23, 10, 12, 43)
        .single()
        .expect("date");
    let mut batch = test_task_only_sync_batch(now, 0, 0);
    let baseline = estimate_http_rollup_d1_queries(&batch);
    let account_id = ProviderAccountId("account-plan-estimate".to_string());
    batch.account_plan_observations = vec![statsai_core::AccountPlanProjectionV1 {
        schema_version: statsai_core::ACCOUNT_PLAN_PROJECTION_SCHEMA_VERSION.to_string(),
        projection_id: "projection-plan-estimate".to_string(),
        semantic_fingerprint: "a".repeat(64),
        device_id: batch.device_id.clone(),
        provider: "codex".to_string(),
        provider_account_id: account_id.clone(),
        raw_plan_name: "plus".to_string(),
        plan_name: "Plus".to_string(),
        observed_at: now,
        active_from: None,
        active_until: None,
        is_current_snapshot: true,
        evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
        confidence: Confidence::High,
    }];
    batch.account_evidence_summaries = vec![statsai_core::AccountEvidenceSummaryV1 {
        schema_version: statsai_core::ACCOUNT_EVIDENCE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: "evidence-summary-estimate".to_string(),
        device_id: batch.device_id.clone(),
        provider: "codex".to_string(),
        provider_account_id: account_id,
        first_strong_observed_at: Some(now),
        last_strong_observed_at: Some(now),
        strong_observation_count: 1,
        directly_bound_conversations: 0,
        uncovered_gap_count: 0,
        conflict_count: 0,
        evidence_kinds: vec![statsai_core::AccountEvidenceKind::AuthSnapshot],
    }];

    assert_eq!(
        estimate_http_rollup_d1_queries(&batch),
        baseline + 5,
        "metadata, evidence-alias, ownership lookup, and possible cleanup must be budgeted"
    );
}

#[test]
fn code_change_metrics_use_the_backends_batched_collection_limit() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let mut batch = test_task_only_sync_batch(now, 0, 0);
    batch.code_change_metrics = (0..1_000)
        .map(|index| test_code_change_metric(index, now))
        .collect();

    let chunks = split_http_rollup_sync_batches_without_snapshot(&batch);

    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].code_change_metrics.len(), 1_000);
}

#[test]
fn http_rollup_project_counts_include_path_only_projects() {
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-path-only-project"),
        LocationOrigin::Configured,
    );
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let mut summary = test_summary("codex", &source, now, 10, None);
    summary.project = Some(ProjectInfo {
        project_id: "project-path-only".to_string(),
        project_label: Some("hi".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/Users/example/Documents/Codex/2026-05-29/hi".to_string()),
    });
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_path_only_project".to_string(),
        device_id: "device".to_string(),
        sources: vec![],
        accounts: vec![],
        source_account_assignments: vec![],
        subscriptions: vec![],
        account_plan_observations: vec![],
        account_evidence_summaries: vec![],
        events: vec![],
        summaries: vec![summary],
        task_buckets: vec![],
        task_verifications: vec![],
        code_change_metrics: vec![],
        quota_cycle_contributions: vec![],
        authoritative_snapshot: None,
        created_at: now,
    };

    assert_eq!(http_rollup_project_count(&batch), 1);
    assert_eq!(http_rollup_project_location_count(&batch), 1);
}

fn test_dense_task_only_sync_batch(now: DateTime<Utc>, span_count: usize) -> SyncBatch {
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollup-dense-task-only"),
        LocationOrigin::Configured,
    );
    let spans = (0..span_count)
        .map(|index| {
            let started_at = now + Duration::minutes(index as i64);
            let ended_at = started_at + Duration::minutes(1);
            TaskSpan {
                schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                span_id: TaskSpanId(format!("dense-span-{index}")),
                provider: "codex".to_string(),
                source_id: source.source_id.clone(),
                span_kind: "codex_task".to_string(),
                source_record_id: None,
                source_file_path_hash: None,
                summary_id: None,
                session_id: Some(format!("dense-session-{index}")),
                thread_id: Some(format!("dense-thread-{index}")),
                title: format!("Dense task {index}"),
                normalized_title: format!("dense task {index}"),
                title_source: Some("thread_name".to_string()),
                summary_preview: None,
                todo_excerpt: None,
                issue_keys: Vec::new(),
                branch_family: Some("main".to_string()),
                project_bucket: "dense-bucket".to_string(),
                project: Some(ProjectInfo {
                    project_id: "project-dense".to_string(),
                    project_label: Some("Dense".to_string()),
                    repo_remote_hash: Some("repo-dense".to_string()),
                    repo_label: Some("statsai/dense".to_string()),
                    branch_hash: Some("branch-dense".to_string()),
                    branch_label: Some("main".to_string()),
                    path_hash: Some("path-dense".to_string()),
                    path_label: Some("/workspace/dense".to_string()),
                }),
                git: None,
                usage: UsageCounts {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    total_tokens: Some(15),
                    requests: Some(1),
                    ..UsageCounts::default()
                },
                estimated_cost_usd: Some(25),
                estimated_cost_micro_usd: Some(250_000),
                event_count: 1,
                has_usage_evidence: true,
                total_messages: 2,
                user_messages: 1,
                assistant_messages: 1,
                developer_messages: 0,
                linked_event_ids: Vec::new(),
                confidence: Confidence::High,
                is_meta: false,
                started_at,
                ended_at: Some(ended_at),
                duration_seconds: Some(60),
            }
        })
        .collect::<Vec<_>>();
    let members = spans
        .iter()
        .enumerate()
        .map(|(index, span)| WorkItemMember {
            work_item_id: WorkItemId("dense-work-item".to_string()),
            span_id: span.span_id.clone(),
            ordinal: index,
        })
        .collect::<Vec<_>>();
    let last_span = spans.last().expect("last dense span");

    SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_dense_task_only".to_string(),
        device_id: "device".to_string(),
        sources: Vec::new(),
        accounts: Vec::new(),
        source_account_assignments: Vec::new(),
        subscriptions: Vec::new(),
        account_plan_observations: Vec::new(),
        account_evidence_summaries: Vec::new(),
        events: Vec::new(),
        summaries: Vec::new(),
        task_buckets: vec![TaskBucketSnapshot {
            project_bucket: "dense-bucket".to_string(),
            generated_at: last_span.ended_at.expect("dense task bucket end timestamp"),
            applied_verification_cursor: None,
            work_items: vec![WorkItem {
                schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
                work_item_id: WorkItemId("dense-work-item".to_string()),
                anchor_span_id: spans.first().expect("first dense span").span_id.clone(),
                tail_span_id: last_span.span_id.clone(),
                project_bucket: "dense-bucket".to_string(),
                title: "Dense task".to_string(),
                normalized_title: "dense task".to_string(),
                status: TaskStatus::NeedsReview,
                confidence: Confidence::Medium,
                started_at: spans.first().expect("first dense span").started_at,
                ended_at: last_span.ended_at.expect("dense task bucket end timestamp"),
                duration_seconds: Some((span_count as u64).saturating_mul(60)),
                span_count: span_count as u64,
                event_count: span_count as u64,
                total_input_tokens: (span_count as u64).saturating_mul(10),
                total_cache_creation_tokens: 0,
                total_cache_read_tokens: 0,
                total_output_tokens: (span_count as u64).saturating_mul(5),
                total_reasoning_tokens: 0,
                total_tokens: (span_count as u64).saturating_mul(15),
                estimated_cost_usd: Some((span_count as i64).saturating_mul(25)),
                estimated_cost_micro_usd: Some((span_count as i64).saturating_mul(250_000)),
                providers: vec!["codex".to_string()],
                issue_keys: Vec::new(),
                repo_label: Some("statsai/dense".to_string()),
                branch_labels: vec!["main".to_string()],
                path_label: Some("/workspace/dense".to_string()),
                summary_preview: None,
                todo_excerpt: None,
                no_git: false,
                cross_provider: false,
                continuation_reasons: Vec::new(),
                review_reasons: vec!["needs_review".to_string()],
            }],
            members,
            spans,
        }],
        task_verifications: Vec::new(),
        code_change_metrics: Vec::new(),
        quota_cycle_contributions: Vec::new(),
        authoritative_snapshot: None,
        created_at: now,
    }
}

fn test_multi_bucket_dense_task_only_sync_batch(
    now: DateTime<Utc>,
    bucket_count: usize,
    span_count_per_bucket: usize,
) -> SyncBatch {
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollup-multi-dense-task-only"),
        LocationOrigin::Configured,
    );
    let task_buckets = (0..bucket_count)
        .map(|bucket_index| {
            let project_bucket = format!("dense-bucket-{bucket_index}");
            let work_item_id = WorkItemId(format!("dense-work-item-{bucket_index}"));
            let spans = (0..span_count_per_bucket)
                .map(|span_index| {
                    let offset_minutes = (bucket_index * span_count_per_bucket + span_index) as i64;
                    let started_at = now + Duration::minutes(offset_minutes);
                    let ended_at = started_at + Duration::minutes(1);
                    TaskSpan {
                        schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                        span_id: TaskSpanId(format!(
                            "dense-bucket-{bucket_index}-span-{span_index}"
                        )),
                        provider: "codex".to_string(),
                        source_id: source.source_id.clone(),
                        span_kind: "codex_task".to_string(),
                        source_record_id: None,
                        source_file_path_hash: None,
                        summary_id: None,
                        session_id: Some(format!(
                            "dense-bucket-{bucket_index}-session-{span_index}"
                        )),
                        thread_id: Some(format!("dense-bucket-{bucket_index}-thread-{span_index}")),
                        title: format!("Dense task {bucket_index}-{span_index}"),
                        normalized_title: format!("dense task {bucket_index}-{span_index}"),
                        title_source: Some("thread_name".to_string()),
                        summary_preview: None,
                        todo_excerpt: None,
                        issue_keys: Vec::new(),
                        branch_family: Some("main".to_string()),
                        project_bucket: project_bucket.clone(),
                        project: Some(ProjectInfo {
                            project_id: format!("project-dense-{bucket_index}"),
                            project_label: Some(format!("Dense {bucket_index}")),
                            repo_remote_hash: Some(format!("repo-dense-{bucket_index}")),
                            repo_label: Some(format!("statsai/dense-{bucket_index}")),
                            branch_hash: Some("branch-dense".to_string()),
                            branch_label: Some("main".to_string()),
                            path_hash: Some(format!("path-dense-{bucket_index}")),
                            path_label: Some(format!("/workspace/dense-{bucket_index}")),
                        }),
                        git: None,
                        usage: UsageCounts {
                            input_tokens: Some(10),
                            output_tokens: Some(5),
                            total_tokens: Some(15),
                            requests: Some(1),
                            ..UsageCounts::default()
                        },
                        estimated_cost_usd: Some(25),
                        estimated_cost_micro_usd: Some(250_000),
                        event_count: 1,
                        has_usage_evidence: true,
                        total_messages: 2,
                        user_messages: 1,
                        assistant_messages: 1,
                        developer_messages: 0,
                        linked_event_ids: Vec::new(),
                        confidence: Confidence::High,
                        is_meta: false,
                        started_at,
                        ended_at: Some(ended_at),
                        duration_seconds: Some(60),
                    }
                })
                .collect::<Vec<_>>();
            let members = spans
                .iter()
                .enumerate()
                .map(|(span_index, span)| WorkItemMember {
                    work_item_id: work_item_id.clone(),
                    span_id: span.span_id.clone(),
                    ordinal: span_index,
                })
                .collect::<Vec<_>>();
            let first_span = spans.first().expect("first dense span");
            let last_span = spans.last().expect("last dense span");
            TaskBucketSnapshot {
                project_bucket: project_bucket.clone(),
                generated_at: last_span.ended_at.expect("dense task bucket end timestamp"),
                applied_verification_cursor: None,
                work_items: vec![WorkItem {
                    schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
                    work_item_id: work_item_id.clone(),
                    anchor_span_id: first_span.span_id.clone(),
                    tail_span_id: last_span.span_id.clone(),
                    project_bucket,
                    title: format!("Dense task bucket {bucket_index}"),
                    normalized_title: format!("dense task bucket {bucket_index}"),
                    status: TaskStatus::NeedsReview,
                    confidence: Confidence::Medium,
                    started_at: first_span.started_at,
                    ended_at: last_span.ended_at.expect("dense task bucket end timestamp"),
                    duration_seconds: Some((span_count_per_bucket as u64).saturating_mul(60)),
                    span_count: span_count_per_bucket as u64,
                    event_count: span_count_per_bucket as u64,
                    total_input_tokens: (span_count_per_bucket as u64).saturating_mul(10),
                    total_cache_creation_tokens: 0,
                    total_cache_read_tokens: 0,
                    total_output_tokens: (span_count_per_bucket as u64).saturating_mul(5),
                    total_reasoning_tokens: 0,
                    total_tokens: (span_count_per_bucket as u64).saturating_mul(15),
                    estimated_cost_usd: Some((span_count_per_bucket as i64).saturating_mul(25)),
                    estimated_cost_micro_usd: Some(
                        (span_count_per_bucket as i64).saturating_mul(250_000),
                    ),
                    providers: vec!["codex".to_string()],
                    issue_keys: Vec::new(),
                    repo_label: Some(format!("statsai/dense-{bucket_index}")),
                    branch_labels: vec!["main".to_string()],
                    path_label: Some(format!("/workspace/dense-{bucket_index}")),
                    summary_preview: None,
                    todo_excerpt: None,
                    no_git: false,
                    cross_provider: false,
                    continuation_reasons: Vec::new(),
                    review_reasons: vec!["needs_review".to_string()],
                }],
                members,
                spans,
            }
        })
        .collect::<Vec<_>>();

    SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_multi_dense_task_only".to_string(),
        device_id: "device".to_string(),
        sources: Vec::new(),
        accounts: Vec::new(),
        source_account_assignments: Vec::new(),
        subscriptions: Vec::new(),
        account_plan_observations: Vec::new(),
        account_evidence_summaries: Vec::new(),
        events: Vec::new(),
        summaries: Vec::new(),
        task_buckets,
        task_verifications: Vec::new(),
        code_change_metrics: Vec::new(),
        quota_cycle_contributions: Vec::new(),
        authoritative_snapshot: None,
        created_at: now,
    }
}

#[test]
fn dense_single_task_bucket_stays_within_batched_d1_budget_estimate() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = test_dense_task_only_sync_batch(now, 240);

    assert!(
        estimate_http_rollup_d1_queries(&batch) <= HTTP_ROLLUP_D1_QUERY_BUDGET,
        "dense single-bucket task sync should fit after batched task writes"
    );
}

#[test]
fn multi_bucket_dense_task_sync_splits_to_fit_chunked_write_budget() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = test_multi_bucket_dense_task_only_sync_batch(now, 5, 600);

    let chunks = split_http_rollup_sync_batches(&batch);

    assert!(chunks.len() > 1);
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.task_buckets.len())
            .sum::<usize>(),
        batch.task_buckets.len()
    );
    assert!(chunks
        .iter()
        .all(|chunk| estimate_http_rollup_d1_queries(chunk) <= HTTP_ROLLUP_D1_QUERY_BUDGET));
}
