use super::*;

#[test]
fn scan_skips_files_when_legacy_codex_auth_signature_is_cached() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-legacy-auth-cache"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-legacy-auth-cache/session.jsonl";
    let current_candidate = ScanCandidateFile {
        path: PathBuf::from(file_path),
        cache_key: file_path.to_string(),
        cache_signature: "sig-current".to_string(),
        compatible_cache_signatures: vec!["sig-legacy-auth".to_string()],
    };
    store
        .record_scan_file_entries(
            &source.source_id,
            &[ScanFileStateEntry {
                cache_key: current_candidate.cache_key.clone(),
                cache_signature: "sig-legacy-auth".to_string(),
            }],
        )
        .expect("record legacy scan cache");

    let scan_calls = Arc::new(Mutex::new(0u64));
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![current_candidate],
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: Some(scan_calls.clone()),
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    assert_eq!(*scan_calls.lock().expect("scan calls"), 0);

    let stored_entries = store
        .scan_file_entries(&source.source_id)
        .expect("stored scan file entries");
    assert_eq!(
        stored_entries,
        vec![ScanFileStateEntry {
            cache_key: file_path.to_string(),
            cache_signature: "sig-current".to_string(),
        }]
    );

    let second_scan_calls = Arc::new(Mutex::new(0u64));
    let rotated_legacy_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![ScanCandidateFile {
            path: PathBuf::from(file_path),
            cache_key: file_path.to_string(),
            cache_signature: "sig-current".to_string(),
            compatible_cache_signatures: vec!["sig-legacy-auth-rotated".to_string()],
        }],
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: Some(second_scan_calls.clone()),
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(rotated_legacy_adapter)],
    )
    .expect("second scan");

    assert_eq!(*second_scan_calls.lock().expect("scan calls"), 0);
}

#[test]
fn no_cache_scan_reselects_unchanged_files() {
    let store = Store::in_memory().expect("store");
    let source_id = statsai_core::SourceId("src-no-cache".to_string());
    let compatible_signatures = HashMap::new();
    let entries = vec![
        ScanFileStateEntry {
            cache_key: "/tmp/a.jsonl".to_string(),
            cache_signature: "sig-a-1".to_string(),
        },
        ScanFileStateEntry {
            cache_key: "/tmp/b.jsonl".to_string(),
            cache_signature: "sig-b-1".to_string(),
        },
    ];

    let initial = select_scan_file_entries(
        &store,
        &source_id,
        &entries,
        &compatible_signatures,
        false,
        false,
        false,
    )
    .expect("initial selection");
    assert_eq!(initial, entries);
    store
        .record_scan_file_entries(&source_id, &entries)
        .expect("record cache state");

    let default_selection = select_scan_file_entries(
        &store,
        &source_id,
        &entries,
        &compatible_signatures,
        false,
        false,
        false,
    )
    .expect("default selection");
    assert!(default_selection.is_empty());

    let no_cache_selection = select_scan_file_entries(
        &store,
        &source_id,
        &entries,
        &compatible_signatures,
        false,
        true,
        false,
    )
    .expect("no-cache selection");
    assert_eq!(no_cache_selection, entries);

    let replace_selection = select_scan_file_entries(
        &store,
        &source_id,
        &entries,
        &compatible_signatures,
        true,
        false,
        false,
    )
    .expect("replace selection");
    assert_eq!(replace_selection, entries);
}

#[test]
fn full_source_rescan_replaces_existing_source_records() {
    assert!(should_replace_source_records_for_scan(
        true, false, 0, 0, false
    ));
    assert!(should_replace_source_records_for_scan(
        false, true, 0, 0, false
    ));
    assert!(should_replace_source_records_for_scan(
        false, false, 2, 2, false
    ));
    assert!(should_replace_source_records_for_scan(
        false, false, 0, 0, true
    ));
    assert!(!should_replace_source_records_for_scan(
        false, false, 2, 1, false
    ));
    assert!(!should_replace_source_records_for_scan(
        false, false, 0, 0, false
    ));
}

#[test]
fn cache_invalidation_reconciles_quota_records_by_file() {
    assert!(!should_replace_all_source_quota_records(false, false));
    assert!(should_replace_all_source_quota_records(true, false));
    assert!(should_replace_all_source_quota_records(false, true));
}

