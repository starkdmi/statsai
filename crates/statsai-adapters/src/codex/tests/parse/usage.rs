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

fn scan_session_lines(lines: &[String]) -> (crate::AdapterScan, usize, usize) {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let path = sessions.join("session.jsonl");
    let mut file = File::create(&path).expect("fixture");
    for line in lines {
        writeln!(file, "{line}").expect("write");
    }
    drop(file);
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");
    let quota = crate::archive::collect_codex_quota_observations(&source, &path).expect("quota");
    let positive_samples = quota
        .iter()
        .filter(|record| {
            record
                .observation
                .usage_sample
                .as_ref()
                .is_some_and(|usage| usage.computed_total() > 0)
        })
        .count();
    (scan, quota.len(), positive_samples)
}

fn token_count_line(timestamp: &str, limit_id: &str, total: Option<Value>, last: Value) -> String {
    // `type` has to lead `payload`. The line classifier matches a fixed header
    // prefix, and serde_json's map orders keys alphabetically.
    let total_field = total
        .map(|total| format!(r#""total_token_usage":{total},"#))
        .unwrap_or_default();
    format!(
        r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{{total_field}"last_token_usage":{last}}},"rate_limits":{{"limit_id":"{limit_id}","primary":{{"used_percent":10,"window_minutes":300,"resets_at":1790000000}},"secondary":{{"used_percent":4,"window_minutes":10080,"resets_at":1790400000}},"plan_type":"plus"}}}}}}"#
    )
}

fn turn_bounds(started_at: &str, completed_at: &str) -> (String, String) {
    (
        format!(
            r#"{{"timestamp":"{started_at}","type":"event_msg","payload":{{"type":"task_started","started_at":"{started_at}"}}}}"#
        ),
        format!(
            r#"{{"timestamp":"{completed_at}","type":"event_msg","payload":{{"type":"task_complete","completed_at":"{completed_at}"}}}}"#
        ),
    )
}

#[test]
fn codex_counts_an_unchanged_token_total_once_for_every_limit_bucket() {
    // Older builds repeat total_token_usage with an identical last_token_usage.
    // Newer builds do it once more per rate-limit bucket. Neither copy is a
    // new response, so the turn total has to stay on the first line.
    let usage = serde_json::json!({
        "input_tokens": 100,
        "cached_input_tokens": 25,
        "output_tokens": 20,
        "reasoning_output_tokens": 5,
        "total_tokens": 120
    });
    let (started, completed) = turn_bounds("2026-09-01T10:00:00Z", "2026-09-01T10:00:04Z");
    let lines = vec![
        started,
        token_count_line(
            "2026-09-01T10:00:01Z",
            "codex",
            Some(usage.clone()),
            usage.clone(),
        ),
        token_count_line(
            "2026-09-01T10:00:02Z",
            "codex",
            Some(usage.clone()),
            usage.clone(),
        ),
        token_count_line(
            "2026-09-01T10:00:03Z",
            "premium",
            Some(usage.clone()),
            usage,
        ),
        completed,
    ];

    let (scan, quota_len, positive_samples) = scan_session_lines(&lines);

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].usage.input_tokens, Some(75));
    assert_eq!(scan.events[0].usage.cache_read_tokens, Some(25));
    assert_eq!(scan.events[0].usage.output_tokens, Some(15));
    assert_eq!(scan.events[0].usage.reasoning_tokens, Some(5));
    assert_eq!(scan.events[0].usage.computed_total(), 120);
    assert_eq!(scan.events[0].usage.requests, Some(1));
    assert_eq!(
        quota_len, 3,
        "each bucket still produces a quota observation"
    );
    assert_eq!(positive_samples, 1, "repeated totals carry no usage sample");
}

