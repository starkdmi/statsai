use super::*;

#[test]
fn claude_normalizes_projects_path_to_config_root() {
    let adapter = ClaudeCodeAdapter;
    let source = claude_source_for_root(
        &adapter,
        Path::new("/tmp/claude-home/projects"),
        LocationOrigin::Configured,
    );

    assert_eq!(source.provider, CLAUDE_CODE_PROVIDER);
    assert_eq!(source.path_label.as_deref(), Some("/tmp/claude-home"));
}

#[test]
fn claude_extracts_project_path_and_git_metadata_from_sessions_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let projects = root.join("projects");
    let project_store = projects.join("example-workspace");
    let workspace = root.join("workspace").join("ExampleWorkspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "https://github.com/example-org/example-workspace.git",
        "main",
    );

    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"abc\",\"fullPath\":\"{}\",\"gitBranch\":\"main\",\"projectPath\":\"{}\"}}]}}",
            session_path.display(),
            workspace.display()
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

    assert_eq!(scan.events.len(), 1);
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(workspace.to_string_lossy().as_ref())
    );
    assert_eq!(project.project_label.as_deref(), Some("ExampleWorkspace"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("example-org/example-workspace")
    );
    assert_eq!(project.branch_label.as_deref(), Some("main"));
}

#[test]
fn claude_subagent_transcripts_inherit_project_path_from_sessions_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let projects = root.join("projects");
    let project_store = projects.join("example-workspace");
    let workspace = root.join("workspace").join("ExampleWorkspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "https://github.com/example-org/example-workspace.git",
        "feature/example-subagent-fix",
    );

    let session_file = project_store.join("session-123.jsonl");
    let subagent_dir = project_store.join("session-123").join("subagents");
    std::fs::create_dir_all(&subagent_dir).expect("subagent dir");
    let subagent_file = subagent_dir.join("agent-a.jsonl");
    std::fs::write(
        &subagent_file,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("subagent session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-123\",\"fullPath\":\"{}\",\"gitBranch\":\"feature/example-subagent-fix\",\"projectPath\":\"{}\"}}]}}",
            session_file.display(),
            workspace.display()
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

    assert_eq!(scan.events.len(), 1);
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(workspace.to_string_lossy().as_ref())
    );
    assert_eq!(project.project_label.as_deref(), Some("ExampleWorkspace"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("example-org/example-workspace")
    );
    assert_eq!(
        project.branch_label.as_deref(),
        Some("feature/example-subagent-fix")
    );
}

#[test]
fn claude_project_store_root_falls_back_to_original_path_when_session_index_misses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let projects = root.join("projects");
    let project_store = projects.join("-home-example-src-ExampleWorkspace");
    let workspace = root.join("workspace").join("ExampleWorkspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "https://github.com/example-org/example-workspace.git",
        "main",
    );

    let subagent_dir = project_store.join("unindexed-session").join("subagents");
    std::fs::create_dir_all(&subagent_dir).expect("subagent dir");
    let subagent_file = subagent_dir.join("agent-a.jsonl");
    std::fs::write(
        &subagent_file,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("subagent session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"originalPath\":\"{}\",\"entries\":[{{\"sessionId\":\"indexed-session\",\"fullPath\":\"{}\",\"gitBranch\":\"main\",\"projectPath\":\"{}\"}}]}}",
            workspace.display(),
            project_store.join("indexed-session.jsonl").display(),
            workspace.display()
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

    assert_eq!(scan.events.len(), 1);
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(workspace.to_string_lossy().as_ref())
    );
    assert_eq!(project.project_label.as_deref(), Some("ExampleWorkspace"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("example-org/example-workspace")
    );
}

#[test]
fn claude_extracts_project_context_from_jsonl_when_session_index_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root
        .join("projects")
        .join("-home-example-src-ExampleWorkspace");
    let workspace = root.join("workspace").join("ExampleWorkspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "https://github.com/example-org/example-workspace.git",
        "main",
    );

    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        format!(
            "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"cwd\":\"{}\",\"gitBranch\":\"feature/jsonl-project\",\"message\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}}}\n",
            workspace.display()
        ),
    )
    .expect("session");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(workspace.to_string_lossy().as_ref())
    );
    assert_eq!(project.project_label.as_deref(), Some("ExampleWorkspace"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("example-org/example-workspace")
    );
    assert_eq!(
        project.branch_label.as_deref(),
        Some("feature/jsonl-project")
    );
}

