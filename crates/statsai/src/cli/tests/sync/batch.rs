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
fn failed_http_sync_without_ack_reconciles_without_re_uploading_history() {
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
    // The failure recorded no chunk, so the cursor never moved and the already
    // acknowledged summary stays acknowledged. Re-sending it -- and on a real
    // account, every other summary with it -- is not what recovery requires.
    assert!(
        retry_batch.summaries.is_empty(),
        "an unacknowledged failure must not re-upload acknowledged history"
    );
    // What it does require is the authoritative snapshot, so the server can retire
    // anything the interrupted sync left inconsistent.
    assert!(
        retry_batch.authoritative_snapshot.is_some(),
        "recovery still has to let the server reconcile the mirror"
    );

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
