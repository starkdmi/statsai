use super::*;

#[test]
fn codex_usage_only_scan_skips_task_preview_fallback_parsing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");

    let session_path = sessions.join("session.jsonl");
    let mut file = File::create(&session_path).expect("session file");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:00Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hi"}}]"#
    )
    .expect("write malformed task-only message");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:01Z","usage":{{"input_tokens":3,"output_tokens":4}}}}"#
    )
    .expect("write usage");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options_without_tasks()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.diagnostics.invalid_rows, 0);
    assert!(scan.task_spans.is_empty());
}

#[test]
fn codex_uses_last_token_usage_not_cumulative_total() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"model":"gpt-5-codex","total_token_usage":{{"input_tokens":900,"cached_input_tokens":300,"output_tokens":100,"reasoning_output_tokens":50,"total_tokens":1000}},"last_token_usage":{{"input_tokens":90,"cached_input_tokens":30,"output_tokens":10,"reasoning_output_tokens":5,"total_tokens":100}}}}}}}}"#
    )
    .expect("write");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].usage.input_tokens, Some(60));
    assert_eq!(scan.events[0].usage.output_tokens, Some(5));
    assert_eq!(scan.events[0].usage.computed_total(), 100);
    assert_eq!(scan.events[0].usage.cache_read_tokens, Some(30));
    assert_eq!(scan.events[0].usage.reasoning_tokens, Some(5));
    assert!(scan.events[0].cost.estimated_api_equivalent_usd.is_some());
}

#[test]
fn codex_subtracts_cumulative_total_usage_when_last_usage_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"model":"gpt-5","total_token_usage":{{"input_tokens":100,"cached_input_tokens":10,"output_tokens":50,"total_tokens":150}}}}}}}}"#
    )
    .expect("write first");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:01:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"model":"gpt-5","total_token_usage":{{"input_tokens":250,"cached_input_tokens":30,"output_tokens":75,"total_tokens":325}}}}}}}}"#
    )
    .expect("write second");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 2);
    assert_eq!(scan.events[0].usage.input_tokens, Some(90));
    assert_eq!(scan.events[1].usage.input_tokens, Some(130));
    assert_eq!(scan.events[1].usage.cache_read_tokens, Some(20));
    assert_eq!(scan.events[1].usage.output_tokens, Some(25));
    assert_eq!(scan.events[1].usage.total_tokens, Some(175));
}

#[test]
fn codex_task_complete_usage_is_not_emitted_twice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("completion-usage.jsonl")).expect("fixture");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:00Z","type":"event_msg","payload":{{"type":"task_started","started_at":"2026-05-01T00:00:00Z"}}}}"#
    )
    .expect("write start");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:02Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":120}}}}}}}}"#
    )
    .expect("write token count");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:03Z","type":"event_msg","payload":{{"type":"task_complete","completed_at":"2026-05-01T00:00:03Z","duration_ms":3000}},"usage":{{"input_tokens":90,"cached_input_tokens":30,"output_tokens":45,"reasoning_output_tokens":15,"total_tokens":150}}}}"#
    )
    .expect("write completion");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].usage.input_tokens, Some(60));
    assert_eq!(scan.events[0].usage.cache_read_tokens, Some(30));
    assert_eq!(scan.events[0].usage.output_tokens, Some(30));
    assert_eq!(scan.events[0].usage.reasoning_tokens, Some(15));
    assert_eq!(scan.events[0].usage.total_tokens, Some(150));
}

#[test]
fn codex_turn_usage_consumes_all_token_count_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("multi-token-count.jsonl")).expect("fixture");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:00Z","type":"event_msg","payload":{{"type":"task_started","started_at":"2026-05-01T00:00:00Z"}}}}"#
    )
    .expect("write start");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:01Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":40,"cached_input_tokens":10,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":60}},"total_token_usage":{{"input_tokens":40,"cached_input_tokens":10,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":60}}}}}}}}"#
    )
    .expect("write first token count");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:02Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":120,"cached_input_tokens":30,"output_tokens":60,"reasoning_output_tokens":15,"total_tokens":180}}}}}}}}"#
    )
    .expect("write second token count");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:03Z","type":"event_msg","payload":{{"type":"task_complete","completed_at":"2026-05-01T00:00:03Z","duration_ms":3000}}}}"#
    )
    .expect("write completion");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].usage.input_tokens, Some(90));
    assert_eq!(scan.events[0].usage.cache_read_tokens, Some(30));
    assert_eq!(scan.events[0].usage.output_tokens, Some(45));
    assert_eq!(scan.events[0].usage.reasoning_tokens, Some(15));
    assert_eq!(scan.events[0].usage.total_tokens, Some(180));
    assert_eq!(scan.events[0].usage.requests, Some(2));
}

#[test]
fn codex_headless_usage_shapes_are_parsed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("exec.jsonl")).expect("fixture");
    writeln!(
        file,
        r#"{{"data":{{"timestamp":"2026-05-01T00:00:00Z","model":"gpt-5","usage":{{"prompt_tokens":10,"completion_tokens":5,"cached_tokens":3}}}}}}"#
    )
    .expect("write");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].usage.input_tokens, Some(7));
    assert_eq!(scan.events[0].usage.output_tokens, Some(5));
    assert_eq!(scan.events[0].usage.cache_read_tokens, Some(3));
}

#[test]
fn codex_usage_counts_normalize_inclusive_subtotals() {
    let value: Value = serde_json::json!({
        "input_tokens": 100,
        "cached_input_tokens": 30,
        "output_tokens": 10,
        "reasoning_output_tokens": 5,
        "total_tokens": 110
    });

    let usage = codex_usage_counts_from_value(&value);

    assert_eq!(usage.input_tokens, Some(70));
    assert_eq!(usage.cache_read_tokens, Some(30));
    assert_eq!(usage.output_tokens, Some(5));
    assert_eq!(usage.reasoning_tokens, Some(5));
    assert_eq!(usage.computed_total(), 110);
}

#[test]
fn codex_caps_cached_input_to_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:00Z","usage":{{"input_tokens":10,"cached_input_tokens":30,"output_tokens":5}}}}"#
    )
    .expect("write");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events[0].usage.input_tokens, Some(0));
    assert_eq!(scan.events[0].usage.cache_read_tokens, Some(10));
}