#[test]
fn claude_falls_back_to_valid_project_path_when_cwd_is_invalid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("workspace").join("ExampleWorkspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "https://github.com/example-org/example-workspace.git",
        "main",
    );

    for invalid_cwd in [Value::Null, serde_json::json!(42), serde_json::json!("   ")] {
        let value = serde_json::json!({
            "cwd": invalid_cwd,
            "projectPath": workspace.to_string_lossy(),
            "gitBranch": "main"
        });
        let mut cache = ProjectContextCache::new();
        let project = claude_project_context_from_value(&value, None, &mut cache)
            .expect("projectPath fallback");

        assert_eq!(
            project.path_label.as_deref(),
            Some(workspace.to_string_lossy().as_ref())
        );
        assert_eq!(
            project.repo_label.as_deref(),
            Some("example-org/example-workspace")
        );
    }
}

#[test]
fn claude_jsonl_project_context_overrides_stale_session_index_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root.join("projects").join("example-workspace");
    let stale_workspace = root.join("workspace").join("OldWorkspace");
    let current_workspace = root.join("workspace").join("CurrentWorkspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&stale_workspace).expect("stale workspace");
    std::fs::create_dir_all(&current_workspace).expect("current workspace");
    write_git_fixture(
        &stale_workspace,
        "https://github.com/example-org/old-workspace.git",
        "old-branch",
    );
    write_git_fixture(
        &current_workspace,
        "https://github.com/example-org/current-workspace.git",
        "main",
    );

    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        format!(
            "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"sessionId\":\"abc\",\"cwd\":\"{}\",\"message\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}}}\n",
            current_workspace.display()
        ),
    )
    .expect("session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"abc\",\"fullPath\":\"{}\",\"gitBranch\":\"old-branch\",\"projectPath\":\"{}\"}}]}}",
            session_path.display(),
            stale_workspace.display()
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

    assert_eq!(scan.events.len(), 1);
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(current_workspace.to_string_lossy().as_ref())
    );
    assert_eq!(project.project_label.as_deref(), Some("CurrentWorkspace"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("example-org/current-workspace")
    );
    assert_eq!(project.branch_label.as_deref(), Some("main"));

    assert_eq!(scan.task_spans.len(), 1);
    let task = &scan.task_spans[0];
    assert_eq!(task.project.as_ref(), Some(project));
    assert_eq!(task.project_bucket, project_bucket_key(Some(project)));
    assert_eq!(task.linked_event_ids, vec![scan.events[0].event_id.clone()]);
}

#[test]
fn claude_source_scans_projects_child_when_config_root_is_given() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    let transcripts = dir.path().join("transcripts");
    std::fs::create_dir_all(&projects).expect("projects");
    std::fs::create_dir_all(&transcripts).expect("transcripts");

    let mut project_file = File::create(projects.join("session.jsonl")).expect("project file");
    writeln!(
        project_file,
        "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}}}"
    )
    .expect("write project");
    let mut transcript_file =
        File::create(transcripts.join("transcript.jsonl")).expect("transcript file");
    writeln!(
        transcript_file,
        "{{\"message\":{{\"usage\":{{\"input_tokens\":3,\"output_tokens\":4}}}}}}"
    )
    .expect("write transcript");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");
    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.diagnostics.raw_rows, 1);
    assert_eq!(scan.events[0].usage.computed_total(), 3);
}

#[test]
fn claude_deduplicates_repeated_usage_by_message_and_request_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(&projects).expect("projects");
    let mut file = File::create(projects.join("session.jsonl")).expect("session");

    for (timestamp, uuid) in [
        ("2026-08-05T14:51:09.702Z", "record-1"),
        ("2026-08-05T14:51:09.710Z", "record-2"),
        ("2026-08-05T14:51:11.102Z", "record-3"),
    ] {
        writeln!(
            file,
            r#"{{"timestamp":"{timestamp}","sessionId":"session-1","uuid":"{uuid}","requestId":"request-1","message":{{"id":"message-1","model":"claude-opus-5","usage":{{"input_tokens":2,"cache_creation_input_tokens":746016,"cache_read_input_tokens":16038,"output_tokens":1479}}}}}}"#
        )
        .expect("repeated request");
    }
    writeln!(
        file,
        r#"{{"timestamp":"2026-08-05T14:51:14.209Z","sessionId":"session-1","uuid":"record-4","requestId":"request-2","message":{{"id":"message-1","model":"claude-opus-5","usage":{{"input_tokens":2,"cache_creation_input_tokens":746016,"cache_read_input_tokens":16038,"output_tokens":1479}}}}}}"#
    )
    .expect("distinct request");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 2);
    assert_eq!(scan.diagnostics.duplicate_events, 2);
    assert_eq!(
        scan.events
            .iter()
            .map(|event| event.usage.computed_total())
            .sum::<u64>(),
        1_527_070
    );
}

