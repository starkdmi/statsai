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
fn full_rescans_reread_every_file() {
    assert!(should_reread_every_file(true, false, 0, 0, false));
    assert!(should_reread_every_file(false, true, 0, 0, false));
    assert!(should_reread_every_file(false, false, 2, 2, false));
    assert!(should_reread_every_file(false, false, 0, 0, true));
    assert!(!should_reread_every_file(false, false, 2, 1, false));
    assert!(!should_reread_every_file(false, false, 0, 0, false));
}

#[test]
fn scan_file_reconciliation_tells_a_moved_file_from_a_gone_one() {
    let store = Store::in_memory().expect("store");
    let source_id = statsai_core::SourceId("src-moved-cache".to_string());
    let entry = |key: &str| ScanFileStateEntry {
        cache_key: key.to_string(),
        cache_signature: format!("sig-{key}"),
    };
    let archived_name = "rollout-2026-05-28T10-00-00-019e733b-6b32-7fb2-b570-e8f68052bb36.jsonl";
    let deleted_name = "rollout-2026-05-28T11-00-00-019e733b-6b32-7fb2-b570-e8f68052bb37.jsonl";
    store
        .record_scan_file_entries(
            &source_id,
            &[
                entry(&format!("/codex/sessions/2026/05/28/{archived_name}")),
                entry(&format!("/codex/sessions/2026/05/28/{deleted_name}")),
            ],
        )
        .expect("record tracked cache state");

    let reconciliation = select_scan_file_reconciliation(
        &store,
        &source_id,
        &[entry(&format!("/codex/archived_sessions/{archived_name}"))],
        &HashMap::new(),
        false,
        false,
        false,
    )
    .expect("reconciliation");

    assert_eq!(
        reconciliation.moved_entries,
        vec![entry(&format!(
            "/codex/sessions/2026/05/28/{archived_name}"
        ))]
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
fn a_move_retires_task_spans_even_when_tasks_are_not_collected() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-move"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let name = "rollout-2026-06-12T11-00-00-019e733b-6b32-7fb2-b570-e8f68052bb36.jsonl";
    let old_path = format!("/tmp/codex-task-move/sessions/2026/06/12/{name}");
    let new_path = format!("/tmp/codex-task-move/archived_sessions/{name}");
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 11, 0, 0)
        .single()
        .expect("started_at");
    let scan = |include_tasks: bool, path: &str, with_span: bool| {
        let event = test_scan_event(&source, path, started_at, "event", 100);
        let span = test_task_span(
            &source,
            path,
            started_at,
            "span",
            "Archive the old rollouts",
            &event,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![test_scan_candidate(path, "sig-1")],
            scan_result: statsai_adapters::AdapterScan {
                events: vec![event],
                task_spans: if with_span { vec![span] } else { Vec::new() },
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: None,
        };
        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks,
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
    };

    scan(true, &old_path, true);
    assert_eq!(store.task_spans().expect("task spans").len(), 1);
    assert_eq!(store.work_items().expect("work items").len(), 1);

    // Moved while tasks are not collected: its span goes with its old path.
    scan(false, &new_path, false);
    assert!(store.task_spans().expect("task spans").is_empty());
    assert!(store.work_items().expect("work items").is_empty());

    // The file at the new path was recorded without tasks, so the next scan
    // that collects them reads it again.
    scan(true, &new_path, true);
    let spans = store.task_spans().expect("task spans");
    assert_eq!(spans.len(), 1);
    assert_eq!(
        spans[0].source_file_path_hash.as_deref(),
        Some(hash_text(&new_path).as_str())
    );
    assert_eq!(store.work_items().expect("work items").len(), 1);
}

#[test]
fn a_full_reread_replaces_task_spans_without_a_file_hash() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-hashless-spans"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let index = "/tmp/claude-hashless-spans/sessions-index.json";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 11, 0, 0)
        .single()
        .expect("started_at");
    // An index entry with no transcript path: its span has no file hash, and
    // its id follows the entry's `modified` time.
    let scan = |span_id: &str, no_cache: bool| {
        let event = test_scan_event(&source, index, started_at, "event", 100);
        let mut span = test_task_span(
            &source,
            index,
            started_at,
            "span",
            "Index the sessions",
            &event,
        );
        span.span_id = statsai_core::TaskSpanId(span_id.to_string());
        span.source_file_path_hash = None;
        let adapter = TestAdapter {
            provider: "claude_code",
            discovered: vec![source.clone()],
            candidates: vec![test_scan_candidate(index, "sig-1")],
            scan_result: statsai_adapters::AdapterScan {
                events: vec![event],
                task_spans: vec![span],
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
                no_cache,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(adapter)],
        )
        .expect("scan");
    };

    scan("span-modified-1", false);
    // The entry's `modified` time changed; `--no-cache` rereads it.
    scan("span-modified-2", true);

    let spans = store.task_spans().expect("task spans");
    assert_eq!(
        spans
            .iter()
            .map(|span| span.span_id.0.as_str())
            .collect::<Vec<_>>(),
        vec!["span-modified-2"]
    );
    assert_eq!(store.work_items().expect("work items").len(), 1);
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

#[test]
fn full_rereads_retire_hashless_rows_and_keep_gone_files_rows() {
    // `--no-cache`, and a scan that finds every current file changed.
    for (no_cache, b_signature) in [(true, "sig-b-1"), (false, "sig-b-2")] {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-full-reread-legacy"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let file_gone = "/tmp/codex-full-reread-legacy/gone.jsonl";
        let file_b = "/tmp/codex-full-reread-legacy/b.jsonl";
        store
            .record_scan_file_entries(
                &source.source_id,
                &[
                    ScanFileStateEntry {
                        cache_key: file_gone.to_string(),
                        cache_signature: "sig-gone-1".to_string(),
                    },
                    ScanFileStateEntry {
                        cache_key: file_b.to_string(),
                        cache_signature: "sig-b-1".to_string(),
                    },
                ],
            )
            .expect("record initial cache");
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 4, 10, 0, 0)
            .single()
            .expect("started_at");
        let gone_at = started_at - chrono::Duration::days(1);
        let mut legacy_summary = test_summary("codex", &source, started_at, 75, None);
        legacy_summary.summary_id = summary_id("codex", &source.source_id, "legacy-summary");
        store
            .insert_events(&[
                test_event("codex", &source, started_at, None, TokenParts::total(75)),
                test_scan_event(&source, file_gone, gone_at, "event-gone", 40),
            ])
            .expect("seed events");
        store
            .upsert_summaries(&[
                legacy_summary,
                test_scan_summary(&source, file_gone, gone_at, "summary-gone", 40),
            ])
            .expect("seed summaries");

        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![test_scan_candidate(file_b, b_signature)],
            scan_result: statsai_adapters::AdapterScan {
                events: vec![test_scan_event(&source, file_b, started_at, "event-b", 125)],
                summaries: vec![test_scan_summary(
                    &source,
                    file_b,
                    started_at,
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
                no_cache,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(adapter)],
        )
        .expect("full reread");

        let mut record_ids = store
            .events_for_source(&source.source_id)
            .expect("events")
            .into_iter()
            .filter_map(|event| event.parse_evidence.and_then(|e| e.source_record_id))
            .collect::<Vec<_>>();
        record_ids.sort();
        assert_eq!(
            record_ids,
            vec!["event-b".to_string(), "event-gone".to_string()],
            "no_cache={no_cache}: the hashless row goes, the gone file's row stays"
        );
        let mut summary_ids = store
            .summaries_for_source(&source.source_id)
            .expect("summaries")
            .into_iter()
            .map(|summary| summary.summary_id.0)
            .collect::<Vec<_>>();
        summary_ids.sort();
        let mut expected = vec![
            summary_id("codex", &source.source_id, "summary-b").0,
            summary_id("codex", &source.source_id, "summary-gone").0,
        ];
        expected.sort();
        assert_eq!(summary_ids, expected, "no_cache={no_cache}");
        // The gone file's cache entry stays, so a later move is still recognised.
        assert!(store
            .scan_file_entries(&source.source_id)
            .expect("cache")
            .iter()
            .any(|entry| entry.cache_key == file_gone));
    }
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

/// The real Codex adapter, pointed at one source instead of the machine's.
struct PinnedCodexAdapter {
    source: SourceLocation,
}

impl ProviderAdapter for PinnedCodexAdapter {
    fn id(&self) -> &'static str {
        statsai_adapters::CodexAdapter.id()
    }

    fn version(&self) -> &'static str {
        statsai_adapters::CodexAdapter.version()
    }

    fn provider(&self) -> &'static str {
        statsai_adapters::CodexAdapter.provider()
    }

    fn discover(&self) -> Vec<SourceLocation> {
        vec![self.source.clone()]
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        statsai_adapters::CodexAdapter.scan_candidates(source)
    }

    fn scan(
        &self,
        source: &SourceLocation,
        options: &ScanOptions,
    ) -> Result<statsai_adapters::AdapterScan> {
        statsai_adapters::CodexAdapter.scan(source, options)
    }
}

