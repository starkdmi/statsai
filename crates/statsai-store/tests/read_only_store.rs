//! `Store::open_read_only`: what a process that must never write can open and
//! read, including while another process scans into the same store.

use chrono::{DateTime, Duration, TimeZone, Utc};
use rusqlite::Connection;
use statsai_core::{
    account_identity_observation_id, account_plan_observation_id, activity_invocation_id,
    provider_account_id, source_account_assignment_id, summary_id, AccountEvidenceKind,
    AccountEvidenceSummaryV1, AccountIdentityObservationV1, AccountPlanObservationV1,
    AccountPlanProjectionV1, ActivityCoverageLevel, ActivityCoverageV1, ActivityDurationKind,
    ActivityFamily, ActivityInvocationV1, ActivityKind, ActivityOutcome, ActivityRollupV1,
    CodeChangeMetric, CodeChangeMetricKind, CodeLineCounts, Confidence, CostInfo, CoverageStatus,
    EventId, EventSource, IdentitySource, LocationOrigin, ModelInfo, PrivacyInfo, PrivacyMode,
    ProviderAccount, ProviderAccountId, QuotaCycleContributionV1, QuotaObservationRecordV1,
    SessionInfo, SessionRollupV1, SourceAccountAssignment, SourceKind, SourceLocation,
    SummaryMetadata, SyncBatch, TaskBucketSnapshot, TaskSpan, TaskSpanId, TaskStatus,
    TaskVerification, TaskVerificationAction, TaskVerificationCursor, TaskVerificationId,
    UsageCounts, UsageEvent, UsageSummary, WorkItem, WorkItemId, WorkItemMember,
    ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION, ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION,
    ACTIVITY_COVERAGE_SCHEMA_VERSION, ACTIVITY_INVOCATION_SCHEMA_VERSION, ACTIVITY_PARSER_REVISION,
    CODE_CHANGE_METRIC_SCHEMA_VERSION, PROVIDER_ACCOUNT_SCHEMA_VERSION,
    SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION, SYNC_BATCH_SCHEMA_VERSION, TASK_SPAN_SCHEMA_VERSION,
    TASK_VERIFICATION_SCHEMA_VERSION, USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
    WORK_ITEM_SCHEMA_VERSION,
};
use statsai_store::{
    ActivityPersistMode, ActivityStatus, CacheReportQuery, QuotaQuery, ReadOnlyOpenError,
    ReadStore, SessionFilter, SessionSort, Store, CURRENT_SCHEMA_VERSION,
};
use std::path::{Path, PathBuf};

fn started_at(index: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0)
        .single()
        .expect("time")
        + Duration::minutes(i64::from(index))
}

fn source() -> SourceLocation {
    SourceLocation::local_adapter(
        "codex",
        "read-only-test",
        "0",
        Path::new("/tmp/statsai-read-only-test"),
        LocationOrigin::Configured,
    )
}

fn event(index: u32) -> UsageEvent {
    let at = started_at(index);
    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: EventId(format!("event_read_only_{index:04}")),
        device_id: "device_read_only".to_string(),
        provider: "codex".to_string(),
        source_id: source().source_id,
        provider_account_id: None,
        subscription_id: None,
        source: EventSource {
            adapter_id: "read-only-test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "jsonl".to_string(),
            source_path_hash: None,
            source_record_id: None,
            parse_confidence: Confidence::High,
        },
        session: SessionInfo {
            session_id: format!("session_read_only_{}", index % 3),
            local_session_id_hash: Some(format!("session_hash_read_only_{}", index % 3)),
            // Session rollups list only sessions with a title or a project.
            title: Some(format!("Read-only session {}", index % 3)),
            started_at: at,
            ended_at: None,
            duration_seconds: None,
            turn_started_at: None,
        },
        model: Some(ModelInfo {
            normalized_name: Some("gpt-6-sol".to_string()),
            ..ModelInfo::default()
        }),
        usage: UsageCounts {
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            cache_read_tokens: Some(500),
            requests: Some(1),
            ..UsageCounts::default()
        },
        runtime: None,
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: None,
            provider_reported_micro_usd: None,
            pricing_source: None,
            pricing_version: None,
            confidence: Confidence::Low,
        },
        parse_evidence: None,
        project: None,
        git: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        created_at: at,
        imported_at: at,
        context: None,
    }
}

fn store_path(directory: &tempfile::TempDir) -> PathBuf {
    directory.path().join("statsai.sqlite")
}

/// A store as the CLI leaves it: written, then closed.
fn written_store(path: &Path, events: u32) {
    let store = Store::open(path).expect("create store");
    store.upsert_source(&source()).expect("source");
    let events = (0..events).map(event).collect::<Vec<_>>();
    store.insert_events(&events).expect("events");
}

fn claude_source() -> SourceLocation {
    SourceLocation::local_adapter(
        "claude_code",
        "read-only-test",
        "0",
        Path::new("/tmp/statsai-read-only-test-claude"),
        LocationOrigin::Configured,
    )
}

fn codex_account_id() -> ProviderAccountId {
    provider_account_id("codex", "read-only")
}

fn claude_account_id() -> ProviderAccountId {
    provider_account_id("claude_code", "read-only")
}

