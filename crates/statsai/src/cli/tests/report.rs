use super::support::*;
use super::*;

#[test]
fn report_range_cli_requires_from_or_to() {
    let error =
        Cli::try_parse_from(["statsai", "report", "range"]).expect_err("range without bounds");
    let message = error.to_string();
    assert!(
        message.contains("--from") || message.contains("--to"),
        "{message}"
    );
}

#[test]
fn report_range_cli_parses_from_and_to() {
    let cli = Cli::try_parse_from([
        "statsai",
        "report",
        "range",
        "--from",
        "2026-01-01",
        "--to",
        "2026-03-31",
        "--json",
    ])
    .expect("parse report range");
    assert!(matches!(
        cli.command,
        Command::Report(ReportCommand {
            command: ReportSubcommand::Range {
                from: Some(ref from),
                to: Some(ref to),
                json: true,
                verbose: false,
                subscriptions: false,
            },
        }) if from == "2026-01-01" && to == "2026-03-31"
    ));
}

#[test]
fn report_range_cli_rfc3339_midnight_keeps_timestamp_label() {
    let store = Store::in_memory().expect("store");
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("now");
    let command = ReportCommand {
        command: ReportSubcommand::Range {
            from: Some("2026-05-01T00:00:00Z".to_string()),
            to: Some("2026-05-15".to_string()),
            json: false,
            verbose: false,
            subscriptions: false,
        },
    };
    let (report, ..) =
        usage_report_from_command(command, &store, now).expect("rfc3339 midnight from");
    assert_eq!(report.label, "2026-05-01T00:00:00+00:00 to 2026-05-15");
}

#[test]
fn report_range_cli_filters_stored_events() {
    let store = Store::in_memory().expect("store");
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("now");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-report-range"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let before = test_event(
        "codex",
        &source,
        Utc.with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("before"),
        None,
        TokenParts::total(50),
    );
    let inside = test_event(
        "codex",
        &source,
        Utc.with_ymd_and_hms(2026, 5, 10, 18, 0, 0)
            .single()
            .expect("inside"),
        None,
        TokenParts::total(100),
    );
    let after = test_event(
        "codex",
        &source,
        Utc.with_ymd_and_hms(2026, 5, 20, 9, 0, 0)
            .single()
            .expect("after"),
        None,
        TokenParts::total(200),
    );
    store
        .insert_events(&[before, inside, after])
        .expect("insert events");

    let command = ReportCommand {
        command: ReportSubcommand::Range {
            from: Some("2026-05-01".to_string()),
            to: Some("2026-05-15".to_string()),
            json: true,
            verbose: false,
            subscriptions: false,
        },
    };
    let (report, json, verbose, subscriptions) =
        usage_report_from_command(command, &store, now).expect("range report");

    assert!(json);
    assert!(!verbose);
    assert!(!subscriptions);
    assert_eq!(report.label, "2026-05-01 to 2026-05-15");
    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 100);
}

#[test]
fn report_range_cli_from_only_includes_events_through_now() {
    let store = Store::in_memory().expect("store");
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("now");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-report-from-only"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let before = test_event(
        "codex",
        &source,
        Utc.with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("before"),
        None,
        TokenParts::total(50),
    );
    let inside = test_event(
        "codex",
        &source,
        Utc.with_ymd_and_hms(2026, 5, 10, 18, 0, 0)
            .single()
            .expect("inside"),
        None,
        TokenParts::total(100),
    );
    store
        .insert_events(&[before, inside])
        .expect("insert events");

    let command = ReportCommand {
        command: ReportSubcommand::Range {
            from: Some("2026-05-01".to_string()),
            to: None,
            json: false,
            verbose: false,
            subscriptions: false,
        },
    };
    let (report, ..) = usage_report_from_command(command, &store, now).expect("from-only");
    assert_eq!(report.label, "2026-05-01 to 2026-05-25T12:00:00+00:00");
    assert_eq!(report.until, now);
    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 100);
}

