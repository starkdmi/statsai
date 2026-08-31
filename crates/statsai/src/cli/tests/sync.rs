use super::support::*;
use super::*;

#[test]
fn sync_sanitization_removes_record_level_evidence() {
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/.codex"),
        LocationOrigin::Configured,
    );
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("now");
    let mut event = test_event("codex", &source, now, None, TokenParts::total(100));
    event.source.source_record_id = Some("/tmp/.codex/sessions/log.jsonl:12".to_string());
    event.project = Some(ProjectInfo {
        project_id: "project-event-path-only".to_string(),
        project_label: Some("hi".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("event-path-hash".to_string()),
        path_label: Some("/Users/example/Documents/Codex/2026-05-29/hi".to_string()),
    });
    event.parse_evidence = Some(ParseEvidence {
        event_key_version: "test.v1".to_string(),
        source_file_path_hash: Some("hash".to_string()),
        source_line_number: Some(12),
        source_record_id: Some("/tmp/.codex/sessions/log.jsonl:12".to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::ManualHint,
    });

    let mut summary = test_summary("codex", &source, now, 100, None);
    summary.source.source_record_id = Some("reported_jul11.json:daily:2025-07-11".to_string());
    summary.parse_evidence = event.parse_evidence.clone();
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

    let event = sanitize_event_for_sync(event);
    let summary = sanitize_summary_for_sync(summary);

    assert!(event.source.source_record_id.is_none());
    let event_evidence = event.parse_evidence.expect("event evidence");
    assert!(event_evidence.source_record_id.is_none());
    assert!(event_evidence.source_line_number.is_none());
    assert_eq!(
        event_evidence.source_file_path_hash.as_deref(),
        Some("hash")
    );
    let event_project = event.project.expect("path-only event project");
    assert_eq!(
        event_project.path_label.as_deref(),
        Some("/Users/example/Documents/Codex/2026-05-29/hi")
    );
    assert!(event.privacy.contains_file_paths);

    assert!(summary.source.source_record_id.is_none());
    let summary_evidence = summary.parse_evidence.expect("summary evidence");
    assert!(summary_evidence.source_record_id.is_none());
    assert!(summary_evidence.source_line_number.is_none());
    assert_eq!(
        summary_evidence.source_file_path_hash.as_deref(),
        Some("hash")
    );
    let project = summary.project.expect("repo-backed project");
    assert_eq!(project.repo_remote_hash.as_deref(), Some("repo-hash"));
    assert_eq!(project.repo_label.as_deref(), Some("owner/repo"));
    assert_eq!(project.path_hash.as_deref(), Some("path-hash"));
    assert_eq!(
        project.path_label.as_deref(),
        Some("/Users/example/work/ai-stats")
    );
    assert!(summary.privacy.contains_file_paths);
}

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
fn http_sync_uses_configured_or_default_api_endpoint() {
    let previous = std::env::var("STATSAI_API_URL").ok();
    std::env::set_var("STATSAI_API_URL", "https://sync.example.com");
    let endpoint = http_sync_endpoint(&test_sync_command("http")).expect("http endpoint");
    if let Some(value) = previous {
        std::env::set_var("STATSAI_API_URL", value);
    } else {
        std::env::remove_var("STATSAI_API_URL");
    }

    assert_eq!(endpoint, "https://sync.example.com/api/sync/batches");
}

#[test]
fn http_sync_builds_rollup_batches_without_raw_events() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollups"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let event = test_event(
        "codex",
        &source,
        Utc::now(),
        Some(provider_account_id("codex", "personal")),
        TokenParts {
            input: 10,
            output: 5,
            cached_input: 0,
            reasoning: 0,
            total: 15,
            cost: Some(10),
        },
    );
    store.insert_event(&event).expect("event");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert!(!batch.summaries.is_empty());
    assert!(batch.summaries.iter().all(is_daily_rollup_summary));
}

#[test]
fn http_sync_excludes_non_daily_stats_cache_summaries_from_rollup_batches() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-rollup-stats-cache"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let now = Utc
        .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
        .single()
        .expect("now");
    let start = Utc
        .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
        .single()
        .expect("start");
    let mut summary = test_summary("claude_code", &source, now, 500, None);
    summary.metadata.summary_format = "claude_stats_cache".to_string();
    summary.period_start = Some(start);
    summary.period_end = Some(now);
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert!(batch.summaries.is_empty());
}

#[test]
fn http_sync_keeps_grok_build_summary_only_sessions_in_rollup_batches() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "grok_build",
        "test",
        "0",
        Path::new("/tmp/grok-build-http-rollup"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let now = Utc
        .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
        .single()
        .expect("now");
    let start = Utc
        .with_ymd_and_hms(2026, 5, 13, 8, 0, 0)
        .single()
        .expect("start");
    let mut summary = test_summary("grok_build", &source, now, 500, None);
    summary.source.source_kind = SourceKind::LocalAdapter;
    summary.source.source_type = "build-session.json".to_string();
    summary.metadata.summary_format = "grok_build_session_summary".to_string();
    summary.period_start = Some(start);
    summary.period_end = Some(now);
    summary.summary_id = summary_id("grok_build", &source.source_id, "session-summary");
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert_eq!(
        batch.summaries[0].metadata.summary_format,
        "grok_build_session_summary"
    );
}

#[test]
fn http_sync_excludes_multi_day_external_daily_summaries_from_rollup_batches() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-rollup-external-multi-day"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let start = Utc
        .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
        .single()
        .expect("start");
    let end = Utc
        .with_ymd_and_hms(2026, 5, 14, 23, 59, 59)
        .single()
        .expect("end");
    let mut summary = test_summary("claude_code", &source, end, 500, None);
    summary.source.source_kind = SourceKind::ExternalReport;
    summary.metadata.summary_format = "external_daily".to_string();
    summary.period_start = Some(start);
    summary.period_end = Some(end);
    summary.summary_id = summary_id("claude_code", &source.source_id, "external-multi-day");
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert!(batch.summaries.is_empty());
}

#[test]
fn http_sync_keeps_one_day_external_daily_summaries_in_rollup_batches() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-rollup-external-daily"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let now = Utc
        .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
        .single()
        .expect("now");
    let start = Utc
        .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
        .single()
        .expect("start");
    let mut summary = test_summary("claude_code", &source, now, 500, None);
    summary.source.source_kind = SourceKind::ExternalReport;
    summary.metadata.summary_format = "external_daily".to_string();
    summary.period_start = Some(start);
    summary.period_end = Some(now);
    summary.summary_id = summary_id("claude_code", &source.source_id, "external-daily");
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert_eq!(batch.summaries[0].metadata.summary_format, "external_daily");
}

#[test]
fn http_sync_keeps_offset_local_day_external_daily_summaries_in_rollup_batches() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-rollup-external-offset-daily"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let start = Utc
        .with_ymd_and_hms(2026, 5, 13, 7, 0, 0)
        .single()
        .expect("start");
    let end = Utc
        .with_ymd_and_hms(2026, 5, 14, 6, 59, 59)
        .single()
        .expect("end");
    let mut summary = test_summary("claude_code", &source, end, 500, None);
    summary.source.source_kind = SourceKind::ExternalReport;
    summary.metadata.summary_format = "external_daily".to_string();
    summary.period_start = Some(start);
    summary.period_end = Some(end);
    summary.summary_id = summary_id("claude_code", &source.source_id, "external-offset-daily");
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert_eq!(batch.summaries[0].metadata.summary_format, "external_daily");
}

#[test]
fn http_sync_keeps_dst_fallback_external_daily_summaries_in_rollup_batches() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-rollup-external-dst-fallback-daily"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let start = Utc
        .with_ymd_and_hms(2026, 11, 1, 7, 0, 0)
        .single()
        .expect("start");
    let end = Utc
        .with_ymd_and_hms(2026, 11, 2, 7, 59, 59)
        .single()
        .expect("end");
    let mut summary = test_summary("claude_code", &source, end, 500, None);
    summary.source.source_kind = SourceKind::ExternalReport;
    summary.metadata.summary_format = "external_daily".to_string();
    summary.period_start = Some(start);
    summary.period_end = Some(end);
    summary.summary_id = summary_id(
        "claude_code",
        &source.source_id,
        "external-dst-fallback-daily",
    );
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert_eq!(batch.summaries[0].metadata.summary_format, "external_daily");
}

#[test]
fn http_sync_keeps_one_day_manual_daily_summaries_in_rollup_batches() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-rollup-manual-daily"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let now = Utc
        .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
        .single()
        .expect("now");
    let start = Utc
        .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
        .single()
        .expect("start");
    let mut summary = test_summary("claude_code", &source, now, 500, None);
    summary.source.source_kind = SourceKind::Manual;
    summary.metadata.summary_format = "manual_daily".to_string();
    summary.period_start = Some(start);
    summary.period_end = Some(now);
    summary.summary_id = summary_id("claude_code", &source.source_id, "manual-daily");
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
}