fn account(provider: &str, id: ProviderAccountId) -> ProviderAccount {
    let at = started_at(0);
    ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: id,
        provider: provider.to_string(),
        identity_source: IdentitySource::UserConfigured,
        provider_user_id: None,
        provider_user_id_hash: Some(format!("user-hash-{provider}")),
        email_hash: None,
        email: None,
        org_id_hash: None,
        account_label: Some(format!("{provider} read-only")),
        plan_name: None,
        confidence: Confidence::High,
        verified_at: None,
        created_at: at,
        updated_at: at,
    }
}

fn assignment(source: &SourceLocation, account: &ProviderAccountId) -> SourceAccountAssignment {
    let from = started_at(0) - Duration::days(30);
    SourceAccountAssignment {
        schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
        assignment_id: source_account_assignment_id(&source.source_id, account, from),
        source_id: source.source_id.clone(),
        provider: source.provider.clone(),
        provider_account_id: account.clone(),
        started_at: from,
        ended_at: None,
        record_source: IdentitySource::UserConfigured,
        verified_at: None,
        created_at: from,
        updated_at: from,
    }
}

fn manual_summary(name: &str, format: &str) -> UsageSummary {
    let start = Utc
        .with_ymd_and_hms(2026, 7, 2, 0, 0, 0)
        .single()
        .expect("summary day");
    UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: summary_id("codex", &source().source_id, name),
        device_id: "device_read_only".to_string(),
        provider: "codex".to_string(),
        source_id: source().source_id,
        provider_account_id: Some(codex_account_id()),
        source: EventSource {
            adapter_id: "read-only-test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::Manual,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "json".to_string(),
            source_path_hash: None,
            source_record_id: Some(format!("record:{name}")),
            parse_confidence: Confidence::Medium,
        },
        model: None,
        models: Vec::new(),
        usage: UsageCounts {
            input_tokens: Some(2_000),
            total_tokens: Some(2_000),
            ..UsageCounts::default()
        },
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: Some(40_000),
            provider_reported_micro_usd: None,
            pricing_source: None,
            pricing_version: None,
            confidence: Confidence::Medium,
        },
        parse_evidence: None,
        project: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        metrics: None,
        period_start: Some(start),
        period_end: Some(start + Duration::days(1)),
        observed_at: start,
        metadata: SummaryMetadata {
            summary_format: format.to_string(),
            summary_version: Some("1".to_string()),
            total_sessions: None,
            total_messages: None,
            last_computed_at: Some(start),
        },
        imported_at: start,
    }
}

fn quota_record(index: u32, used_percent: f64) -> QuotaObservationRecordV1 {
    let observed_at = started_at(0) + Duration::hours(i64::from(index) * 6);
    let resets_at = started_at(0) + Duration::days(5);
    serde_json::from_value(serde_json::json!({
        "observation": {
            "schema_version": "quota_observation.v1",
            "observation_id": format!("quota-observation-read-only-{index}"),
            "semantic_fingerprint": format!("quota-semantic-read-only-{index}"),
            "provider": "codex",
            "source_id": source().source_id,
            "provider_account_id": null,
            "observed_at": observed_at,
            "source_file_path_hash": "quota-file",
            "source_record_id": format!("quota-record-{index}"),
            "source_line_number": index + 1,
            "payload_hash": format!("quota-payload-{index}"),
            "usage_sample": null,
            "usage_event_id": null,
            "usage_link_kind": "none",
            "status": {
                "plan_type": "pro",
                "individual_limit": null,
                "spend_control_state": null,
                "reached_type": null,
                "credits": {
                    "has_credits": false,
                    "unlimited": false,
                    "balance": null,
                    "balance_raw": null
                }
            }
        },
        "windows": [{
            "schema_version": "quota_window_observation.v1",
            "window_observation_id": format!("quota-window-read-only-{index}"),
            "observation_id": format!("quota-observation-read-only-{index}"),
            "provider_slot": "secondary",
            "limit_id": "codex",
            "window_minutes": 10080,
            "used_percent": used_percent,
            "resets_at": resets_at,
            "resets_at_epoch_seconds": resets_at.timestamp()
        }],
        "raw_rate_limits": {"secondary": {"used_percent": used_percent}}
    }))
    .expect("quota record")
}

