use super::*;

#[test]
fn pending_http_sync_summary_counts_include_summary_only_usage() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "grok_build",
        "test",
        "0",
        Path::new("/tmp/grok-pending-sync-summary"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let target = "https://api.example.com/api/sync/batches";

    let mut summary = test_store_summary(&source, now, 70);
    summary.summary_id = summary_id(&source.provider, &source.source_id, "pending-summary");
    summary.source.source_kind = SourceKind::LocalAdapter;
    summary.metadata.summary_format = "grok_build_session_summary".to_string();
    summary.period_start = Some(now);
    summary.period_end = Some(now);
    store.upsert_summary(&summary).expect("summary");

    let mut backfill = test_store_summary(&source, now, 500);
    backfill.summary_id = summary_id(&source.provider, &source.source_id, "manual-backfill");
    backfill.source.source_kind = SourceKind::Manual;
    backfill.metadata.summary_format = "manual_period_summary".to_string();
    backfill.period_start = Some(now - chrono::Duration::days(4));
    backfill.period_end = Some(now);
    store.upsert_summary(&backfill).expect("backfill summary");

    let counts = store
        .pending_http_sync_summary_counts(target, "device")
        .expect("pending counts");
    assert_eq!(
        counts,
        PendingSyncSummaryCounts {
            rollups: 0,
            passthrough_summaries: 2,
            retired_entities: 0,
            quota_cycle_contributions: 0,
            total: 2,
            days: 5,
        }
    );

    store
        .record_summaries_synced("http", target, &[summary, backfill])
        .expect("record synced");

    let counts = store
        .pending_http_sync_summary_counts(target, "device")
        .expect("pending counts after sync");
    assert_eq!(counts.total, 0);
}

#[test]
fn pending_http_sync_summary_counts_include_edited_passthrough_summaries() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "grok_build",
        "test",
        "0",
        Path::new("/tmp/grok-edited-pending-sync-summary"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let target = "https://api.example.com/api/sync/batches";

    let mut summary = test_store_summary(&source, now, 70);
    summary.summary_id = summary_id(&source.provider, &source.source_id, "editable-summary");
    summary.source.source_kind = SourceKind::LocalAdapter;
    summary.metadata.summary_format = "grok_build_session_summary".to_string();
    summary.period_start = Some(now);
    summary.period_end = Some(now);
    store.upsert_summary(&summary).expect("summary");

    store
        .record_summaries_synced("http", target, &[summary.clone()])
        .expect("record synced");
    assert_eq!(
        store
            .pending_http_sync_summary_counts(target, "device")
            .expect("counts after sync")
            .total,
        0
    );

    let mut edited = summary.clone();
    edited.usage.total_tokens = Some(80);
    store.upsert_summary(&edited).expect("edited summary");

    let counts = store
        .pending_http_sync_summary_counts(target, "device")
        .expect("pending counts after edit");
    assert_eq!(
        counts,
        PendingSyncSummaryCounts {
            rollups: 0,
            passthrough_summaries: 1,
            retired_entities: 0,
            quota_cycle_contributions: 0,
            total: 1,
            days: 1,
        }
    );
}

#[test]
fn pending_http_sync_summary_counts_include_retirement_only_reconciliation() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-retirement-only-pending-sync"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let target = "https://api.example.com/api/sync/batches";
    let event = test_store_event(&source, Utc::now(), "retired-event");
    store.insert_event(&event).expect("event");
    let rollups = store.all_sync_rollup_summaries().expect("initial rollups");
    assert_eq!(rollups.len(), 1);

    store
        .record_sources_synced("http", target, std::slice::from_ref(&source))
        .expect("record source synced");
    store
        .record_summaries_synced("http", target, &rollups)
        .expect("record rollup synced");
    assert_eq!(
        store
            .pending_http_sync_summary_counts(target, "device")
            .expect("settled counts")
            .total,
        0
    );

    store
        .delete_events_for_sources(std::slice::from_ref(&source.source_id))
        .expect("retire source events");
    let counts = store
        .pending_http_sync_summary_counts(target, "device")
        .expect("retirement counts");

    assert_eq!(counts.rollups, 0);
    assert_eq!(counts.passthrough_summaries, 0);
    assert_eq!(counts.retired_entities, 1);
    assert_eq!(
        counts.total, 1,
        "retirement-only reconciliation must surface as pending upload work"
    );
}