#[test]
fn codex_ignores_the_phantom_token_count_after_compaction() {
    // After `compacted`, Codex emits a token_count whose cumulative total is
    // unchanged and whose last_token_usage is only the new context size.
    let total = serde_json::json!({
        "input_tokens": 200,
        "cached_input_tokens": 40,
        "output_tokens": 30,
        "reasoning_output_tokens": 10,
        "total_tokens": 230
    });
    let last = serde_json::json!({
        "input_tokens": 80,
        "cached_input_tokens": 20,
        "output_tokens": 16,
        "reasoning_output_tokens": 4,
        "total_tokens": 96
    });
    let phantom_last = serde_json::json!({
        "input_tokens": 4,
        "cached_input_tokens": 0,
        "output_tokens": 1,
        "reasoning_output_tokens": 0,
        "total_tokens": 5
    });
    let (started, completed) = turn_bounds("2026-09-01T10:00:00Z", "2026-09-01T10:00:05Z");
    let lines = vec![
        started,
        token_count_line("2026-09-01T10:00:01Z", "codex", Some(total.clone()), last),
        r#"{"timestamp":"2026-09-01T10:00:02Z","type":"compacted","payload":{"message":"synthetic","window_number":1}}"#.to_string(),
        token_count_line("2026-09-01T10:00:03Z", "codex", Some(total), phantom_last),
        completed,
    ];

    let (scan, quota_len, positive_samples) = scan_session_lines(&lines);

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].usage.computed_total(), 96);
    assert_eq!(scan.events[0].usage.requests, Some(1));
    assert_eq!(quota_len, 2);
    assert_eq!(positive_samples, 1);
}

#[test]
fn codex_interleaved_sessions_keep_their_own_usage_source_and_totals() {
    let usage = r#"{"input_tokens":90,"output_tokens":10,"total_tokens":100}"#;
    // Records may name their session at the top level or only by
    // `payload.thread_id`; a sub-agent's `payload.session_id` is its parent.
    for record in [
        format!(
            r#"{{"timestamp":"2026-09-01T10:00:02Z","session_id":"session-a","type":"token_usage_record","payload":{{"usage":{usage}}}}}"#
        ),
        format!(
            r#"{{"timestamp":"2026-09-01T10:00:02Z","type":"token_usage_record","payload":{{"thread_id":"session-a","session_id":"session-parent","usage":{usage}}}}}"#
        ),
    ] {
        assert_interleaved_sessions_keep_their_own_usage(usage, record);
    }
}

fn assert_interleaved_sessions_keep_their_own_usage(usage: &str, record: String) {
    // Session A switches to records; session B stays on token_count. B's first
    // cumulative total equals A's last one and is still a new response for B.
    let token_count = |timestamp: &str, session: &str| {
        format!(
            r#"{{"timestamp":"{timestamp}","session_id":"{session}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{usage},"last_token_usage":{usage}}},"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":10,"window_minutes":300,"resets_at":1790000000}},"plan_type":"plus"}}}}}}"#
        )
    };
    let task = |timestamp: &str, session: &str, kind: &str, at: &str| {
        format!(
            r#"{{"timestamp":"{timestamp}","session_id":"{session}","type":"event_msg","payload":{{"type":"{kind}","{at}":"{timestamp}"}}}}"#
        )
    };
    let lines = vec![
        task(
            "2026-09-01T10:00:00Z",
            "session-a",
            "task_started",
            "started_at",
        ),
        task(
            "2026-09-01T10:00:01Z",
            "session-b",
            "task_started",
            "started_at",
        ),
        record,
        token_count("2026-09-01T10:00:03Z", "session-a"),
        token_count("2026-09-01T10:00:04Z", "session-b"),
        task(
            "2026-09-01T10:00:05Z",
            "session-a",
            "task_complete",
            "completed_at",
        ),
        task(
            "2026-09-01T10:00:06Z",
            "session-b",
            "task_complete",
            "completed_at",
        ),
    ];

    let (scan, quota_len, positive_samples) = scan_session_lines(&lines);

    // The archive quota scan keys cumulative totals the same way, so B's
    // sample is not mistaken for a repeat of A's.
    assert_eq!(quota_len, 2);
    assert_eq!(positive_samples, 2);
    let mut totals = scan
        .events
        .iter()
        .map(|event| {
            (
                event.session.local_session_id_hash.clone(),
                event.usage.computed_total(),
            )
        })
        .collect::<Vec<_>>();
    totals.sort();
    let mut expected = vec![
        (Some(hash_text("session-a")), 100),
        (Some(hash_text("session-b")), 100),
    ];
    expected.sort();
    assert_eq!(totals, expected);
}

