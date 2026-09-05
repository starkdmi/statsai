use super::support::*;
use super::*;

#[test]
fn replace_matching_summaries_targets_reported_imports_only() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::reported_usage(
        "claude_code",
        SourceKind::ExternalReport,
        "reported-usage-summary",
        "0",
        "external-report",
        None,
    );
    let local_source = SourceLocation::local_adapter(
        "claude_code",
        "claude-code-local-jsonl",
        "0",
        Path::new("/tmp/.claude"),
        LocationOrigin::Configured,
    );
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("now");
    let mut reported = test_summary("claude_code", &source, now, 100, None);
    reported.source.source_kind = SourceKind::ExternalReport;
    reported.metadata.summary_format = "external_daily".to_string();
    let mut local = test_summary("claude_code", &local_source, now, 200, None);
    local.source.source_kind = SourceKind::LocalSummary;
    local.metadata.summary_format = "external_daily".to_string();
    store.upsert_summary(&reported).expect("reported summary");
    store.upsert_summary(&local).expect("local summary");

    let record = ReportedUsageSummaryRecord {
        source,
        summary: reported.clone(),
    };
    let report = ReportedImportReport {
        path: PathBuf::from("reported_usage_summaries.json"),
        records: vec![ReportedImportRecord {
            record,
            legacy_replacement_source_ids: Vec::new(),
        }],
        warnings: Vec::new(),
    };

    let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");
    assert_eq!(matches, vec![reported.summary_id]);
}

#[test]
fn replace_matching_summaries_is_scoped_to_source_and_period() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::reported_usage(
        "claude_code",
        SourceKind::ExternalReport,
        "reported-usage-summary",
        "0",
        "reported-file-a",
        None,
    );
    let other_source = SourceLocation::reported_usage(
        "claude_code",
        SourceKind::ExternalReport,
        "reported-usage-summary",
        "0",
        "reported-file-b",
        None,
    );
    let now = Utc
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .single()
        .expect("now");

    let mut matching = test_summary("claude_code", &source, now, 100, None);
    matching.source.source_kind = SourceKind::ExternalReport;
    matching.metadata.summary_format = "external_daily".to_string();
    matching.period_start = Some(now - Duration::days(1));
    matching.period_end = Some(now);

    let mut same_file_different_day = test_summary("claude_code", &source, now, 200, None);
    same_file_different_day.summary_id = summary_id("claude_code", &source.source_id, "other-day");
    same_file_different_day.source.source_kind = SourceKind::ExternalReport;
    same_file_different_day.metadata.summary_format = "external_daily".to_string();
    same_file_different_day.period_start = Some(now - Duration::days(2));
    same_file_different_day.period_end = Some(now - Duration::days(1));

    let mut same_period_different_file = test_summary("claude_code", &other_source, now, 300, None);
    same_period_different_file.source.source_kind = SourceKind::ExternalReport;
    same_period_different_file.metadata.summary_format = "external_daily".to_string();
    same_period_different_file.period_start = matching.period_start;
    same_period_different_file.period_end = matching.period_end;

    store.upsert_summary(&matching).expect("matching summary");
    store
        .upsert_summary(&same_file_different_day)
        .expect("same file different day");
    store
        .upsert_summary(&same_period_different_file)
        .expect("same period different file");

    let incoming = ReportedUsageSummaryRecord {
        source,
        summary: matching.clone(),
    };
    let report = ReportedImportReport {
        path: PathBuf::from("reported-file-a.json"),
        records: vec![ReportedImportRecord {
            record: incoming,
            legacy_replacement_source_ids: Vec::new(),
        }],
        warnings: Vec::new(),
    };

    let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");

    assert_eq!(matches, vec![matching.summary_id]);
}