#[test]
fn no_cache_rescan_reconciles_quota_records_instead_of_deleting_the_source() {
    // `--no-cache` rereads every file, so the file-level path already rewrites everything it
    // produces. Deleting the source first walked every observation and window on a store with
    // six figures of rows, which is the stall a documented flag must not have.
    assert!(!should_replace_all_source_quota_records(false, false));
    // The full reread still replaces the source's records, so the reconciliation branch --
    // the one that also retires rows outside the rescanned file set -- is the branch it takes.
    assert!(should_replace_source_records_for_scan(
        false, true, 0, 0, false
    ));
    // An explicit destructive rebuild keeps the blanket delete.
    assert!(should_replace_all_source_quota_records(true, false));
}

#[test]
fn scan_file_reconciliation_tracks_removed_candidates() {
    let store = Store::in_memory().expect("store");
    let source_id = statsai_core::SourceId("src-removed-cache".to_string());
    let tracked = vec![
        ScanFileStateEntry {
            cache_key: "/tmp/a.jsonl".to_string(),
            cache_signature: "sig-a-1".to_string(),
        },
        ScanFileStateEntry {
            cache_key: "/tmp/b.jsonl".to_string(),
            cache_signature: "sig-b-1".to_string(),
        },
    ];
    store
        .record_scan_file_entries(&source_id, &tracked)
        .expect("record tracked cache state");

    let reconciliation = select_scan_file_reconciliation(
        &store,
        &source_id,
        &[ScanFileStateEntry {
            cache_key: "/tmp/b.jsonl".to_string(),
            cache_signature: "sig-b-1".to_string(),
        }],
        &HashMap::new(),
        false,
        false,
        false,
    )
    .expect("reconciliation");

    assert!(reconciliation.pending_entries.is_empty());
    assert_eq!(
        reconciliation.removed_entries,
        vec![ScanFileStateEntry {
            cache_key: "/tmp/a.jsonl".to_string(),
            cache_signature: "sig-a-1".to_string(),
        }]
    );
}

#[test]
fn partial_scan_removes_rows_that_disappear_from_changed_file() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-partial-rescan"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_a = "/tmp/codex-partial-rescan/a.jsonl";
    let file_b = "/tmp/codex-partial-rescan/b.jsonl";
    let initial_candidates = vec![
        test_scan_candidate(file_a, "sig-a-1"),
        test_scan_candidate(file_b, "sig-b-1"),
    ];
    let next_candidates = vec![
        test_scan_candidate(file_a, "sig-a-2"),
        test_scan_candidate(file_b, "sig-b-1"),
    ];
    let a_started_at = Utc
        .with_ymd_and_hms(2026, 5, 1, 10, 0, 0)
        .single()
        .expect("a_started_at");
    let b_started_at = Utc
        .with_ymd_and_hms(2026, 5, 2, 10, 0, 0)
        .single()
        .expect("b_started_at");
    let initial_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: initial_candidates,
        scan_result: statsai_adapters::AdapterScan {
            events: vec![
                test_scan_event(&source, file_a, a_started_at, "event-a", 100),
                test_scan_event(&source, file_b, b_started_at, "event-b", 200),
            ],
            summaries: vec![
                test_scan_summary(&source, file_a, a_started_at, "summary-a", 100),
                test_scan_summary(&source, file_b, b_started_at, "summary-b", 200),
            ],
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(initial_adapter)],
    )
    .expect("initial scan");

    assert_eq!(store.event_count().expect("event count"), 2);
    assert_eq!(store.summary_count().expect("summary count"), 2);
    assert_eq!(store.sync_rollup_count().expect("rollup count"), 2);

    let changed_only_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: next_candidates,
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(changed_only_adapter)],
    )
    .expect("partial scan");

    let events = store.events_for_source(&source.source_id).expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_record_id.as_deref()),
        Some("event-b")
    );
    let summaries = store
        .summaries_for_source(&source.source_id)
        .expect("summaries");
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].summary_id,
        summary_id("codex", &source.source_id, "summary-b")
    );
    assert_eq!(store.sync_rollup_count().expect("rollup count"), 1);
}

