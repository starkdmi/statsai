pub(super) use super::support::*;
pub(crate) use super::*;

mod batch;
mod chunking;
mod http;

#[test]
fn dry_run_sync_does_not_write_file() {
    let store = Store::in_memory().expect("store");
    let dir = tempfile::tempdir().expect("tempdir");
    let output = dir.path().join("batch.json");

    sync(
        SyncCommand {
            output: Some(output.clone()),
            dry_run: true,
            ..test_sync_command("file")
        },
        &store,
        "device",
    )
    .expect("sync dry run");

    assert!(!output.exists());
}

#[test]
fn dry_run_sync_does_not_persist_sync_preferences() {
    let store = Store::in_memory().expect("store");

    sync(
        SyncCommand {
            dry_run: true,
            include_projects: true,
            ..test_sync_command("file")
        },
        &store,
        "device",
    )
    .expect("sync dry run");

    assert_eq!(
        store.sync_preferences().expect("sync preferences"),
        SyncPreferences::default()
    );
}

#[test]
fn http_dry_run_does_not_require_auth_or_clear_sync_tracking() {
    let store = Store::in_memory().expect("store");
    let endpoint = "https://api.example.com/api/sync/batches".to_string();
    store
        .record_sync_success("http", &endpoint, "batch_local", &[], &[], None)
        .expect("sync success");
    let state_before = store
        .sync_state("http", &endpoint)
        .expect("sync state")
        .expect("present");

    let previous_api_url = std::env::var("STATSAI_API_URL").ok();
    let previous_sync_token = std::env::var("STATSAI_SYNC_TOKEN").ok();
    std::env::set_var(
        "STATSAI_API_URL",
        format!("https://{}-dry-run-authless.invalid", std::process::id()),
    );
    std::env::remove_var("STATSAI_SYNC_TOKEN");

    let result = sync(
        SyncCommand {
            endpoint: Some(endpoint.clone()),
            dry_run: true,
            ..test_sync_command("http")
        },
        &store,
        "device",
    );

    if let Some(value) = previous_api_url {
        std::env::set_var("STATSAI_API_URL", value);
    } else {
        std::env::remove_var("STATSAI_API_URL");
    }
    if let Some(value) = previous_sync_token {
        std::env::set_var("STATSAI_SYNC_TOKEN", value);
    } else {
        std::env::remove_var("STATSAI_SYNC_TOKEN");
    }

    result.expect("sync dry run");

    let state_after = store
        .sync_state("http", &endpoint)
        .expect("sync state")
        .expect("present");
    assert_eq!(state_after, state_before);
}

#[test]
fn full_dry_run_does_not_clear_pending_http_resume_state() {
    let store = Store::in_memory().expect("store");
    let endpoint = "https://api.example.com/api/sync/batches".to_string();
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-full-dry-run-resume"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let event = test_event(
        "codex",
        &source,
        now,
        Some(provider_account_id("codex", "personal")),
        TokenParts::total(10),
    );
    store.insert_event(&event).expect("event");
    store.rebuild_sync_rollups().expect("rebuild");

    let initial_command = SyncCommand {
        endpoint: Some(endpoint.clone()),
        ..test_sync_command("http")
    };
    let target = sync_target(&initial_command).expect("target");
    let (initial_batch, _) =
        build_sync_batch(&initial_command, &store, "device", &target).expect("initial batch");
    let expected_logical_batch_id = logical_http_rollup_batch_id(&initial_batch.batch_id);
    record_rollup_sync_chunk_success(
        &store,
        "http",
        &target,
        &expected_logical_batch_id,
        &initial_batch,
    )
    .expect("record partial sync state");

    let state = store
        .sync_state("http", &target)
        .expect("sync state")
        .expect("present");
    assert_eq!(
        state.pending_resume_batch_id.as_deref(),
        Some(expected_logical_batch_id.as_str())
    );

    let full_dry_run_command = SyncCommand {
        endpoint: Some(endpoint),
        full: true,
        dry_run: true,
        ..test_sync_command("http")
    };
    let (dry_run_batch, dry_run_mode) =
        build_sync_batch(&full_dry_run_command, &store, "device", &target)
            .expect("full dry-run batch");
    assert_eq!(dry_run_mode, SyncPayloadMode::Rollups);
    assert_eq!(dry_run_batch.summaries.len(), 1);

    let state_after = store
        .sync_state("http", &target)
        .expect("sync state")
        .expect("present");
    assert_eq!(
        state_after.pending_resume_batch_id, state.pending_resume_batch_id,
        "dry-run must not mutate pending resume state"
    );
}