#[test]
fn replace_matching_summaries_matches_legacy_alias_formats_after_canonicalization() {
    let store = Store::in_memory().expect("store");
    let input = ReportedUsageSummaryInput {
        schema_version: "reported_usage_summary_input.v1".to_string(),
        provider: "claude_code".to_string(),
        provider_account_id: Some("acct-personal".to_string()),
        provider_user_id: None,
        email: None,
        account_label: Some("personal".to_string()),
        source_kind: SourceKind::Manual,
        source_name: "user_reported_usage".to_string(),
        evidence_id: Some("screenshot:2025-07-11".to_string()),
        evidence_path: Some("/tmp/user-report.png".to_string()),
        report_format: "ccusage_daily".to_string(),
        report_version: Some("manual.v1".to_string()),
        period_start: Some(
            Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                .single()
                .expect("start"),
        ),
        period_end: Some(
            Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                .single()
                .expect("end"),
        ),
        observed_at: None,
        model: None,
        usage: UsageCounts {
            total_tokens: Some(100),
            ..UsageCounts::default()
        },
        cost: None,
        confidence: Some(Confidence::Medium),
    };

    let incoming = build_reported_import_record(input, "device").expect("incoming");
    let mut legacy = incoming.record.summary.clone();
    legacy.metadata.summary_format = "ccusage_daily".to_string();
    legacy.source.source_type = "ccusage_daily".to_string();
    store
        .upsert_source(&incoming.record.source)
        .expect("source");
    store.upsert_summary(&legacy).expect("legacy summary");

    let report = ReportedImportReport {
        path: PathBuf::from("reported-file-a.json"),
        records: vec![incoming.clone()],
        warnings: Vec::new(),
    };

    let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");

    assert_eq!(matches, vec![legacy.summary_id]);
}

#[test]
fn replace_matching_summaries_matches_legacy_alias_formats_without_evidence() {
    let store = Store::in_memory().expect("store");
    let input = ReportedUsageSummaryInput {
        schema_version: "reported_usage_summary_input.v1".to_string(),
        provider: "claude_code".to_string(),
        provider_account_id: Some("acct-personal".to_string()),
        provider_user_id: None,
        email: None,
        account_label: Some("personal".to_string()),
        source_kind: SourceKind::Manual,
        source_name: "user_reported_usage".to_string(),
        evidence_id: None,
        evidence_path: None,
        report_format: "ccusage_daily".to_string(),
        report_version: Some("manual.v1".to_string()),
        period_start: Some(
            Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                .single()
                .expect("start"),
        ),
        period_end: Some(
            Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                .single()
                .expect("end"),
        ),
        observed_at: None,
        model: None,
        usage: UsageCounts {
            total_tokens: Some(100),
            ..UsageCounts::default()
        },
        cost: None,
        confidence: Some(Confidence::Medium),
    };

    let incoming = build_reported_import_record(input, "device").expect("incoming");
    let mut legacy = incoming.record.summary.clone();
    let legacy_source = SourceLocation::reported_usage(
        "claude_code",
        SourceKind::Manual,
        "reported-usage-summary",
        "0",
        "claude_code:user_reported_usage:acct-personal:ccusage_daily",
        None,
    );
    assert_ne!(legacy_source.source_id, incoming.record.source.source_id);
    legacy.source_id = legacy_source.source_id.clone();
    legacy.metadata.summary_format = "ccusage_daily".to_string();
    legacy.source.source_type = "ccusage_daily".to_string();
    legacy.source.source_path_hash = legacy_source.path_hash.clone();
    store.upsert_source(&legacy_source).expect("legacy source");
    store.upsert_summary(&legacy).expect("legacy summary");

    let other_legacy_source = SourceLocation::reported_usage(
        "claude_code",
        SourceKind::Manual,
        "reported-usage-summary",
        "0",
        "claude_code:other_report:acct-personal:ccusage_daily",
        None,
    );
    let mut other_legacy = legacy.clone();
    other_legacy.summary_id = summary_id(
        "claude_code",
        &other_legacy_source.source_id,
        "other-source-same-period",
    );
    other_legacy.source_id = other_legacy_source.source_id.clone();
    other_legacy.source.source_path_hash = other_legacy_source.path_hash.clone();
    store
        .upsert_source(&other_legacy_source)
        .expect("other legacy source");
    store
        .upsert_summary(&other_legacy)
        .expect("other legacy summary");

    let report = ReportedImportReport {
        path: PathBuf::from("reported-file-a.json"),
        records: vec![incoming.clone()],
        warnings: Vec::new(),
    };

    let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");

    assert_eq!(matches, vec![legacy.summary_id]);
    store
        .delete_summaries(&matches)
        .expect("delete legacy summary");
    assert_eq!(
        delete_orphaned_legacy_reported_sources(
            &store,
            &[ReportedImportReport {
                path: PathBuf::from("reported-file-a.json"),
                records: vec![incoming],
                warnings: Vec::new(),
            }]
        )
        .expect("delete legacy source"),
        1
    );
    assert!(store
        .source(&legacy_source.source_id)
        .expect("legacy source")
        .is_none());
    assert!(store
        .source(&other_legacy_source.source_id)
        .expect("other legacy source")
        .is_some());
}