#[test]
fn scan_with_include_tasks_backfills_files_cached_without_tasks() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-backfill"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-backfill/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 9, 38, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "event-task", 150);
    let task_span = test_task_span(
        &source,
        file_path,
        started_at,
        "task-span-a",
        "Backfill local tasks",
        &event,
    );
    let candidate = test_scan_candidate(file_path, "sig-a");
    let initial_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![candidate.clone()],
        scan_result: statsai_adapters::AdapterScan {
            events: vec![event.clone()],
            task_spans: vec![task_span.clone()],
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(initial_adapter)],
    )
    .expect("initial scan");
    assert!(store.task_spans().expect("initial task spans").is_empty());

    let scan_calls = Arc::new(Mutex::new(0u64));
    let backfill_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![candidate],
        scan_result: statsai_adapters::AdapterScan {
            events: vec![event],
            task_spans: vec![task_span],
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: Some(scan_calls.clone()),
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: true,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(backfill_adapter)],
    )
    .expect("task backfill scan");

    assert_eq!(*scan_calls.lock().expect("scan calls"), 1);
    let spans = store.task_spans().expect("task spans");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].title, "Backfill local tasks");
    let work_items = store.work_items().expect("work items");
    assert_eq!(work_items.len(), 1);
    assert_eq!(work_items[0].title, "Backfill local tasks");
}

#[test]
fn partial_scan_removes_stale_task_spans_and_rebuilds_work_items() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-partial-rescan"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_a = "/tmp/codex-task-partial-rescan/a.jsonl";
    let file_b = "/tmp/codex-task-partial-rescan/b.jsonl";
    let initial_candidates = vec![
        test_scan_candidate(file_a, "sig-a-1"),
        test_scan_candidate(file_b, "sig-b-1"),
    ];
    let next_candidates = vec![
        test_scan_candidate(file_a, "sig-a-2"),
        test_scan_candidate(file_b, "sig-b-1"),
    ];
    let a_started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 11, 0, 0)
        .single()
        .expect("a_started_at");
    let b_started_at = Utc
        .with_ymd_and_hms(2026, 6, 14, 11, 0, 0)
        .single()
        .expect("b_started_at");
    let event_a = test_scan_event(&source, file_a, a_started_at, "event-a", 100);
    let event_b = test_scan_event(&source, file_b, b_started_at, "event-b", 200);
    let mut span_a = test_task_span(
        &source,
        file_a,
        a_started_at,
        "span-a",
        "Implement task cleanup",
        &event_a,
    );
    let mut span_b = test_task_span(
        &source,
        file_b,
        b_started_at,
        "span-b",
        "Implement task benchmark reporting",
        &event_b,
    );
    span_a.session_id = Some("session-a".to_string());
    span_b.session_id = Some("session-b".to_string());

    let initial_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: initial_candidates,
        scan_result: statsai_adapters::AdapterScan {
            events: vec![event_a.clone(), event_b.clone()],
            task_spans: vec![span_a, span_b.clone()],
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: true,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(initial_adapter)],
    )
    .expect("initial scan");

    assert_eq!(store.task_spans().expect("task spans").len(), 2);
    assert_eq!(store.work_items().expect("work items").len(), 2);

    let changed_only_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: next_candidates,
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: true,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(changed_only_adapter)],
    )
    .expect("partial scan");

    let spans = store.task_spans().expect("task spans");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].source_record_id.as_deref(), Some("span-b"));

    let work_items = store.work_items().expect("work items");
    assert_eq!(work_items.len(), 1);
    assert_eq!(work_items[0].title, span_b.title);
}