#[test]
fn http_sync_keeps_one_day_manual_daily_summaries_without_period_end() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-rollup-manual-daily-missing-end"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let start = Utc
        .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
        .single()
        .expect("start");
    let observed_at = Utc
        .with_ymd_and_hms(2026, 5, 16, 12, 0, 0)
        .single()
        .expect("observed_at");
    let mut summary = test_summary("claude_code", &source, observed_at, 500, None);
    summary.source.source_kind = SourceKind::Manual;
    summary.metadata.summary_format = "manual_daily".to_string();
    summary.period_start = Some(start);
    summary.period_end = None;
    summary.observed_at = observed_at;
    summary.summary_id = summary_id("claude_code", &source.source_id, "manual-daily-missing-end");
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
}

#[test]
fn http_sync_keeps_one_day_manual_daily_summaries_without_period_start() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-rollup-manual-daily-missing-start"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let period_end = Utc
        .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
        .single()
        .expect("period_end");
    let observed_at = Utc
        .with_ymd_and_hms(2026, 5, 16, 12, 0, 0)
        .single()
        .expect("observed_at");
    let mut summary = test_summary("claude_code", &source, observed_at, 500, None);
    summary.source.source_kind = SourceKind::Manual;
    summary.metadata.summary_format = "manual_daily".to_string();
    summary.period_start = None;
    summary.period_end = Some(period_end);
    summary.observed_at = observed_at;
    summary.summary_id = summary_id(
        "claude_code",
        &source.source_id,
        "manual-daily-missing-start",
    );
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
}

#[test]
fn http_sync_keeps_one_day_manual_daily_summaries_without_period_bounds() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-rollup-manual-daily-missing-bounds"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let observed_at = Utc
        .with_ymd_and_hms(2026, 5, 13, 12, 0, 0)
        .single()
        .expect("observed_at");
    let mut summary = test_summary("claude_code", &source, observed_at, 500, None);
    summary.source.source_kind = SourceKind::Manual;
    summary.metadata.summary_format = "manual_daily".to_string();
    summary.period_start = None;
    summary.period_end = None;
    summary.observed_at = observed_at;
    summary.summary_id = summary_id(
        "claude_code",
        &source.source_id,
        "manual-daily-missing-bounds",
    );
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
}

#[test]
fn http_sync_keeps_legacy_ccusage_daily_summaries_in_rollup_batches() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-rollup-ccusage-daily"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let now = Utc
        .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
        .single()
        .expect("now");
    let start = Utc
        .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
        .single()
        .expect("start");
    let mut summary = test_summary("claude_code", &source, now, 500, None);
    summary.source.source_kind = SourceKind::Manual;
    summary.metadata.summary_format = "ccusage_daily".to_string();
    summary.period_start = Some(start);
    summary.period_end = Some(now);
    summary.summary_id = summary_id("claude_code", &source.source_id, "ccusage-daily");
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert_eq!(batch.summaries[0].metadata.summary_format, "ccusage_daily");
}

#[test]
fn http_sync_keeps_exact_manual_period_summaries_in_rollup_batches() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-rollup-manual-period"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let start = Utc
        .with_ymd_and_hms(2025, 9, 4, 0, 0, 0)
        .single()
        .expect("start");
    let end = Utc
        .with_ymd_and_hms(2025, 9, 9, 23, 59, 59)
        .single()
        .expect("end");
    let mut summary = test_summary("claude_code", &source, end, 500, None);
    summary.source.source_kind = SourceKind::Manual;
    summary.metadata.summary_format = "manual_period_summary".to_string();
    summary.period_start = Some(start);
    summary.period_end = Some(end);
    summary.summary_id = summary_id("claude_code", &source.source_id, "manual-period");
    store.upsert_summary(&summary).expect("summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert_eq!(
        batch.summaries[0].metadata.summary_format,
        "manual_period_summary"
    );
}

#[test]
fn first_http_incremental_sync_sends_full_rollup_history_for_new_target() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-first-sync"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let event = test_event(
        "codex",
        &source,
        Utc::now(),
        Some(provider_account_id("codex", "personal")),
        TokenParts {
            input: 10,
            output: 5,
            cached_input: 0,
            reasoning: 0,
            total: 15,
            cost: Some(10),
        },
    );
    store.insert_event(&event).expect("event");
    store.rebuild_sync_rollups().expect("rebuild");

    let existing_rollups = store
        .all_sync_rollup_summaries()
        .expect("all rollups for new target");
    assert_eq!(existing_rollups.len(), 1);

    store
        .mark_sync_rollups_synced(
            &existing_rollups
                .iter()
                .map(|summary| summary.summary_id.clone())
                .collect::<Vec<_>>(),
        )
        .expect("clear dirty flags");
    assert!(store
        .dirty_sync_rollup_summaries()
        .expect("dirty rollups")
        .is_empty());

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        since_last: true,
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert!(batch.events.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert!(is_daily_rollup_summary(&batch.summaries[0]));
}