#[test]
fn import_migrates_legacy_alias_summary_without_replace() {
    let store = Store::in_memory().expect("store");
    let input = ReportedUsageSummaryInput {
        schema_version: "reported_usage_summary_input.v1".to_string(),
        provider: "claude_code".to_string(),
        provider_account_id: Some("acct-personal".to_string()),
        provider_user_id: None,
        email: None,
        account_label: Some("personal".to_string()),
        source_kind: SourceKind::Manual,
        source_name: "user_reported_usage".to_string(),
        evidence_id: None,
        evidence_path: None,
        report_format: "manual_daily".to_string(),
        report_version: Some("manual.v1".to_string()),
        period_start: Some(
            Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                .single()
                .expect("start"),
        ),
        period_end: Some(
            Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                .single()
                .expect("end"),
        ),
        observed_at: None,
        model: None,
        usage: UsageCounts {
            total_tokens: Some(100),
            ..UsageCounts::default()
        },
        cost: None,
        confidence: Some(Confidence::Medium),
    };

    let incoming = build_reported_import_record(input, "device").expect("incoming");
    let canonical_source = incoming.record.source.clone();
    let canonical_source_id = incoming.record.source.source_id.clone();
    let canonical_summary_id = incoming.record.summary.summary_id.clone();
    let legacy_source = SourceLocation::reported_usage(
        "claude_code",
        SourceKind::Manual,
        "reported-usage-summary",
        "0",
        "claude_code:user_reported_usage:acct-personal:ccusage_daily",
        None,
    );
    let mut legacy = incoming.record.summary.clone();
    legacy.summary_id = summary_id("claude_code", &legacy_source.source_id, "legacy-alias");
    legacy.source_id = legacy_source.source_id.clone();
    legacy.metadata.summary_format = "ccusage_daily".to_string();
    legacy.source.source_type = "ccusage_daily".to_string();
    legacy.source.source_path_hash = legacy_source.path_hash.clone();
    let provider_account_id = ProviderAccountId("acct-personal".to_string());
    let legacy_assignment = test_assignment(
        &legacy_source,
        &provider_account_id,
        Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0)
            .single()
            .expect("assignment start"),
        None,
        Utc.with_ymd_and_hms(2025, 7, 12, 0, 0, 0)
            .single()
            .expect("assignment updated"),
    );
    let mut existing_canonical_assignment = test_assignment(
        &canonical_source,
        &provider_account_id,
        legacy_assignment.started_at,
        Some(
            Utc.with_ymd_and_hms(2025, 8, 1, 0, 0, 0)
                .single()
                .expect("canonical assignment end"),
        ),
        Utc.with_ymd_and_hms(2025, 7, 20, 0, 0, 0)
            .single()
            .expect("canonical assignment updated"),
    );
    existing_canonical_assignment.record_source = IdentitySource::SourceConfig;
    existing_canonical_assignment.verified_at = Some(
        Utc.with_ymd_and_hms(2025, 7, 20, 0, 0, 0)
            .single()
            .expect("canonical assignment verified"),
    );
    store
        .upsert_source(&canonical_source)
        .expect("canonical source");
    store
        .upsert_source_account_assignment(&existing_canonical_assignment)
        .expect("canonical assignment");
    store.upsert_source(&legacy_source).expect("legacy source");
    store.upsert_summary(&legacy).expect("legacy summary");
    store
        .upsert_source_account_assignment(&legacy_assignment)
        .expect("legacy assignment");

    let report = ReportedImportReport {
        path: PathBuf::from("reported-file-a.json"),
        records: vec![incoming],
        warnings: Vec::new(),
    };

    import_reported_summary_records(&store, &[report], false, false, false).expect("import");

    let summaries = store.summaries().expect("summaries");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].summary_id, canonical_summary_id);
    assert_eq!(summaries[0].source_id, canonical_source_id);
    assert!(store
        .source(&legacy_source.source_id)
        .expect("legacy source")
        .is_none());
    assert!(store
        .source(&canonical_source.source_id)
        .expect("canonical source")
        .is_some());
    assert!(store
        .list_source_account_assignments_for_source(&legacy_source.source_id)
        .expect("legacy assignments")
        .is_empty());
    let canonical_assignments = store
        .list_source_account_assignments_for_source(&canonical_source.source_id)
        .expect("canonical assignments");
    assert_eq!(canonical_assignments.len(), 1);
    assert_eq!(
        canonical_assignments[0].provider_account_id,
        provider_account_id
    );
    assert_eq!(
        canonical_assignments[0].started_at,
        legacy_assignment.started_at
    );
    assert_eq!(
        canonical_assignments[0].ended_at,
        existing_canonical_assignment.ended_at
    );
    assert_eq!(
        canonical_assignments[0].record_source,
        existing_canonical_assignment.record_source
    );
    assert_eq!(
        canonical_assignments[0].verified_at,
        existing_canonical_assignment.verified_at
    );
}

