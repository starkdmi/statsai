use super::*;
use chrono::{TimeZone, Utc};
use statsai_core::{
    source_id, CodeChangeMetric, CodeChangeMetricKind, CodeLineCounts, Confidence, CoverageStatus,
    ProjectInfo, SourceKind, SyncAuthoritativeSnapshot, TaskBucketSnapshot, TaskSpan, TaskSpanId,
    TaskStatus, TaskVerdict, TaskVerification, TaskVerificationAction, TaskVerificationCursor,
    TaskVerificationId, UsageCounts, WorkItem, WorkItemId, WorkItemMember,
    CODE_CHANGE_METRIC_SCHEMA_VERSION, SYNC_ACK_V1_SCHEMA_VERSION, SYNC_ACK_V4_SCHEMA_VERSION,
    SYNC_BATCH_V1_SCHEMA_VERSION, SYNC_BATCH_V2_SCHEMA_VERSION, SYNC_BATCH_V3_SCHEMA_VERSION,
    SYNC_BATCH_V4_SCHEMA_VERSION, SYNC_BATCH_V5_SCHEMA_VERSION, TASK_SPAN_SCHEMA_VERSION,
    TASK_VERIFICATION_SCHEMA_VERSION, WORK_ITEM_SCHEMA_VERSION,
};

fn empty_batch() -> SyncBatch {
    SyncBatch {
        schema_version: SYNC_BATCH_V4_SCHEMA_VERSION.to_string(),
        batch_id: "batch_test".to_string(),
        device_id: "device_test".to_string(),
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
        authoritative_snapshot: None,
        created_at: Utc::now(),
    }
}

fn test_header(name: &str, value: &str) -> Header {
    Header::from_bytes(name, value).expect("valid test header")
}

#[test]
fn loopback_resolution_rejects_empty_and_mixed_results() {
    let loopback_v4 = "127.0.0.1:8765".parse().expect("IPv4 loopback");
    let loopback_v6 = "[::1]:8765".parse().expect("IPv6 loopback");
    let non_loopback = "192.0.2.1:8765".parse().expect("non-loopback");

    assert!(validated_loopback_addr(&[]).is_err());
    assert!(validated_loopback_addr(&[loopback_v4, non_loopback]).is_err());
    assert_eq!(
        validated_loopback_addr(&[loopback_v6, loopback_v4]).expect("all loopback"),
        loopback_v6
    );
}

#[test]
fn loopback_literal_is_resolved_once_to_a_socket_address() {
    assert_eq!(
        resolve_loopback_addr("127.0.0.1:8765").expect("loopback address"),
        "127.0.0.1:8765".parse().expect("expected address")
    );
    assert!(resolve_loopback_addr("192.0.2.1:8765").is_err());
}

#[test]
fn health_is_public_but_rejects_browser_origins() {
    assert_eq!(
        validate_http_request(&Method::Get, "/health", &[], None, "secret"),
        Ok(())
    );
    assert_eq!(
        validate_http_request(
            &Method::Get,
            "/health",
            &[test_header("Origin", "https://attacker.example")],
            None,
            "secret",
        ),
        Err(HttpRejection {
            status: StatusCode(403),
            message: "browser-originated requests are not allowed",
        })
    );
}

#[test]
fn data_routes_require_the_daemon_bearer_token() {
    assert_eq!(
        validate_http_request(&Method::Get, "/accounts", &[], None, "secret"),
        Err(HttpRejection {
            status: StatusCode(401),
            message: "missing or invalid bearer token",
        })
    );
    assert_eq!(
        validate_http_request(
            &Method::Get,
            "/accounts",
            &[test_header("Authorization", "Bearer secret")],
            None,
            "secret",
        ),
        Ok(())
    );
}

#[test]
fn sync_route_requires_json_and_rejects_oversized_declared_bodies() {
    let authorization = test_header("Authorization", "Bearer secret");
    assert_eq!(
        validate_http_request(
            &Method::Post,
            "/v1/sync/batches",
            &[
                authorization.clone(),
                test_header("Content-Type", "text/plain")
            ],
            Some(2),
            "secret",
        ),
        Err(HttpRejection {
            status: StatusCode(415),
            message: "content-type must be application/json",
        })
    );
    assert_eq!(
        validate_http_request(
            &Method::Post,
            "/v1/sync/batches",
            &[
                authorization,
                test_header("Content-Type", "application/json; charset=utf-8"),
            ],
            Some(MAX_SYNC_BATCH_BYTES + 1),
            "secret",
        ),
        Err(HttpRejection {
            status: StatusCode(413),
            message: "sync batch is too large",
        })
    );
}

