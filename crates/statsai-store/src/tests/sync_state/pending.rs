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