#[test]
fn claude_streaming_snapshots_keep_the_final_usage_for_one_request() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(&projects).expect("projects");
    let mut file = File::create(projects.join("session.jsonl")).expect("session");

    for (timestamp, uuid, output) in [
        ("2026-08-05T14:51:09.702Z", "record-1", 100),
        ("2026-08-05T14:51:09.710Z", "record-2", 180),
        ("2026-08-05T14:51:11.102Z", "record-3", 240),
    ] {
        writeln!(
            file,
            r#"{{"timestamp":"{timestamp}","sessionId":"session-1","uuid":"{uuid}","requestId":"request-1","message":{{"id":"message-1","model":"claude-opus-5","usage":{{"input_tokens":2,"cache_creation_input_tokens":1000,"cache_read_input_tokens":8000,"output_tokens":{output}}}}}}}"#
        )
        .expect("streaming snapshot");
    }
    writeln!(
        file,
        r#"{{"timestamp":"2026-08-05T14:51:14.209Z","sessionId":"session-1","uuid":"record-4","requestId":"request-2","message":{{"id":"message-1","model":"claude-opus-5","usage":{{"input_tokens":2,"cache_creation_input_tokens":1000,"cache_read_input_tokens":8000,"output_tokens":90}}}}}}"#
    )
    .expect("distinct request");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 2);
    assert_eq!(scan.diagnostics.duplicate_events, 2);

    let streamed = &scan.events[0];
    assert_eq!(streamed.usage.input_tokens, Some(2));
    assert_eq!(streamed.usage.cache_creation_tokens, Some(1000));
    assert_eq!(streamed.usage.cache_read_tokens, Some(8000));
    assert_eq!(streamed.usage.output_tokens, Some(240));
    assert_eq!(streamed.usage.computed_total(), 9242);
    assert_eq!(
        streamed
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_line_number),
        Some(3)
    );
    let expected_cost = statsai_pricing::estimate_cost_at(
        CLAUDE_CODE_PROVIDER,
        streamed.model.as_ref(),
        &streamed.usage,
        &streamed.created_at,
    );
    assert_eq!(
        streamed.cost.estimated_api_equivalent_micro_usd,
        expected_cost.estimated_api_equivalent_micro_usd
    );
    assert!(streamed
        .cost
        .estimated_api_equivalent_micro_usd
        .is_some_and(|micro_usd| micro_usd > 0));
    assert_eq!(
        streamed.cost.estimated_api_equivalent_usd,
        expected_cost.estimated_api_equivalent_usd
    );

    let distinct_request = &scan.events[1];
    assert_ne!(distinct_request.event_id, streamed.event_id);
    assert_eq!(distinct_request.usage.output_tokens, Some(90));
    assert_eq!(
        scan.events
            .iter()
            .map(|event| event.usage.computed_total())
            .sum::<u64>(),
        9242 + 9092
    );
}

#[test]
fn claude_streaming_snapshots_with_equal_timestamps_resolve_by_source_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(&projects).expect("projects");
    let mut file = File::create(projects.join("session.jsonl")).expect("session");

    for (uuid, output) in [("record-1", 100), ("record-2", 180), ("record-3", 240)] {
        writeln!(
            file,
            r#"{{"timestamp":"2026-08-05T14:51:09.702Z","sessionId":"session-1","uuid":"{uuid}","requestId":"request-1","message":{{"id":"message-1","model":"claude-opus-5","usage":{{"input_tokens":2,"cache_creation_input_tokens":1000,"cache_read_input_tokens":8000,"output_tokens":{output}}}}}}}"#
        )
        .expect("streaming snapshot");
    }

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.diagnostics.duplicate_events, 2);
    assert_eq!(scan.events[0].usage.output_tokens, Some(240));
    assert_eq!(
        scan.events[0]
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_line_number),
        Some(3)
    );
}