#[test]
fn incremental_http_sync_includes_repriced_rollups_without_full() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-reprice-sync"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let started_at = Utc
        .with_ymd_and_hms(2026, 7, 29, 12, 0, 0)
        .single()
        .expect("started_at");
    let mut event = test_event(
        "codex",
        &source,
        started_at,
        None,
        TokenParts {
            input: 1_000_000,
            cached_input: 1_000_000,
            output: 1_000_000,
            reasoning: 0,
            total: 4_000_000,
            cost: None,
        },
    );
    event.model = Some(ModelInfo {
        name: Some("codex-auto-review".to_string()),
        normalized_name: Some("codex-auto-review".to_string()),
        provider_model_id: Some("codex-auto-review".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    store.insert_event(&event).expect("legacy unpriced event");
    store.rebuild_sync_rollups().expect("rebuild");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    assert!(!command.full);
    assert!(!command.rebuild_rollups);
    let target = sync_target(&command).expect("target");
    let (initial_batch, initial_mode) =
        build_sync_batch(&command, &store, "device", &target).expect("initial batch");
    assert_eq!(initial_mode, SyncPayloadMode::Rollups);
    assert_eq!(initial_batch.summaries.len(), 1);
    assert!(initial_batch.summaries[0]
        .cost
        .estimated_api_equivalent_usd
        .is_none());
    record_rollup_sync_success(&store, "http", &target, &initial_batch)
        .expect("record initial sync");

    let (repeat_batch, _) =
        build_sync_batch(&command, &store, "device", &target).expect("repeat batch");
    assert!(
        repeat_batch.summaries.is_empty(),
        "synced rollups must stay unpublished until pricing changes them"
    );

    let report = store.ensure_current_pricing().expect("automatic reprice");
    assert_eq!(report.changed_events, 1);
    assert_eq!(report.refreshed_rollups, 1);

    let (incremental_batch, incremental_mode) =
        build_sync_batch(&command, &store, "device", &target).expect("incremental batch");
    assert_eq!(incremental_mode, SyncPayloadMode::Rollups);
    assert_eq!(incremental_batch.summaries.len(), 1);
    assert!(is_daily_rollup_summary(&incremental_batch.summaries[0]));
    assert_eq!(
        incremental_batch.summaries[0]
            .cost
            .estimated_api_equivalent_usd,
        store
            .events()
            .expect("repriced events")
            .into_iter()
            .next()
            .expect("one event")
            .cost
            .estimated_api_equivalent_usd
    );
    assert!(incremental_batch.summaries[0]
        .cost
        .estimated_api_equivalent_usd
        .is_some());
}

#[test]
fn incremental_http_sync_includes_repriced_passthrough_summaries_without_full() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-reprice-passthrough"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let start = Utc
        .with_ymd_and_hms(2026, 7, 28, 0, 0, 0)
        .single()
        .expect("start");
    let end = Utc
        .with_ymd_and_hms(2026, 7, 29, 23, 59, 59)
        .single()
        .expect("end");
    let mut summary = test_summary("codex", &source, end, 4_000_000, None);
    summary.source.source_kind = SourceKind::LocalAdapter;
    summary.source.source_type = "build-session.json".to_string();
    summary.metadata.summary_format = "grok_build_session_summary".to_string();
    summary.period_start = Some(start);
    summary.period_end = Some(end);
    summary.model = Some(ModelInfo {
        name: Some("codex-auto-review".to_string()),
        normalized_name: Some("codex-auto-review".to_string()),
        provider_model_id: Some("codex-auto-review".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    summary.usage = UsageCounts {
        input_tokens: Some(1_000_000),
        cache_creation_tokens: Some(1_000_000),
        cache_read_tokens: Some(1_000_000),
        output_tokens: Some(1_000_000),
        total_tokens: Some(4_000_000),
        ..UsageCounts::default()
    };
    store.upsert_summary(&summary).expect("passthrough summary");

    let command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    assert!(!command.full);
    let target = sync_target(&command).expect("target");
    let (initial_batch, _) =
        build_sync_batch(&command, &store, "device", &target).expect("initial batch");
    assert_eq!(initial_batch.summaries.len(), 1);
    assert!(!is_daily_rollup_summary(&initial_batch.summaries[0]));
    assert!(initial_batch.summaries[0]
        .cost
        .estimated_api_equivalent_usd
        .is_none());
    record_rollup_sync_success(&store, "http", &target, &initial_batch)
        .expect("record initial sync");

    let (repeat_batch, _) =
        build_sync_batch(&command, &store, "device", &target).expect("repeat batch");
    assert!(repeat_batch.summaries.is_empty());

    let report = store.ensure_current_pricing().expect("automatic reprice");
    assert_eq!(report.changed_summaries, 1);

    let (incremental_batch, incremental_mode) =
        build_sync_batch(&command, &store, "device", &target).expect("incremental batch");
    assert_eq!(incremental_mode, SyncPayloadMode::Rollups);
    assert_eq!(incremental_batch.summaries.len(), 1);
    assert!(!is_daily_rollup_summary(&incremental_batch.summaries[0]));
    assert!(incremental_batch.summaries[0]
        .cost
        .estimated_api_equivalent_usd
        .is_some());
}

#[test]
fn http_incremental_rollups_are_tracked_per_target() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollup-targets"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let account_id = provider_account_id("codex", "personal");
    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("started_at");
    let first = test_event(
        "codex",
        &source,
        started_at,
        Some(account_id.clone()),
        TokenParts {
            input: 10,
            output: 5,
            cached_input: 0,
            reasoning: 0,
            total: 15,
            cost: Some(10),
        },
    );
    store.insert_event(&first).expect("first event");
    store.rebuild_sync_rollups().expect("rebuild");

    let mut passthrough = test_summary(
        "grok_build",
        &source,
        started_at + Duration::minutes(30),
        70,
        Some(account_id.clone()),
    );
    passthrough.summary_id = summary_id("grok_build", &source.source_id, "session-summary");
    passthrough.source.source_kind = SourceKind::LocalAdapter;
    passthrough.source.source_type = "build-session.json".to_string();
    passthrough.metadata.summary_format = "grok_build_session_summary".to_string();
    passthrough.period_start = Some(started_at);
    passthrough.period_end = Some(started_at + Duration::minutes(30));
    store
        .upsert_summary(&passthrough)
        .expect("passthrough summary");

    let local_command = SyncCommand {
        endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let local_target = sync_target(&local_command).expect("local target");
    let (local_batch, local_mode) =
        build_sync_batch(&local_command, &store, "device", &local_target)
            .expect("local initial batch");
    assert_eq!(local_mode, SyncPayloadMode::Rollups);
    assert_eq!(local_batch.summaries.len(), 2);
    assert!(local_batch.summaries.iter().any(is_daily_rollup_summary));
    assert!(local_batch
        .summaries
        .iter()
        .any(|summary| summary.metadata.summary_format == "grok_build_session_summary"));
    assert!(local_batch.authoritative_snapshot.is_some());
    record_rollup_sync_success(&store, "http", &local_target, &local_batch)
        .expect("record local sync");

    let (local_repeat_batch, local_repeat_mode) =
        build_sync_batch(&local_command, &store, "device", &local_target)
            .expect("local repeat batch");
    assert_eq!(local_repeat_mode, SyncPayloadMode::Rollups);
    assert!(
        local_repeat_batch.summaries.is_empty(),
        "plain HTTP sync should be incremental after a target was synced"
    );
    assert!(local_repeat_batch.authoritative_snapshot.is_none());

    let local_full_command = SyncCommand {
        endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
        full: true,
        ..test_sync_command("http")
    };
    let (local_full_batch, local_full_mode) =
        build_sync_batch(&local_full_command, &store, "device", &local_target)
            .expect("local full batch");
    assert_eq!(local_full_mode, SyncPayloadMode::Rollups);
    assert_eq!(
        local_full_batch.summaries.len(),
        2,
        "--full should deliberately resend synced rollups and passthrough summaries"
    );
    assert!(local_full_batch
        .summaries
        .iter()
        .any(is_daily_rollup_summary));
    assert!(local_full_batch
        .summaries
        .iter()
        .any(|summary| summary.metadata.summary_format == "grok_build_session_summary"));
    assert!(local_full_batch.authoritative_snapshot.is_some());

    let local_incremental_command = SyncCommand {
        endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
        since_last: true,
        ..test_sync_command("http")
    };
    let (local_incremental_batch, _) =
        build_sync_batch(&local_incremental_command, &store, "device", &local_target)
            .expect("local incremental batch");
    assert!(local_incremental_batch.summaries.is_empty());
    assert!(local_incremental_batch.authoritative_snapshot.is_none());

    let second = test_event(
        "codex",
        &source,
        started_at + Duration::hours(1),
        Some(account_id),
        TokenParts {
            input: 20,
            output: 5,
            cached_input: 0,
            reasoning: 0,
            total: 25,
            cost: Some(20),
        },
    );
    store.insert_event(&second).expect("second event");
    assert_eq!(
        store
            .dirty_sync_rollup_summaries()
            .expect("dirty after second event")
            .len(),
        1
    );

    let remote_command = SyncCommand {
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let remote_target = sync_target(&remote_command).expect("remote target");
    let (remote_batch, remote_mode) =
        build_sync_batch(&remote_command, &store, "device", &remote_target).expect("remote batch");
    assert_eq!(remote_mode, SyncPayloadMode::Rollups);
    assert_eq!(remote_batch.summaries.len(), 2);
    assert!(remote_batch.summaries.iter().any(is_daily_rollup_summary));
    assert!(remote_batch
        .summaries
        .iter()
        .any(|summary| summary.metadata.summary_format == "grok_build_session_summary"));
    record_rollup_sync_success(&store, "http", &remote_target, &remote_batch)
        .expect("record remote sync");
    assert!(store
        .dirty_sync_rollup_summaries()
        .expect("dirty after remote sync")
        .is_empty());

    let (local_catchup_batch, local_catchup_mode) =
        build_sync_batch(&local_incremental_command, &store, "device", &local_target)
            .expect("local catchup batch");
    assert_eq!(local_catchup_mode, SyncPayloadMode::Rollups);
    assert_eq!(local_catchup_batch.summaries.len(), 1);
    assert_eq!(
        local_catchup_batch.summaries[0].usage.total_tokens,
        Some(40)
    );
}

#[test]
fn http_incremental_sync_sends_authoritative_snapshot_after_local_rollup_retirement() {
    let store = Store::in_memory().expect("store");
    let retired_source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-retired-rollup"),
        LocationOrigin::Configured,
    );
    let retained_source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-http-retained-rollup"),
        LocationOrigin::Configured,
    );
    store
        .upsert_source(&retired_source)
        .expect("retired source");
    store
        .upsert_source(&retained_source)
        .expect("retained source");

    let retired_event = test_event(
        "claude_code",
        &retired_source,
        Utc.with_ymd_and_hms(2026, 8, 2, 10, 0, 0)
            .single()
            .expect("started_at"),
        Some(provider_account_id("claude_code", "personal")),
        TokenParts {
            input: 10,
            output: 5,
            cached_input: 100,
            reasoning: 0,
            total: 115,
            cost: Some(10),
        },
    );
    let retained_event = test_event(
        "claude_code",
        &retained_source,
        Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0)
            .single()
            .expect("retained started_at"),
        Some(provider_account_id("claude_code", "personal")),
        TokenParts {
            input: 20,
            output: 10,
            cached_input: 200,
            reasoning: 0,
            total: 230,
            cost: Some(20),
        },
    );
    store.insert_event(&retired_event).expect("retired event");
    store.insert_event(&retained_event).expect("retained event");

    let command = SyncCommand {
        endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (initial_batch, _) =
        build_sync_batch(&command, &store, "device", &target).expect("initial batch");
    assert_eq!(initial_batch.summaries.len(), 2);
    assert!(initial_batch.authoritative_snapshot.is_some());
    record_rollup_sync_success(&store, "http", &target, &initial_batch)
        .expect("record initial sync");

    store
        .delete_events_for_sources(std::slice::from_ref(&retired_source.source_id))
        .expect("retire source events");
    assert_eq!(
        store
            .all_sync_rollup_summaries()
            .expect("remaining rollups")
            .len(),
        1
    );

    let (retirement_batch, _) =
        build_sync_batch(&command, &store, "device", &target).expect("retirement batch");
    assert!(
        retirement_batch.summaries.is_empty(),
        "retirement-only reconciliation must not resend unchanged historical rollups"
    );
    assert!(
        retirement_batch.authoritative_snapshot.is_some(),
        "removing a previously synced rollup must send a server deletion signal"
    );
    record_rollup_sync_success(&store, "http", &target, &retirement_batch)
        .expect("record retirement sync");

    let (settled_batch, _) =
        build_sync_batch(&command, &store, "device", &target).expect("settled batch");
    assert!(settled_batch.summaries.is_empty());
    assert!(
        settled_batch.authoritative_snapshot.is_none(),
        "successful reconciliation must clear retired local sync tracking"
    );
}

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
fn http_rollup_sync_retries_smaller_batches_after_budget_rejection() {
    let store = Store::in_memory().expect("store");
    let endpoint = "https://api.example.com/api/sync/batches".to_string();
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollup-retry"),
        LocationOrigin::Configured,
    );
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let summaries: Vec<_> = (0..4)
        .map(|index| {
            let mut summary = test_summary(
                "codex",
                &source,
                now + Duration::days(index as i64),
                10,
                None,
            );
            summary.summary_id = statsai_core::SummaryId(format!("summary-{index}"));
            summary.metadata.summary_format = "daily_rollup.v1".to_string();
            sanitize_summary_for_sync(summary)
        })
        .collect();
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_retry".to_string(),
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
    let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_for_send = Arc::clone(&observed);

    send_http_rollup_chunk_with_retry_using(&batch, &|chunk| {
            observed_for_send
                .lock()
                .expect("observed lock")
                .push((chunk.batch_id.clone(), chunk.summaries.len()));
            if chunk.summaries.len() > 2 {
                return Err(anyhow::Error::msg(
                    r#"sync endpoint returned HTTP 413: {"error":"sync_batch_d1_query_budget_exceeded","estimatedQueries":53,"maxQueries":45}"#,
                ));
            }
            record_rollup_sync_chunk_success(&store, "http", &endpoint, &logical_batch_id, chunk)
        })
        .expect("send");

    let observed = observed.lock().expect("observed lock").clone();
    assert_eq!(
        observed,
        vec![
            ("batch_retry".to_string(), 4),
            ("batch_retry_part_1_of_2".to_string(), 2),
            ("batch_retry_part_2_of_2".to_string(), 2),
        ]
    );
    let state = store
        .sync_state("http", &endpoint)
        .expect("sync state")
        .expect("present");
    assert_eq!(state.last_batch_id, "batch_retry");
    let pending = store
        .pending_summaries_for_sync(
            "http",
            &endpoint,
            &batch
                .summaries
                .iter()
                .cloned()
                .map(sanitize_summary_for_sync)
                .collect::<Vec<_>>(),
        )
        .expect("pending summaries");
    assert!(pending.is_empty());
}

#[test]
fn http_rollup_sync_retries_smaller_batches_after_payload_too_large() {
    let store = Store::in_memory().expect("store");
    let endpoint = "https://api.example.com/api/sync/batches".to_string();
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollup-too-large"),
        LocationOrigin::Configured,
    );
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let summaries: Vec<_> = (0..4)
        .map(|index| {
            let mut summary = test_summary(
                "codex",
                &source,
                now + Duration::days(index as i64),
                10,
                None,
            );
            summary.summary_id = statsai_core::SummaryId(format!("summary-too-large-{index}"));
            summary.metadata.summary_format = "daily_rollup.v1".to_string();
            summary
        })
        .collect();
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_too_large".to_string(),
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
    let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_for_send = Arc::clone(&observed);

    send_http_rollup_chunk_with_retry_using(&batch, &|chunk| {
        observed_for_send
            .lock()
            .expect("observed lock")
            .push((chunk.batch_id.clone(), chunk.summaries.len()));
        if chunk.summaries.len() > 2 {
            return Err(anyhow::Error::msg(
                r#"sync endpoint returned HTTP 413: {"error":"sync_batch_too_large"}"#,
            ));
        }
        record_rollup_sync_chunk_success(&store, "http", &endpoint, &logical_batch_id, chunk)
    })
    .expect("send");

    let observed = observed.lock().expect("observed lock").clone();
    assert_eq!(
        observed,
        vec![
            ("batch_too_large".to_string(), 4),
            ("batch_too_large_part_1_of_2".to_string(), 2),
            ("batch_too_large_part_2_of_2".to_string(), 2),
        ]
    );
    let state = store
        .sync_state("http", &endpoint)
        .expect("sync state")
        .expect("present");
    assert_eq!(state.last_batch_id, batch.batch_id);
}

#[test]
fn http_rollup_sync_restarts_full_snapshot_after_snapshot_failure() {
    let store = Store::in_memory().expect("store");
    let endpoint = "https://api.example.com/api/sync/batches".to_string();
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollup-resume"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let account_id = provider_account_id("codex", "personal");
    for index in 0..26 {
        let event = test_event(
            "codex",
            &source,
            now + Duration::days(index as i64),
            Some(account_id.clone()),
            TokenParts::total(10),
        );
        store.insert_event(&event).expect("event");
    }
    store.rebuild_sync_rollups().expect("rebuild");

    let command = SyncCommand {
        endpoint: Some(endpoint.clone()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");
    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert_eq!(batch.sources.len(), 1);
    assert_eq!(batch.summaries.len(), 26);
    let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_for_send = Arc::clone(&observed);
    let mut observed_error = None;

    for chunk in split_http_rollup_sync_batches(&batch) {
        let result = send_http_rollup_chunk_with_retry_using(&chunk, &|chunk| {
            observed_for_send.lock().expect("observed lock").push((
                chunk.batch_id.clone(),
                chunk.sources.len(),
                chunk.summaries.len(),
                chunk.authoritative_snapshot.is_some(),
            ));
            if chunk.authoritative_snapshot.is_some() {
                return Err(anyhow::Error::msg(
                    r#"sync endpoint returned HTTP 429: {"error":"rate_limited","retryAfterSeconds":60}"#,
                ));
            }
            record_rollup_sync_chunk_success(&store, "http", &target, &logical_batch_id, chunk)
        });
        if let Err(send_error) = result {
            observed_error = Some(send_error);
            break;
        }
    }
    let error = observed_error.expect("rate limit should stop the snapshot request");
    assert!(error.to_string().contains("HTTP 429"));
    store
        .record_sync_failure("http", &target)
        .expect("record sync failure");

    let observed = observed.lock().expect("observed lock").clone();
    assert_eq!(
        observed,
        vec![
            (format!("{}_sources_1", batch.batch_id), 1, 0, false),
            (format!("{}_part_1_of_2", batch.batch_id), 0, 25, false),
            (format!("{}_part_2_of_2", batch.batch_id), 0, 1, false),
            (format!("{}_snapshot_1", batch.batch_id), 0, 0, true),
        ]
    );

    let sync_sources: Vec<_> = store
        .list_sources()
        .expect("sources")
        .into_iter()
        .map(sanitize_source_for_sync)
        .collect();
    assert!(store
        .pending_sources_for_sync("http", &target, &sync_sources)
        .expect("pending sources")
        .is_empty());

    let sync_rollups: Vec<_> = store
        .all_sync_rollup_summaries()
        .expect("rollups")
        .into_iter()
        .map(sanitize_summary_for_sync)
        .collect();
    let pending_rollups = store
        .pending_summaries_for_sync("http", &target, &sync_rollups)
        .expect("pending rollups");
    assert!(pending_rollups.is_empty());
    let state = store
        .sync_state("http", &target)
        .expect("sync state")
        .expect("present");
    assert_eq!(state.last_batch_id, batch.batch_id);

    let (resume_batch, resume_mode) =
        build_sync_batch(&command, &store, "device", &target).expect("resume batch");
    assert_eq!(resume_mode, SyncPayloadMode::Rollups);
    assert!(resume_batch.sources.is_empty());
    assert_eq!(resume_batch.summaries.len(), 26);
    assert!(resume_batch.authoritative_snapshot.is_some());
    let state_after_build = store
        .sync_state("http", &target)
        .expect("sync state")
        .expect("present");
    assert_eq!(
        state_after_build.pending_resume_batch_id, state.pending_resume_batch_id,
        "building the replacement snapshot must not clear resume state"
    );

    let since_last_command = SyncCommand {
        endpoint: Some(endpoint),
        since_last: true,
        ..test_sync_command("http")
    };
    let (since_last_resume, _) = build_sync_batch(&since_last_command, &store, "device", &target)
        .expect("since-last resume batch");
    assert_eq!(since_last_resume.summaries.len(), 26);
    assert!(since_last_resume.authoritative_snapshot.is_some());
}

#[test]
fn failed_http_sync_without_ack_keeps_next_default_sync_full_history() {
    let store = Store::in_memory().expect("store");
    let endpoint = "https://api.example.com/api/sync/batches".to_string();
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-no-partial-resume"),
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

    let command = SyncCommand {
        endpoint: Some(endpoint.clone()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (initial_batch, initial_mode) =
        build_sync_batch(&command, &store, "device", &target).expect("initial batch");
    assert_eq!(initial_mode, SyncPayloadMode::Rollups);
    assert_eq!(initial_batch.summaries.len(), 1);
    record_rollup_sync_success(&store, "http", &target, &initial_batch)
        .expect("record initial sync");

    store
        .record_sync_failure("http", &target)
        .expect("record failed sync");

    let state = store
        .sync_state("http", &target)
        .expect("sync state")
        .expect("present");
    assert!(state.pending_resume_batch_id.is_none());
    assert!(state.failure_count > 0);

    let (retry_batch, retry_mode) =
        build_sync_batch(&command, &store, "device", &target).expect("retry batch");
    assert_eq!(retry_mode, SyncPayloadMode::Rollups);
    assert_eq!(retry_batch.summaries.len(), 1);

    let since_last_command = SyncCommand {
        endpoint: Some(endpoint),
        since_last: true,
        ..test_sync_command("http")
    };
    let (since_last_batch, since_last_mode) =
        build_sync_batch(&since_last_command, &store, "device", &target)
            .expect("since-last retry batch");
    assert_eq!(since_last_mode, SyncPayloadMode::Rollups);
    assert!(
        since_last_batch.summaries.is_empty(),
        "explicit --since-last should not force full history after an unacknowledged failure"
    );
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
fn http_rollup_chunk_is_resent_after_a_transient_endpoint_failure() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = test_task_only_sync_batch(now, 1, 1);
    let attempts = std::cell::Cell::new(0_usize);
    // A restarted worker answers with a plain-text body, so the decision to
    // resend cannot depend on parsing an error code out of JSON.
    let send = |_: &SyncBatch| -> Result<()> {
        attempts.set(attempts.get() + 1);
        if attempts.get() == 1 {
            Err(anyhow::anyhow!(
                "sync endpoint returned HTTP 503: Your worker restarted mid-request. \
                     Please try sending the request again. Only GET or HEAD requests are \
                     retried automatically."
            ))
        } else {
            Ok(())
        }
    };

    let delays = std::cell::RefCell::new(Vec::new());
    send_http_rollup_chunk_with_retry_using_sleep(&batch, &send, &|delay| {
        delays.borrow_mut().push(delay)
    })
    .expect("transient failure is resent rather than aborting the run");
    assert_eq!(attempts.get(), 2);
    assert_eq!(delays.into_inner(), vec![StdDuration::from_secs(1)]);
}

#[test]
fn http_rollup_chunk_stops_resending_a_transient_failure_that_never_clears() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = test_task_only_sync_batch(now, 1, 1);
    let attempts = std::cell::Cell::new(0_usize);
    let send = |_: &SyncBatch| -> Result<()> {
        attempts.set(attempts.get() + 1);
        Err(anyhow::anyhow!(
            "sync endpoint returned HTTP 502: Bad gateway"
        ))
    };

    let delays = std::cell::RefCell::new(Vec::new());
    let error = send_http_rollup_chunk_with_retry_using_sleep(&batch, &send, &|delay| {
        delays.borrow_mut().push(delay)
    })
    .expect_err("an endpoint that never recovers still fails the run");

    // The original failure is reported rather than a retry-shaped summary of
    // it, and the run gives up instead of resending forever.
    assert!(error.to_string().contains("502"));
    assert_eq!(attempts.get(), 4);
    // Each attempt waits twice as long as the one before it.
    assert_eq!(
        delays.into_inner(),
        vec![
            StdDuration::from_secs(1),
            StdDuration::from_secs(2),
            StdDuration::from_secs(4),
        ]
    );
}

#[test]
fn http_rollup_chunk_does_not_resend_a_decided_rejection() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = test_task_only_sync_batch(now, 1, 1);
    let attempts = std::cell::Cell::new(0_usize);
    // The endpoint decided about this batch. Sending it again could only be
    // rejected the same way, and a conflict repeated on a schedule is worse
    // than one reported immediately.
    let send = |_: &SyncBatch| -> Result<()> {
        attempts.set(attempts.get() + 1);
        Err(anyhow::anyhow!(
            r#"sync endpoint returned HTTP 409: {{"error":"batch_id_payload_conflict"}}"#
        ))
    };

    let error = send_http_rollup_chunk_with_retry_using_sleep(&batch, &send, &|_| {
        panic!("a decided rejection must not wait to be resent")
    })
    .expect_err("conflict is reported");

    assert!(error.to_string().contains("batch_id_payload_conflict"));
    assert_eq!(attempts.get(), 1);
}

#[test]
fn http_rollup_rate_limit_is_left_to_the_endpoints_own_retry_after() {
    // 429 carries a `Retry-After` this backoff cannot read, so resending on
    // our own schedule would ignore the delay the endpoint asked for.
    assert!(!is_transient_http_sync_error(&anyhow::anyhow!(
        r#"sync endpoint returned HTTP 429: {{"error":"sync_write_user"}}"#
    )));
    assert!(is_transient_http_sync_error(&anyhow::anyhow!(
        "sync endpoint returned HTTP 503: Your worker restarted mid-request."
    )));
    // A body that is not JSON at all must still yield its status.
    assert_eq!(
        http_sync_error_status(&anyhow::anyhow!(
            "sync endpoint returned HTTP 504: Gateway timeout"
        )),
        Some(504)
    );
    // Anything that is not a sync endpoint failure has no status to read.
    assert_eq!(
        http_sync_error_status(&anyhow::anyhow!("connection reset by peer")),
        None
    );
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
fn record_sync_batch_success_marks_task_entities_synced_for_file_sink() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let store = Store::in_memory().expect("store");
    let batch = test_task_only_sync_batch(now, 1, 1);
    for bucket in &batch.task_buckets {
        store
            .replace_task_bucket_snapshot(bucket)
            .expect("seed task bucket snapshot");
    }
    for verification in &batch.task_verifications {
        store
            .merge_task_verification(verification)
            .expect("seed task verification");
    }

    record_sync_batch_success(&store, "file", "/tmp/statsai-sync-batch.json", &batch)
        .expect("record sync batch success");

    assert!(store
        .pending_task_bucket_snapshots_for_sync(
            "file",
            "/tmp/statsai-sync-batch.json",
            &batch.device_id,
            false,
            None,
        )
        .expect("pending task buckets")
        .is_empty());
    assert!(store
        .pending_task_verifications_for_sync("file", "/tmp/statsai-sync-batch.json")
        .expect("pending task verifications")
        .is_empty());
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

#[test]
fn custom_http_sinks_skip_task_verification_feed_derivation() {
    assert_eq!(
        http_task_verification_feed_url("https://example.com/custom-sync"),
        None
    );
    assert_eq!(
        http_task_verification_feed_url("https://api.example.com/api/sync/batches"),
        Some("https://api.example.com/api/task-sync/verifications".to_string())
    );
}

#[test]
fn optional_task_verification_feed_statuses_do_not_fail_sync() {
    assert!(optional_task_verification_feed_status(404));
    assert!(optional_task_verification_feed_status(405));
    assert!(optional_task_verification_feed_status(501));
    assert!(!optional_task_verification_feed_status(400));
    assert!(!optional_task_verification_feed_status(429));
    assert!(!optional_task_verification_feed_status(500));
}

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

fn test_task_only_sync_batch(
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

fn test_code_change_metric(index: usize, now: DateTime<Utc>) -> statsai_core::CodeChangeMetric {
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
fn logical_http_rollup_batch_id_strips_known_chunk_suffixes() {
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_part_11_of_11"),
        "batch_1"
    );
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_part_11_of_11_part_1_of_2"),
        "batch_1"
    );
    assert_eq!(logical_http_rollup_batch_id("batch_1_sources_1"), "batch_1");
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_part_3_of_9_sources_1"),
        "batch_1"
    );
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_subscriptions_2"),
        "batch_1"
    );
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_task_buckets_2"),
        "batch_1"
    );
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_part_3_of_9_task_verifications_4"),
        "batch_1"
    );
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_code_changes_3"),
        "batch_1"
    );
    assert_eq!(logical_http_rollup_batch_id("batch_1"), "batch_1");
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_part_final"),
        "batch_1_part_final"
    );
}