pub(crate) fn test_task_only_sync_batch(
    now: DateTime<Utc>,
    bucket_count: usize,
    verification_count: usize,
) -> SyncBatch {
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollup-task-only"),
        LocationOrigin::Configured,
    );
    let task_buckets = (0..bucket_count)
        .map(|index| {
            let started_at = now + Duration::minutes(index as i64);
            let ended_at = started_at + Duration::minutes(5);
            let span_id = TaskSpanId(format!("span-task-{index}"));
            let work_item_id = WorkItemId(format!("work-task-{index}"));
            TaskBucketSnapshot {
                project_bucket: format!("bucket-task-{index}"),
                generated_at: ended_at,
                applied_verification_cursor: None,
                work_items: vec![WorkItem {
                    schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
                    work_item_id: work_item_id.clone(),
                    anchor_span_id: span_id.clone(),
                    tail_span_id: span_id.clone(),
                    project_bucket: format!("bucket-task-{index}"),
                    title: format!("Task {index}"),
                    normalized_title: format!("task {index}"),
                    status: TaskStatus::NeedsReview,
                    confidence: Confidence::Medium,
                    started_at,
                    ended_at,
                    duration_seconds: Some(300),
                    span_count: 1,
                    event_count: 1,
                    total_input_tokens: 10,
                    total_cache_creation_tokens: 0,
                    total_cache_read_tokens: 0,
                    total_output_tokens: 5,
                    total_reasoning_tokens: 0,
                    total_tokens: 15,
                    estimated_cost_usd: Some(25),
                    estimated_cost_micro_usd: Some(250_000),
                    providers: vec!["codex".to_string()],
                    issue_keys: Vec::new(),
                    repo_label: Some("statsai/repo".to_string()),
                    branch_labels: vec!["main".to_string()],
                    path_label: Some("/workspace/statsai".to_string()),
                    summary_preview: None,
                    todo_excerpt: None,
                    no_git: false,
                    cross_provider: false,
                    continuation_reasons: Vec::new(),
                    review_reasons: vec!["needs_review".to_string()],
                }],
                members: vec![WorkItemMember {
                    work_item_id,
                    span_id: span_id.clone(),
                    ordinal: 0,
                }],
                spans: vec![TaskSpan {
                    schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                    span_id,
                    provider: "codex".to_string(),
                    source_id: source.source_id.clone(),
                    span_kind: "codex_task".to_string(),
                    source_record_id: None,
                    source_file_path_hash: None,
                    summary_id: None,
                    session_id: Some(format!("session-task-{index}")),
                    thread_id: Some(format!("thread-task-{index}")),
                    title: format!("Task {index}"),
                    normalized_title: format!("task {index}"),
                    title_source: Some("thread_name".to_string()),
                    summary_preview: None,
                    todo_excerpt: None,
                    issue_keys: Vec::new(),
                    branch_family: Some("main".to_string()),
                    project_bucket: format!("bucket-task-{index}"),
                    project: None,
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
                    duration_seconds: Some(300),
                }],
            }
        })
        .collect::<Vec<_>>();
    let task_verifications = (0..verification_count)
        .map(|index| {
            let timestamp = now + Duration::minutes(index as i64);
            TaskVerification {
                schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
                verification_id: TaskVerificationId(format!("tvf-task-{index}")),
                action_key: format!("status:span-task-{index}"),
                action: TaskVerificationAction::Reject {
                    work_item_id: WorkItemId(format!("work-task-{index}")),
                    anchor_span_id: TaskSpanId(format!("span-task-{index}")),
                    reason: TaskVerdict::Meta,
                },
                created_at: timestamp,
                updated_at: timestamp,
            }
        })
        .collect::<Vec<_>>();

    SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_task_only".to_string(),
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
        task_verifications,
        code_change_metrics: Vec::new(),
        quota_cycle_contributions: Vec::new(),
        authoritative_snapshot: None,
        created_at: now,
    }
}