#[test]
fn claude_stats_cache_is_parsed_as_summary_not_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("projects")).expect("projects");
    let mut file = File::create(dir.path().join("stats-cache.json")).expect("stats cache");
    writeln!(
        file,
        r#"{{
          "version": 2,
          "lastComputedDate": "2026-05-13",
          "firstSessionDate": "2026-01-21T17:21:43.119Z",
          "totalSessions": 61,
          "totalMessages": 15679,
          "modelUsage": {{
            "claude-opus-4-5-thinking": {{
              "inputTokens": 113622256,
              "outputTokens": 387,
              "cacheReadInputTokens": 282480618,
              "cacheCreationInputTokens": 10,
              "costUSD": 12.5
            }},
            "unknown/zero-usage-empty": {{
              "inputTokens": 0,
              "outputTokens": 0,
              "cacheReadInputTokens": 0,
              "cacheCreationInputTokens": 0
            }}
          }}
        }}"#
    )
    .expect("write");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert!(scan.events.is_empty());
    assert_eq!(scan.summaries.len(), 1);
    assert_eq!(scan.diagnostics.skipped_zero_events, 1);
    assert_eq!(
        scan.summaries[0]
            .model
            .as_ref()
            .and_then(|model| model.name.as_deref()),
        Some("claude-opus-4-5-thinking")
    );
    assert_eq!(scan.summaries[0].usage.input_tokens, Some(113622256));
    assert_eq!(scan.summaries[0].usage.cache_read_tokens, Some(282480618));
    assert_eq!(scan.summaries[0].usage.cache_creation_tokens, Some(10));
    assert_eq!(scan.summaries[0].usage.output_tokens, Some(387));
    assert_eq!(scan.summaries[0].cost.provider_reported_usd, Some(1250));
    assert_eq!(scan.summaries[0].metadata.total_sessions, Some(61));
    assert_eq!(scan.summaries[0].metadata.total_messages, Some(15679));
}

#[test]
fn claude_stats_cache_zero_cost_family_alias_still_estimates() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("projects")).expect("projects");
    let mut file = File::create(dir.path().join("stats-cache.json")).expect("stats cache");
    writeln!(
        file,
        r#"{{
          "version": 2,
          "lastComputedDate": "2026-05-13",
          "firstSessionDate": "2026-01-21T17:21:43.119Z",
          "totalSessions": 1,
          "totalMessages": 10,
          "modelUsage": {{
            "claude-opus-4-6-thinking": {{
              "inputTokens": 1000000,
              "outputTokens": 1000000,
              "cacheReadInputTokens": 1000000,
              "cacheCreationInputTokens": 0,
              "costUSD": 0
            }}
          }}
        }}"#
    )
    .expect("write");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert!(scan.events.is_empty());
    assert_eq!(scan.summaries.len(), 1);
    assert_eq!(
        scan.summaries[0]
            .model
            .as_ref()
            .and_then(|model| model.normalized_name.as_deref()),
        Some("claude-opus-4-6")
    );
    assert_eq!(scan.summaries[0].cost.provider_reported_usd, None);
    assert_eq!(
        scan.summaries[0].cost.estimated_api_equivalent_usd,
        Some(3050)
    );
}

#[test]
fn claude_stats_cache_does_not_estimate_aggregate_across_pricing_boundary() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("projects")).expect("projects");
    std::fs::write(
        dir.path().join("stats-cache.json"),
        r#"{
          "version": 2,
          "lastComputedDate": "2026-09-01",
          "firstSessionDate": "2026-08-31T00:00:00Z",
          "totalSessions": 2,
          "totalMessages": 4,
          "modelUsage": {
            "claude-sonnet-5": {
              "inputTokens": 1000000,
              "outputTokens": 1000000,
              "cacheReadInputTokens": 0,
              "cacheCreationInputTokens": 0
            }
          }
        }"#,
    )
    .expect("stats cache");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    assert_eq!(
        scan.summaries[0].cost.estimated_api_equivalent_micro_usd,
        None
    );
    assert_eq!(scan.summaries[0].cost.estimated_api_equivalent_usd, None);
    assert_eq!(
        scan.summaries[0].cost.pricing_source.as_deref(),
        Some("unknown")
    );
}