#[test]
fn import_migrates_evidence_backed_legacy_alias_summary_without_replace() {
    let store = Store::in_memory().expect("store");
    let input = ReportedUsageSummaryInput {
        schema_version: "reported_usage_summary_input.v1".to_string(),
        provider: "claude_code".to_string(),
        provider_account_id: Some("acct-personal".to_string()),
        provider_user_id: None,
        email: None,
        account_label: Some("personal".to_string()),
        source_kind: SourceKind::Manual,
        source_name: "user_reported_usage".to_string(),
        evidence_id: Some("screenshot:2025-07-11".to_string()),
        evidence_path: Some("/tmp/user-report.png".to_string()),
        report_format: "ccusage_daily".to_string(),
        report_version: Some("manual.v1".to_string()),
        period_start: Some(
            Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                .single()
                .expect("start"),
        ),
        period_end: Some(
            Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                .single()
                .expect("end"),
        ),
        observed_at: None,
        model: None,
        usage: UsageCounts {
            total_tokens: Some(100),
            ..UsageCounts::default()
        },
        cost: None,
        confidence: Some(Confidence::Medium),
    };

    let incoming = build_reported_import_record(input, "device").expect("incoming");
    let canonical_summary_id = incoming.record.summary.summary_id.clone();
    let mut legacy = incoming.record.summary.clone();
    legacy.summary_id = summary_id(
        "claude_code",
        &incoming.record.source.source_id,
        "legacy-evidence-alias",
    );
    legacy.metadata.summary_format = "ccusage_daily".to_string();
    legacy.source.source_type = "ccusage_daily".to_string();
    store
        .upsert_source(&incoming.record.source)
        .expect("source");
    store.upsert_summary(&legacy).expect("legacy summary");

    let report = ReportedImportReport {
        path: PathBuf::from("reported-file-a.json"),
        records: vec![incoming],
        warnings: Vec::new(),
    };

    import_reported_summary_records(&store, &[report], false, false, false).expect("import");

    let summaries = store.summaries().expect("summaries");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].summary_id, canonical_summary_id);
    assert_eq!(summaries[0].metadata.summary_format, "manual_daily");
}

#[test]
fn import_overlays_estimated_cost_when_store_ruleset_is_already_current() {
    let store = Store::in_memory().expect("store");
    store
        .ensure_current_pricing()
        .expect("apply current ruleset");
    let noop = store.ensure_current_pricing().expect("already current");
    assert!(noop.already_current);

    let period_end = Utc
        .with_ymd_and_hms(2026, 7, 29, 23, 59, 59)
        .single()
        .expect("end");
    let input = ReportedUsageSummaryInput {
        schema_version: "reported_usage_summary_input.v1".to_string(),
        provider: "codex".to_string(),
        provider_account_id: None,
        provider_user_id: None,
        email: None,
        account_label: None,
        source_kind: SourceKind::ExternalReport,
        source_name: "legacy_catalog_export".to_string(),
        evidence_id: Some("legacy-catalog:2026-07-29".to_string()),
        evidence_path: None,
        report_format: "manual_period_summary".to_string(),
        report_version: Some("manual.v1".to_string()),
        period_start: Some(
            Utc.with_ymd_and_hms(2026, 7, 29, 0, 0, 0)
                .single()
                .expect("start"),
        ),
        period_end: Some(period_end),
        observed_at: None,
        model: Some(ModelInfo {
            name: Some("codex-auto-review".to_string()),
            normalized_name: Some("codex-auto-review".to_string()),
            provider_model_id: Some("codex-auto-review".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        }),
        usage: UsageCounts {
            input_tokens: Some(1_000_000),
            cache_creation_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            total_tokens: Some(4_000_000),
            ..UsageCounts::default()
        },
        cost: Some(CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: Some(1),
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: Some(10_000),
            provider_reported_micro_usd: None,
            pricing_source: Some("official:stale".to_string()),
            pricing_version: Some("official:stale".to_string()),
            confidence: Confidence::Low,
        }),
        confidence: Some(Confidence::Medium),
    };

    let incoming = build_reported_import_record(input, "device").expect("incoming");
    assert_ne!(
        incoming.record.summary.cost.estimated_api_equivalent_usd,
        Some(1)
    );
    assert_eq!(
        incoming.record.summary.cost.pricing_version.as_deref(),
        Some(statsai_store::PRICING_CATALOG_VERSION)
    );
    assert!(incoming
        .record
        .summary
        .cost
        .estimated_api_equivalent_usd
        .is_some());
    let expected_usd = incoming.record.summary.cost.estimated_api_equivalent_usd;

    import_reported_summary_records(
        &store,
        &[ReportedImportReport {
            path: PathBuf::from("legacy-catalog.json"),
            records: vec![incoming],
            warnings: Vec::new(),
        }],
        false,
        false,
        false,
    )
    .expect("import");

    let stored = store.summaries().expect("summaries");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].cost.estimated_api_equivalent_usd, expected_usd);
    assert_eq!(
        stored[0].cost.pricing_version.as_deref(),
        Some(statsai_store::PRICING_CATALOG_VERSION)
    );
    let still_current = store.ensure_current_pricing().expect("still current");
    assert!(still_current.already_current);
    assert_eq!(
        store.summaries().expect("unchanged")[0]
            .cost
            .estimated_api_equivalent_usd,
        expected_usd
    );
}