pub(crate) fn test_code_change_metric(
    index: usize,
    now: DateTime<Utc>,
) -> statsai_core::CodeChangeMetric {
    statsai_core::CodeChangeMetric {
        schema_version: statsai_core::CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: format!("metric-retry-{index}"),
        device_id: "device".to_string(),
        day: now.date_naive(),
        project_id: None,
        repository_hash: None,
        commit_hash: None,
        kind: statsai_core::CodeChangeMetricKind::AgentEdit,
        counts: statsai_core::CodeLineCounts::default(),
        attribution_confidence: None,
        trace_coverage: statsai_core::CoverageStatus::Complete,
        git_coverage: statsai_core::CoverageStatus::Complete,
    }
}

#[test]
fn remote_sync_batch_match_requires_same_last_batch_id() {
    let store = Store::in_memory().expect("store");
    store
        .record_sync_success(
            "http",
            "https://api.example.com/api/sync/batches",
            "batch_1_part_2_of_2",
            &[],
            &[],
            None,
        )
        .expect("record sync success");
    let local_state = store
        .sync_state("http", "https://api.example.com/api/sync/batches")
        .expect("state")
        .expect("present");

    assert!(remote_sync_batch_matches_local_state(
        &json!({
            "device": {
                "last_sync_batch_id": "batch_1"
            }
        }),
        &local_state
    ));
    assert!(!remote_sync_batch_matches_local_state(
        &json!({
            "device": {
                "last_sync_batch_id": null
            }
        }),
        &local_state
    ));
    assert!(!remote_sync_batch_matches_local_state(
        &json!({
            "device": {
                "last_sync_batch_id": "batch_2"
            }
        }),
        &local_state
    ));
}

#[test]
fn code_change_dedup_warning_covers_only_unblinded_http_commit_uploads() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let agent_edit = test_code_change_metric(0, now);
    let mut committed = test_code_change_metric(1, now);
    committed.kind = statsai_core::CodeChangeMetricKind::Committed;

    assert!(code_change_dedup_warning("http", false, std::slice::from_ref(&committed)).is_some());
    assert!(code_change_dedup_warning("http", true, std::slice::from_ref(&committed)).is_none());
    assert!(code_change_dedup_warning("file", false, std::slice::from_ref(&committed)).is_none());
    assert!(code_change_dedup_warning("http", false, std::slice::from_ref(&agent_edit)).is_none());
    assert!(code_change_dedup_warning("http", false, &[]).is_none());
}

#[test]
fn device_remote_reset_response_requires_explicit_device_scope() {
    assert!(ensure_device_remote_reset_response(&json!({
        "ok": true,
        "scope": "device_mirror",
        "device_id": "device-1"
    }))
    .is_ok());
    assert!(ensure_device_remote_reset_response(&json!({
        "ok": true,
        "scope": "mirror"
    }))
    .is_err());
}