#[test]
fn claude_scan_respects_selected_cache_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(&projects).expect("projects");

    let first = projects.join("a.jsonl");
    let second = projects.join("b.jsonl");
    std::fs::write(
        &first,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("first");
    std::fs::write(
        &second,
        "{\"timestamp\":\"2026-05-01T00:01:00Z\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n",
    )
    .expect("second");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
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
    assert_eq!(scan.diagnostics.files_scanned, 1);
    assert_eq!(scan.diagnostics.files_skipped_unchanged, 1);
    assert_eq!(scan.events[0].usage.computed_total(), 3);
}

#[test]
fn claude_scan_candidates_change_when_sessions_index_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    let project_store = projects.join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("session");
    let sessions_index = project_store.join("sessions-index.json");
    std::fs::write(
        &sessions_index,
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-1\",\"fullPath\":\"{}\",\"projectPath\":\"/tmp/workspace-a\"}}]}}",
            session_path.display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let first = claude_scan_candidates(&source, "test-adapter").expect("first candidates");
    std::thread::sleep(std::time::Duration::from_millis(5));
    std::fs::write(
        &sessions_index,
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-1\",\"fullPath\":\"{}\",\"projectPath\":\"/tmp/workspace-b\"}}]}}",
            session_path.display()
        ),
    )
    .expect("updated session index");

    let second = claude_scan_candidates(&source, "test-adapter").expect("second candidates");

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0].cache_key, canonical_display(&session_path));
    assert_eq!(second[0].cache_key, canonical_display(&session_path));
    assert_ne!(first[0].cache_signature, second[0].cache_signature);
}

#[test]
fn claude_scan_candidates_invalidate_legacy_cache_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    let project_store = projects.join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("session");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let legacy_namespace = {
        let adapter_id = source.adapter_id.as_deref().unwrap_or("");
        let path_hash = source.path_hash.as_deref().unwrap_or("");
        hash_text(&format!(
            "{SCAN_CACHE_SIGNATURE_VERSION}:{}:{:?}:{adapter_id}:{}:{path_hash}:{}",
            source.provider, source.source_kind, "test-adapter", "project-context.v1"
        ))
    };
    let legacy_namespaces = ScanCacheNamespaces {
        current: legacy_namespace,
        compatible: Vec::new(),
    };
    let legacy_candidate = scan_candidate(session_path.clone(), None, &legacy_namespaces);
    let current = claude_scan_candidates(&source, "test-adapter").expect("current candidates");

    assert_eq!(current.len(), 1);
    assert_eq!(current[0].cache_key, canonical_display(&session_path));
    assert_ne!(legacy_candidate.cache_signature, current[0].cache_signature);
}

#[test]
fn claude_archive_candidates_use_a_scoped_parser_revision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project_store = dir.path().join("projects").join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"sessionId\":\"session-1\",\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
    )
    .expect("session");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );
    let adapter = ClaudeCodeAdapter;

    let usage = adapter.scan_candidates(&source).expect("usage candidates");
    let archive = adapter
        .archive_scan_candidates(&source)
        .expect("archive candidates");

    assert_eq!(usage.len(), 1);
    assert_eq!(archive.len(), 1);
    assert_eq!(archive[0].cache_key, usage[0].cache_key);
    assert_ne!(archive[0].cache_signature, usage[0].cache_signature);
    assert!(archive[0].compatible_cache_signatures.is_empty());
}

#[test]
fn usage_counts_support_common_shapes() {
    let value: Value = serde_json::json!({
        "inputTokens": 10,
        "outputTokens": 20,
        "cacheCreationInputTokens": 2,
        "cacheReadInputTokens": 3
    });
    let usage = claude_usage_counts_from_value(&value);
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(20));
    assert_eq!(usage.cache_creation_tokens, Some(2));
    assert_eq!(usage.cache_read_tokens, Some(3));
    assert_eq!(usage.computed_total(), 35);
}