fn work_item_and_span(index: u32) -> (WorkItem, WorkItemMember, TaskSpan) {
    let begun = started_at(index * 10);
    let ended = begun + Duration::minutes(5);
    let bucket = format!("bucket-read-only-{index}");
    let span_id = TaskSpanId(format!("span-read-only-{index}"));
    let work_item_id = WorkItemId(format!("work-read-only-{index}"));
    let usage = UsageCounts {
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        requests: Some(1),
        ..UsageCounts::default()
    };
    let item = WorkItem {
        schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
        work_item_id: work_item_id.clone(),
        anchor_span_id: span_id.clone(),
        tail_span_id: span_id.clone(),
        project_bucket: bucket.clone(),
        title: format!("Task {index}"),
        normalized_title: format!("task {index}"),
        status: TaskStatus::NeedsReview,
        confidence: Confidence::Medium,
        started_at: begun,
        ended_at: ended,
        duration_seconds: Some(300),
        span_count: 1,
        event_count: 1,
        total_input_tokens: 10,
        total_cache_creation_tokens: 0,
        total_cache_read_tokens: 0,
        total_output_tokens: 5,
        total_reasoning_tokens: 0,
        total_tokens: 15,
        estimated_cost_usd: None,
        estimated_cost_micro_usd: Some(250_000),
        providers: vec!["codex".to_string()],
        issue_keys: Vec::new(),
        repo_label: Some("owner/repo".to_string()),
        branch_labels: vec!["main".to_string()],
        path_label: Some("/tmp/statsai-read-only-project".to_string()),
        summary_preview: None,
        todo_excerpt: None,
        no_git: false,
        cross_provider: false,
        continuation_reasons: Vec::new(),
        review_reasons: vec!["needs_review".to_string()],
    };
    let member = WorkItemMember {
        work_item_id,
        span_id: span_id.clone(),
        ordinal: 0,
    };
    let span = TaskSpan {
        schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
        span_id,
        provider: "codex".to_string(),
        source_id: source().source_id,
        span_kind: "codex_task".to_string(),
        source_record_id: Some(format!("task-record-{index}")),
        source_file_path_hash: None,
        summary_id: None,
        session_id: Some(format!("task-session-{index}")),
        thread_id: Some(format!("task-thread-{index}")),
        title: format!("Task {index}"),
        normalized_title: format!("task {index}"),
        title_source: Some("thread_name".to_string()),
        summary_preview: None,
        todo_excerpt: None,
        issue_keys: Vec::new(),
        branch_family: Some("main".to_string()),
        project_bucket: bucket,
        project: None,
        git: None,
        usage,
        estimated_cost_usd: None,
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
        started_at: begun,
        ended_at: Some(ended),
        duration_seconds: Some(300),
    };
    (item, member, span)
}

/// Tasks, task verifications and code-change metrics, received the way a
/// synced batch delivers them.
fn task_and_code_change_batch() -> SyncBatch {
    let task_buckets = (0..2)
        .map(|index| {
            let (item, member, span) = work_item_and_span(index);
            TaskBucketSnapshot {
                project_bucket: item.project_bucket.clone(),
                generated_at: item.ended_at,
                applied_verification_cursor: None,
                work_items: vec![item],
                members: vec![member],
                spans: vec![span],
            }
        })
        .collect();
    let verified_at = started_at(30);
    let code_change_metric = |index: u32| CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: format!("metric-read-only-{index}"),
        device_id: "device_read_only".to_string(),
        day: started_at(0).date_naive(),
        project_id: Some("project-read-only".to_string()),
        repository_hash: Some("repository-read-only".to_string()),
        commit_hash: None,
        kind: CodeChangeMetricKind::AgentEdit,
        counts: CodeLineCounts::default(),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Complete,
        git_coverage: CoverageStatus::Complete,
    };
    SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_read_only".to_string(),
        device_id: "device_read_only".to_string(),
        sources: Vec::new(),
        accounts: Vec::new(),
        source_account_assignments: Vec::new(),
        subscriptions: Vec::new(),
        account_plan_observations: Vec::new(),
        account_evidence_summaries: Vec::new(),
        events: Vec::new(),
        summaries: Vec::new(),
        task_buckets,
        task_verifications: vec![TaskVerification {
            schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
            verification_id: TaskVerificationId("tvf-read-only".to_string()),
            action_key: "status:span-read-only-0".to_string(),
            action: TaskVerificationAction::Accept {
                work_item_id: WorkItemId("work-read-only-0".to_string()),
                anchor_span_id: TaskSpanId("span-read-only-0".to_string()),
            },
            created_at: verified_at,
            updated_at: verified_at,
        }],
        code_change_metrics: vec![code_change_metric(0), code_change_metric(1)],
        quota_cycle_contributions: Vec::new(),
        activity_rollups: Vec::new(),
        activity_coverage: Vec::new(),
        sessions: Vec::new(),
        authoritative_snapshot: None,
        created_at: verified_at,
    }
}