#[test]
fn incremental_http_sync_sends_late_claude_assignment_without_full() {
    let endpoint = "https://api.example.com/api/sync/batches".to_string();
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "claude_code",
        "claude-code-local-jsonl",
        "0.3.3",
        Path::new("/tmp/claude-http-late-assignment"),
        LocationOrigin::Configured,
    );
    source.verified_state_hash =
        verified_source_observation_hash(&VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        })
        .expect("blocked observation hash");
    store.upsert_source(&source).expect("source");

    let authenticated_at = Utc
        .with_ymd_and_hms(2026, 8, 7, 12, 0, 0)
        .single()
        .expect("authenticated_at");
    let event_at = authenticated_at + Duration::hours(1);
    store
        .insert_event(&test_event(
            "claude_code",
            &source,
            event_at,
            None,
            TokenParts::total(15),
        ))
        .expect("unassigned event");
    store.rebuild_sync_rollups().expect("initial rollups");

    let command = SyncCommand {
        endpoint: Some(endpoint),
        ..test_sync_command("http")
    };
    assert!(!command.full);
    let target = sync_target(&command).expect("target");
    let (initial_batch, initial_mode) =
        build_sync_batch(&command, &store, "device", &target).expect("initial batch");
    assert_eq!(initial_mode, SyncPayloadMode::Rollups);
    assert!(initial_batch.accounts.is_empty());
    assert!(initial_batch.source_account_assignments.is_empty());
    assert_eq!(initial_batch.summaries.len(), 1);
    let unassigned_summary_id = initial_batch.summaries[0].summary_id.clone();
    record_rollup_sync_success(&store, "http", &target, &initial_batch)
        .expect("record initial sync");

    let inferred_observation = VerifiedSourceObservation::Inferred {
        identity: Box::new(VerifiedSourceState {
            provider_user_id: Some("claude-account".to_string()),
            email: Some("claude@example.com".to_string()),
            account_label: None,
            plan_name: None,
            authenticated_at: Some(authenticated_at),
            verified_at: Some(authenticated_at),
            subscription: None,
        }),
        basis: SourceIdentityInference::CachedLocalProfile,
        settings_modified_at: None,
    };
    let inferred_hash =
        verified_source_observation_hash(&inferred_observation).expect("inferred observation hash");
    reconcile_verified_source_state(&store, &mut source, &inferred_observation, inferred_hash)
        .expect("inferred Claude state");
    store.rebuild_sync_rollups().expect("reattributed rollups");

    let (incremental_batch, incremental_mode) =
        build_sync_batch(&command, &store, "device", &target).expect("incremental batch");
    assert_eq!(incremental_mode, SyncPayloadMode::Rollups);
    assert_eq!(incremental_batch.accounts.len(), 1);
    assert_eq!(incremental_batch.source_account_assignments.len(), 1);
    assert_eq!(incremental_batch.summaries.len(), 1);
    assert!(incremental_batch.summaries[0].provider_account_id.is_some());
    let snapshot = incremental_batch
        .authoritative_snapshot
        .as_ref()
        .expect("retired unassigned rollup requires an authoritative snapshot");
    assert!(!snapshot.summary_ids.contains(&unassigned_summary_id));
    assert!(snapshot
        .summary_ids
        .contains(&incremental_batch.summaries[0].summary_id));
}