#[test]
fn ingest_empty_sync_batch_returns_ack() {
    let store = Store::in_memory().expect("store");
    let ack = ingest_sync_batch(&store, &empty_batch()).expect("ack");

    assert_eq!(ack.schema_version, SYNC_ACK_V4_SCHEMA_VERSION);
    assert_eq!(ack.batch_id, "batch_test");
    assert_eq!(ack.accepted.events, 0);
    assert_eq!(ack.duplicates.events, 0);
    assert!(ack.rejected.is_empty());
}

#[test]
fn ingest_rejects_projection_collections_the_loopback_store_cannot_persist() {
    let store = Store::in_memory().expect("store");
    let mut batch = empty_batch();
    batch.account_plan_observations.push(
        serde_json::from_value(json!({
            "schema_version": "account_plan_projection.v1",
            "projection_id": "projection-1",
            "semantic_fingerprint": "fingerprint-1",
            "device_id": batch.device_id,
            "provider": "codex",
            "provider_account_id": "account-1",
            "raw_plan_name": "pro",
            "plan_name": "Pro",
            "observed_at": "2026-08-20T12:00:00Z",
            "active_from": null,
            "active_until": null,
            "is_current_snapshot": true,
            "evidence_kind": "auth_snapshot",
            "confidence": "high"
        }))
        .expect("plan projection"),
    );

    // `empty_batch` is v4, which has no acknowledgement counter for these,
    // so the version guard fires before the persistence one.
    let versioned = ingest_sync_batch(&store, &batch).expect_err("account plan below v5");
    assert!(versioned
        .to_string()
        .contains("account-plan evidence requires sync_batch.v5"));

    batch.schema_version = SYNC_BATCH_V5_SCHEMA_VERSION.to_string();
    let error = ingest_sync_batch(&store, &batch).expect_err("unsupported projection");
    assert!(error
        .to_string()
        .contains("account-plan evidence is not supported by the loopback daemon"));
    assert!(store
        .account_plan_observations()
        .expect("local plan ledger")
        .is_empty());
}

#[test]
fn ingest_v1_batch_returns_v1_ack_schema() {
    let store = Store::in_memory().expect("store");
    let mut batch = empty_batch();
    batch.schema_version = SYNC_BATCH_V1_SCHEMA_VERSION.to_string();

    let ack = ingest_sync_batch(&store, &batch).expect("ack");
    assert_eq!(ack.schema_version, SYNC_ACK_V1_SCHEMA_VERSION);
}

#[test]
fn ingest_v2_batch_rejects_code_change_metrics() {
    let store = Store::in_memory().expect("store");
    let mut batch = empty_batch();
    batch.schema_version = SYNC_BATCH_V2_SCHEMA_VERSION.to_string();
    batch.code_change_metrics.push(CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "metric-v2".to_string(),
        device_id: "device_test".to_string(),
        day: Utc::now().date_naive(),
        project_id: None,
        repository_hash: None,
        commit_hash: None,
        kind: CodeChangeMetricKind::AgentEdit,
        counts: CodeLineCounts::default(),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Complete,
        git_coverage: CoverageStatus::Unavailable,
    });

    let error = ingest_sync_batch(&store, &batch).expect_err("v2 metric payload");
    assert!(error
        .to_string()
        .contains("code-change metrics require sync_batch.v3"));
}

#[test]
fn ingest_v3_batch_rejects_quota_cycle_contributions() {
    let store = Store::in_memory().expect("store");
    let mut batch = empty_batch();
    batch.schema_version = SYNC_BATCH_V3_SCHEMA_VERSION.to_string();
    batch
        .quota_cycle_contributions
        .push(statsai_core::QuotaCycleContributionV1 {
            schema_version: statsai_core::QUOTA_CYCLE_CONTRIBUTION_SCHEMA_VERSION.to_string(),
            contribution_id: "quota_cycle_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            provider: "codex".to_string(),
            provider_account_id: statsai_core::ProviderAccountId("acct-1".to_string()),
            limit_id: None,
            window_minutes: statsai_core::QUOTA_WEEKLY_WINDOW_MINUTES,
            representative_reset: Utc::now(),
            representative_reset_epoch_seconds: Utc::now().timestamp(),
            has_schedule_overlap: false,
            daily_envelopes: Vec::new(),
            boundary_slices: Vec::new(),
        });

    let error = ingest_sync_batch(&store, &batch).expect_err("v3 quota payload");
    assert!(error
        .to_string()
        .contains("quota cycle contributions require sync_batch.v4"));
}