/// A store with rows for every read beyond usage totals: usage, imported
/// summaries, accounts, quota windows and a manual weekly schedule, plan and
/// identity evidence, activity, sessions, tasks and code changes. It has no
/// subscriptions or sync state.
fn written_store_with_everything(path: &Path) {
    let store = Store::open(path).expect("create store");
    let codex = source();
    let claude = claude_source();
    store.upsert_source(&codex).expect("codex source");
    store.upsert_source(&claude).expect("claude source");
    for (provider, id) in [
        ("codex", codex_account_id()),
        ("claude_code", claude_account_id()),
    ] {
        store
            .upsert_account(&account(provider, id))
            .expect("account");
    }
    store
        .upsert_source_account_assignment(&assignment(&codex, &codex_account_id()))
        .expect("codex assignment");
    store
        .upsert_source_account_assignment(&assignment(&claude, &claude_account_id()))
        .expect("claude assignment");

    let mut events = (0..6).map(event).collect::<Vec<_>>();
    for event in &mut events {
        event.provider_account_id = Some(codex_account_id());
    }
    events.extend((0..3).map(|index| {
        let mut event = event(100 + index);
        event.event_id = EventId(format!("event_read_only_claude_{index}"));
        event.provider = "claude_code".to_string();
        event.source_id = claude.source_id.clone();
        event.provider_account_id = Some(claude_account_id());
        event.session.session_id = "session_read_only_claude".to_string();
        event.session.local_session_id_hash = Some("session_hash_read_only_claude".to_string());
        event
    }));
    store.insert_events(&events).expect("events");

    for (name, format) in [
        ("manual-daily", "manual_daily"),
        ("ccusage-daily", "ccusage_daily"),
    ] {
        store
            .upsert_summary(&manual_summary(name, format))
            .expect("summary");
    }

    store
        .upsert_quota_observations(&[
            quota_record(0, 10.0),
            quota_record(1, 35.5),
            quota_record(2, 50.0),
        ])
        .expect("quota observations");
    store
        .upsert_weekly_reset_anchor(
            "claude_code",
            &claude_account_id(),
            started_at(0) - Duration::hours(3),
        )
        .expect("weekly reset anchor");

    let evidence_at = started_at(0) - Duration::hours(1);
    store
        .upsert_account_plan_observations(&[AccountPlanObservationV1 {
            schema_version: ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: account_plan_observation_id(
                &codex.source_id,
                Some(&codex_account_id()),
                "pro",
                "Pro",
                evidence_at,
                AccountEvidenceKind::AuthSnapshot,
            ),
            provider: "codex".to_string(),
            source_id: codex.source_id.clone(),
            provider_account_id: Some(codex_account_id()),
            raw_plan_name: "pro".to_string(),
            plan_name: "Pro".to_string(),
            observed_at: evidence_at,
            active_from: None,
            active_until: None,
            is_current_snapshot: true,
            evidence_kind: AccountEvidenceKind::AuthSnapshot,
            confidence: Confidence::High,
            parser_version: "read-only-test".to_string(),
            artifact_path_hash: "a".repeat(64),
            record_fingerprint: "b".repeat(64),
        }])
        .expect("plan observation");
    store
        .upsert_account_identity_observations(&[AccountIdentityObservationV1 {
            schema_version: ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: account_identity_observation_id(
                &codex.source_id,
                AccountEvidenceKind::AuthSnapshot,
                evidence_at,
                "identity-read-only",
            ),
            provider: "codex".to_string(),
            source_id: codex.source_id.clone(),
            provider_account_id: Some(codex_account_id()),
            provider_user_id_hash: Some("user-hash-codex".to_string()),
            email_hash: None,
            conversation_id_hash: None,
            turn_id_hash: None,
            observed_at: evidence_at,
            evidence_kind: AccountEvidenceKind::AuthSnapshot,
            confidence: Confidence::High,
            auth_mode: None,
            application_version: None,
            parser_version: "read-only-test".to_string(),
            artifact_kind: "auth_json".to_string(),
            artifact_path_hash: "c".repeat(64),
            record_fingerprint: "identity-read-only".to_string(),
        }])
        .expect("identity observation");

    let invocation = |index: u32, name: &str, family| ActivityInvocationV1 {
        schema_version: ACTIVITY_INVOCATION_SCHEMA_VERSION.to_string(),
        invocation_id: activity_invocation_id(&["codex", "read-only", &index.to_string()]),
        provider: "codex".to_string(),
        source_id: codex.source_id.clone(),
        provider_account_id: Some(codex_account_id()),
        source_file_path_hash: "activity-file".to_string(),
        observed_at: started_at(index),
        kind: ActivityKind::Tool,
        display_name: name.to_string(),
        family,
        mcp_server: None,
        mcp_tool: None,
        plugin: None,
        skill_catalog: None,
        model: None,
        outcome: ActivityOutcome::Succeeded,
        duration_ms: Some(100),
        duration_kind: Some(ActivityDurationKind::Reported),
        evidence: "codex-native-items".to_string(),
        parser_revision: ACTIVITY_PARSER_REVISION.to_string(),
    };
    let day = started_at(0).date_naive().to_string();
    store
        .persist_activity_scan(
            "device_read_only",
            &codex.source_id,
            &[
                invocation(0, "exec", ActivityFamily::Shell),
                invocation(1, "apply_patch", ActivityFamily::FileWrite),
            ],
            &[ActivityCoverageV1 {
                schema_version: ACTIVITY_COVERAGE_SCHEMA_VERSION.to_string(),
                coverage_id: "activity-coverage-read-only".to_string(),
                device_id: "device_read_only".to_string(),
                source_id: codex.source_id.clone(),
                provider: "codex".to_string(),
                day: day.clone(),
                day_end: day,
                kind: ActivityKind::Tool,
                level: ActivityCoverageLevel::Complete,
                evidence: "codex-native-items".to_string(),
                parser_revision: ACTIVITY_PARSER_REVISION.to_string(),
            }],
            &["activity-file".to_string()],
            ActivityPersistMode::ReplaceFiles,
            None,
        )
        .expect("activity");

    store
        .ingest_sync_batch(&task_and_code_change_batch())
        .expect("tasks and code changes");
}

fn recorded_schema_version(path: &Path) -> Option<i64> {
    statsai_store::database_schema_version(path).expect("schema version")
}

fn open_error(path: &Path) -> ReadOnlyOpenError {
    match Store::open_read_only(path) {
        Ok(_) => panic!("{} must not open read-only", path.display()),
        Err(error) => error,
    }
}