#[test]
fn codex_record_without_usage_keeps_token_count_as_the_usage_source() {
    // A record that carries no usable usage cannot stand in for the
    // token_count lines after it, or those responses would vanish.
    let total = serde_json::json!({
        "input_tokens": 100,
        "output_tokens": 20,
        "total_tokens": 120
    });
    let (started, completed) = turn_bounds("2026-09-01T10:00:00Z", "2026-09-01T10:00:05Z");
    let lines = vec![
        started,
        r#"{"timestamp":"2026-09-01T10:00:01Z","type":"token_usage_record","payload":{"turn_id":"00000000-0000-7000-8000-0000000000a1"}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:02Z","type":"token_usage_record","payload":{"usage":{}}}"#.to_string(),
        token_count_line("2026-09-01T10:00:03Z", "codex", Some(total.clone()), total),
        completed,
    ];

    let (scan, _, _) = scan_session_lines(&lines);

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].usage.computed_total(), 120);
    assert_eq!(scan.events[0].usage.requests, Some(1));
}

#[test]
fn codex_unpaired_record_does_not_suppress_a_later_turn() {
    // Turn one ends on a record with no token_count (compaction). Turn two
    // has no usable record, and its token_count happens to repeat that usage.
    let usage = serde_json::json!({"input_tokens": 90, "output_tokens": 10, "total_tokens": 100});
    let (first_started, first_completed) =
        turn_bounds("2026-09-01T10:00:00Z", "2026-09-01T10:00:02Z");
    let (second_started, second_completed) =
        turn_bounds("2026-09-01T10:01:00Z", "2026-09-01T10:01:02Z");
    let lines = vec![
        first_started,
        format!(
            r#"{{"timestamp":"2026-09-01T10:00:01Z","type":"token_usage_record","payload":{{"usage":{usage}}}}}"#
        ),
        first_completed,
        second_started,
        token_count_line("2026-09-01T10:01:01Z", "codex", Some(usage.clone()), usage),
        second_completed,
    ];

    let (scan, _, _) = scan_session_lines(&lines);

    assert_eq!(scan.events.len(), 2);
    assert!(scan
        .events
        .iter()
        .all(|event| event.usage.computed_total() == 100));
}

#[test]
fn codex_malformed_record_after_records_began_keeps_its_token_count() {
    // The first response is recorded normally; the second response's record
    // is malformed, so its token_count is the only evidence of it.
    let first = serde_json::json!({"input_tokens": 90, "output_tokens": 10, "total_tokens": 100});
    let first_total = first.clone();
    let second = serde_json::json!({"input_tokens": 45, "output_tokens": 5, "total_tokens": 50});
    let second_total =
        serde_json::json!({"input_tokens": 135, "output_tokens": 15, "total_tokens": 150});
    let (started, completed) = turn_bounds("2026-09-01T10:00:00Z", "2026-09-01T10:00:09Z");
    let lines = vec![
        started,
        format!(
            r#"{{"timestamp":"2026-09-01T10:00:01Z","type":"token_usage_record","payload":{{"usage":{first}}}}}"#
        ),
        token_count_line("2026-09-01T10:00:02Z", "codex", Some(first_total), first),
        r#"{"timestamp":"2026-09-01T10:00:03Z","type":"token_usage_record","payload":{"usage":{}}}"#.to_string(),
        token_count_line("2026-09-01T10:00:04Z", "codex", Some(second_total), second),
        completed,
    ];

    let (scan, _, _) = scan_session_lines(&lines);

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].usage.computed_total(), 150);
    assert_eq!(scan.events[0].usage.requests, Some(2));
}

#[test]
fn codex_counts_a_decreased_token_total_as_a_reset() {
    // A fork or resume can restart the cumulative total. That is new usage,
    // unlike an exact repeat of the previous total.
    let first = serde_json::json!({
        "input_tokens": 80,
        "output_tokens": 20,
        "total_tokens": 100
    });
    let reset = serde_json::json!({
        "input_tokens": 30,
        "output_tokens": 10,
        "total_tokens": 40
    });
    let (started, completed) = turn_bounds("2026-09-01T10:00:00Z", "2026-09-01T10:00:04Z");
    let lines = vec![
        started,
        token_count_line("2026-09-01T10:00:01Z", "codex", Some(first.clone()), first),
        token_count_line("2026-09-01T10:00:02Z", "codex", Some(reset.clone()), reset),
        completed,
    ];

    let (scan, _, positive_samples) = scan_session_lines(&lines);

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].usage.input_tokens, Some(110));
    assert_eq!(scan.events[0].usage.output_tokens, Some(30));
    assert_eq!(scan.events[0].usage.computed_total(), 140);
    assert_eq!(scan.events[0].usage.requests, Some(2));
    assert_eq!(positive_samples, 2);
}