#[test]
fn full_http_sync_resends_metadata_after_tracking_is_cleared() {
    let endpoint = "https://api.example.com/api/sync/batches".to_string();
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-reset-tracking"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("started_at");
    let verified_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
        .single()
        .expect("verified_at");
    apply_verified_source_state(
        &store,
        &source,
        Some(&VerifiedSourceState {
            provider_user_id: Some("acct-real".to_string()),
            email: Some("verified@example.com".to_string()),
            account_label: None,
            plan_name: Some("Plus".to_string()),
            authenticated_at: Some(started_at),
            verified_at: Some(verified_at),
            subscription: Some(VerifiedSubscriptionState {
                plan_name: "Plus".to_string(),
                price: 2000,
                currency: "USD".to_string(),
                billing_period: BillingPeriod::Monthly,
                paid_at: Some(started_at),
                started_at,
                ended_at: None,
                current_period_ends_at: Some(started_at + Duration::days(30)),
                status: SubscriptionStatus::Active,
                verified_at: Some(verified_at),
            }),
        }),
    )
    .expect("verified state");

    let account_id = store.list_accounts().expect("accounts")[0]
        .provider_account_id
        .clone();
    let event = test_event(
        "codex",
        &source,
        started_at + Duration::hours(1),
        Some(account_id),
        TokenParts {
            input: 10,
            output: 5,
            cached_input: 0,
            reasoning: 0,
            total: 15,
            cost: Some(10),
        },
    );
    store.insert_event(&event).expect("event");
    store.rebuild_sync_rollups().expect("rebuild");

    let command = SyncCommand {
        endpoint: Some(endpoint.clone()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");

    let (initial_batch, initial_mode) =
        build_sync_batch(&command, &store, "device", &target).expect("initial batch");
    assert_eq!(initial_mode, SyncPayloadMode::Rollups);
    record_rollup_sync_success(&store, "http", &target, &initial_batch)
        .expect("record initial sync");

    let all_sources = store.list_sources().expect("sources");
    let all_accounts = store.list_accounts().expect("accounts");

    let sync_sources: Vec<_> = all_sources
        .iter()
        .cloned()
        .map(sanitize_source_for_sync)
        .collect();
    let sync_accounts: Vec<_> = all_accounts
        .iter()
        .cloned()
        .map(sanitize_account_for_sync)
        .collect();
    assert_eq!(
        store
            .pending_sources_for_sync("http", &target, &sync_sources)
            .expect("pending sources")
            .len(),
        0
    );
    assert_eq!(
        store
            .pending_accounts_for_sync("http", &target, &sync_accounts)
            .expect("pending accounts")
            .len(),
        0
    );

    let local_state = store
        .sync_state("http", &target)
        .expect("state")
        .expect("present");
    let local_verify = sync_local_verify(&store, "http", &target, Some(&local_state), false)
        .expect("local verify");
    assert_eq!(
        remote_metadata_gap_reason(
            &json!({
                "device": {
                    "last_sync_batch_id": initial_batch.batch_id
                },
                "mirrorCounts": {
                    "sources": 0,
                    "accounts": 0,
                    "source_account_assignments": 0,
                    "subscriptions": 0,
                    "summaries": 0,
                    "sync_batches": 1
                }
            }),
            &local_verify
        )
        .as_deref(),
        Some("sources 0!=1, accounts 0!=1, source_account_assignments 0!=1")
    );

    store
        .clear_sync_tracking_for_target("http", &target)
        .expect("clear tracking");

    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");
    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert_eq!(batch.sources.len(), 1);
    assert_eq!(batch.accounts.len(), 1);
    assert_eq!(batch.source_account_assignments.len(), 1);
    assert!(batch.subscriptions.is_empty());
    assert_eq!(batch.summaries.len(), 1);
    assert!(is_daily_rollup_summary(&batch.summaries[0]));
}

#[test]
fn http_verify_status_url_points_at_worker_status_endpoint() {
    assert_eq!(
        http_verify_status_url("https://api.example.com/api/sync/batches").expect("status"),
        "https://api.example.com/api/sync/status"
    );
}

#[test]
fn http_preflight_status_url_points_at_lightweight_worker_status_endpoint() {
    assert_eq!(
        http_preflight_status_url("https://api.example.com/api/sync/batches").expect("status"),
        "https://api.example.com/api/sync/status?view=preflight"
    );
}

#[test]
fn only_the_configured_hosted_endpoint_requires_a_device_login() {
    let hosted = "https://api.example.com/api/sync/batches";
    assert!(http_endpoint_requires_authentication(hosted, hosted));
    assert!(http_endpoint_requires_authentication(
        "https://api.example.com/api/sync/batches/",
        hosted
    ));
    // A self-hosted deployment serves the same route and may accept
    // unauthenticated batches, so the path shape must not imply a login.
    assert!(!http_endpoint_requires_authentication(
        "https://sync.example.com/api/sync/batches",
        hosted
    ));
    assert!(!http_endpoint_requires_authentication(
        "https://sync.example.com/custom/batch-ingest",
        hosted
    ));
}

#[test]
fn custom_http_endpoint_skips_optional_remote_preflight() {
    let command = SyncCommand {
        auth_token: Some("token".to_string()),
        ..test_sync_command("http")
    };

    let preflight =
        load_http_sync_preflight(&command, "https://sync.example.com/custom/batch-ingest")
            .expect("custom endpoint preflight");

    assert_eq!(preflight.auth_token.as_deref(), Some("token"));
    assert!(preflight.remote.is_none());
}

#[test]
fn remote_hosted_tasks_enabled_defaults_true_when_capability_missing() {
    assert!(remote_hosted_tasks_enabled(&json!({
        "device": {
            "last_sync_batch_id": "batch-1"
        }
    })));
}

#[test]
fn remote_hosted_tasks_enabled_reads_explicit_false_capability() {
    assert!(!remote_hosted_tasks_enabled(&json!({
        "capabilities": {
            "hostedTasks": false
        }
    })));
}

#[test]
fn remote_code_change_identity_key_reads_account_scoped_blinding_key() {
    let encoded = "ab".repeat(32);
    assert_eq!(
        remote_code_change_identity_key(&json!({
            "capabilities": {
                "codeChangeIdentityKey": encoded
            }
        }))
        .expect("identity key"),
        Some([0xab; 32])
    );
    assert_eq!(
        remote_code_change_identity_key(&json!({ "capabilities": {} }))
            .expect("missing identity key"),
        None
    );
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
fn remote_code_change_identity_key_rejects_malformed_keys() {
    for value in [json!("not-hex"), json!("ab"), json!(42)] {
        assert!(remote_code_change_identity_key(&json!({
            "capabilities": {
                "codeChangeIdentityKey": value
            }
        }))
        .is_err());
    }
}

#[test]
fn optional_http_sync_preflight_statuses_do_not_disable_task_sync() {
    assert!(optional_http_sync_preflight_status(404));
    assert!(optional_http_sync_preflight_status(405));
    assert!(optional_http_sync_preflight_status(501));
    assert!(!optional_http_sync_preflight_status(400));
    assert!(!optional_http_sync_preflight_status(500));
}

#[test]
fn http_reset_url_points_at_worker_reset_endpoint() {
    assert_eq!(
        http_reset_url("https://api.example.com/api/sync/batches").expect("reset"),
        "https://api.example.com/api/sync/reset"
    );
}

#[test]
fn credentialed_http_helpers_reject_remote_plaintext_before_request() {
    let endpoint = "http://api.example.com/api/sync/batches";

    for result in [
        http_remote_verify(endpoint, "token"),
        http_remote_reset(endpoint, "token"),
    ] {
        let error = result.expect_err("remote plaintext must fail");
        assert!(error.to_string().contains("requires HTTPS"));
    }

    let command = SyncCommand {
        auth_token: Some("token".to_string()),
        ..test_sync_command("http")
    };
    let error = load_http_sync_preflight(&command, endpoint)
        .expect_err("remote plaintext preflight must fail");
    assert!(error.to_string().contains("requires HTTPS"));
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
fn sync_batch_serialization_excludes_local_task_entities() {
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch-test".to_string(),
        device_id: "device-test".to_string(),
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
        created_at: Utc
            .with_ymd_and_hms(2026, 6, 14, 13, 0, 0)
            .single()
            .expect("created_at"),
    };

    let value = serde_json::to_value(&batch).expect("serialize sync batch");
    assert!(value.get("task_buckets").is_none());
    assert!(value.get("task_verifications").is_none());
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
fn build_sync_batch_respects_project_and_task_opt_ins() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-project-sync-opt-in"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let now = Utc
        .with_ymd_and_hms(2026, 7, 7, 12, 0, 0)
        .single()
        .expect("now");
    let mut event = test_event("codex", &source, now, None, TokenParts::total(120));
    event.project = Some(ProjectInfo {
        project_id: "project-repo-backed".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/repo".to_string()),
        branch_hash: Some("branch-hash".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/Users/example/work/ai-stats".to_string()),
    });
    store.insert_event(&event).expect("event");

    let mut summary = test_summary("codex", &source, now, 120, None);
    summary.project = event.project.clone();
    store.upsert_summary(&summary).expect("summary");

    let mut task_batch = test_task_only_sync_batch(now, 1, 1);
    task_batch.task_buckets[0].spans[0].source_record_id =
        Some("codex_task_span.v1:raw-session:42".to_string());
    for bucket in &task_batch.task_buckets {
        store
            .replace_task_bucket_snapshot(bucket)
            .expect("seed task bucket");
    }
    for verification in &task_batch.task_verifications {
        store
            .merge_task_verification(verification)
            .expect("seed task verification");
    }

    let default_command = test_sync_command("file");
    let default_target = sync_target(&default_command).expect("default target");
    let (default_batch, default_mode) =
        build_sync_batch(&default_command, &store, "device", &default_target)
            .expect("default batch");
    assert_eq!(default_mode, SyncPayloadMode::Raw);
    assert_eq!(default_batch.events.len(), 1);
    assert!(default_batch.events[0].project.is_none());
    assert_eq!(default_batch.summaries.len(), 1);
    assert!(default_batch.summaries[0].project.is_none());
    assert!(default_batch.task_buckets.is_empty());
    assert!(default_batch.task_verifications.is_empty());

    let project_opt_in_command = SyncCommand {
        include_projects: true,
        ..test_sync_command("file")
    };
    let project_opt_in_target =
        sync_target(&project_opt_in_command).expect("project opt-in target");
    let (project_opt_in_batch, project_opt_in_mode) = build_sync_batch(
        &project_opt_in_command,
        &store,
        "device",
        &project_opt_in_target,
    )
    .expect("project opt-in batch");
    assert_eq!(project_opt_in_mode, SyncPayloadMode::Raw);
    assert_eq!(project_opt_in_batch.events.len(), 1);
    assert!(project_opt_in_batch.events[0].project.is_some());
    assert_eq!(project_opt_in_batch.summaries.len(), 1);
    assert!(project_opt_in_batch.summaries[0].project.is_some());
    assert!(project_opt_in_batch.task_buckets.is_empty());
    assert!(project_opt_in_batch.task_verifications.is_empty());

    store
        .set_sync_preferences(SyncPreferences {
            include_projects: true,
            include_tasks: false,
        })
        .expect("persist sync preferences");
    let (persisted_batch, persisted_mode) =
        build_sync_batch(&default_command, &store, "device", &default_target)
            .expect("persisted batch");
    assert_eq!(persisted_mode, SyncPayloadMode::Raw);
    assert!(persisted_batch.events[0].project.is_some());
    assert!(persisted_batch.summaries[0].project.is_some());
    assert!(persisted_batch.task_buckets.is_empty());
    assert!(persisted_batch.task_verifications.is_empty());

    let task_opt_in_command = SyncCommand {
        include_tasks: true,
        ..test_sync_command("file")
    };
    let task_opt_in_target = sync_target(&task_opt_in_command).expect("task opt-in target");
    let (task_opt_in_batch, task_opt_in_mode) =
        build_sync_batch(&task_opt_in_command, &store, "device", &task_opt_in_target)
            .expect("task opt-in batch");
    assert_eq!(task_opt_in_mode, SyncPayloadMode::Raw);
    assert!(task_opt_in_batch.events[0].project.is_some());
    assert!(task_opt_in_batch.summaries[0].project.is_some());
    assert_eq!(task_opt_in_batch.task_buckets.len(), 1);
    assert_eq!(task_opt_in_batch.task_verifications.len(), 1);
    let synced_span = &task_opt_in_batch.task_buckets[0].spans[0];
    assert!(synced_span.source_record_id.is_none());
    assert!(synced_span.session_id.is_none());
    assert!(synced_span.thread_id.is_none());
}

#[test]
fn code_change_metric_project_ids_follow_sync_project_preferences() {
    let store = Store::in_memory().expect("store");
    let now = Utc
        .with_ymd_and_hms(2026, 7, 7, 12, 0, 0)
        .single()
        .expect("now");
    let mut seed_batch = test_task_only_sync_batch(now, 0, 0);
    let mut metric = test_code_change_metric(0, now);
    metric.project_id = Some("project-private".to_string());
    metric.repository_hash = Some("repository-private".to_string());
    seed_batch.code_change_metrics.push(metric);
    store
        .ingest_sync_batch(&seed_batch)
        .expect("seed code-change metric");

    let default_command = SyncCommand {
        dry_run: true,
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&default_command).expect("target");
    let (default_batch, _) =
        build_sync_batch(&default_command, &store, "device", &target).expect("default batch");
    assert_eq!(default_batch.code_change_metrics.len(), 1);
    assert!(default_batch.code_change_metrics[0].project_id.is_none());
    assert!(default_batch.code_change_metrics[0]
        .repository_hash
        .is_none());

    store
        .record_code_change_metrics_synced("http", &target, &default_batch.code_change_metrics)
        .expect("record sanitized metric");
    store
        .record_sync_success("http", &target, "batch_metric_default", &[], &[], None)
        .expect("record sync state");
    let (unchanged_batch, _) =
        build_sync_batch(&default_command, &store, "device", &target).expect("unchanged batch");
    assert!(unchanged_batch.code_change_metrics.is_empty());

    let include_command = SyncCommand {
        include_projects: true,
        dry_run: true,
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let (included_batch, _) =
        build_sync_batch(&include_command, &store, "device", &target).expect("included batch");
    assert_eq!(
        included_batch.code_change_metrics[0].project_id.as_deref(),
        Some("project-private")
    );
    assert!(included_batch.code_change_metrics[0]
        .repository_hash
        .is_some());

    store
        .set_sync_preferences(SyncPreferences {
            include_projects: true,
            include_tasks: false,
        })
        .expect("persist project opt-in");
    let exclude_command = SyncCommand {
        exclude_projects: true,
        full: true,
        dry_run: true,
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let (excluded_batch, _) =
        build_sync_batch(&exclude_command, &store, "device", &target).expect("excluded batch");
    assert_eq!(excluded_batch.code_change_metrics.len(), 1);
    assert!(excluded_batch.code_change_metrics[0].project_id.is_none());
    assert!(excluded_batch.code_change_metrics[0]
        .repository_hash
        .is_none());
}

#[test]
fn code_change_metric_sync_sanitization_removes_raw_commit_hash() {
    let now = Utc
        .with_ymd_and_hms(2026, 7, 7, 12, 0, 0)
        .single()
        .expect("now");
    let mut metric = test_code_change_metric(0, now);
    metric.commit_hash = Some("0123456789abcdef-public-commit".to_string());

    let sanitized = sanitize_code_change_metric_for_sync(metric, true);

    assert!(sanitized.commit_hash.is_none());
}

#[test]
fn code_change_sync_excludes_metrics_owned_by_peer_devices() {
    let store = Store::in_memory().expect("store");
    let now = Utc
        .with_ymd_and_hms(2026, 7, 7, 12, 0, 0)
        .single()
        .expect("now");
    let mut seed_batch = test_task_only_sync_batch(now, 0, 0);
    let mut local_metric = test_code_change_metric(0, now);
    local_metric.device_id = "local-device".to_string();
    let mut peer_metric = test_code_change_metric(1, now);
    peer_metric.device_id = "peer-device".to_string();
    seed_batch.code_change_metrics = vec![local_metric.clone(), peer_metric];
    store
        .ingest_sync_batch(&seed_batch)
        .expect("seed code-change metrics");
    let command = SyncCommand {
        dry_run: true,
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");

    let (batch, _) = build_sync_batch(&command, &store, "local-device", &target)
        .expect("build local sync batch");

    assert_eq!(batch.code_change_metrics, vec![local_metric.clone()]);
    assert_eq!(
        batch
            .authoritative_snapshot
            .expect("authoritative snapshot")
            .code_change_metric_ids,
        vec![local_metric.metric_id]
    );
}

#[test]
fn quota_contributions_reach_the_batch_and_its_authoritative_ids() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-quota-v4-sync"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let observed_at = Utc
        .with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
        .single()
        .expect("observed at");
    let account_id = ProviderAccountId("account-quota-sync".to_string());
    store
        .upsert_source_account_assignment(&SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: SourceAccountAssignmentId("assignment-quota-sync".to_string()),
            source_id: source.source_id.clone(),
            provider: "codex".to_string(),
            provider_account_id: account_id,
            started_at: observed_at - Duration::days(1),
            ended_at: None,
            record_source: IdentitySource::UserConfigured,
            verified_at: Some(observed_at),
            created_at: observed_at,
            updated_at: observed_at,
        })
        .expect("assignment");
    let reset_at = observed_at + Duration::days(7);
    let quota_record: QuotaObservationRecordV1 = serde_json::from_value(json!({
        "observation": {
            "schema_version": "quota_observation.v1",
            "observation_id": "quota-observation-sync",
            "semantic_fingerprint": "quota-semantic-sync",
            "provider": "codex",
            "source_id": source.source_id,
            "provider_account_id": null,
            "observed_at": observed_at,
            "source_file_path_hash": "file-hash",
            "source_record_id": "record-id",
            "source_line_number": 1,
            "payload_hash": "payload-hash",
            "usage_sample": null,
            "usage_event_id": null,
            "usage_link_kind": "none",
            "status": {
                "plan_type": "pro",
                "individual_limit": {
                    "account_email": "private@example.com",
                    "nested": {"token": "provider-secret"}
                },
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
            "window_observation_id": "quota-window-sync",
            "observation_id": "quota-observation-sync",
            "provider_slot": "primary",
            "limit_id": "subscription",
            "window_minutes": 10080,
            "used_percent": 25.0,
            "resets_at": reset_at,
            "resets_at_epoch_seconds": reset_at.timestamp()
        }],
        "raw_rate_limits": {"primary": {"used_percent": 25.0}}
    }))
    .expect("quota record");
    store
        .upsert_quota_observations(&[quota_record])
        .expect("quota observation");

    let command = SyncCommand {
        dry_run: true,
        endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, _) =
        build_sync_batch(&command, &store, "device-quota", &target).expect("v4 quota batch");

    assert_eq!(
        batch.schema_version,
        statsai_core::SYNC_BATCH_V5_SCHEMA_VERSION
    );
    assert_eq!(batch.quota_cycle_contributions.len(), 1);
    // The uploaded contribution carries no provider status at all, so the
    // plan, credits, and `individual_limit` blob a window observation holds
    // cannot reach the backend even by accident.
    let uploaded =
        serde_json::to_value(&batch.quota_cycle_contributions[0]).expect("serialize contribution");
    assert_eq!(uploaded.get("latest_status"), None);
    let contribution_id = batch.quota_cycle_contributions[0].contribution_id.clone();
    assert_eq!(
        batch
            .authoritative_snapshot
            .as_ref()
            .expect("authoritative snapshot")
            .quota_cycle_contribution_ids,
        vec![contribution_id.clone()]
    );
    let chunks = split_http_rollup_sync_batches(&batch);
    assert!(chunks.iter().any(|chunk| {
        chunk
            .quota_cycle_contributions
            .iter()
            .any(|contribution| contribution.contribution_id == contribution_id)
    }));
    assert!(chunks.iter().any(|chunk| {
        chunk
            .authoritative_snapshot
            .as_ref()
            .is_some_and(|snapshot| {
                snapshot
                    .quota_cycle_contribution_ids
                    .contains(&contribution_id)
            })
    }));

    record_rollup_sync_success(&store, "http", &target, &batch).expect("record initial quota sync");
    let (unchanged, _) =
        build_sync_batch(&command, &store, "device-quota", &target).expect("unchanged quota batch");
    assert!(unchanged.quota_cycle_contributions.is_empty());
    assert!(unchanged.authoritative_snapshot.is_none());

    store
        .delete_quota_observations_for_sources(std::slice::from_ref(&source.source_id))
        .expect("delete quota evidence");
    let (retirement, _) = build_sync_batch(&command, &store, "device-quota", &target)
        .expect("quota retirement batch");
    assert!(retirement.quota_cycle_contributions.is_empty());
    assert!(retirement
        .authoritative_snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.quota_cycle_contribution_ids.is_empty()));
}

#[test]
fn sanitize_account_for_sync_preserves_user_configured_label() {
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: provider_account_id("codex", "personal"),
        provider: "codex".to_string(),
        identity_source: IdentitySource::UserConfigured,
        provider_user_id: Some("provider-user-secret".to_string()),
        provider_user_id_hash: Some("a".repeat(64)),
        email: Some("private@example.com".to_string()),
        email_hash: Some("b".repeat(64)),
        org_id_hash: None,
        account_label: Some("personal".to_string()),
        plan_name: Some("Pro".to_string()),
        confidence: Confidence::Medium,
        verified_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let sanitized = sanitize_account_for_sync(account);
    assert_eq!(sanitized.account_label.as_deref(), Some("personal"));
    // The account's own identity travels: without it the dashboard can
    // only name an account by its `acct_` hash, and telling your own
    // accounts apart is why they sync in the first place.
    assert_eq!(
        sanitized.provider_user_id.as_deref(),
        Some("provider-user-secret")
    );
    assert_eq!(sanitized.email.as_deref(), Some("private@example.com"));
    assert_eq!(
        sanitized.provider_user_id_hash.as_deref(),
        Some("a".repeat(64).as_str())
    );
    assert_eq!(
        sanitized.email_hash.as_deref(),
        Some("b".repeat(64).as_str())
    );
    // A plan is evidence now, not an account attribute.
    assert_eq!(sanitized.plan_name, None);
}

#[test]
fn sync_rollup_stats_summaries_roll_up_events_by_day_and_account() {
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sync-rollup-stats"),
        LocationOrigin::Configured,
    );
    let account = provider_account_id("codex", "personal");
    let day1_a = Utc
        .with_ymd_and_hms(2026, 5, 20, 10, 0, 0)
        .single()
        .expect("day1a");
    let day1_b = Utc
        .with_ymd_and_hms(2026, 5, 20, 11, 0, 0)
        .single()
        .expect("day1b");
    let day2 = Utc
        .with_ymd_and_hms(2026, 5, 21, 9, 0, 0)
        .single()
        .expect("day2");

    let summaries = build_sync_rollup_stats_summaries(
        &[
            test_event(
                "codex",
                &source,
                day1_a,
                Some(account.clone()),
                TokenParts {
                    input: 10,
                    output: 5,
                    cached_input: 0,
                    reasoning: 0,
                    total: 15,
                    cost: Some(10),
                },
            ),
            test_event(
                "codex",
                &source,
                day1_b,
                Some(account.clone()),
                TokenParts {
                    input: 20,
                    output: 10,
                    cached_input: 0,
                    reasoning: 0,
                    total: 30,
                    cost: Some(30),
                },
            ),
            test_event(
                "codex",
                &source,
                day2,
                Some(account),
                TokenParts {
                    input: 7,
                    output: 3,
                    cached_input: 0,
                    reasoning: 0,
                    total: 10,
                    cost: Some(5),
                },
            ),
        ],
        "device",
    );

    assert_eq!(summaries.len(), 2);
    let total_tokens: u64 = summaries
        .iter()
        .map(|summary| summary.usage.total_tokens.unwrap_or(0))
        .sum();
    assert_eq!(total_tokens, 55);
    assert!(summaries
        .iter()
        .all(|summary| summary.metadata.summary_format == "daily_rollup.v1"));
}
