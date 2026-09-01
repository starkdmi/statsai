use super::*;

#[test]
fn claude_partial_jsonl_scan_only_emits_selected_task_spans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root.join("projects").join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");

    let first = project_store.join("first.jsonl");
    let second = project_store.join("second.jsonl");
    std::fs::write(
        &first,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"sessionId\":\"session-a\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("first");
    std::fs::write(
        &second,
        "{\"timestamp\":\"2026-05-01T00:01:00Z\",\"sessionId\":\"session-b\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n",
    )
    .expect("second");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            concat!(
                "{{\"version\":1,\"entries\":[",
                "{{\"sessionId\":\"session-a\",\"fullPath\":\"{}\",\"summary\":\"Fix parser bug\"}},",
                "{{\"sessionId\":\"session-b\",\"fullPath\":\"{}\",\"summary\":\"Review release notes\"}}",
                "]}}"
            ),
            first.display(),
            second.display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let selected = [canonical_display(&first)].into_iter().collect();
    let scan = scan_claude_source(
        &ClaudeCodeAdapter,
        &source,
        &ScanOptions {
            device_id: "device".to_string(),
            collect_tasks: true,
            selected_cache_keys: Some(selected),
        },
    )
    .expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.task_spans.len(), 1);
    assert_eq!(scan.task_spans[0].session_id.as_deref(), Some("session-a"));
    assert_eq!(scan.task_spans[0].title, "Fix parser bug");
    assert_eq!(scan.task_spans[0].usage.computed_total(), 3);
    assert_eq!(scan.task_spans[0].linked_event_ids.len(), 1);
}

#[test]
fn claude_partial_stats_cache_scan_does_not_emit_unscanned_task_spans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root.join("projects").join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");

    let session = project_store.join("session.jsonl");
    std::fs::write(
        &session,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"sessionId\":\"session-a\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-a\",\"fullPath\":\"{}\",\"summary\":\"Investigate cache issue\"}}]}}",
            session.display()
        ),
    )
    .expect("session index");
    std::fs::write(
        root.join("stats-cache.json"),
        r#"{
          "version": 2,
          "lastComputedDate": "2026-05-13",
          "firstSessionDate": "2026-05-01T00:00:00Z",
          "totalSessions": 1,
          "totalMessages": 2,
          "modelUsage": {
            "claude-opus-4-5-thinking": {
              "inputTokens": 11,
              "outputTokens": 7,
              "cacheReadInputTokens": 0,
              "cacheCreationInputTokens": 0,
              "costUSD": 0.12
            }
          }
        }"#,
    )
    .expect("stats cache");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let selected = [canonical_display(&root.join("stats-cache.json"))]
        .into_iter()
        .collect();
    let scan = scan_claude_source(
        &ClaudeCodeAdapter,
        &source,
        &ScanOptions {
            device_id: "device".to_string(),
            collect_tasks: true,
            selected_cache_keys: Some(selected),
        },
    )
    .expect("scan");

    assert!(scan.events.is_empty());
    assert_eq!(scan.summaries.len(), 1);
    assert!(scan.task_spans.is_empty());
}

#[test]
fn claude_task_entry_matches_scanned_file_handles_jsonl_suffix_mismatch() {
    let path = Path::new("/tmp/example-session");
    let scanned = [canonical_display(&path.with_extension("jsonl"))]
        .into_iter()
        .collect();

    assert!(claude_task_entry_matches_scanned_file(path, &scanned));
    assert!(claude_task_entry_matches_scanned_file(
        &path.with_extension("jsonl"),
        &scanned
    ));
}

#[test]
fn claude_task_spans_use_reconciliation_hash_for_suffix_mismatched_index_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root.join("projects").join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");

    let session = project_store.join("session-a.jsonl");
    std::fs::write(
        &session,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"sessionId\":\"session-a\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-a\",\"fullPath\":\"{}\",\"summary\":\"Investigate cleanup mismatch\"}}]}}",
            session.with_extension("").display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.task_spans.len(), 1);
    assert_eq!(
        scan.task_spans[0].source_file_path_hash.as_deref(),
        Some(hash_text(&canonical_display(&session)).as_str())
    );
}

#[test]
fn claude_scan_skips_task_entries_when_task_collection_is_disabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root.join("projects").join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");

    let session = project_store.join("session-a.jsonl");
    std::fs::write(
        &session,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"sessionId\":\"session-a\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-a\",\"fullPath\":\"{}\",\"summary\":\"Skip task collection\"}}]}}",
            session.display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let scan =
        scan_claude_source(&ClaudeCodeAdapter, &source, &options_without_tasks()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert!(scan.task_spans.is_empty());
}