#[test]
fn codex_keeps_last_token_usage_when_the_cumulative_total_is_missing() {
    let (started, completed) = turn_bounds("2026-09-01T10:00:00Z", "2026-09-01T10:00:04Z");
    let lines = vec![
        started,
        token_count_line(
            "2026-09-01T10:00:01Z",
            "codex",
            None,
            serde_json::json!({"input_tokens": 40, "output_tokens": 10, "total_tokens": 50}),
        ),
        token_count_line(
            "2026-09-01T10:00:02Z",
            "codex",
            None,
            serde_json::json!({"input_tokens": 20, "output_tokens": 5, "total_tokens": 25}),
        ),
        completed,
    ];

    let (scan, _, _) = scan_session_lines(&lines);

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].usage.input_tokens, Some(60));
    assert_eq!(scan.events[0].usage.output_tokens, Some(15));
    assert_eq!(scan.events[0].usage.computed_total(), 75);
    assert_eq!(scan.events[0].usage.requests, Some(2));
}

#[test]
fn codex_compaction_fixture_skips_the_repeated_total_after_compacted() {
    // The fixture's second token_count repeats total_token_usage
    // (total_tokens 11081643) after `compacted`, while last_token_usage changes
    // to the post-compaction context (total_tokens 17983). That line is not a
    // billed response. Counting it added 17983 on top of the four advancing
    // rows: 318182 + 23975 + 24069 + 23998 = 390224.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/compaction");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &root,
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");
    let mut totals: Vec<u64> = scan
        .events
        .iter()
        .map(|event| event.usage.computed_total())
        .collect();
    totals.sort_unstable();

    assert_eq!(
        totals,
        vec![23_975, 23_998, 24_069, 318_182],
        "the phantom 17983 context size must not be added"
    );
    assert_eq!(totals.iter().sum::<u64>(), 390_224);
    assert!(
        scan.quota_observations.len() >= 5,
        "the phantom line still produces a quota observation"
    );
    assert!(scan
        .quota_observations
        .iter()
        .any(|record| { record.observation.usage_sample.is_none() }));
    assert!(scan.quota_observations.iter().all(|record| {
        record
            .observation
            .usage_sample
            .as_ref()
            .map(UsageCounts::computed_total)
            != Some(17_983)
    }));
}

#[test]
fn codex_usage_counts_treat_cache_writes_as_an_inclusive_input_subset() {
    let value = serde_json::json!({
        "input_tokens": 1000,
        "cached_input_tokens": 600,
        "cache_write_input_tokens": 100,
        "output_tokens": 0,
        "total_tokens": 1000
    });

    let usage = codex_usage_counts_from_value(&value);

    assert_eq!(usage.input_tokens, Some(300));
    assert_eq!(usage.cache_read_tokens, Some(600));
    assert_eq!(usage.cache_creation_tokens, Some(100));
    assert_eq!(usage.computed_total(), 1000);

    let camel = serde_json::json!({
        "input_tokens": 1000,
        "cached_input_tokens": 600,
        "cacheWriteInputTokens": 100
    });
    let camel_usage = codex_usage_counts_from_value(&camel);
    assert_eq!(camel_usage.input_tokens, Some(300));
    assert_eq!(camel_usage.cache_read_tokens, Some(600));
    assert_eq!(camel_usage.cache_creation_tokens, Some(100));
}

fn scan_fixture(relative_root: &str) -> crate::AdapterScan {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative_root);
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &root,
        LocationOrigin::Configured,
    );
    scan_codex_source(&CodexAdapter, &source, &options()).expect("scan")
}