#[test]
fn ingest_v4_batch_refuses_quota_cycle_contributions_it_cannot_store() {
    let store = Store::in_memory().expect("store");
    let mut batch = empty_batch();
    batch.schema_version = SYNC_BATCH_V4_SCHEMA_VERSION.to_string();
    batch
        .quota_cycle_contributions
        .push(statsai_core::QuotaCycleContributionV1 {
            schema_version: statsai_core::QUOTA_CYCLE_CONTRIBUTION_SCHEMA_VERSION.to_string(),
            contribution_id: "quota_cycle_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            provider: "codex".to_string(),
            provider_account_id: statsai_core::ProviderAccountId("acct-1".to_string()),
            limit_id: None,
            window_minutes: statsai_core::QUOTA_WEEKLY_WINDOW_MINUTES,
            representative_reset: Utc::now(),
            representative_reset_epoch_seconds: Utc::now().timestamp(),
            has_schedule_overlap: false,
            daily_envelopes: Vec::new(),
            boundary_slices: Vec::new(),
        });

    // Accepting them would tell the sender to stop offering cycles this
    // store has nowhere to put.
    let error = ingest_sync_batch(&store, &batch).expect_err("unsupported contributions");
    assert!(error
        .to_string()
        .contains("quota cycle contributions are not supported"));
}

#[test]
fn ingest_v3_batch_rejects_metric_owned_by_another_device_before_persisting() {
    let store = Store::in_memory().expect("store");
    let mut batch = empty_batch();
    let metric = CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "metric-other-device".to_string(),
        device_id: batch.device_id.clone(),
        day: Utc::now().date_naive(),
        project_id: None,
        repository_hash: None,
        commit_hash: None,
        kind: CodeChangeMetricKind::AgentEdit,
        counts: CodeLineCounts::default(),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Complete,
        git_coverage: CoverageStatus::Unavailable,
    };
    batch.code_change_metrics.push(metric.clone());
    ingest_sync_batch(&store, &batch).expect("matching metric owner");
    batch.batch_id = "batch_mismatched_owner".to_string();
    batch.code_change_metrics[0].device_id = "other_device".to_string();

    let error = ingest_sync_batch(&store, &batch).expect_err("mismatched metric owner");

    assert!(error
        .to_string()
        .contains("code-change metric device_id must match batch device_id"));
    assert_eq!(
        store
            .list_code_change_metrics(false)
            .expect("stored metrics"),
        vec![metric]
    );
}

#[test]
fn ingest_rejects_unsupported_schema() {
    let store = Store::in_memory().expect("store");
    let mut batch = empty_batch();
    batch.schema_version = "sync_batch.v0".to_string();

    let error = ingest_sync_batch(&store, &batch).expect_err("unsupported schema");
    assert!(error.to_string().contains("unsupported sync batch schema"));
}

#[test]
fn ingest_rejects_authoritative_snapshot_before_persisting_batch() {
    let store = Store::in_memory().expect("store");
    let mut batch = empty_batch();
    batch.task_verifications = vec![test_task_verification()];
    batch.authoritative_snapshot = Some(SyncAuthoritativeSnapshot {
        snapshot_id: "snapshot_test".to_string(),
        part_index: 0,
        part_count: 1,
        ..SyncAuthoritativeSnapshot::default()
    });

    let error = ingest_sync_batch(&store, &batch).expect_err("unsupported snapshot");

    assert!(error
        .to_string()
        .contains("authoritative snapshots are not supported"));
    assert!(store
        .task_verifications()
        .expect("task verifications")
        .is_empty());
}