#[test]
fn pending_http_sync_summary_counts_match_default_http_passthrough_payloads() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "grok_build",
        "test",
        "0",
        Path::new("/tmp/grok-project-pending-sync-summary"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let target = "https://api.example.com/api/sync/batches";

    let mut summary = test_store_summary(&source, now, 70);
    summary.summary_id = summary_id(&source.provider, &source.source_id, "project-summary");
    summary.source.source_kind = SourceKind::LocalAdapter;
    summary.metadata.summary_format = "grok_build_session_summary".to_string();
    summary.period_start = Some(now);
    summary.period_end = Some(now);
    summary.project = Some(ProjectInfo {
        project_id: "project-repo-backed".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/repo".to_string()),
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/Users/example/work/ai-stats".to_string()),
    });
    summary.privacy.contains_file_paths = true;
    store.upsert_summary(&summary).expect("summary");

    store
        .record_summaries_synced(
            "http",
            target,
            &[sanitize_summary_for_default_http_sync(summary.clone())],
        )
        .expect("record synced");

    let counts = store
        .pending_http_sync_summary_counts(target, "device")
        .expect("pending counts after sync");
    assert_eq!(counts.total, 0);
}

#[test]
fn pending_http_sync_summary_counts_with_projects_detect_opt_in_backfill() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "grok_build",
        "test",
        "0",
        Path::new("/tmp/grok-project-opt-in-pending-sync-summary"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let target = "https://api.example.com/api/sync/batches";

    let mut summary = test_store_summary(&source, now, 70);
    summary.summary_id = summary_id(&source.provider, &source.source_id, "project-summary");
    summary.source.source_kind = SourceKind::LocalAdapter;
    summary.metadata.summary_format = "grok_build_session_summary".to_string();
    summary.period_start = Some(now);
    summary.period_end = Some(now);
    summary.project = Some(ProjectInfo {
        project_id: "project-repo-backed".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/repo".to_string()),
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/Users/example/work/ai-stats".to_string()),
    });
    summary.privacy.contains_file_paths = true;
    store.upsert_summary(&summary).expect("summary");

    store
        .record_summaries_synced(
            "http",
            target,
            &[sanitize_summary_for_default_http_sync(summary.clone())],
        )
        .expect("record synced");

    assert_eq!(
        store
            .pending_http_sync_summary_counts(target, "device")
            .expect("default payload counts")
            .total,
        0
    );
    assert_eq!(
        store
            .pending_http_sync_summary_counts_with_projects(target, "device", true)
            .expect("project payload counts")
            .total,
        1
    );
}

#[test]
fn pending_http_sync_summary_counts_include_code_change_only_uploads() {
    let store = Store::in_memory().expect("store");
    let target = "https://api.example.com/api/sync/batches";
    let metric = CodeChangeMetric {
        schema_version: statsai_core::CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "pending-code-change".to_string(),
        device_id: "device".to_string(),
        day: Utc::now().date_naive(),
        project_id: Some("project".to_string()),
        repository_hash: Some("repository".to_string()),
        commit_hash: None,
        kind: statsai_core::CodeChangeMetricKind::AgentEdit,
        counts: statsai_core::CodeLineCounts::classified(statsai_core::CodeCategory::Source, 3, 1),
        attribution_confidence: None,
        trace_coverage: statsai_core::CoverageStatus::Complete,
        git_coverage: statsai_core::CoverageStatus::Complete,
    };
    store
        .ingest_code_change_metrics_inner(std::slice::from_ref(&metric))
        .expect("store metric");
    let mut peer_metric = metric.clone();
    peer_metric.metric_id = "peer-code-change".to_string();
    peer_metric.device_id = "peer-device".to_string();
    store
        .ingest_code_change_metrics_inner(std::slice::from_ref(&peer_metric))
        .expect("store peer metric");

    let counts = store
        .pending_http_sync_summary_counts_with_projects(target, "device", false)
        .expect("pending counts");

    assert_eq!(counts.total, 1);
    assert_eq!(counts.days, 1);

    let sanitized = sanitize_code_change_metric_for_sync(metric.clone(), false);
    store
        .record_code_change_metrics_synced("http", target, &[sanitized])
        .expect("record sanitized metric synced");
    assert_eq!(
        store
            .pending_http_sync_summary_counts_with_projects(target, "device", false)
            .expect("settled default counts")
            .total,
        0
    );
    assert_eq!(
        store
            .pending_http_sync_summary_counts_with_projects(target, "device", true)
            .expect("project backfill counts")
            .total,
        1
    );
}