/// Runs every read the read-only store exposes, so a write hidden in any of
/// them fails the test that calls this.
fn read_everything(reader: &ReadStore) {
    let now = started_at(0) + Duration::days(1);
    reader.schema_version().expect("schema version");
    reader.data_version().expect("data version");
    reader
        .applied_pricing_ruleset_version()
        .expect("pricing ruleset");
    reader.event_count().expect("event count");
    reader.token_total().expect("token total");
    reader
        .session_rollups_in_period(
            None,
            now,
            &SessionFilter::default(),
            SessionSort::default(),
            100,
            0,
        )
        .expect("session rollups");
    reader
        .session_stats_in_period(None, now, &SessionFilter::default())
        .expect("session stats");
    reader
        .all_sync_rollup_summaries()
        .expect("sync rollup summaries");
    reader
        .daily_rollups_between("2026-01-01", "2026-12-31")
        .expect("daily rollups");
    reader
        .cache_report(&CacheReportQuery::all_time(now), &Utc)
        .expect("cache report");
    reader
        .quota_status(&QuotaQuery::default())
        .expect("quota status");
    reader
        .quota_windows(&QuotaQuery::default())
        .expect("quota windows");
    reader
        .quota_observations(&QuotaQuery::default(), true)
        .expect("quota observations");
    reader
        .weekly_reset_anchor(
            "claude_code",
            &statsai_core::ProviderAccountId("acct_read_only".to_string()),
        )
        .expect("weekly reset anchor");
    reader.list_sources().expect("sources");
    reader.list_accounts().expect("accounts");
    reader
        .list_source_account_assignments()
        .expect("source account assignments");
    reader.list_subscriptions().expect("subscriptions");
    reader.list_sync_states().expect("sync states");
    reader.summaries().expect("summaries");
    reader.account(&codex_account_id()).expect("one account");
    reader
        .quota_cycle_contributions(&QuotaQuery::default(), "device_read_only")
        .expect("quota cycle contributions");
    reader
        .account_plan_projections("device_read_only")
        .expect("plan projections");
    reader
        .account_evidence_summaries("device_read_only")
        .expect("evidence summaries");
    reader.all_activity_rollups().expect("activity rollups");
    reader.all_activity_coverage().expect("activity coverage");
    reader
        .activity_invocation_count()
        .expect("activity invocation count");
    reader
        .activity_status(None, None, true)
        .expect("activity status");
    reader.all_session_rollups().expect("all session rollups");
    let work_items = reader.work_items().expect("work items");
    let work_item_id = work_items
        .first()
        .map(|item| item.work_item_id.clone())
        .unwrap_or_else(|| WorkItemId("work_read_only".to_string()));
    reader.work_item(&work_item_id).expect("one work item");
    reader.task_spans().expect("task spans");
    reader
        .task_spans_for_work_item(&work_item_id)
        .expect("work item spans");
    reader.task_verifications().expect("task verifications");
    for bucket in reader.task_project_buckets().expect("task buckets") {
        reader
            .task_bucket_snapshot(&bucket, None)
            .expect("task bucket snapshot");
    }
    reader
        .list_code_change_metrics(false)
        .expect("code change metrics");
    reader
        .with_read_snapshot(|snapshot| snapshot.event_count())
        .expect("read snapshot");
}

#[test]
fn a_missing_store_is_reported_and_not_created() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join(".statsai").join("statsai.sqlite");

    match open_error(&path) {
        ReadOnlyOpenError::NotFound { path: reported } => assert_eq!(reported, path),
        other => panic!("expected NotFound, got {other:?}"),
    }
    assert!(!path.exists(), "a read-only open created the database");
    assert!(
        !path.parent().expect("parent").exists(),
        "a read-only open created the store directory"
    );
}

#[test]
fn a_fresh_store_opens_and_reads_empty() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    drop(Store::open(&path).expect("create store"));

    let reader = Store::open_read_only(&path).expect("open fresh store read-only");

    assert_eq!(
        reader.schema_version().expect("schema"),
        CURRENT_SCHEMA_VERSION
    );
    assert_eq!(reader.event_count().expect("events"), 0);
    assert!(reader.list_sources().expect("sources").is_empty());
    assert!(reader
        .all_sync_rollup_summaries()
        .expect("summaries")
        .is_empty());
    read_everything(&reader);
}

/// Schema 31 is current as of this change; the test follows
/// `CURRENT_SCHEMA_VERSION`, so it keeps covering "a store this binary wrote".
#[test]
fn a_current_v31_store_reads_like_the_writer_and_is_left_byte_for_byte_unchanged() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 6);
    #[cfg(unix)]
    {
        // Store::open would tighten this to 0600; a reader must not.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen permissions");
    }
    let before = std::fs::read(&path).expect("database bytes");
    assert_eq!(recorded_schema_version(&path), Some(CURRENT_SCHEMA_VERSION));

    let reader = Store::open_read_only(&path).expect("open read-only");
    read_everything(&reader);
    assert_eq!(reader.event_count().expect("events"), 6);
    assert_eq!(reader.list_sources().expect("sources").len(), 1);
    // No writer has repriced this store, and a reader must not either.
    assert_eq!(
        reader.applied_pricing_ruleset_version().expect("ruleset"),
        None
    );
    let reader_summaries = reader.all_sync_rollup_summaries().expect("summaries");
    let reader_sessions = reader
        .session_rollups_in_period(
            None,
            started_at(0) + Duration::days(1),
            &SessionFilter::default(),
            SessionSort::default(),
            100,
            0,
        )
        .expect("sessions");
    assert!(!reader_summaries.is_empty());
    assert_eq!(reader_sessions.len(), 3);
    drop(reader);

    assert_eq!(
        std::fs::read(&path).expect("database bytes"),
        before,
        "reading changed the database file"
    );
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    if wal.exists() {
        assert_eq!(
            std::fs::metadata(&wal).expect("wal").len(),
            0,
            "a reader wrote to the WAL"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644, "a read-only open changed permissions");
    }

    // The writer sees the same rows the reader did.
    let writer = Store::open(&path).expect("writer");
    assert_eq!(
        writer.all_sync_rollup_summaries().expect("summaries"),
        reader_summaries
    );
    assert_eq!(
        writer
            .session_rollups_in_period(
                None,
                started_at(0) + Duration::days(1),
                &SessionFilter::default(),
                SessionSort::default(),
                100,
                0,
            )
            .expect("sessions"),
        reader_sessions
    );
}