use super::super::import::dedupe_cursor_snapshots;
use statsai_adapters::parse_cursor_usage_csv;

fn cursor_fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../statsai-adapters/tests/fixtures/cursor")
        .join(relative)
}

fn import_cursor(store: &Store, relative: &str) {
    import_cursor_usage_events(&[cursor_fixture(relative)], store, "device", false, false)
        .expect("import cursor csv");
}

fn stored_totals(store: &Store) -> (usize, u64) {
    let events = store.events().expect("events");
    let total = events
        .iter()
        .map(|event| event.usage.computed_total())
        .sum();
    (events.len(), total)
}

#[test]
fn cursor_import_order_does_not_change_the_result() {
    let forwards = Store::in_memory().expect("store");
    import_cursor(&forwards, "snapshots/export-early.csv");
    import_cursor(&forwards, "snapshots/export-late.csv");

    let backwards = Store::in_memory().expect("store");
    import_cursor(&backwards, "snapshots/export-late.csv");
    import_cursor(&backwards, "snapshots/export-early.csv");

    assert_eq!(stored_totals(&forwards), stored_totals(&backwards));
    // 40000 (grown) + 3000 (unchanged) + 1000 (new in the later export).
    assert_eq!(stored_totals(&forwards), (3, 44_000));
}

#[test]
fn cursor_import_keeps_the_larger_snapshot_of_a_growing_row() {
    let store = Store::in_memory().expect("store");
    import_cursor(&store, "snapshots/export-late.csv");
    let after_late = stored_totals(&store);

    import_cursor(&store, "snapshots/export-early.csv");

    assert_eq!(stored_totals(&store), after_late);
}

#[test]
fn cursor_import_is_idempotent() {
    let store = Store::in_memory().expect("store");
    import_cursor(&store, "basic/usage-events-basic.csv");
    let first = stored_totals(&store);

    import_cursor(&store, "basic/usage-events-basic.csv");

    assert_eq!(stored_totals(&store), first);
    assert_eq!(first.0, 6);
}

#[test]
fn cursor_import_preserves_a_reported_charge_through_the_store() {
    let store = Store::in_memory().expect("store");
    import_cursor(&store, "usage-based/usage-events-charged.csv");
    store.ensure_current_pricing().expect("refresh pricing");

    let charged: Vec<_> = store
        .events()
        .expect("events")
        .into_iter()
        .filter(|event| event.cost.provider_reported_micro_usd.is_some())
        .collect();

    assert_eq!(charged.len(), 2);
    let mut amounts: Vec<i64> = charged
        .iter()
        .filter_map(|event| event.cost.provider_reported_micro_usd)
        .collect();
    amounts.sort_unstable();
    assert_eq!(amounts, vec![123_400, 2_500_000]);
    assert!(charged
        .iter()
        .all(|event| event.cost.pricing_source.as_deref() == Some("cursor.usage_event.cost")));
}