#[test]
fn codex_usage_record_fixture_counts_each_record_once_across_eras() {
    // Mixed era: token_count rows before the file's first token_usage_record,
    // then records only. The paired token_count and the post-compaction
    // phantom must not be added, and thread_token_usage is not the response.
    let scan = scan_fixture("tests/fixtures/codex/usage-record/mixed");

    assert_eq!(
        scan.events.len(),
        2,
        "one legacy turn and one record-era turn"
    );
    let legacy = &scan.events[0].usage;
    assert_eq!(legacy.input_tokens, Some(100));
    assert_eq!(legacy.output_tokens, Some(10));
    assert_eq!(legacy.computed_total(), 110);
    assert_eq!(legacy.requests, Some(1));
    assert_eq!(
        scan.events[0]
            .model
            .as_ref()
            .and_then(|model| model.provider_model_id.as_deref()),
        Some("gpt-5")
    );

    let recorded = &scan.events[1].usage;
    assert_eq!(recorded.input_tokens, Some(650));
    assert_eq!(recorded.cache_read_tokens, Some(350));
    assert_eq!(recorded.cache_creation_tokens, Some(0));
    assert_eq!(recorded.output_tokens, Some(45));
    assert_eq!(recorded.reasoning_tokens, Some(15));
    assert_eq!(recorded.computed_total(), 1_060);
    assert_eq!(recorded.requests, Some(2));
    assert_eq!(
        scan.events[1]
            .model
            .as_ref()
            .and_then(|model| model.provider_model_id.as_deref()),
        Some("gpt-5")
    );

    let session_total: u64 = scan
        .events
        .iter()
        .map(|event| event.usage.computed_total())
        .sum();
    assert_eq!(session_total, 1_170);
    assert_eq!(
        scan.quota_observations.len(),
        5,
        "token_count lines still supply quota after records begin"
    );
    // A token_count carries no usage once records begin, but its quota
    // observation still belongs to the turn that the records fed.
    let linked_to = |event_index: usize| {
        scan.quota_observations
            .iter()
            .filter(|record| {
                record.observation.usage_event_id.as_ref()
                    == Some(&scan.events[event_index].event_id)
            })
            .count()
    };
    assert_eq!(linked_to(0), 1, "legacy advancing token_count");
    assert_eq!(linked_to(1), 1, "record-era paired token_count");
    assert!(scan.quota_observations.iter().all(|record| {
        let positive = record
            .observation
            .usage_sample
            .as_ref()
            .is_some_and(|usage| usage.computed_total() > 0);
        positive == record.observation.usage_event_id.is_some()
    }));
}

#[test]
fn codex_subagent_usage_record_fixture_sums_records_and_keeps_parent_ids() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex/usage-record/subagent");
    let path = root.join("sessions/2026/09/01/rollout-fixture-usage-record-subagent.jsonl");
    let records: Vec<CodexTokenUsageRecord> = std::fs::read_to_string(&path)
        .expect("fixture")
        .lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            codex_token_usage_record_from_value(&value)
        })
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[0].parent_session_id(),
        Some("00000000-0000-7000-8000-000000000001")
    );
    assert_eq!(
        records[0].root_turn_id.as_deref(),
        Some("00000000-0000-7000-8000-0000000000a1")
    );
    assert_eq!(
        records[0].turn_id.as_deref(),
        Some("00000000-0000-7000-8000-0000000000b2")
    );
    assert_ne!(records[0].turn_id, records[0].root_turn_id);
    assert_eq!(
        records[0].response_id.as_deref(),
        Some("resp_000000000000000000000000000000000000000000000000003")
    );
    assert_eq!(
        records[1].parent_session_id(),
        Some("00000000-0000-7000-8000-000000000001")
    );
    let record_total: u64 = records
        .iter()
        .map(|record| record.usage.computed_total())
        .sum();

    let scan = scan_fixture("tests/fixtures/codex/usage-record/subagent");
    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].usage.computed_total(), record_total);
    assert_eq!(scan.events[0].usage.computed_total(), 120);
    assert_eq!(scan.events[0].usage.input_tokens, Some(80));
    assert_eq!(scan.events[0].usage.cache_read_tokens, Some(20));
    assert_eq!(scan.events[0].usage.output_tokens, Some(20));
    assert_eq!(scan.events[0].usage.requests, Some(2));
    assert_eq!(
        scan.events[0]
            .model
            .as_ref()
            .and_then(|model| model.provider_model_id.as_deref()),
        Some("gpt-5")
    );

    let mixed = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "tests/fixtures/codex/usage-record/mixed/sessions/2026/09/01/rollout-fixture-usage-record.jsonl",
    );
    let main_records: Vec<CodexTokenUsageRecord> = std::fs::read_to_string(mixed)
        .expect("fixture")
        .lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            codex_token_usage_record_from_value(&value)
        })
        .collect();
    assert!(main_records.len() >= 2);
    assert!(main_records
        .iter()
        .all(|record| record.parent_session_id().is_none()));
    assert!(main_records
        .iter()
        .all(|record| record.turn_id == record.root_turn_id));
}