#[test]
fn pending_http_sync_summary_counts_include_account_plan_and_evidence_only_uploads() {
    let store = Store::in_memory().expect("store");
    let target = "https://api.example.com/api/sync/batches";
    let observed_at = Utc
        .with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
        .single()
        .expect("observed at");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-pending-account-evidence"),
        LocationOrigin::Configured,
    );
    let account_id = ProviderAccountId("pending-account".to_string());
    store.upsert_source(&source).expect("source");
    store
        .upsert_account_identity_observations(&[statsai_core::AccountIdentityObservationV1 {
            schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: "pending-identity".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: Some(account_id.clone()),
            provider_user_id_hash: Some("provider-id-hash".to_string()),
            email_hash: None,
            conversation_id_hash: None,
            turn_id_hash: None,
            observed_at,
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence: Confidence::High,
            auth_mode: Some("chatgpt".to_string()),
            application_version: None,
            parser_version: "test.v1".to_string(),
            artifact_kind: "auth_json".to_string(),
            artifact_path_hash: "path-hash".to_string(),
            record_fingerprint: "identity-fingerprint".to_string(),
        }])
        .expect("identity evidence");
    store
        .upsert_account_plan_observations(&[statsai_core::AccountPlanObservationV1 {
            schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: "pending-plan".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id,
            provider_account_id: Some(account_id),
            raw_plan_name: "plus".to_string(),
            plan_name: "Plus".to_string(),
            observed_at,
            active_from: None,
            active_until: None,
            is_current_snapshot: true,
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence: Confidence::High,
            parser_version: "test.v1".to_string(),
            artifact_path_hash: "path-hash".to_string(),
            record_fingerprint: "plan-fingerprint".to_string(),
        }])
        .expect("plan evidence");

    let plans = store
        .account_plan_projections("device")
        .expect("plan projections");
    let evidence = store
        .account_evidence_summaries("device")
        .expect("evidence summaries");
    assert_eq!(plans.len(), 1);
    assert_eq!(evidence.len(), 1);
    assert_eq!(
        store
            .pending_http_sync_summary_counts(target, "device")
            .expect("pending counts")
            .total,
        2
    );

    store
        .record_account_plan_projections_synced("http", target, &plans)
        .expect("record plan synced");
    store
        .record_account_evidence_summaries_synced("http", target, &evidence)
        .expect("record evidence synced");
    assert_eq!(
        store
            .pending_http_sync_summary_counts(target, "device")
            .expect("settled counts")
            .total,
        0
    );
}

#[test]
fn one_plan_fact_seen_by_two_sources_projects_once() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = Store::open(&directory.path().join("store.sqlite")).expect("open store");
    let observed_at = Utc::now();
    let account_id = statsai_core::ProviderAccountId("account-shared".to_string());

    // The same account on the same plan at the same instant, seen by two installs.
    // A projection is per device and drops `source_id`, so these are one fact.
    let observation =
        |observation_id: &str, source_id: &str| statsai_core::AccountPlanObservationV1 {
            schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: observation_id.to_string(),
            provider: "claude_code".to_string(),
            source_id: statsai_core::SourceId(source_id.to_string()),
            provider_account_id: Some(account_id.clone()),
            raw_plan_name: "claude_pro".to_string(),
            plan_name: "Pro".to_string(),
            observed_at,
            active_from: None,
            active_until: None,
            is_current_snapshot: true,
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence: Confidence::Medium,
            parser_version: "test.v1".to_string(),
            artifact_path_hash: "path-hash".to_string(),
            record_fingerprint: "plan-fingerprint".to_string(),
        };
    store
        .upsert_account_plan_observations(&[
            observation("plan-source-a", "source-a"),
            observation("plan-source-b", "source-b"),
        ])
        .expect("plan evidence from two sources");

    let plans = store
        .account_plan_projections("device")
        .expect("plan projections");

    // Sending both copies makes the whole batch invalid at the hosted mirror, which
    // rejects a repeated `projection_id` and fails every chunk behind it.
    assert_eq!(plans.len(), 1, "one plan fact must project exactly once");
}

