use super::*;

/// Codex reports cached input inside `input_tokens`.
fn usage_record(timestamp: &str, input: u64, cached: u64) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","type":"token_usage_record","payload":{{"usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":{}}}}}}}"#,
        input + 50
    )
}

fn token_count(timestamp: &str, total: u64, input: u64, cached: u64) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{total},"cached_input_tokens":0,"output_tokens":0,"total_tokens":{total}}},"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":{}}}}}}}}}"#,
        input + 50
    )
}

fn scan_lines(lines: &[String]) -> AdapterScan {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::write(sessions.join("rollout.jsonl"), lines.join("\n") + "\n").expect("fixture");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );
    scan_codex_source(&CodexAdapter, &source, &options_without_tasks()).expect("scan")
}

fn at(timestamp: &str) -> Option<chrono::DateTime<Utc>> {
    Some(timestamp.parse().expect("timestamp"))
}

fn turn_lines(compaction: String) -> Vec<String> {
    vec![
        r#"{"timestamp":"2026-09-01T10:00:00Z","type":"session_meta","payload":{"id":"thread-1"}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:09Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","input":""}}"#.to_string(),
        usage_record("2026-09-01T10:00:10Z", 30_000, 0),
        r#"{"timestamp":"2026-09-01T10:00:12Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c1","output":"ok"}}"#.to_string(),
        token_count("2026-09-01T10:00:12.500Z", 30_050, 30_000, 0),
        usage_record("2026-09-01T10:00:20Z", 32_000, 29_952),
        compaction,
        usage_record("2026-09-01T10:00:30Z", 8_024, 1_024),
        r#"{"timestamp":"2026-09-01T10:00:40Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#.to_string(),
    ]
}

#[test]
fn codex_turns_list_their_calls_with_request_starts_and_compaction() {
    let scan = scan_lines(&turn_lines(
        r#"{"timestamp":"2026-09-01T10:00:21Z","type":"compacted","payload":{"message":"","replacement_history":[]}}"#.to_string(),
    ));

    assert_eq!(scan.events.len(), 1);
    let event = &scan.events[0];
    assert_eq!(
        event.usage.requests,
        Some(3),
        "the paired token_count is not a call"
    );
    let calls = &event.context.as_ref().expect("context").calls;
    assert_eq!(
        calls
            .iter()
            .map(|call| (call.requested_at, call.after_compaction))
            .collect::<Vec<_>>(),
        [
            (at("2026-09-01T10:00:01Z"), false),
            (at("2026-09-01T10:00:12Z"), false),
            (None, true),
        ]
    );
    assert_eq!(calls[1].cache_read_tokens, Some(29_952));
    assert_eq!(calls[1].input_tokens, Some(2_048));
    assert_eq!(calls[1].cache_creation_tokens, Some(0));
    assert_eq!(calls[2].completed_at, at("2026-09-01T10:00:30Z").unwrap());
}

#[test]
fn an_oversized_compaction_row_still_marks_the_boundary() {
    let compaction = format!(
        r#"{{"timestamp":"2026-09-01T10:00:21Z","type":"response_item","payload":{{"type":"compaction","encrypted_content":"{}"}}}}"#,
        "x".repeat(MAX_JSONL_RECORD_BYTES + 1)
    );
    let scan = scan_lines(&turn_lines(compaction));

    assert_eq!(scan.diagnostics.oversized_rows, 1);
    let calls = &scan.events[0].context.as_ref().expect("context").calls;
    assert!(calls[2].after_compaction);
}

#[test]
fn token_counts_alone_do_not_place_request_starts() {
    let lines = vec![
        r#"{"timestamp":"2026-09-01T10:00:00Z","type":"session_meta","payload":{"id":"thread-1"}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:12Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#.to_string(),
        token_count("2026-09-01T10:00:12.500Z", 30_050, 30_000, 0),
        r#"{"timestamp":"2026-09-01T10:00:30Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c2","output":"ok"}}"#.to_string(),
        token_count("2026-09-01T10:00:30.500Z", 62_150, 32_000, 29_952),
    ];
    let scan = scan_lines(&lines);

    assert_eq!(scan.events.len(), 2);
    assert!(scan.events.iter().all(|event| event.context.is_none()));
}