#[test]
fn health_payload_reports_daemon_version() {
    assert_eq!(health_payload()["status"], "ok");
    assert_eq!(health_payload()["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn ingest_persists_task_payloads_before_acknowledging_them() {
    let store = Store::in_memory().expect("store");
    let mut batch = empty_batch();
    batch.task_buckets = vec![test_task_bucket_snapshot()];
    batch.task_verifications = vec![test_task_verification()];

    let ack = ingest_sync_batch(&store, &batch).expect("ack");

    assert_eq!(ack.accepted.task_buckets, 1);
    assert_eq!(ack.accepted.task_verifications, 1);
    assert_eq!(ack.duplicates.task_verifications, 0);
    assert_eq!(store.task_spans().expect("task spans").len(), 1);
    assert_eq!(store.work_items().expect("work items").len(), 1);
    assert_eq!(
        store
            .task_verifications()
            .expect("task verifications")
            .len(),
        1
    );
}

#[test]
fn ingest_rebuilds_stale_task_buckets_against_newer_local_verifications() {
    let store = Store::in_memory().expect("store");
    store
        .merge_task_verification(&test_task_verification())
        .expect("seed verification");

    let mut batch = empty_batch();
    batch.task_buckets = vec![test_task_bucket_snapshot()];

    let ack = ingest_sync_batch(&store, &batch).expect("ack");

    assert_eq!(ack.accepted.task_buckets, 1);
    assert_eq!(ack.accepted.task_verifications, 0);
    let work_items = store.work_items().expect("work items");
    assert_eq!(work_items.len(), 1);
    assert_eq!(work_items[0].status, TaskStatus::RejectedMeta);
    assert!(work_items[0]
        .review_reasons
        .iter()
        .any(|reason| reason.starts_with("manual_reject:")));
}

#[test]
fn rejected_batch_rolls_back_earlier_task_verifications() {
    let store = Store::in_memory().expect("store");
    let first = test_task_verification();
    let mut duplicate_id = first.clone();
    duplicate_id.action = TaskVerificationAction::Accept {
        work_item_id: WorkItemId("work_other".to_string()),
        anchor_span_id: TaskSpanId("span_other".to_string()),
    };
    duplicate_id.action_key = duplicate_id.action.action_key();
    let mut batch = empty_batch();
    batch.task_verifications = vec![first, duplicate_id];

    ingest_sync_batch(&store, &batch).expect_err("duplicate verification id");

    assert!(store
        .task_verifications()
        .expect("task verifications")
        .is_empty());
}

fn test_task_bucket_snapshot() -> TaskBucketSnapshot {
    let started_at = Utc
        .with_ymd_and_hms(2026, 7, 5, 10, 0, 0)
        .single()
        .expect("start");
    let ended_at = Utc
        .with_ymd_and_hms(2026, 7, 5, 10, 5, 0)
        .single()
        .expect("end");
    let span_id = TaskSpanId("span_ingest_test".to_string());
    let work_item_id = WorkItemId("work_ingest_test".to_string());

    TaskBucketSnapshot {
        project_bucket: "bucket-ingest".to_string(),
        generated_at: ended_at,
        applied_verification_cursor: Some(TaskVerificationCursor {
            updated_at: ended_at,
            verification_id: TaskVerificationId("tvf-ingest-cursor".to_string()),
        }),
        work_items: vec![WorkItem {
            schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
            work_item_id: work_item_id.clone(),
            anchor_span_id: span_id.clone(),
            tail_span_id: span_id.clone(),
            project_bucket: "bucket-ingest".to_string(),
            title: "Implement hosted task sync".to_string(),
            normalized_title: "implement hosted task sync".to_string(),
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
            summary_preview: Some("Implement hosted task sync".to_string()),
            todo_excerpt: Some("todo hosted task sync".to_string()),
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
            source_id: source_id("codex", SourceKind::LocalAdapter, "daemon-ingest"),
            span_kind: "codex_task".to_string(),
            source_record_id: None,
            source_file_path_hash: None,
            summary_id: None,
            session_id: Some("session-ingest".to_string()),
            thread_id: Some("thread-ingest".to_string()),
            title: "Implement hosted task sync".to_string(),
            normalized_title: "implement hosted task sync".to_string(),
            title_source: Some("thread_name".to_string()),
            summary_preview: Some("Implement hosted task sync".to_string()),
            todo_excerpt: Some("todo hosted task sync".to_string()),
            issue_keys: Vec::new(),
            branch_family: Some("main".to_string()),
            project_bucket: "bucket-ingest".to_string(),
            project: Some(ProjectInfo {
                project_id: "project-ingest".to_string(),
                project_label: Some("StatsAI".to_string()),
                repo_remote_hash: Some("repo-hash-ingest".to_string()),
                repo_label: Some("statsai/repo".to_string()),
                branch_hash: Some("branch-hash-ingest".to_string()),
                branch_label: Some("main".to_string()),
                path_hash: Some("path-hash-ingest".to_string()),
                path_label: Some("/workspace/statsai".to_string()),
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
            duration_seconds: Some(300),
        }],
    }
}

fn test_task_verification() -> TaskVerification {
    let created_at = Utc
        .with_ymd_and_hms(2026, 7, 5, 10, 6, 0)
        .single()
        .expect("created_at");
    TaskVerification {
        schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
        verification_id: TaskVerificationId("tvf-ingest-1".to_string()),
        action_key: "anchor:span_ingest_test".to_string(),
        action: TaskVerificationAction::Reject {
            work_item_id: WorkItemId("work_ingest_test".to_string()),
            anchor_span_id: TaskSpanId("span_ingest_test".to_string()),
            reason: TaskVerdict::Meta,
        },
        created_at,
        updated_at: created_at,
    }
}