#[test]
fn cached_scan_picks_up_a_codex_thread_rename() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let index_path = codex_root.join("session_index.jsonl");
    std::fs::write(
        &index_path,
        "{\"id\":\"thread-1\",\"thread_name\":\"First name\"}\n",
    )
    .expect("session index");
    std::fs::write(
        sessions.join("rollout.jsonl"),
        [
            r#"{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{"id":"thread-1"}}"#,
            r#"{"timestamp":"2026-06-01T08:00:00Z","type":"turn_context","payload":{"model":"gpt-5"}}"#,
            r#"{"timestamp":"2026-06-01T08:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120},"total_token_usage":{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}}}}"#,
        ]
        .join("\n")
            + "\n",
    )
    .expect("rollout");

    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "codex-local-jsonl",
        statsai_adapters::CodexAdapter.version(),
        &codex_root,
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let scan = || {
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
            vec![Box::new(PinnedCodexAdapter {
                source: source.clone(),
            })],
        )
        .expect("scan");
    };
    let titles = || {
        store
            .all_session_rollups()
            .expect("sessions")
            .into_iter()
            .map(|session| session.title)
            .collect::<Vec<_>>()
    };

    scan();
    assert_eq!(titles(), vec![Some("First name".to_string())]);

    // Codex appends the rename to the index and leaves the rollout alone.
    std::thread::sleep(std::time::Duration::from_millis(5));
    std::fs::write(
        &index_path,
        "{\"id\":\"thread-1\",\"thread_name\":\"First name\"}\n\
         {\"id\":\"thread-1\",\"thread_name\":\"Renamed thread\"}\n",
    )
    .expect("renamed index");
    scan();
    assert_eq!(titles(), vec![Some("Renamed thread".to_string())]);
}