/// What the reads beyond usage totals return, in a form two stores can be
/// compared by. A bucket snapshot's `generated_at` is the time of the read,
/// so it is left out. Reads that take arguments are made with values that
/// change the result, so a reader that ignored one would not match.
#[derive(Debug, PartialEq)]
struct BeyondUsage {
    summaries: Vec<UsageSummary>,
    account: Option<ProviderAccount>,
    quota_cycle_contributions: Vec<QuotaCycleContributionV1>,
    codex_quota_cycle_contributions: Vec<QuotaCycleContributionV1>,
    account_plan_projections: Vec<AccountPlanProjectionV1>,
    account_evidence_summaries: Vec<AccountEvidenceSummaryV1>,
    activity_rollups: Vec<ActivityRollupV1>,
    activity_coverage: Vec<ActivityCoverageV1>,
    activity_invocation_count: u64,
    activity_status: ActivityStatus,
    claude_activity_status: ActivityStatus,
    session_rollups: Vec<SessionRollupV1>,
    work_items: Vec<WorkItem>,
    work_item: Option<WorkItem>,
    task_spans: Vec<TaskSpan>,
    work_item_spans: Vec<TaskSpan>,
    task_verifications: Vec<TaskVerification>,
    task_project_buckets: Vec<String>,
    task_bucket_snapshots: Vec<TaskBucketSnapshot>,
    code_change_metrics: Vec<CodeChangeMetric>,
    dirty_code_change_metrics: Vec<CodeChangeMetric>,
}

/// The cursor a bucket snapshot echoes back as applied.
fn applied_cursor() -> TaskVerificationCursor {
    TaskVerificationCursor {
        updated_at: started_at(30),
        verification_id: TaskVerificationId("tvf-read-only".to_string()),
    }
}

macro_rules! read_beyond_usage {
    ($store:expr) => {{
        let store = $store;
        let work_items = store.work_items().expect("work items");
        let work_item_id = work_items
            .first()
            .expect("a work item")
            .work_item_id
            .clone();
        let task_project_buckets = store
            .task_project_buckets()
            .expect("task buckets")
            .into_iter()
            .collect::<Vec<_>>();
        let task_bucket_snapshots = task_project_buckets
            .iter()
            .map(|bucket| {
                let mut snapshot = store
                    .task_bucket_snapshot(bucket, Some(applied_cursor()))
                    .expect("task bucket snapshot");
                snapshot.generated_at = DateTime::<Utc>::UNIX_EPOCH;
                snapshot
            })
            .collect();
        BeyondUsage {
            summaries: store.summaries().expect("summaries"),
            account: store.account(&codex_account_id()).expect("account"),
            quota_cycle_contributions: store
                .quota_cycle_contributions(&QuotaQuery::default(), "device_read_only")
                .expect("quota cycle contributions"),
            codex_quota_cycle_contributions: store
                .quota_cycle_contributions(
                    &QuotaQuery {
                        provider: Some("codex".to_string()),
                        ..QuotaQuery::default()
                    },
                    "device_read_only",
                )
                .expect("codex quota cycle contributions"),
            account_plan_projections: store
                .account_plan_projections("device_read_only")
                .expect("plan projections"),
            account_evidence_summaries: store
                .account_evidence_summaries("device_read_only")
                .expect("evidence summaries"),
            activity_rollups: store.all_activity_rollups().expect("activity rollups"),
            activity_coverage: store.all_activity_coverage().expect("activity coverage"),
            activity_invocation_count: store.activity_invocation_count().expect("invocation count"),
            activity_status: store
                .activity_status(None, None, true)
                .expect("activity status"),
            claude_activity_status: store
                .activity_status(Some("claude_code"), Some(ActivityKind::Tool), false)
                .expect("claude activity status"),
            session_rollups: store.all_session_rollups().expect("session rollups"),
            work_items,
            work_item: store.work_item(&work_item_id).expect("work item"),
            task_spans: store.task_spans().expect("task spans"),
            work_item_spans: store
                .task_spans_for_work_item(&work_item_id)
                .expect("work item spans"),
            task_verifications: store.task_verifications().expect("verifications"),
            task_project_buckets,
            task_bucket_snapshots,
            code_change_metrics: store
                .list_code_change_metrics(false)
                .expect("code change metrics"),
            dirty_code_change_metrics: store
                .list_code_change_metrics(true)
                .expect("dirty code change metrics"),
        }
    }};
}