#[test]
fn the_strongest_evidence_wins_a_projection_collision() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = Store::open(&directory.path().join("store.sqlite")).expect("open store");
    let observed_at = Utc::now();
    let account_id = statsai_core::ProviderAccountId("account-shared".to_string());

    // `projection_id` is derived from a fingerprint that omits `confidence` and
    // compares plan names case-insensitively, so these two collide -- while carrying
    // different payloads. Whichever row the query returned first must not decide
    // which one reaches the mirror.
    let observation =
        |observation_id: &str, source_id: &str, raw_plan_name: &str, confidence: Confidence| {
            statsai_core::AccountPlanObservationV1 {
                schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
                observation_id: observation_id.to_string(),
                provider: "claude_code".to_string(),
                source_id: statsai_core::SourceId(source_id.to_string()),
                provider_account_id: Some(account_id.clone()),
                raw_plan_name: raw_plan_name.to_string(),
                plan_name: "Pro".to_string(),
                observed_at,
                active_from: None,
                active_until: None,
                is_current_snapshot: true,
                evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
                confidence,
                parser_version: "test.v1".to_string(),
                artifact_path_hash: "path-hash".to_string(),
                record_fingerprint: "plan-fingerprint".to_string(),
            }
        };
    store
        .upsert_account_plan_observations(&[
            observation("plan-weak", "source-a", "CLAUDE_PRO", Confidence::Low),
            observation("plan-strong", "source-b", "claude_pro", Confidence::High),
        ])
        .expect("plan evidence of differing strength");

    let plans = store
        .account_plan_projections("device")
        .expect("plan projections");

    assert_eq!(plans.len(), 1, "the collision still yields one projection");
    assert_eq!(
        plans[0].confidence,
        Confidence::High,
        "the stronger evidence must survive the collision"
    );
}

fn activity_test_source() -> statsai_core::SourceLocation {
    statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-activity-pending"),
        LocationOrigin::Configured,
    )
}

fn activity_rollup(
    source: &statsai_core::SourceLocation,
    rollup_id: &str,
    calls: u64,
) -> statsai_core::ActivityRollupV1 {
    let seen = Utc
        .with_ymd_and_hms(2026, 1, 1, 9, 0, 0)
        .single()
        .expect("timestamp");
    statsai_core::ActivityRollupV1 {
        schema_version: statsai_core::ACTIVITY_ROLLUP_SCHEMA_VERSION.to_string(),
        rollup_id: rollup_id.to_string(),
        device_id: "device".to_string(),
        source_id: source.source_id.clone(),
        provider: "codex".to_string(),
        provider_account_id: None,
        day: "2026-01-01".to_string(),
        kind: statsai_core::ActivityKind::Tool,
        entity_key: "shell".to_string(),
        display_name: "shell".to_string(),
        family: statsai_core::ActivityFamily::Shell,
        mcp_server: None,
        mcp_tool: None,
        plugin: None,
        skill_catalog: None,
        model: Some("gpt-5.4".to_string()),
        calls,
        succeeded: calls,
        failed: 0,
        unknown: 0,
        duration_samples: calls,
        duration_sum_ms: calls * 10,
        duration_max_ms: Some(10),
        duration_kind: Some(statsai_core::ActivityDurationKind::Reported),
        first_seen: seen,
        last_seen: seen,
        evidence: "codex-native-items".to_string(),
    }
}

fn activity_coverage_row(
    source: &statsai_core::SourceLocation,
    coverage_id: &str,
    level: statsai_core::ActivityCoverageLevel,
) -> statsai_core::ActivityCoverageV1 {
    statsai_core::ActivityCoverageV1 {
        schema_version: statsai_core::ACTIVITY_COVERAGE_SCHEMA_VERSION.to_string(),
        coverage_id: coverage_id.to_string(),
        device_id: "device".to_string(),
        source_id: source.source_id.clone(),
        provider: "codex".to_string(),
        day: "2026-01-01".to_string(),
        day_end: "2026-01-01".to_string(),
        kind: statsai_core::ActivityKind::Tool,
        level,
        evidence: "codex-native-items".to_string(),
        parser_revision: statsai_core::ACTIVITY_PARSER_REVISION.to_string(),
    }
}

/// The bug this replaced: `dirty` is one global flag, so acknowledging a rollup
/// at the first target hid it from every other target permanently.
#[test]
fn activity_rollups_stay_pending_for_targets_that_never_acknowledged_them() {
    let store = Store::in_memory().expect("store");
    let source = activity_test_source();
    let first = "http://127.0.0.1:8787/api/sync/batches";
    let second = "https://dev-api.example.com/api/sync/batches";
    let rollups = vec![activity_rollup(&source, "rollup-a", 3)];

    assert_eq!(
        store
            .pending_activity_rollups_for_sync("http", first, &rollups)
            .expect("pending first")
            .len(),
        1
    );

    store
        .record_activity_rollups_synced("http", first, &rollups)
        .expect("record first");

    assert!(store
        .pending_activity_rollups_for_sync("http", first, &rollups)
        .expect("pending first after")
        .is_empty());
    assert_eq!(
        store
            .pending_activity_rollups_for_sync("http", second, &rollups)
            .expect("pending second")
            .len(),
        1,
        "a second target must still receive rollups it never acknowledged"
    );
}