#[test]
fn partial_scan_with_legacy_rows_falls_back_to_full_source_reconcile() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-partial-legacy"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_a = "/tmp/codex-partial-legacy/a.jsonl";
    let file_b = "/tmp/codex-partial-legacy/b.jsonl";
    let tracked_entries = vec![
        ScanFileStateEntry {
            cache_key: file_a.to_string(),
            cache_signature: "sig-a-1".to_string(),
        },
        ScanFileStateEntry {
            cache_key: file_b.to_string(),
            cache_signature: "sig-b-1".to_string(),
        },
    ];
    store
        .record_scan_file_entries(&source.source_id, &tracked_entries)
        .expect("record initial cache");

    let a_started_at = Utc
        .with_ymd_and_hms(2026, 5, 3, 10, 0, 0)
        .single()
        .expect("a_started_at");
    let b_started_at = Utc
        .with_ymd_and_hms(2026, 5, 4, 10, 0, 0)
        .single()
        .expect("b_started_at");
    let legacy_event_a = test_event("codex", &source, a_started_at, None, TokenParts::total(50));
    let legacy_event_b = test_event("codex", &source, b_started_at, None, TokenParts::total(75));
    let mut legacy_summary_a = test_summary("codex", &source, a_started_at, 50, None);
    legacy_summary_a.summary_id = summary_id("codex", &source.source_id, "legacy-summary-a");
    let mut legacy_summary_b = test_summary("codex", &source, b_started_at, 75, None);
    legacy_summary_b.summary_id = summary_id("codex", &source.source_id, "legacy-summary-b");
    store
        .insert_events(&[legacy_event_a, legacy_event_b])
        .expect("seed legacy events");
    store
        .upsert_summaries(&[legacy_summary_a, legacy_summary_b])
        .expect("seed legacy summaries");

    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![
            test_scan_candidate(file_a, "sig-a-2"),
            test_scan_candidate(file_b, "sig-b-1"),
        ],
        scan_result: statsai_adapters::AdapterScan {
            events: vec![test_scan_event(
                &source,
                file_b,
                b_started_at,
                "event-b",
                125,
            )],
            summaries: vec![test_scan_summary(
                &source,
                file_b,
                b_started_at,
                "summary-b",
                125,
            )],
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("reconcile scan");

    let events = store.events_for_source(&source.source_id).expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_record_id.as_deref()),
        Some("event-b")
    );
    let summaries = store
        .summaries_for_source(&source.source_id)
        .expect("summaries");
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].summary_id,
        summary_id("codex", &source.source_id, "summary-b")
    );
}

fn test_scan_summary(
    source: &SourceLocation,
    file_path: &str,
    observed_at: DateTime<Utc>,
    record_id: &str,
    total_tokens: u64,
) -> UsageSummary {
    let mut summary = test_summary("codex", source, observed_at, total_tokens, None);
    summary.summary_id = summary_id("codex", &source.source_id, record_id);
    summary.source.source_kind = SourceKind::LocalAdapter;
    summary.source.source_type = "jsonl".to_string();
    summary.source.source_record_id = Some(record_id.to_string());
    summary.parse_evidence = Some(ParseEvidence {
        event_key_version: "test-scan-summary.v1".to_string(),
        source_file_path_hash: Some(hash_text(file_path)),
        source_line_number: None,
        source_record_id: Some(record_id.to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::Unresolved,
    });
    summary
}

#[test]
fn scan_rewrites_task_span_links_to_canonical_event_ids() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-link-rewrite"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-link-rewrite/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 20, 12, 0, 0)
        .single()
        .expect("started_at");
    let existing_event = test_scan_event(&source, file_path, started_at, "existing", 100);
    store
        .insert_event(&existing_event)
        .expect("insert existing event");

    let mut duplicate_event = existing_event.clone();
    duplicate_event.event_id = event_id("codex", &source.source_id, "duplicate", None, started_at);
    duplicate_event.source.source_record_id = Some("duplicate".to_string());
    if let Some(parse_evidence) = duplicate_event.parse_evidence.as_mut() {
        parse_evidence.source_record_id = Some("duplicate".to_string());
    }
    let span = test_task_span(
        &source,
        file_path,
        started_at,
        "duplicate-span",
        "Rewrite canonical task links",
        &duplicate_event,
    );

    let insert_result = store
        .insert_events_with_resolution(&[duplicate_event])
        .expect("insert duplicate event");
    assert_eq!(insert_result.inserted, 0);

    let mut spans = vec![span];
    rewrite_task_span_linked_event_ids(&mut spans, &insert_result.canonical_event_ids);
    store.upsert_task_spans(&spans).expect("upsert spans");

    let stored_spans = store.task_spans().expect("task spans");
    assert_eq!(stored_spans.len(), 1);
    assert_eq!(
        stored_spans[0].linked_event_ids,
        vec![existing_event.event_id.clone()]
    );
}
