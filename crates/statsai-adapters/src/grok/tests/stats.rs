use super::*;

#[test]
fn grok_build_session_summary_records_local_session_stats() {
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
            "info": {"id": "session-1", "cwd": dir.path()},
            "updated_at": "2026-06-09T13:53:52Z",
            "num_messages": 12,
            "current_model_id": "grok-build",
            "chat_format_version": 1,
            "git_remotes": ["https://github.com/example/repo.git"],
            "head_branch": "main"
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "sessionDurationSeconds": 60,
            "avgTimeToFirstTokenMs": 1200,
            "avgResponseTimeMs": 2400,
            "turnCount": 3,
            "userMessageCount": 3,
            "assistantMessageCount": 9,
            "contextTokensUsed": 42_000,
            "contextWindowTokens": 256_000
        })
        .to_string(),
    )
    .expect("signals");
    std::fs::write(
        session.join("chat_history.jsonl"),
        [
            serde_json::json!({"type": "system", "content": "system"}).to_string(),
            serde_json::json!({"type": "user", "content": [{"type": "text", "text": "hello"}]})
                .to_string(),
            serde_json::json!({"type": "assistant", "content": "hi"}).to_string(),
            serde_json::json!({"type": "reasoning", "summary": "thinking"}).to_string(),
            serde_json::json!({"type": "tool_result", "content": "ok"}).to_string(),
        ]
        .join("\n"),
    )
    .expect("chat history");
    std::fs::write(
        session.join("updates.jsonl"),
        [
            serde_json::json!({"params": {"_meta": {"promptId": "p1", "totalTokens": 41_000}}})
                .to_string(),
            serde_json::json!({"params": {"_meta": {"promptId": "p1", "totalTokens": 45_000}}})
                .to_string(),
            serde_json::json!({"params": {"_meta": {"promptId": "p2", "totalTokens": 7_000}}})
                .to_string(),
            serde_json::json!({"params": {"update": {"tokens_used": 40_000}}}).to_string(),
        ]
        .join("\n"),
    )
    .expect("updates");
    std::fs::write(
        session.join("events.jsonl"),
        serde_json::json!({"type": "turn", "phase": "done"}).to_string(),
    )
    .expect("events");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 0);
    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(summary.provider, GROK_BUILD_PROVIDER);
    assert_eq!(summary.metadata.total_sessions, Some(1));
    assert_eq!(summary.metadata.total_messages, Some(12));
    assert_eq!(summary.usage.input_tokens, Some(52_000));
    assert_eq!(summary.usage.total_tokens, Some(52_000));
    assert_eq!(summary.usage.requests, Some(3));
    assert_eq!(summary.cost.estimated_api_equivalent_usd, Some(5));
    assert_eq!(summary.cost.confidence, Confidence::Low);
    let project = summary.project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(display_path(dir.path()).as_str())
    );
    assert_eq!(
        project.path_hash.as_deref(),
        Some(path_hash(dir.path()).as_str())
    );
    assert_eq!(project.repo_label.as_deref(), Some("example/repo"));
    assert_eq!(project.branch_label.as_deref(), Some("main"));
    assert_eq!(
        summary
            .metrics
            .as_ref()
            .and_then(|metrics| metrics.user_messages),
        Some(3)
    );
    assert_eq!(
        summary
            .metadata
            .summary_version
            .as_deref()
            .map(|value| value.contains("reasoning=1")),
        Some(true)
    );
    assert_eq!(
        summary
            .metadata
            .summary_version
            .as_deref()
            .map(|value| value.contains("chat_rows=5")),
        Some(true)
    );
    assert_eq!(
        summary
            .metadata
            .summary_version
            .as_deref()
            .map(|value| value.contains("prompts=2;prompt_context_tokens=52000")),
        Some(true)
    );
}

#[test]
fn grok_build_prefers_unified_log_inference_usage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-usage");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-usage", "cwd": dir.path()},
            "updated_at": "2026-06-09T13:53:52Z",
            "current_model_id": "grok-composer-2.5-fast",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("updates.jsonl"),
        serde_json::json!({"params": {"_meta": {"promptId": "p1", "totalTokens": 999_999}}})
            .to_string(),
    )
    .expect("updates");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-06-09T14:22:45.131Z",
                "sid": "session-usage",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 1_000_000,
                    "cached_prompt_tokens": 400_000,
                    "completion_tokens": 100_000,
                    "reasoning_tokens": 50_000,
                    "model_elapsed_ms": 3_000,
                    "ttft_ms": 1_000
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-06-09T14:22:48.525Z",
                "sid": "other-session",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 9_000_000,
                    "cached_prompt_tokens": 0,
                    "completion_tokens": 9_000_000
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

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(summary.usage.input_tokens, Some(600_000));
    assert_eq!(summary.usage.cache_read_tokens, Some(400_000));
    assert_eq!(summary.usage.cache_creation_tokens, None);
    assert_eq!(summary.usage.output_tokens, Some(100_000));
    assert_eq!(summary.usage.reasoning_tokens, Some(50_000));
    assert_eq!(summary.usage.requests, Some(1));
    assert_eq!(summary.cost.estimated_api_equivalent_usd, Some(425));
    assert_eq!(summary.cost.confidence, Confidence::Medium);
    assert_eq!(
        summary
            .cost
            .pricing_source
            .as_deref()
            .map(|value| value.contains("cursor_model_pricing:composer-2.5-fast")),
        Some(true)
    );
    assert_eq!(
        summary
            .metadata
            .summary_version
            .as_deref()
            .map(|value| value.contains("inference_rows=1;usage_source=unified_log")),
        Some(true)
    );
}

#[test]
fn grok_build_keeps_aggregate_prompt_context_conservative() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-aggregate");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-aggregate", "cwd": dir.path()},
            "updated_at": "2026-08-16T18:39:58Z",
            "current_model_id": "grok-4.6",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "turnCount": 2,
            "contextTokensUsed": 300_000
        })
        .to_string(),
    )
    .expect("signals");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(
        summary
            .model
            .as_ref()
            .and_then(|model| model.normalized_name.as_deref()),
        Some("grok-4.6")
    );
    assert_eq!(summary.usage.input_tokens, Some(300_000));
    assert_eq!(summary.usage.requests, Some(2));
    assert_eq!(
        summary.cost.estimated_api_equivalent_micro_usd,
        Some(600_000)
    );
    assert_eq!(summary.cost.confidence, Confidence::Low);
    assert_eq!(
        summary.cost.pricing_source.as_deref(),
        Some("xai_api_pricing:grok-4.6:prompt_context_token_footprint")
    );
}