#[test]
fn the_reads_beyond_usage_match_the_writer_and_leave_the_store_unchanged() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store_with_everything(&path);
    let before = std::fs::read(&path).expect("database bytes");

    let reader = Store::open_read_only(&path).expect("open read-only");
    read_everything(&reader);
    let read = read_beyond_usage!(&reader);
    drop(reader);

    // Every read beyond usage totals came back non-empty, and each argument
    // changed what came back.
    assert_eq!(read.summaries.len(), 2);
    assert_eq!(
        read.account
            .as_ref()
            .map(|account| account.provider_account_id.clone()),
        Some(codex_account_id())
    );
    for provider in ["codex", "claude_code"] {
        assert!(
            read.quota_cycle_contributions
                .iter()
                .any(|contribution| contribution.provider == provider),
            "no {provider} quota cycle contribution: {:?}",
            read.quota_cycle_contributions
        );
    }
    assert!(!read.codex_quota_cycle_contributions.is_empty());
    assert!(read
        .codex_quota_cycle_contributions
        .iter()
        .all(|contribution| contribution.provider == "codex"));
    assert!(read.codex_quota_cycle_contributions.len() < read.quota_cycle_contributions.len());
    assert_eq!(read.account_plan_projections.len(), 1);
    assert_eq!(read.account_evidence_summaries.len(), 1);
    assert_eq!(read.activity_rollups.len(), 2);
    assert_eq!(read.activity_coverage.len(), 1);
    assert_eq!(read.activity_invocation_count, 2);
    assert_eq!(read.activity_status.tool_calls, 2);
    assert!(read.activity_status.include_activity);
    assert_eq!(read.claude_activity_status.tool_calls, 0);
    assert!(!read.claude_activity_status.include_activity);
    assert_eq!(read.session_rollups.len(), 4);
    assert_eq!(read.work_items.len(), 2);
    assert_eq!(read.work_item.as_ref(), read.work_items.first());
    assert_eq!(read.task_spans.len(), 2);
    assert_eq!(read.work_item_spans.len(), 1);
    assert_eq!(read.task_verifications.len(), 1);
    assert_eq!(read.task_project_buckets.len(), 2);
    assert!(read.task_bucket_snapshots.iter().all(|snapshot| {
        snapshot.work_items.len() == 1
            && snapshot.spans.len() == 1
            && snapshot.applied_verification_cursor == Some(applied_cursor())
    }));
    assert_eq!(read.code_change_metrics.len(), 2);
    // Metrics received in a batch are already synced, so none is dirty.
    assert!(read.dirty_code_change_metrics.is_empty());

    assert_eq!(
        std::fs::read(&path).expect("database bytes"),
        before,
        "reading changed the database file"
    );
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    if wal.exists() {
        assert_eq!(
            std::fs::metadata(&wal).expect("wal").len(),
            0,
            "a reader wrote to the WAL"
        );
    }

    let writer = Store::open(&path).expect("writer");
    assert_eq!(read_beyond_usage!(&writer), read);
}

#[test]
fn an_older_schema_is_refused_and_not_migrated() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 1);
    let previous = CURRENT_SCHEMA_VERSION - 1;
    Connection::open(&path)
        .expect("raw connection")
        .execute(
            "DELETE FROM schema_migrations WHERE version > ?1",
            [previous],
        )
        .expect("roll the recorded schema back");

    match open_error(&path) {
        ReadOnlyOpenError::SchemaTooOld {
            path: reported,
            found,
            expected,
        } => {
            assert_eq!(reported, path);
            assert_eq!(found, previous);
            assert_eq!(expected, CURRENT_SCHEMA_VERSION);
        }
        other => panic!("expected SchemaTooOld, got {other:?}"),
    }
    assert_eq!(recorded_schema_version(&path), Some(previous));
}

#[test]
fn a_store_from_before_schema_tracking_is_refused_as_version_zero() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    Connection::open(&path)
        .expect("raw connection")
        .execute_batch("CREATE TABLE sources (source_id TEXT PRIMARY KEY);")
        .expect("legacy table");

    match open_error(&path) {
        ReadOnlyOpenError::SchemaTooOld {
            found, expected, ..
        } => {
            assert_eq!((found, expected), (0, CURRENT_SCHEMA_VERSION));
        }
        other => panic!("expected SchemaTooOld, got {other:?}"),
    }
    // A writer's open would have stamped and migrated it.
    assert_eq!(recorded_schema_version(&path), Some(0));
}

#[test]
fn a_newer_schema_is_refused_with_both_versions() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 1);
    let future = CURRENT_SCHEMA_VERSION + 1;
    Connection::open(&path)
        .expect("raw connection")
        .execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, '2026-10-01T00:00:00Z')",
            [future],
        )
        .expect("record a future schema");

    let error = open_error(&path);
    assert!(
        error
            .to_string()
            .contains("newer than this binary supports"),
        "{error}"
    );
    match error {
        ReadOnlyOpenError::SchemaTooNew {
            found, expected, ..
        } => assert_eq!((found, expected), (future, CURRENT_SCHEMA_VERSION)),
        other => panic!("expected SchemaTooNew, got {other:?}"),
    }
    assert_eq!(recorded_schema_version(&path), Some(future));
}

#[test]
fn a_file_that_is_not_a_database_is_an_open_error() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    std::fs::write(
        &path,
        b"definitely not sqlite, but long enough to have a header....",
    )
    .expect("write junk");

    let error = open_error(&path);
    assert!(matches!(error, ReadOnlyOpenError::Open { .. }), "{error:?}");
    // The message says why, on its own and not again through `source`.
    let message = error.to_string();
    assert!(message.contains("file is not a database"), "{message}");
    assert!(std::error::Error::source(&error).is_none());
}