#[test]
fn http_verify_pending_counts_match_sanitized_sync_payloads() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-verify-pending"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: provider_account_id("codex", "personal"),
        provider: "codex".to_string(),
        identity_source: IdentitySource::ManualHint,
        provider_user_id: None,
        provider_user_id_hash: None,
        email: None,
        email_hash: None,
        org_id_hash: None,
        account_label: Some("personal".to_string()),
        plan_name: Some("Pro".to_string()),
        confidence: Confidence::High,
        verified_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    store.upsert_account(&account).expect("account");
    let started_at = Utc::now();

    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id("codex", &account.provider_account_id, "pro", started_at),
        provider: "codex".to_string(),
        provider_account_id: account.provider_account_id.clone(),
        plan_name: "Pro".to_string(),
        price: 2000,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: None,
        renewal_day: None,
        started_at,
        ended_at: None,
        current_period_ends_at: None,
        status: SubscriptionStatus::Active,
        record_source: IdentitySource::UserConfigured,
        verified_at: None,
        notes: Some("private note".to_string()),
    };
    store
        .upsert_subscription(&subscription)
        .expect("subscription");
    let summary = test_summary(
        "codex",
        &source,
        Utc::now(),
        42,
        Some(account.provider_account_id.clone()),
    );
    store.upsert_summary(&summary).expect("summary");

    let target = "https://api.example.com/api/sync/batches".to_string();
    store
        .record_sources_synced("http", &target, &[sanitize_source_for_sync(source.clone())])
        .expect("record sources");
    store
        .record_accounts_synced(
            "http",
            &target,
            &[sanitize_account_for_sync(account.clone())],
        )
        .expect("record accounts");
    store
        .record_subscriptions_synced(
            "http",
            &target,
            &[sanitize_subscription_for_sync(subscription.clone())],
        )
        .expect("record subscriptions");
    store
        .record_summaries_synced(
            "http",
            &target,
            &[sanitize_summary_for_sync(summary.clone())],
        )
        .expect("record summaries");

    let local = sync_local_verify(&store, "http", &target, None, false).expect("local verify");
    assert_eq!(local.pending_sources, 0);
    assert_eq!(local.pending_accounts, 0);
    assert_eq!(local.pending_source_account_assignments, 0);
    assert_eq!(local.pending_subscriptions, 0);
    assert_eq!(local.total_passthrough_summaries, 0);
    assert_eq!(local.pending_passthrough_summaries, 0);
}

#[test]
fn sync_local_verify_uses_sanitized_rollup_hashes() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sanitized-rollups"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let mut event = test_event(
        "codex",
        &source,
        Utc::now(),
        Some(provider_account_id("codex", "personal")),
        TokenParts::total(42),
    );
    event.project = Some(ProjectInfo {
        project_id: "project-repo-backed".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/repo".to_string()),
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/Users/example/work/ai-stats".to_string()),
    });
    store.insert_event(&event).expect("event");
    store.rebuild_sync_rollups().expect("rebuild");

    let target = "https://api.example.com/api/sync/batches".to_string();
    let rollups: Vec<_> = store
        .all_sync_rollup_summaries()
        .expect("rollups")
        .into_iter()
        .map(sanitize_summary_for_sync)
        .collect();
    assert_eq!(rollups.len(), 1);
    assert_eq!(
        rollups[0]
            .project
            .as_ref()
            .and_then(|project| project.path_label.as_deref()),
        Some("/Users/example/work/ai-stats")
    );
    assert!(rollups[0].privacy.contains_file_paths);
    store
        .record_summaries_synced("http", &target, &rollups)
        .expect("record rollups");

    let local = sync_local_verify(&store, "http", &target, None, true).expect("local verify");
    assert_eq!(local.total_rollups, 1);
    assert_eq!(local.pending_rollups, 0);
}

#[test]
fn sync_local_verify_respects_project_sync_opt_in() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-verify-project-opt-in"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let mut event = test_event(
        "codex",
        &source,
        Utc::now(),
        Some(provider_account_id("codex", "personal")),
        TokenParts::total(42),
    );
    event.project = Some(ProjectInfo {
        project_id: "project-repo-backed".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/repo".to_string()),
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/Users/example/work/ai-stats".to_string()),
    });
    store.insert_event(&event).expect("event");
    store.rebuild_sync_rollups().expect("rebuild");

    let target = "https://api.example.com/api/sync/batches".to_string();
    let rollups: Vec<_> = store
        .all_sync_rollup_summaries()
        .expect("rollups")
        .into_iter()
        .map(|summary| sanitize_summary_for_sync_with_projects(summary, false))
        .collect();
    store
        .record_summaries_synced("http", &target, &rollups)
        .expect("record rollups");

    let hidden = sync_local_verify(&store, "http", &target, None, false)
        .expect("local verify without projects");
    let opted_in =
        sync_local_verify(&store, "http", &target, None, true).expect("local verify with projects");

    assert_eq!(hidden.pending_rollups, 0);
    assert_eq!(opted_in.pending_rollups, 1);
}

