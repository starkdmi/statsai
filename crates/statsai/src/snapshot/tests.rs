use super::*;

#[test]
fn format_tokens_scales() {
    assert_eq!(format_tokens(853_000_000), "853M");
    assert_eq!(format_tokens(84_000), "84k");
    assert_eq!(format_tokens(500), "500");
}

#[test]
fn stale_sync_without_pending_rollups_does_not_offer_empty_upload() {
    assert!(!pending_upload_needed(
        false,
        Some(Utc::now() - Duration::hours(45)),
        true,
    ));
}

#[test]
fn fresh_sync_with_no_unsynced_does_not_need_upload() {
    assert!(!pending_upload_needed(
        false,
        Some(Utc::now() - Duration::hours(1)),
        true,
    ));
}

#[test]
fn pending_rollups_need_upload() {
    assert!(pending_upload_needed(
        true,
        Some(Utc::now() - Duration::minutes(5)),
        false,
    ));
}

#[test]
fn failed_sync_state_does_not_count_as_synced() {
    let failed = statsai_store::SyncState {
        sink: "http".to_string(),
        target: "https://api.example.com/api/sync/batches".to_string(),
        last_success_at: Utc::now(),
        last_batch_id: String::new(),
        last_event_started_at: None,
        last_event_id: None,
        last_summary_observed_at: None,
        last_summary_id: None,
        last_task_verification_updated_at: None,
        last_task_verification_id: None,
        failure_count: 1,
        pending_resume_batch_id: None,
    };
    assert!(!has_successful_sync(Some(&failed)));

    let synced = statsai_store::SyncState {
        last_batch_id: "batch_1".to_string(),
        failure_count: 0,
        ..failed
    };
    assert!(has_successful_sync(Some(&synced)));
}

#[test]
fn local_usage_exits_first_run_state_without_dashboard_sync() {
    assert!(is_first_run(false, false, false));
    assert!(!is_first_run(false, false, true));
    assert!(!is_first_run(true, false, false));
    assert!(!is_first_run(false, true, false));
}

#[test]
fn first_run_unlinked_state_is_local_first() {
    let ui = build_ui(SnapshotUiInput {
        logged_in: false,
        first_run: true,
        has_synced: false,
        sync_failures: 0,
        pending_upload: false,
        pending_days: 0,
        week: PeriodStats {
            tokens: 12_000,
            requests: 4,
            cost_cents: None,
        },
        today: PeriodStats {
            tokens: 2_000,
            requests: 1,
            cost_cents: None,
        },
        last_sync_at: None,
    });

    assert_eq!(ui.menu_summary, "StatsAI is tracking locally");
    assert_eq!(ui.primary_action, PrimaryAction::Link);
    assert_eq!(ui.menu_stat_3, "Dashboard · not connected");
}

#[test]
fn pending_upload_copy_names_dashboard_state() {
    let ui = build_ui(SnapshotUiInput {
        logged_in: true,
        first_run: false,
        has_synced: true,
        sync_failures: 0,
        pending_upload: true,
        pending_days: 0,
        week: PeriodStats {
            tokens: 12_000,
            requests: 4,
            cost_cents: None,
        },
        today: PeriodStats {
            tokens: 2_000,
            requests: 1,
            cost_cents: None,
        },
        last_sync_at: Some(Utc::now() - Duration::minutes(30)),
    });

    assert_eq!(ui.menu_summary, "Dashboard sync available");
}

#[test]
fn retirement_only_pending_upload_precedes_no_usage_state() {
    let ui = build_ui(SnapshotUiInput {
        logged_in: true,
        first_run: false,
        has_synced: true,
        sync_failures: 0,
        pending_upload: true,
        pending_days: 0,
        week: PeriodStats {
            tokens: 0,
            requests: 0,
            cost_cents: None,
        },
        today: PeriodStats {
            tokens: 0,
            requests: 0,
            cost_cents: None,
        },
        last_sync_at: Some(Utc::now() - Duration::minutes(30)),
    });

    assert_eq!(ui.primary_action, PrimaryAction::UploadNow);
    assert_eq!(ui.menu_layout, "pending_upload");
}

#[test]
fn background_status_maps_launch_agent_state() {
    let running = background_status(service::BackgroundServiceState {
        plist_installed: true,
        launch_agent_loaded: true,
        daemon_reachable: true,
        daemon_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        stale: false,
    });
    assert!(running.installed);
    assert!(running.running);
    assert_eq!(running.label, "Tracking automatically");

    let missing = background_status(service::BackgroundServiceState {
        plist_installed: false,
        launch_agent_loaded: false,
        daemon_reachable: false,
        daemon_version: None,
        stale: false,
    });
    assert_eq!(missing.label, "Tracking setup needed");

    let stale = background_status(service::BackgroundServiceState {
        plist_installed: true,
        launch_agent_loaded: true,
        daemon_reachable: true,
        daemon_version: None,
        stale: true,
    });
    assert!(stale.installed);
    assert!(!stale.running);
    assert_eq!(stale.label, "Tracking needs restart");
}

