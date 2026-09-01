use super::*;

#[test]
fn grok_build_scan_tolerates_malformed_jsonl_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-malformed");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-malformed", "cwd": dir.path()},
            "updated_at": "2026-06-09T13:53:52Z",
            "current_model_id": "grok-build"
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "turnCount": 1,
            "contextTokensUsed": 999
        })
        .to_string(),
    )
    .expect("signals");
    std::fs::write(
        session.join("chat_history.jsonl"),
        [
            serde_json::json!({"type": "user", "content": "hello"}).to_string(),
            "{\"type\":\"assistant\"".to_string(),
        ]
        .join("\n"),
    )
    .expect("chat");
    std::fs::write(
        session.join("updates.jsonl"),
        [
            serde_json::json!({"params": {"_meta": {"promptId": "p1", "totalTokens": 123}}})
                .to_string(),
            "{\"params\":".to_string(),
        ]
        .join("\n"),
    )
    .expect("updates");
    std::fs::write(
        session.join("events.jsonl"),
        [
            serde_json::json!({"type": "turn"}).to_string(),
            "{".to_string(),
        ]
        .join("\n"),
    )
    .expect("events");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            "{".to_string(),
            serde_json::json!({
                "sid": "session-malformed",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 100,
                    "cached_prompt_tokens": 10,
                    "completion_tokens": 20
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.diagnostics.invalid_rows, 4);
    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(summary.usage.input_tokens, Some(90));
    assert_eq!(summary.usage.cache_read_tokens, Some(10));
    assert_eq!(summary.usage.output_tokens, Some(20));
    assert_eq!(summary.usage.total_tokens, None);
    assert_eq!(summary.usage.requests, Some(1));
}

#[test]
fn grok_summary_candidate_changes_when_session_siblings_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-1");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-1"},
            "updated_at": "2026-06-09T13:53:52Z"
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(session.join("signals.json"), "{}").expect("signals");
    std::fs::write(session.join("chat_history.jsonl"), "").expect("chat");
    std::fs::write(session.join("updates.jsonl"), "").expect("updates");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let before = grok_build_scan_candidates(&source, "0").expect("before");
    std::fs::write(
        session.join("chat_history.jsonl"),
        serde_json::json!({"type": "user", "content": "hello"}).to_string(),
    )
    .expect("updated chat");
    let after = grok_build_scan_candidates(&source, "0").expect("after");

    assert_eq!(before.len(), 1);
    assert_eq!(after.len(), 1);
    assert_eq!(
        before[0].path.file_name().and_then(|name| name.to_str()),
        Some("summary.json")
    );
    assert_ne!(before[0].cache_signature, after[0].cache_signature);
}

#[test]
fn grok_candidates_tolerate_malformed_unified_log_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-1");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-1"},
            "updated_at": "2026-06-09T13:53:52Z"
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(session.join("signals.json"), "{}").expect("signals");
    std::fs::write(session.join("chat_history.jsonl"), "").expect("chat");
    std::fs::write(session.join("updates.jsonl"), "").expect("updates");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            "{".to_string(),
            serde_json::json!({
                "sid": "session-1",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 100,
                    "cached_prompt_tokens": 10,
                    "completion_tokens": 20
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let candidates = grok_build_scan_candidates(&source, "0").expect("candidates");

    assert_eq!(candidates.len(), 1);
}

#[test]
fn grok_summary_candidate_changes_only_for_matching_unified_log_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_a = dir.path().join("sessions/%2Fworkspace/session-a");
    let session_b = dir.path().join("sessions/%2Fworkspace/session-b");
    std::fs::create_dir_all(&session_a).expect("session a");
    std::fs::create_dir_all(&session_b).expect("session b");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs");
    for (session_dir, session_id) in [(&session_a, "session-a"), (&session_b, "session-b")] {
        std::fs::write(
            session_dir.join("summary.json"),
            serde_json::json!({
                "info": {"id": session_id},
                "updated_at": "2026-06-09T13:53:52Z"
            })
            .to_string(),
        )
        .expect("summary");
        std::fs::write(session_dir.join("signals.json"), "{}").expect("signals");
        std::fs::write(session_dir.join("chat_history.jsonl"), "").expect("chat");
        std::fs::write(session_dir.join("updates.jsonl"), "").expect("updates");
    }
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        serde_json::json!({
            "ts": "2026-06-09T14:22:45.131Z",
            "sid": "session-a",
            "msg": "shell.turn.inference_done",
            "ctx": {
                "prompt_tokens": 100,
                "cached_prompt_tokens": 10,
                "completion_tokens": 20
            }
        })
        .to_string(),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let before = grok_build_scan_candidates(&source, "0").expect("before");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-06-09T14:22:45.131Z",
                "sid": "session-a",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 100,
                    "cached_prompt_tokens": 10,
                    "completion_tokens": 20
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-06-09T14:25:45.131Z",
                "sid": "session-b",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 200,
                    "cached_prompt_tokens": 20,
                    "completion_tokens": 30
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("updated unified log");
    let after = grok_build_scan_candidates(&source, "0").expect("after");

    let before_a = before
        .iter()
        .find(|candidate| candidate.path.starts_with(&session_a))
        .expect("candidate a");
    let before_b = before
        .iter()
        .find(|candidate| candidate.path.starts_with(&session_b))
        .expect("candidate b");
    let after_a = after
        .iter()
        .find(|candidate| candidate.path.starts_with(&session_a))
        .expect("candidate a after");
    let after_b = after
        .iter()
        .find(|candidate| candidate.path.starts_with(&session_b))
        .expect("candidate b after");

    assert_eq!(before_a.cache_signature, after_a.cache_signature);
    assert_ne!(before_b.cache_signature, after_b.cache_signature);
}