#[test]
fn claude_usage_counts_preserve_cache_creation_lifetimes() {
    let value: Value = serde_json::json!({
        "input_tokens": 10,
        "output_tokens": 20,
        "cache_creation_input_tokens": 248,
        "cache_creation": {
            "ephemeral_5m_input_tokens": 148,
            "ephemeral_1h_input_tokens": 100
        }
    });

    let usage = claude_usage_counts_from_value(&value);

    assert_eq!(usage.cache_creation_tokens, Some(248));
    assert_eq!(usage.cache_creation_5m_tokens, Some(148));
    assert_eq!(usage.cache_creation_1h_tokens, Some(100));
    assert_eq!(usage.computed_total(), 278);
}

#[test]
fn claude_usage_counts_derive_combined_cache_creation_tokens() {
    let value: Value = serde_json::json!({
        "cache_creation": {
            "ephemeral_5m_input_tokens": 8,
            "ephemeral_1h_input_tokens": 5
        }
    });

    let usage = claude_usage_counts_from_value(&value);

    assert_eq!(usage.cache_creation_tokens, Some(13));
    assert_eq!(usage.cache_creation_5m_tokens, Some(8));
    assert_eq!(usage.cache_creation_1h_tokens, Some(5));
}

#[test]
fn claude_adapter_does_not_infer_reasoning_level_from_thinking_model_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects/workspace");
    std::fs::create_dir_all(&projects).expect("projects");
    std::fs::write(
        projects.join("session.jsonl"),
        serde_json::json!({
            "timestamp": "2026-05-01T00:00:00Z",
            "sessionId": "session-thinking",
            "model": "claude-opus-4-5-thinking",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20
            }
        })
        .to_string()
            + "\n",
    )
    .expect("write session");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let model = scan.events[0].model.as_ref().expect("model");
    assert_eq!(model.name.as_deref(), Some("claude-opus-4-5-thinking"));
    assert_eq!(model.reasoning_level, None);
    assert_eq!(model.reasoning_level_raw, None);
}

#[test]
fn claude_collects_effort_and_effective_speed_but_ignores_service_tier() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects/workspace");
    std::fs::create_dir_all(&projects).expect("projects");
    std::fs::write(
        projects.join("session.jsonl"),
        serde_json::json!({
            "timestamp": "2026-08-01T00:00:00Z",
            "sessionId": "session-fast",
            "type": "assistant",
            "effort": "medium",
            "message": {
                "role": "assistant",
                "model": "claude-opus-5",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "speed": "fast",
                    "service_tier": "priority"
                }
            }
        })
        .to_string()
            + "\n",
    )
    .expect("write session");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let model = scan.events[0].model.as_ref().expect("model");
    assert_eq!(model.speed.as_deref(), Some("fast"));
    assert_eq!(model.reasoning_level, Some(ReasoningLevel::Medium));
    assert_eq!(model.reasoning_level_raw.as_deref(), Some("medium"));
    assert_eq!(
        scan.events[0].cost.estimated_api_equivalent_micro_usd,
        Some(2_000)
    );
    assert_eq!(
        scan.events[0].cost.pricing_source.as_deref(),
        Some("claude_code_api_pricing:claude-opus-5:fast")
    );
    assert!(!serde_json::to_value(model)
        .expect("serialize model")
        .to_string()
        .contains("service_tier"));
    assert!(scan.events[0].runtime.is_none());
}

#[test]
fn claude_carries_max_thinking_tokens_forward_as_raw_reasoning_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects/workspace");
    std::fs::create_dir_all(&projects).expect("projects");
    std::fs::write(
        projects.join("session.jsonl"),
        [
            serde_json::json!({
                "timestamp": "2026-05-01T00:00:00Z",
                "sessionId": "session-thinking-budget",
                "type": "user",
                "thinkingMetadata": {
                    "maxThinkingTokens": 31999
                },
                "message": {
                    "role": "user",
                    "content": "hello"
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-05-01T00:00:02Z",
                "sessionId": "session-thinking-budget",
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "model": "claude-opus-4-5-thinking",
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 20
                    }
                }
            })
            .to_string(),
        ]
        .join("\n")
            + "\n",
    )
    .expect("write session");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let model = scan.events[0].model.as_ref().expect("model");
    assert_eq!(model.name.as_deref(), Some("claude-opus-4-5-thinking"));
    assert_eq!(model.reasoning_level, None);
    assert_eq!(model.reasoning_level_raw.as_deref(), Some("31999"));
}