#[test]
fn a_usage_record_outside_a_turn_carries_its_request_start() {
    let lines = vec![
        r#"{"timestamp":"2026-09-01T10:00:00Z","type":"session_meta","payload":{"id":"thread-1"}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"go"}}"#.to_string(),
        usage_record("2026-09-01T10:00:10Z", 30_000, 0),
    ];
    let scan = scan_lines(&lines);

    assert_eq!(scan.events.len(), 1);
    let context = scan.events[0].context.as_ref().expect("context");
    assert_eq!(context.requested_at, at("2026-09-01T10:00:01Z"));
    assert!(context.calls.is_empty());
}

#[test]
fn turn_calls_carry_the_model_that_served_each() {
    let scan = scan_lines(&[
        r#"{"timestamp":"2026-09-01T10:00:00Z","type":"session_meta","payload":{"id":"thread-1"}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:00Z","type":"turn_context","payload":{"model":"gpt-5"}}"#.to_string(),
        usage_record("2026-09-01T10:00:10Z", 30_000, 0),
        r#"{"timestamp":"2026-09-01T10:00:11Z","type":"turn_context","payload":{"model":"gpt-5.1"}}"#.to_string(),
        usage_record("2026-09-01T10:00:20Z", 30_000, 0),
        r#"{"timestamp":"2026-09-01T10:00:40Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#.to_string(),
    ]);
    let calls = &scan.events[0].context.as_ref().expect("context").calls;
    assert_eq!(
        calls
            .iter()
            .map(|call| call.model.as_deref())
            .collect::<Vec<_>>(),
        [Some("gpt-5"), Some("gpt-5.1")]
    );

    // A model the rollout never named is a fallback guess, not evidence.
    let scan = scan_lines(&turn_lines(String::new()));
    let calls = &scan.events[0].context.as_ref().expect("context").calls;
    assert!(calls.iter().all(|call| call.model.is_none()));
}

#[test]
fn completion_usage_keeps_the_turns_compaction() {
    let completion = |timestamp: &str| {
        format!(
            r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"task_complete","completed_at":"{timestamp}","duration_ms":3000}},"usage":{{"input_tokens":30000,"cached_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":30050}}}}"#
        )
    };
    let compacted = r#"{"timestamp":"2026-09-01T10:00:05Z","type":"compacted","payload":{"message":"","replacement_history":[]}}"#;
    let opening = [
        r#"{"timestamp":"2026-09-01T10:00:00Z","type":"session_meta","payload":{"id":"thread-1"}}"#,
        r#"{"timestamp":"2026-09-01T10:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
    ]
    .map(str::to_string);

    // The completion's own usage is the call after the compaction.
    let mut lines = opening.to_vec();
    lines.extend([compacted.to_string(), completion("2026-09-01T10:00:10Z")]);
    let scan = scan_lines(&lines);
    assert_eq!(scan.events.len(), 1);
    assert!(
        scan.events[0]
            .context
            .as_ref()
            .expect("context")
            .after_compaction
    );

    // A usage line inside the turn took the mark; the completion replaces it.
    let mut lines = opening.to_vec();
    lines.extend([
        compacted.to_string(),
        usage_record("2026-09-01T10:00:08Z", 30_000, 0),
        completion("2026-09-01T10:00:10Z"),
    ]);
    let scan = scan_lines(&lines);
    assert_eq!(scan.events.len(), 1);
    assert!(
        scan.events[0]
            .context
            .as_ref()
            .expect("context")
            .after_compaction
    );
}

#[test]
fn a_call_without_input_counts_keeps_them_unreported() {
    let scan = scan_lines(&[
        r#"{"timestamp":"2026-09-01T10:00:00Z","type":"session_meta","payload":{"id":"thread-1"}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#.to_string(),
        usage_record("2026-09-01T10:00:10Z", 30_000, 0),
        r#"{"timestamp":"2026-09-01T10:00:20Z","type":"token_usage_record","payload":{"usage":{"output_tokens":50,"total_tokens":50}}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:40Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#.to_string(),
    ]);
    let event = &scan.events[0];
    let calls = &event.context.as_ref().expect("context").calls;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].input_tokens, None);
    assert_eq!(calls[1].cache_read_tokens, None);

    let analyzed = statsai_core::analyze_cache_calls([event]);
    assert_eq!(
        analyzed[1].verdict.unclassifiable,
        Some(statsai_core::CacheUnclassifiableReason::NoInputTelemetry)
    );
}