#[test]
fn activity_coverage_stays_pending_for_targets_that_never_acknowledged_it() {
    let store = Store::in_memory().expect("store");
    let source = activity_test_source();
    let first = "http://127.0.0.1:8787/api/sync/batches";
    let second = "https://dev-api.example.com/api/sync/batches";
    let coverage = vec![activity_coverage_row(
        &source,
        "coverage-a",
        statsai_core::ActivityCoverageLevel::Complete,
    )];

    store
        .record_activity_coverage_synced("http", first, &coverage)
        .expect("record first");

    assert!(store
        .pending_activity_coverage_for_sync("http", first, &coverage)
        .expect("pending first after")
        .is_empty());
    assert_eq!(
        store
            .pending_activity_coverage_for_sync("http", second, &coverage)
            .expect("pending second")
            .len(),
        1
    );
}

#[test]
fn activity_rollups_resend_when_the_payload_changed_since_acknowledgement() {
    let store = Store::in_memory().expect("store");
    let source = activity_test_source();
    let target = "https://dev-api.example.com/api/sync/batches";
    let acknowledged = vec![activity_rollup(&source, "rollup-a", 3)];

    store
        .record_activity_rollups_synced("http", target, &acknowledged)
        .expect("record");

    let rescanned = vec![activity_rollup(&source, "rollup-a", 9)];
    let pending = store
        .pending_activity_rollups_for_sync("http", target, &rescanned)
        .expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].calls, 9);
}

/// An upload that never lands must not consume the row. Acknowledgement is
/// written only on batch success, so an interrupted send leaves it pending, and
/// a rescan during the interrupted send is still picked up afterwards.
#[test]
fn interrupted_activity_upload_leaves_rollups_pending() {
    let store = Store::in_memory().expect("store");
    let source = activity_test_source();
    let target = "https://dev-api.example.com/api/sync/batches";
    let in_flight = vec![activity_rollup(&source, "rollup-a", 3)];

    let pending = store
        .pending_activity_rollups_for_sync("http", target, &in_flight)
        .expect("pending");
    assert_eq!(pending.len(), 1, "nothing acknowledged yet");

    // The send fails: no acknowledgement is recorded.
    let retried = store
        .pending_activity_rollups_for_sync("http", target, &in_flight)
        .expect("pending after failure");
    assert_eq!(retried.len(), 1, "a failed upload must not consume the row");

    // A rescan changes the payload while the older one was in flight; the late
    // acknowledgement for the older payload must not mask the newer one.
    store
        .record_activity_rollups_synced("http", target, &in_flight)
        .expect("late ack");
    let rescanned = vec![activity_rollup(&source, "rollup-a", 4)];
    assert_eq!(
        store
            .pending_activity_rollups_for_sync("http", target, &rescanned)
            .expect("pending after rescan")
            .len(),
        1
    );
}

/// Disabling activity sends an empty authoritative snapshot, which retires the
/// target's activity acknowledgements. Re-enabling must therefore backfill
/// without any local flag being flipped.
#[test]
fn disabling_activity_retires_acknowledgements_so_re_enabling_backfills() {
    let store = Store::in_memory().expect("store");
    let source = activity_test_source();
    let target = "https://dev-api.example.com/api/sync/batches";
    let rollups = vec![activity_rollup(&source, "rollup-a", 3)];
    let coverage = vec![activity_coverage_row(
        &source,
        "coverage-a",
        statsai_core::ActivityCoverageLevel::Complete,
    )];

    store
        .record_activity_rollups_synced("http", target, &rollups)
        .expect("record rollups");
    store
        .record_activity_coverage_synced("http", target, &coverage)
        .expect("record coverage");
    assert!(store
        .pending_activity_rollups_for_sync("http", target, &rollups)
        .expect("pending")
        .is_empty());

    // Activity disabled: the snapshot carries no activity ids.
    let disabled_snapshot = statsai_core::SyncAuthoritativeSnapshot {
        snapshot_id: "batch-disabled_authoritative".to_string(),
        part_index: 0,
        part_count: 1,
        ..Default::default()
    };
    assert!(store
        .sync_target_has_retired_entities("http", target, &disabled_snapshot)
        .expect("has retired"));
    store
        .reconcile_sync_tracking_to_authoritative_snapshot("http", target, &disabled_snapshot)
        .expect("reconcile");

    assert_eq!(
        store
            .pending_activity_rollups_for_sync("http", target, &rollups)
            .expect("pending after re-enable")
            .len(),
        1,
        "re-enabling activity must resend rows the remote pruned"
    );
    assert_eq!(
        store
            .pending_activity_coverage_for_sync("http", target, &coverage)
            .expect("coverage after re-enable")
            .len(),
        1
    );
}