#[test]
fn source_status_formats_tracking_disabled_and_missing_states() {
    let tracking = source_status(
        CODEX_PROVIDER,
        SourceStatusDraft {
            display_name: "Codex",
            configured: true,
            discovered: true,
            enabled: true,
            event_count: 42,
            token_count: 12_000,
            estimated_cost_cents: Some(123),
        },
    );
    assert_eq!(tracking.status, "tracking");
    assert_eq!(tracking.label, "Codex · 12k tokens · $1.23");
    assert!(tracking.has_data);

    let disabled = source_status(
        CLAUDE_CODE_PROVIDER,
        SourceStatusDraft {
            display_name: "Claude Code",
            configured: true,
            discovered: true,
            enabled: false,
            event_count: 0,
            token_count: 0,
            estimated_cost_cents: None,
        },
    );
    assert_eq!(disabled.status, "disabled");
    assert_eq!(disabled.label, "Claude Code · disabled");

    let missing = source_status(
        OPENCODE_PROVIDER,
        SourceStatusDraft {
            display_name: "OpenCode",
            configured: false,
            discovered: false,
            enabled: true,
            event_count: 0,
            token_count: 0,
            estimated_cost_cents: None,
        },
    );
    assert_eq!(missing.status, "not_found");
    assert_eq!(missing.label, "OpenCode");

    let detected = source_status(
        GROK_BUILD_PROVIDER,
        SourceStatusDraft {
            display_name: "Grok Build",
            configured: false,
            discovered: true,
            enabled: true,
            event_count: 0,
            token_count: 0,
            estimated_cost_cents: None,
        },
    );
    assert_eq!(detected.status, "found");
    assert_eq!(detected.label, "Grok Build · 0 tokens · $0");
}

#[test]
fn source_status_list_hides_unconfigured_empty_sources() {
    let dir = tempfile::tempdir().expect("tempdir");
    let discovered = statsai_core::SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "grok-build-local-sessions",
        "test",
        dir.path(),
        statsai_core::LocationOrigin::Default,
    );

    let statuses = build_source_statuses_with_discovered(
        &[],
        &HashMap::new(),
        vec![(GROK_BUILD_PROVIDER.to_string(), vec![discovered])],
    );

    assert!(!statuses
        .iter()
        .any(|status| status.provider == GROK_BUILD_PROVIDER));
}

#[test]
fn discovered_configured_source_is_not_counted_or_enabled_twice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut configured = statsai_core::SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "codex-local-jsonl",
        "test",
        dir.path(),
        statsai_core::LocationOrigin::Configured,
    );
    configured.enabled = false;
    let discovered = statsai_core::SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "codex-local-jsonl",
        "test",
        dir.path(),
        statsai_core::LocationOrigin::Default,
    );
    assert_eq!(configured.source_id, discovered.source_id);

    let mut totals = HashMap::new();
    totals.insert(
        CODEX_PROVIDER.to_string(),
        SourceUsageTotals {
            events: 5,
            tokens: 12_000,
            estimated_cost_cents: Some(123),
        },
    );

    let statuses = build_source_statuses_with_discovered(
        &[configured],
        &totals,
        vec![(CODEX_PROVIDER.to_string(), vec![discovered])],
    );
    let codex = statuses
        .iter()
        .find(|status| status.provider == CODEX_PROVIDER)
        .expect("codex status");

    assert!(codex.configured);
    assert!(codex.discovered);
    assert!(!codex.enabled);
    assert_eq!(codex.event_count, 5);
    assert_eq!(codex.token_count, 12_000);
    assert_eq!(codex.estimated_cost_cents, Some(123));
    assert_eq!(codex.status, "disabled");
    assert_eq!(codex.label, "Codex · disabled");
}

#[test]
fn scan_summary_prefers_today_then_week() {
    let week = PeriodStats {
        tokens: 100,
        requests: 7,
        cost_cents: None,
    };
    let today = PeriodStats {
        tokens: 10,
        requests: 2,
        cost_cents: None,
    };
    assert_eq!(
        format_scan_summary(&week, &today),
        "Last scan found 2 requests today"
    );

    let no_today = PeriodStats {
        tokens: 0,
        requests: 0,
        cost_cents: None,
    };
    assert_eq!(
        format_scan_summary(&week, &no_today),
        "Last scan found 7 requests in the last 7 days"
    );
}