/// Reading the version must not set the journal mode or create
/// `schema_migrations`, as the writer's version check does: on a store in
/// rollback-journal mode, or inside a read transaction, that is a write.
#[test]
fn the_schema_version_is_read_without_writing_even_from_a_rollback_journal_store() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 1);
    let journal_mode: String = Connection::open(&path)
        .expect("raw connection")
        .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))
        .expect("leave WAL mode");
    assert_eq!(journal_mode, "delete");
    let before = std::fs::read(&path).expect("database bytes");

    let reader = Store::open_read_only(&path).expect("open read-only");
    assert_eq!(
        reader.schema_version().expect("schema version"),
        CURRENT_SCHEMA_VERSION
    );
    assert_eq!(
        reader
            .with_read_snapshot(|snapshot| snapshot.schema_version())
            .expect("schema version in a snapshot"),
        CURRENT_SCHEMA_VERSION
    );
    reader.check_schema().expect("current schema");
    read_everything(&reader);
    drop(reader);

    assert_eq!(
        std::fs::read(&path).expect("database bytes"),
        before,
        "reading the schema version changed the database file"
    );
}

#[test]
fn check_schema_reports_a_migration_made_after_the_open() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 1);
    let reader = Store::open_read_only(&path).expect("open read-only");
    reader.check_schema().expect("current schema at open");

    let future = CURRENT_SCHEMA_VERSION + 1;
    Connection::open(&path)
        .expect("writer connection")
        .execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, '2026-10-01T00:00:00Z')",
            [future],
        )
        .expect("a newer binary migrates the store");

    assert_eq!(reader.schema_version().expect("schema version"), future);
    match reader.check_schema() {
        Err(ReadOnlyOpenError::SchemaTooNew {
            path: reported,
            found,
            expected,
        }) => {
            assert_eq!(reported, path);
            assert_eq!((found, expected), (future, CURRENT_SCHEMA_VERSION));
        }
        other => panic!("expected SchemaTooNew, got {other:?}"),
    }
}

#[test]
fn every_read_refuses_a_store_migrated_after_the_open() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    written_store(&path, 1);
    let reader = Store::open_read_only(&path).expect("open read-only");
    assert_eq!(reader.event_count().expect("read at open"), 1);

    let future = CURRENT_SCHEMA_VERSION + 1;
    Connection::open(&path)
        .expect("writer connection")
        .execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, '2026-10-01T00:00:00Z')",
            [future],
        )
        .expect("a newer binary migrates the store");

    let refused = |error: anyhow::Error| match error.downcast_ref::<ReadOnlyOpenError>() {
        Some(ReadOnlyOpenError::SchemaTooNew { found, .. }) => assert_eq!(*found, future),
        _ => panic!("expected SchemaTooNew, got {error:#}"),
    };
    refused(reader.event_count().expect_err("a single read"));
    refused(reader.list_sources().expect_err("a list read"));
    refused(
        reader
            .with_read_snapshot(|reader| reader.event_count())
            .expect_err("a read snapshot"),
    );
}

#[test]
fn reads_proceed_during_a_write_and_see_it_once_committed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    let writer = Store::open(&path).expect("writer");
    let reader = Store::open_read_only(&path).expect("reader");
    let version_before = reader.data_version().expect("data version");

    writer
        .apply_scan_update(|store| {
            store.upsert_source(&source())?;
            store.insert_events(&[event(0), event(1)])?;
            // The writer holds an open write transaction here. WAL lets the
            // reader through, at the last committed state.
            assert_eq!(reader.event_count()?, 0);
            assert!(reader.list_sources()?.is_empty());
            Ok(())
        })
        .expect("scan update");

    assert_eq!(reader.event_count().expect("events"), 2);
    assert_eq!(reader.list_sources().expect("sources").len(), 1);
    assert_ne!(
        reader.data_version().expect("data version"),
        version_before,
        "data_version must move when another connection commits"
    );

    // A snapshot keeps its point in time while the writer commits more.
    reader
        .with_read_snapshot(|snapshot| {
            assert_eq!(snapshot.event_count()?, 2);
            writer.insert_events(&[event(2)])?;
            assert_eq!(snapshot.event_count()?, 2);
            assert_eq!(snapshot.all_sync_rollup_summaries()?.len(), 1);
            Ok(())
        })
        .expect("snapshot");
    assert_eq!(reader.event_count().expect("events"), 3);
}

#[test]
fn a_reader_thread_keeps_reading_while_a_writer_thread_commits() {
    const WRITES: u32 = 25;
    let directory = tempfile::tempdir().expect("tempdir");
    let path = store_path(&directory);
    let writer = Store::open(&path).expect("writer");
    writer.upsert_source(&source()).expect("source");
    let done = std::sync::atomic::AtomicBool::new(false);

    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let reader = Store::open_read_only(&path).expect("reader");
            let mut last = 0;
            let mut reads = 0u32;
            loop {
                let finished = done.load(std::sync::atomic::Ordering::Acquire);
                let count = reader.event_count().expect("read during writes");
                assert!(count >= last, "event count went backwards");
                last = count;
                reader
                    .all_sync_rollup_summaries()
                    .expect("summaries during writes");
                reads += 1;
                if finished {
                    return (last, reads);
                }
            }
        });
        for index in 0..WRITES {
            writer.insert_events(&[event(index)]).expect("write");
        }
        done.store(true, std::sync::atomic::Ordering::Release);
        let (last, reads) = reader.join().expect("reader thread");
        assert_eq!(last, u64::from(WRITES));
        assert!(reads > 0);
    });
}