#[test]
fn local_auth_subscriptions_do_not_disable_the_subscription_mirror_check() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-verify-local-auth-subscription"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: provider_account_id("codex", "personal"),
        provider: "codex".to_string(),
        identity_source: IdentitySource::ManualHint,
        provider_user_id: None,
        provider_user_id_hash: None,
        email: None,
        email_hash: None,
        org_id_hash: None,
        account_label: Some("personal".to_string()),
        plan_name: Some("Pro".to_string()),
        confidence: Confidence::High,
        verified_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    store.upsert_account(&account).expect("account");

    let started_at = Utc::now();
    let subscription = |plan: &str, record_source: IdentitySource| Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id("codex", &account.provider_account_id, plan, started_at),
        provider: "codex".to_string(),
        provider_account_id: account.provider_account_id.clone(),
        plan_name: plan.to_string(),
        price: 2000,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: None,
        renewal_day: None,
        started_at,
        ended_at: None,
        current_period_ends_at: None,
        status: SubscriptionStatus::Active,
        record_source,
        verified_at: None,
        notes: None,
    };
    let synced = subscription("Pro", IdentitySource::UserConfigured);
    let local_auth = subscription("Max", IdentitySource::LocalAuth);
    store.upsert_subscription(&synced).expect("subscription");
    store
        .upsert_subscription(&local_auth)
        .expect("local-auth subscription");

    let target = "https://api.example.com/api/sync/batches".to_string();
    store
        .record_sources_synced("http", &target, &[sanitize_source_for_sync(source.clone())])
        .expect("record sources");
    store
        .record_accounts_synced(
            "http",
            &target,
            &[sanitize_account_for_sync(account.clone())],
        )
        .expect("record accounts");
    store
        .record_subscriptions_synced(
            "http",
            &target,
            &[sanitize_subscription_for_sync(synced.clone())],
        )
        .expect("record subscriptions");

    // The local-auth row is never uploaded, so it must not appear in either count:
    // as pending it would suppress the mirror check, and in the total it would
    // report a gap the remote can never close.
    let local = sync_local_verify(&store, "http", &target, None, false).expect("local verify");
    assert_eq!(local.total_subscriptions, 1);
    assert_eq!(local.pending_subscriptions, 0);

    let mirror_counts = |subscriptions: u64| {
        json!({
            "mirrorCounts": {
                "sources": 1,
                "accounts": 1,
                "source_account_assignments": 0,
                "subscriptions": subscriptions
            }
        })
    };
    assert_eq!(
        remote_metadata_gap_reason(&mirror_counts(1), &local),
        None,
        "a mirror holding every uploaded subscription must not report a gap"
    );
    assert_eq!(
        remote_metadata_gap_reason(&mirror_counts(0), &local).as_deref(),
        Some("subscriptions 0!=1"),
        "a mirror that lost the uploaded subscription must still be detected"
    );

    // Promotion to local-auth retires an already-uploaded row. The remote keeps
    // it until the next authoritative snapshot says otherwise, so this must stay
    // syncable incrementally rather than demanding --full.
    let promoted = subscription("Pro", IdentitySource::LocalAuth);
    assert_eq!(promoted.subscription_id, synced.subscription_id);
    store.upsert_subscription(&promoted).expect("promotion");

    let retiring = sync_local_verify(&store, "http", &target, None, false).expect("local verify");
    assert_eq!(retiring.total_subscriptions, 0);
    assert_eq!(retiring.pending_subscriptions, 0);
    assert_eq!(retiring.retired_subscriptions, 1);
    assert_eq!(
        remote_metadata_gap_reason(&mirror_counts(1), &retiring),
        None,
        "a mirror still holding a row awaiting retirement is not a gap"
    );
    assert_eq!(
        remote_metadata_gap_reason(&mirror_counts(0), &retiring),
        None,
        "a mirror that already dropped the retiring row reaches the same end state"
    );
    assert_eq!(
        remote_metadata_gap_reason(&mirror_counts(2), &retiring).as_deref(),
        Some("subscriptions 2!=0..1"),
        "rows beyond the retirement allowance are still reported"
    );
}