#[test]
fn report_range_cli_to_only_includes_events_through_end_date() {
    let store = Store::in_memory().expect("store");
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("now");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-report-to-only"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let inside = test_event(
        "codex",
        &source,
        Utc.with_ymd_and_hms(2026, 5, 10, 18, 0, 0)
            .single()
            .expect("inside"),
        None,
        TokenParts::total(100),
    );
    let after = test_event(
        "codex",
        &source,
        Utc.with_ymd_and_hms(2026, 5, 20, 9, 0, 0)
            .single()
            .expect("after"),
        None,
        TokenParts::total(200),
    );
    store
        .insert_events(&[inside, after])
        .expect("insert events");

    let command = ReportCommand {
        command: ReportSubcommand::Range {
            from: None,
            to: Some("2026-05-15".to_string()),
            json: false,
            verbose: false,
            subscriptions: false,
        },
    };
    let (report, ..) = usage_report_from_command(command, &store, now).expect("to-only");
    assert_eq!(report.label, "through 2026-05-15");
    assert_eq!(report.since, None);
    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 100);
}

#[test]
fn report_range_cli_to_only_includes_pre_unix_events() {
    let store = Store::in_memory().expect("store");
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("now");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-report-pre-unix"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let pre_unix = Utc
        .with_ymd_and_hms(1969, 12, 31, 12, 0, 0)
        .single()
        .expect("pre-unix");
    store
        .insert_events(&[test_event(
            "codex",
            &source,
            pre_unix,
            None,
            TokenParts::total(40),
        )])
        .expect("insert events");

    let command = ReportCommand {
        command: ReportSubcommand::Range {
            from: None,
            to: Some("1969-12-31".to_string()),
            json: false,
            verbose: false,
            subscriptions: false,
        },
    };
    let (report, ..) = usage_report_from_command(command, &store, now).expect("pre-unix range");
    assert_eq!(report.label, "through 1969-12-31");
    assert_eq!(report.since, None);
    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 40);
}

#[test]
fn report_range_cli_future_window_is_empty_not_an_error() {
    let store = Store::in_memory().expect("store");
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("now");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-report-future"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let present = test_event("codex", &source, now, None, TokenParts::total(50));
    store.insert_events(&[present]).expect("insert events");

    let command = ReportCommand {
        command: ReportSubcommand::Range {
            from: Some("2026-09-01".to_string()),
            to: Some("2026-09-30".to_string()),
            json: false,
            verbose: false,
            subscriptions: false,
        },
    };
    let (report, ..) = usage_report_from_command(command, &store, now).expect("future range");
    assert_eq!(report.label, "2026-09-01 to 2026-09-30 (empty)");
    assert_eq!(report.since, Some(now));
    assert_eq!(report.until, now);
    assert!(report.since.is_some_and(|since| since <= report.until));
    assert_eq!(report.total_events, 0);
}

#[test]
fn report_range_cli_future_from_only_is_empty_not_an_error() {
    let store = Store::in_memory().expect("store");
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("now");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-report-future-from"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let present = test_event("codex", &source, now, None, TokenParts::total(50));
    store.insert_events(&[present]).expect("insert events");

    let command = ReportCommand {
        command: ReportSubcommand::Range {
            from: Some("2026-09-01".to_string()),
            to: None,
            json: false,
            verbose: false,
            subscriptions: false,
        },
    };
    let (report, ..) = usage_report_from_command(command, &store, now).expect("future from-only");
    assert_eq!(report.label, "from 2026-09-01 (empty)");
    assert_eq!(report.since, Some(now));
    assert_eq!(report.until, now);
    assert!(report.since.is_some_and(|since| since <= report.until));
    assert_eq!(report.total_events, 0);
}

#[test]
fn usage_report_filters_period_and_groups_by_canonical_account() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("date");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-report"),
        LocationOrigin::Configured,
    );
    let account_id = provider_account_id("codex", "personal@example.com");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: account_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::UserConfigured,
        provider_user_id: None,
        provider_user_id_hash: None,
        email: Some("personal@example.com".to_string()),
        email_hash: None,
        org_id_hash: None,
        account_label: Some("personal".to_string()),
        plan_name: None,
        confidence: Confidence::High,
        verified_at: None,
        created_at: now,
        updated_at: now,
    };
    let recent = test_event(
        "codex",
        &source,
        now - Duration::days(1),
        Some(account_id.clone()),
        TokenParts {
            input: 70,
            cached_input: 20,
            output: 25,
            reasoning: 5,
            total: 100,
            cost: Some(1),
        },
    );
    let old = test_event(
        "codex",
        &source,
        now - Duration::days(10),
        Some(account_id),
        TokenParts {
            input: 120,
            cached_input: 30,
            output: 50,
            reasoning: 0,
            total: 200,
            cost: Some(1),
        },
    );

    let report = build_usage_report(
        &[recent, old],
        &[],
        &[source],
        &[account],
        &[],
        ReportPeriod::LastDays(7),
        now,
    );

    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 100);
    assert_eq!(report.total_usage.input_tokens, 70);
    assert_eq!(report.total_usage.cached_input_tokens, 20);
    assert_eq!(report.total_usage.output_tokens, 25);
    assert_eq!(report.total_usage.reasoning_tokens, 5);
    assert_eq!(report.total_usage.estimated_cost_usd, Some(1));
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].account, "personal");
}