#[test]
fn cursor_import_dry_run_writes_nothing() {
    let store = Store::in_memory().expect("store");

    import_cursor_usage_events(
        &[cursor_fixture("basic/usage-events-basic.csv")],
        &store,
        "device",
        true,
        true,
    )
    .expect("dry run");

    assert_eq!(stored_totals(&store), (0, 0));
    assert!(store.list_sources().expect("sources").is_empty());
}

#[test]
fn cursor_import_accepts_a_directory_of_exports() {
    let store = Store::in_memory().expect("store");

    import_cursor_usage_events(
        &[cursor_fixture("snapshots")],
        &store,
        "device",
        false,
        false,
    )
    .expect("directory import");

    // Both exports in the directory land in one pass, and the overlapping row
    // resolves to the larger snapshot rather than being counted twice.
    assert_eq!(stored_totals(&store), (3, 44_000));
}

#[test]
fn cursor_import_registers_one_manual_source() {
    let store = Store::in_memory().expect("store");
    import_cursor(&store, "snapshots/export-early.csv");
    import_cursor(&store, "snapshots/export-late.csv");

    let sources = store.list_sources().expect("sources");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].provider, "cursor");
    assert_eq!(sources[0].source_kind, SourceKind::Manual);
    // Nothing on disk backs this source, so verification must stay off.
    assert_eq!(
        sources[0].verification_mode,
        statsai_core::SourceVerificationMode::Disabled
    );
}

#[test]
fn cursor_import_totals_match_what_the_store_keeps_across_overlapping_exports() {
    let store = Store::in_memory().expect("store");

    // The two snapshots overlap: the grok row appears in both, grown in the
    // later one. Totals must reflect the winner, not the sum of both copies.
    import_cursor_usage_events(
        &[cursor_fixture("snapshots")],
        &store,
        "device",
        false,
        false,
    )
    .expect("directory import");

    assert_eq!(stored_totals(&store), (3, 44_000));
}

#[test]
fn cursor_snapshot_dedupe_reports_the_totals_the_store_will_keep() {
    let options = ScanOptions {
        device_id: "device".to_string(),
        collect_tasks: false,
        selected_cache_keys: None,
    };
    let mut parsed = Vec::new();
    for name in ["snapshots/export-early.csv", "snapshots/export-late.csv"] {
        parsed.extend(
            parse_cursor_usage_csv(&cursor_fixture(name), &options)
                .expect("parse")
                .events,
        );
    }
    // Summing the exports would report 5 events and 57,000 tokens.
    assert_eq!(parsed.len(), 5);
    assert_eq!(
        parsed
            .iter()
            .map(|event| event.usage.computed_total())
            .sum::<u64>(),
        57_000
    );

    let deduped = dedupe_cursor_snapshots(parsed);

    let store = Store::in_memory().expect("store");
    import_cursor(&store, "snapshots/export-early.csv");
    import_cursor(&store, "snapshots/export-late.csv");
    assert_eq!(
        (
            deduped.len(),
            deduped
                .iter()
                .map(|event| event.usage.computed_total())
                .sum::<u64>()
        ),
        stored_totals(&store)
    );
    assert_eq!(stored_totals(&store), (3, 44_000));
}

#[test]
fn cursor_reimport_preserves_a_connected_account() {
    let store = Store::in_memory().expect("store");
    import_cursor(&store, "basic/usage-events-basic.csv");

    // Connect the source the way `statsai source connect` would, covering the
    // whole fixture range.
    let source = statsai_adapters::cursor_import_source();
    let account_id = statsai_core::ProviderAccountId("cursor-account".to_string());
    let started_at = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("started_at");
    store
        .upsert_source_account_assignment(&test_assignment(
            &source,
            &account_id,
            started_at,
            None,
            started_at,
        ))
        .expect("assignment");
    super::super::source::reattribute_source_records(&store, &source.source_id)
        .expect("reattribute");
    let linked = |store: &Store| {
        store
            .events()
            .expect("events")
            .into_iter()
            .filter(|event| event.provider_account_id.as_ref() == Some(&account_id))
            .count()
    };
    assert_eq!(linked(&store), 6);

    // Re-importing the same export must not strip the attribution.
    import_cursor(&store, "basic/usage-events-basic.csv");
    assert_eq!(linked(&store), 6);

    // A later export covering new activity picks up the connection too.
    import_cursor(&store, "snapshots/export-late.csv");
    assert_eq!(linked(&store), 9);
}