#[test]
fn usage_report_uses_account_registry_label() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("date");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-report-account"),
        LocationOrigin::Configured,
    );
    let account_id = provider_account_id("codex", "stable-provider-id");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: account_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::UserConfigured,
        provider_user_id: None,
        provider_user_id_hash: None,
        email: None,
        email_hash: None,
        org_id_hash: None,
        account_label: Some("work".to_string()),
        plan_name: None,
        confidence: Confidence::Medium,
        verified_at: None,
        created_at: now,
        updated_at: now,
    };
    let event = test_event(
        "codex",
        &source,
        now,
        Some(account_id),
        TokenParts::total(50),
    );

    let report = build_usage_report(
        &[event],
        &[],
        &[source],
        &[account],
        &[],
        ReportPeriod::AllTime,
        now,
    );

    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].account, "work");
    assert_eq!(report.rows[0].usage.total_tokens, 50);
}

#[test]
fn usage_report_keeps_summary_cache_separate_from_event_totals() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("date");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-report-summary"),
        LocationOrigin::Configured,
    );
    let account_id = provider_account_id("claude_code", "personal@example.com");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: account_id.clone(),
        provider: "claude_code".to_string(),
        identity_source: IdentitySource::UserConfigured,
        provider_user_id: None,
        provider_user_id_hash: None,
        email: Some("personal@example.com".to_string()),
        email_hash: None,
        org_id_hash: None,
        account_label: Some("personal".to_string()),
        plan_name: None,
        confidence: Confidence::High,
        verified_at: None,
        created_at: now,
        updated_at: now,
    };
    let event = test_event(
        "claude_code",
        &source,
        now,
        Some(account_id.clone()),
        TokenParts::total(100),
    );
    let summary = test_summary("claude_code", &source, now, 500, Some(account_id.clone()));

    let report = build_usage_report(
        &[event],
        &[summary],
        std::slice::from_ref(&source),
        std::slice::from_ref(&account),
        &[],
        ReportPeriod::AllTime,
        now,
    );

    assert_eq!(report.total_usage.total_tokens, 100);
    assert_eq!(report.total_summary_usage.total_tokens, 500);
    assert_eq!(report.summary_rows.len(), 1);
    assert_eq!(report.summary_rows[0].account, "personal");
    assert_eq!(report.summary_rows[0].direct_event_usage.total_tokens, 100);

    let weekly = build_usage_report(
        &[],
        &[test_summary(
            "claude_code",
            &source,
            now,
            500,
            Some(account_id),
        )],
        std::slice::from_ref(&source),
        std::slice::from_ref(&account),
        &[],
        ReportPeriod::LastDays(7),
        now,
    );
    assert!(weekly.summary_rows.is_empty());
}

#[test]
fn usage_report_keeps_summary_formats_separate() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("date");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-report-summary-kinds"),
        LocationOrigin::Configured,
    );
    let account_id = provider_account_id("claude_code", "personal@example.com");
    let mut stats_cache = test_summary("claude_code", &source, now, 500, Some(account_id.clone()));
    stats_cache.metadata.summary_format = "claude_stats_cache".to_string();
    let mut external = test_summary("claude_code", &source, now, 300, Some(account_id));
    external.summary_id = summary_id("claude_code", &source.source_id, "external");
    external.metadata.summary_format = "external_daily".to_string();

    let report = build_usage_report(
        &[],
        &[stats_cache, external],
        std::slice::from_ref(&source),
        &[],
        &[],
        ReportPeriod::AllTime,
        now,
    );

    assert_eq!(report.summary_rows.len(), 2);
    assert!(report
        .summary_rows
        .iter()
        .any(|row| row.kind == "claude_stats_cache" && row.usage.total_tokens == 500));
    assert!(report
        .summary_rows
        .iter()
        .any(|row| row.kind == "external_daily" && row.usage.total_tokens == 300));
}
