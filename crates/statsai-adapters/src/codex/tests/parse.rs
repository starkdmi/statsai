use super::*;

#[test]
fn codex_discovers_one_logical_source_per_home() {
    let adapter = CodexAdapter;
    let source = codex_source_for_root(
        &adapter,
        Path::new("/tmp/codex-home"),
        LocationOrigin::Configured,
    );

    assert_eq!(source.provider, CODEX_PROVIDER);
    assert_eq!(source.path_label.as_deref(), Some("/tmp/codex-home"));
}

#[test]
fn codex_extracts_cwd_and_git_metadata_from_session_meta() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    let workspace = dir.path().join("workspace").join("ai-stats");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "git@github.com:example-org/example-workspace.git",
        "main",
    );

    let session_path = sessions.join("session.jsonl");
    let mut file = File::create(&session_path).expect("session file");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{{"cwd":"{}","git":{{"repository_url":"git@github.com:example-org/example-workspace.git","branch":"main"}}}}}}"#,
        workspace.display()
    )
    .expect("write session meta");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:01:00Z","usage":{{"input_tokens":10,"output_tokens":5}},"model":"gpt-5"}}"#
    )
    .expect("write usage");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(workspace.to_string_lossy().as_ref())
    );
    assert_eq!(project.project_label.as_deref(), Some("ai-stats"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("example-org/example-workspace")
    );
    assert_eq!(project.branch_label.as_deref(), Some("main"));
}

#[test]
fn codex_line_filter_skips_non_message_response_items() {
    let reasoning = r#"{"timestamp":"2026-06-03T09:36:21.793Z","type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"abc"}}"#;
    let function_call = r#"{"timestamp":"2026-06-03T09:36:24.895Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}"}}"#;
    let user_message = r#"{"timestamp":"2026-06-03T09:36:25.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#;

    assert_eq!(codex_line_kind(reasoning), CodexLineKind::Irrelevant);
    assert_eq!(codex_line_kind(function_call), CodexLineKind::Irrelevant);
    assert_eq!(
        codex_line_kind(user_message),
        CodexLineKind::ResponseItemMessage
    );

    let unrelated_event = r#"{"timestamp":"2026-06-03T09:36:26.000Z","type":"event_msg","payload":{"type":"agent_message","model":"gpt-incorrect"}}"#;
    assert_eq!(codex_line_kind(unrelated_event), CodexLineKind::Irrelevant);
    assert!(!is_codex_quota_line_structurally(reasoning));

    let reordered_quota = r#"{"payload": {"rate_limits": {"primary": {"used_percent": 1, "window_minutes": 300, "resets_at": 1787832000}}, "type": "token_count"}, "type": "event_msg"}"#;
    assert_eq!(codex_line_kind(reordered_quota), CodexLineKind::Irrelevant);
    assert!(is_codex_quota_line_structurally(reordered_quota));
}

#[test]
fn codex_line_kind_uses_header_window_for_large_user_messages() {
    let giant_prompt = "A".repeat(2_000_000);
    let user_message = format!(
        r#"{{"timestamp":"2026-06-03T09:36:25.000Z","type":"event_msg","payload":{{"type":"user_message","message":"{}"}}}}"#,
        giant_prompt
    );
    let headless_usage = r#"{"timestamp":"2026-05-01T00:00:00Z","message":{"usage":{"input_tokens":1,"output_tokens":2}}}"#;

    assert_eq!(
        codex_line_kind(&user_message),
        CodexLineKind::EventUserMessage
    );
    assert_eq!(
        codex_line_kind(headless_usage),
        CodexLineKind::HeadlessUsage
    );
}

#[test]
fn codex_task_spans_prefer_real_user_message_over_wrapper_response_item() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    let workspace = dir.path().join("workspace").join("product-app");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "git@github.com:example-org/example-workspace.git",
        "main",
    );

    let session_path = sessions.join("session.jsonl");
    let mut file = File::create(&session_path).expect("session file");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{{"cwd":"{}","git":{{"repository_url":"git@github.com:example-org/example-workspace.git","branch":"main"}}}}}}"#,
        workspace.display()
    )
    .expect("write session meta");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:01Z","type":"event_msg","payload":{{"type":"task_started","started_at":"2026-06-01T08:00:01Z"}}}}"#
    )
    .expect("write start");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:02Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"<INSTRUCTIONS>\nRead the relevant guide before editing.\n</INSTRUCTIONS>\n<environment_context>\n<cwd>{}</cwd>\n</environment_context>"}}]}}}}"#,
        workspace.display()
    )
    .expect("write wrapper message");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:03Z","type":"event_msg","payload":{{"type":"user_message","message":"Implement device renaming on web and api."}}}}"#
    )
    .expect("write user message");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:04Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}}}}}}}}"#
    )
    .expect("write tokens");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:05Z","type":"event_msg","payload":{{"type":"task_complete","completed_at":"2026-06-01T08:00:05Z","duration_ms":4000}}}}"#
    )
    .expect("write complete");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.task_spans.len(), 1);
    let span = &scan.task_spans[0];
    assert_eq!(span.title, "Implement device renaming on web and api");
    assert_eq!(span.title_source.as_deref(), Some("user_prompt"));
    assert_eq!(
        span.summary_preview.as_deref(),
        Some("Implement device renaming on web and api")
    );
}

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
fn codex_task_spans_keep_provider_native_user_message_when_wrappers_come_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    let workspace = dir.path().join("workspace").join("product-app");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "git@github.com:example-org/example-workspace.git",
        "main",
    );

    let session_path = sessions.join("session.jsonl");
    let mut file = File::create(&session_path).expect("session file");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{{"cwd":"{}","git":{{"repository_url":"git@github.com:example-org/example-workspace.git","branch":"main"}}}}}}"#,
        workspace.display()
    )
    .expect("write session meta");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:01Z","type":"event_msg","payload":{{"type":"task_started","started_at":"2026-06-01T08:00:01Z"}}}}"#
    )
    .expect("write start");
    for index in 0..3 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-06-01T08:00:0{}Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"<environment_context>\n<cwd>{}</cwd>\n</environment_context>\n# Wrapper {}\nCode review guidelines"}}]}}}}"#,
            index + 2,
            workspace.display(),
            index + 1,
        )
        .expect("write wrapper message");
    }
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:05Z","type":"event_msg","payload":{{"type":"user_message","message":"Implement device renaming on web and api."}}}}"#
    )
    .expect("write user message");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:06Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}}}}}}}}"#
    )
    .expect("write tokens");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:07Z","type":"event_msg","payload":{{"type":"task_complete","completed_at":"2026-06-01T08:00:07Z","duration_ms":6000}}}}"#
    )
    .expect("write complete");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.task_spans.len(), 1);
    let span = &scan.task_spans[0];
    assert_eq!(span.title, "Implement device renaming on web and api");
    assert_eq!(span.title_source.as_deref(), Some("user_prompt"));
}

#[test]
fn codex_task_spans_fall_back_to_response_item_when_event_message_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    let workspace = dir.path().join("workspace").join("product-app");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "git@github.com:example-org/example-workspace.git",
        "main",
    );

    let session_path = sessions.join("session.jsonl");
    let mut file = File::create(&session_path).expect("session file");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{{"cwd":"{}","git":{{"repository_url":"git@github.com:example-org/example-workspace.git","branch":"main"}}}}}}"#,
        workspace.display()
    )
    .expect("write session meta");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:01Z","type":"event_msg","payload":{{"type":"task_started","started_at":"2026-06-01T08:00:01Z"}}}}"#
    )
    .expect("write start");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:02Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Please fix the task title fallback for older Codex logs."}}]}}}}"#
    )
    .expect("write response item");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:03Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}}}}}}}}"#
    )
    .expect("write tokens");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:04Z","type":"event_msg","payload":{{"type":"task_complete","completed_at":"2026-06-01T08:00:04Z","duration_ms":3000}}}}"#
    )
    .expect("write complete");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.task_spans.len(), 1);
    let span = &scan.task_spans[0];
    assert_eq!(
        span.title,
        "fix the task title fallback for older Codex logs"
    );
    assert_eq!(span.title_source.as_deref(), Some("user_prompt"));
}

#[test]
fn codex_task_spans_capture_thread_id_from_session_meta() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    let workspace = dir.path().join("workspace").join("ai-stats");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "git@github.com:example-org/example-workspace.git",
        "main",
    );

    let session_path = sessions.join("session.jsonl");
    let mut file = File::create(&session_path).expect("session file");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{{"id":"thread-123","thread_name":"Fix parser bug","cwd":"{}","git":{{"repository_url":"git@github.com:example-org/example-workspace.git","branch":"main"}}}}}}"#,
        workspace.display()
    )
    .expect("write session meta");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:00Z","type":"turn_context","payload":{{"model":"gpt-5"}}}}"#
    )
    .expect("write context");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:01Z","type":"event_msg","payload":{{"type":"task_started","started_at":"2026-06-01T08:00:01Z"}}}}"#
    )
    .expect("write start");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:02Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Fix parser bug in statsai scan"}}]}}}}"#
    )
    .expect("write user message");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:03Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}}}}}}}}"#
    )
    .expect("write tokens");
    writeln!(
        file,
        r#"{{"timestamp":"2026-06-01T08:00:04Z","type":"event_msg","payload":{{"type":"task_complete","completed_at":"2026-06-01T08:00:04Z","duration_ms":3000}}}}"#
    )
    .expect("write complete");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.task_spans.len(), 1);
    assert_eq!(scan.task_spans[0].thread_id.as_deref(), Some("thread-123"));
    // The declared session id is the session identity now: it is the same
    // UUID telemetry calls `conversation.id`, so usage, tasks, and
    // account bindings all meet on one key instead of a file path.
    assert_eq!(scan.task_spans[0].session_id.as_deref(), Some("thread-123"));
    assert_eq!(scan.task_spans[0].title, "Fix parser bug");
}

#[test]
fn codex_source_scans_sessions_and_archived_sessions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    let archived = dir.path().join("archived_sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::create_dir_all(&archived).expect("archived");

    let mut active_file = File::create(sessions.join("active.jsonl")).expect("active fixture");
    writeln!(
        active_file,
        "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}"
    )
    .expect("write active");
    let mut archived_file = File::create(archived.join("old.jsonl")).expect("archived fixture");
    writeln!(
        archived_file,
        "{{\"timestamp\":\"2026-05-02T00:00:00Z\",\"usage\":{{\"input_tokens\":3,\"output_tokens\":4}}}}"
    )
    .expect("write archived");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");
    assert_eq!(scan.events.len(), 2);
    assert_eq!(scan.diagnostics.raw_rows, 2);
}

#[test]
fn codex_scan_respects_selected_cache_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let first = sessions.join("a.jsonl");
    let second = sessions.join("b.jsonl");
    std::fs::write(
        &first,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("first");
    std::fs::write(
        &second,
        "{\"timestamp\":\"2026-05-01T00:01:00Z\",\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}\n",
    )
    .expect("second");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let selected = [canonical_display(&second)].into_iter().collect();
    let scan = scan_codex_source(
        &CodexAdapter,
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
    assert_eq!(scan.events[0].usage.computed_total(), 7);
}

#[test]
fn codex_scan_candidates_ignore_auth_json_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let session_path = sessions.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("session");
    std::fs::write(
        dir.path().join("auth.json"),
        "{\"chatgpt_account_id\":\"acct-one\"}\n",
    )
    .expect("auth one");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let first = codex_scan_candidates(&source, "test-adapter").expect("first candidates");
    std::thread::sleep(std::time::Duration::from_millis(5));
    std::fs::write(
        dir.path().join("auth.json"),
        "{\"chatgpt_account_id\":\"acct-two\"}\n",
    )
    .expect("auth two");
    let second = codex_scan_candidates(&source, "test-adapter").expect("second candidates");

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0].cache_key, canonical_display(&session_path));
    assert_eq!(first[0].cache_signature, second[0].cache_signature);
}

#[test]
fn codex_scan_candidates_accept_legacy_auth_dependent_signatures() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let session_path = sessions.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("session");
    std::fs::write(
        dir.path().join("auth.json"),
        "{\"chatgpt_account_id\":\"acct-one\"}\n",
    )
    .expect("auth");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );
    let candidates = codex_scan_candidates(&source, "test-adapter").expect("candidates");
    let auth_dependency = file_metadata_signature(&codex_source_root(dir.path()).join("auth.json"));
    let cache_namespaces = scan_cache_namespaces(&source, "test-adapter");
    let legacy_candidate = scan_candidate(
        session_path.clone(),
        Some(auth_dependency.as_str()),
        &cache_namespaces,
    );

    assert_eq!(candidates.len(), 1);
    assert!(candidates[0]
        .compatible_cache_signatures
        .contains(&legacy_candidate.cache_signature));
}

#[test]
fn codex_scan_candidates_accept_legacy_missing_auth_signatures() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let session_path = sessions.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("session");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );
    let candidates = codex_scan_candidates(&source, "test-adapter").expect("candidates");
    let auth_dependency = file_metadata_signature(&codex_source_root(dir.path()).join("auth.json"));
    let cache_namespaces = scan_cache_namespaces(&source, "test-adapter");
    let legacy_candidate = scan_candidate(
        session_path.clone(),
        Some(auth_dependency.as_str()),
        &cache_namespaces,
    );

    assert_eq!(candidates.len(), 1);
    assert!(candidates[0]
        .compatible_cache_signatures
        .contains(&legacy_candidate.cache_signature));
}

#[test]
fn codex_scan_candidates_are_stable_for_same_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let session_path = sessions.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("session");

    let hinted = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );
    let remapped = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let first = codex_scan_candidates(&hinted, "test-adapter").expect("first candidates");
    let second = codex_scan_candidates(&remapped, "test-adapter").expect("second candidates");

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0].cache_key, canonical_display(&session_path));
    assert_eq!(first[0].cache_signature, second[0].cache_signature);
}

#[test]
fn codex_scan_candidates_are_stable_across_package_versions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::write(
        sessions.join("session.jsonl"),
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("session");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let before = codex_scan_candidates(&source, "0.3.1").expect("before");
    let after = codex_scan_candidates(&source, "0.3.2").expect("after");

    assert_eq!(before[0].cache_signature, after[0].cache_signature);
}

#[test]
fn codex_scan_candidates_accept_same_release_versioned_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let session_path = sessions.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("session");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );
    let namespaces = scan_cache_namespaces(&source, "0.3.1");
    let legacy_signature = build_scan_cache_signature(
        &namespaces.compatible[0],
        &file_metadata_signature(&session_path),
        None,
    );

    let candidates = codex_scan_candidates(&source, "0.3.1").expect("candidates");

    assert!(candidates[0]
        .compatible_cache_signatures
        .contains(&legacy_signature));
}

#[test]
fn codex_scan_candidates_invalidate_legacy_cache_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let session_path = sessions.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("session");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let legacy_namespace = {
        let adapter_id = source.adapter_id.as_deref().unwrap_or("");
        let path_hash = source.path_hash.as_deref().unwrap_or("");
        hash_text(&format!(
            "{SCAN_CACHE_SIGNATURE_VERSION}:{}:{:?}:{adapter_id}:{}:{path_hash}",
            source.provider, source.source_kind, "test-adapter"
        ))
    };
    let legacy_namespaces = ScanCacheNamespaces {
        current: legacy_namespace,
        compatible: Vec::new(),
    };
    let legacy_candidate = scan_candidate(session_path.clone(), None, &legacy_namespaces);
    let current = codex_scan_candidates(&source, "test-adapter").expect("current candidates");

    assert_eq!(current.len(), 1);
    assert_eq!(current[0].cache_key, canonical_display(&session_path));
    assert_ne!(legacy_candidate.cache_signature, current[0].cache_signature);
}

#[test]
fn codex_source_path_pointing_at_sessions_uses_parent_auth_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join(".codex");
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::write(
        root.join("auth.json"),
        "{\"chatgpt_account_id\":\"acct-real\"}\n",
    )
    .expect("auth");
    std::fs::write(
        sessions.join("session.jsonl"),
        "{\"timestamp\":\"2026-05-01T00:01:00Z\",\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}\n",
    )
    .expect("session");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &sessions,
        LocationOrigin::Configured,
    );

    let candidates = codex_scan_candidates(&source, "test-adapter").expect("candidates");
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].cache_key,
        canonical_display(&sessions.join("session.jsonl"))
    );
    assert_eq!(scan.events.len(), 1);
    assert_eq!(
        scan.verified_source_state
            .as_ref()
            .and_then(|state| state.provider_user_id.as_deref()),
        Some("acct-real")
    );
}

#[test]
fn codex_root_without_usage_directories_has_no_candidates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("not-a-codex-home");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        root.join("history.jsonl"),
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("history");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &root,
        LocationOrigin::Configured,
    );

    let candidates = codex_scan_candidates(&source, "test-adapter").expect("candidates");
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert!(candidates.is_empty());
    assert!(scan.events.is_empty());
}

#[test]
fn codex_dedupes_copied_branch_history_and_keeps_branch_delta() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");

    let mut parent =
        File::create(sessions.join("2026-05-12T08-00-00-parent.jsonl")).expect("parent");
    writeln!(
        parent,
        r#"{{"timestamp":"2026-05-12T08:00:00.000Z","type":"turn_context","payload":{{"model":"gpt-5.2"}}}}"#
    )
    .expect("write parent context");
    writeln!(
        parent,
        r#"{{"timestamp":"2026-05-12T08:01:00.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":1000,"cached_input_tokens":100,"output_tokens":200,"reasoning_output_tokens":20,"total_tokens":1200}}}}}}}}"#
    )
    .expect("write parent tokens");

    let mut branch =
        File::create(sessions.join("2026-05-12T08-02-00-branch.jsonl")).expect("branch");
    writeln!(
        branch,
        r#"{{"timestamp":"2026-05-12T08:00:00.000Z","type":"turn_context","payload":{{"model":"gpt-5.2"}}}}"#
    )
    .expect("write branch context");
    writeln!(
        branch,
        r#"{{"timestamp":"2026-05-12T08:01:00.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":1000,"cached_input_tokens":100,"output_tokens":200,"reasoning_output_tokens":20,"total_tokens":1200}}}}}}}}"#
    )
    .expect("write branch copied parent tokens");
    writeln!(
        branch,
        r#"{{"timestamp":"2026-05-12T08:02:00.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":1600,"cached_input_tokens":300,"output_tokens":450,"reasoning_output_tokens":40,"total_tokens":2050}}}}}}}}"#
    )
    .expect("write branch delta tokens");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 2);
    assert_eq!(scan.diagnostics.duplicate_events, 1);

    assert_eq!(scan.events[0].usage.input_tokens, Some(900));
    assert_eq!(scan.events[0].usage.cache_read_tokens, Some(100));
    assert_eq!(scan.events[0].usage.output_tokens, Some(180));
    assert_eq!(scan.events[0].usage.reasoning_tokens, Some(20));
    assert_eq!(scan.events[0].usage.total_tokens, Some(1200));

    assert_eq!(scan.events[1].usage.input_tokens, Some(400));
    assert_eq!(scan.events[1].usage.cache_read_tokens, Some(200));
    assert_eq!(scan.events[1].usage.output_tokens, Some(230));
    assert_eq!(scan.events[1].usage.reasoning_tokens, Some(20));
    assert_eq!(scan.events[1].usage.total_tokens, Some(850));
}

#[test]
fn codex_prefers_active_session_copy_over_archived_duplicate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    let archived = dir.path().join("archived_sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::create_dir_all(&archived).expect("archived");

    let active_path = sessions.join("dup.jsonl");
    let archived_path = archived.join("dup.jsonl");
    std::fs::write(
        &active_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("active write");
    std::fs::write(
        &archived_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("archived write");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");
    let active_hash = hash_text(&canonical_display(&active_path));

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.diagnostics.duplicate_events, 1);
    assert_eq!(
        scan.events[0]
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_file_path_hash.as_deref()),
        Some(active_hash.as_str())
    );
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
fn codex_rollout_turns_include_runtime_and_message_metrics() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("rollout.jsonl")).expect("fixture");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:00Z","type":"turn_context","payload":{{"model":"gpt-5"}}}}"#
    )
    .expect("write context");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:01Z","type":"event_msg","payload":{{"type":"task_started","started_at":"2026-05-01T00:00:01Z"}}}}"#
    )
    .expect("write start");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:02Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hello"}}]}}}}"#
    )
    .expect("write user");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:05Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":120}}}}}}}}"#
    )
    .expect("write tokens");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:06Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"hi"}}]}}}}"#
    )
    .expect("write assistant");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:06Z","type":"event_msg","payload":{{"type":"task_complete","completed_at":"2026-05-01T00:00:06Z","duration_ms":5000,"time_to_first_token_ms":1200}}}}"#
    )
    .expect("write complete");

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
    assert_eq!(scan.events[0].usage.cache_read_tokens, Some(20));
    assert_eq!(scan.events[0].usage.output_tokens, Some(30));
    assert_eq!(scan.events[0].usage.reasoning_tokens, Some(10));
    assert_eq!(scan.events[0].usage.total_tokens, Some(120));
    assert_eq!(
        scan.events[0].session.started_at.to_rfc3339(),
        "2026-05-01T00:00:01+00:00"
    );
    assert_eq!(
        scan.events[0]
            .session
            .ended_at
            .expect("ended_at")
            .to_rfc3339(),
        "2026-05-01T00:00:06+00:00"
    );
    assert_eq!(scan.events[0].session.duration_seconds, Some(5));
    let runtime = scan.events[0].runtime.as_ref().expect("runtime");
    assert_eq!(runtime.latency_ms, Some(5000));
    assert_eq!(runtime.latency_source, Some(LatencySource::Explicit));
    assert_eq!(runtime.time_to_first_token_ms, Some(1200));
    assert_eq!(runtime.total_messages, Some(2));
    assert_eq!(runtime.user_messages, Some(1));
    assert_eq!(runtime.assistant_messages, Some(1));
    assert_eq!(runtime.developer_messages, Some(0));
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
fn codex_rollout_turns_match_interleaved_records_by_session_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("interleaved.jsonl")).expect("fixture");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:00Z","session_id":"session-a","type":"event_msg","payload":{{"type":"task_started","started_at":"2026-05-01T00:00:00Z"}}}}"#
    )
    .expect("write session a start");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:01Z","session_id":"session-b","type":"event_msg","payload":{{"type":"task_started","started_at":"2026-05-01T00:00:01Z"}}}}"#
    )
    .expect("write session b start");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:02Z","session_id":"session-a","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":140}},"total_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":140}}}}}}}}"#
    )
    .expect("write session a tokens");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:03Z","session_id":"session-a","type":"event_msg","payload":{{"type":"task_complete","completed_at":"2026-05-01T00:00:03Z"}}}}"#
    )
    .expect("write session a complete");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:04Z","session_id":"session-b","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":160,"cached_input_tokens":40,"output_tokens":60,"reasoning_output_tokens":20,"total_tokens":280}},"total_token_usage":{{"input_tokens":160,"cached_input_tokens":40,"output_tokens":60,"reasoning_output_tokens":20,"total_tokens":280}}}}}}}}"#
    )
    .expect("write session b tokens");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:05Z","session_id":"session-b","type":"event_msg","payload":{{"type":"task_complete","completed_at":"2026-05-01T00:00:05Z"}}}}"#
    )
    .expect("write session b complete");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 2);
    let mut events = scan.events.iter().collect::<Vec<_>>();
    events.sort_by_key(|event| event.usage.total_tokens);

    assert_eq!(events[0].usage.total_tokens, Some(140));
    assert_eq!(
        events[0]
            .session
            .local_session_id_hash
            .as_deref()
            .expect("session a hash"),
        hash_text("session-a")
    );
    assert_eq!(
        events[0].session.started_at.to_rfc3339(),
        "2026-05-01T00:00:00+00:00"
    );
    assert_eq!(
        events[0]
            .session
            .ended_at
            .expect("session a ended")
            .to_rfc3339(),
        "2026-05-01T00:00:03+00:00"
    );
    assert_eq!(events[0].session.duration_seconds, Some(3));

    assert_eq!(events[1].usage.total_tokens, Some(280));
    assert_eq!(
        events[1]
            .session
            .local_session_id_hash
            .as_deref()
            .expect("session b hash"),
        hash_text("session-b")
    );
    assert_eq!(
        events[1].session.started_at.to_rfc3339(),
        "2026-05-01T00:00:01+00:00"
    );
    assert_eq!(
        events[1]
            .session
            .ended_at
            .expect("session b ended")
            .to_rfc3339(),
        "2026-05-01T00:00:05+00:00"
    );
    assert_eq!(events[1].session.duration_seconds, Some(4));
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
fn codex_rollout_derives_runtime_from_turn_timestamps_when_duration_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("legacy-rollout.jsonl")).expect("fixture");
    writeln!(
        file,
        r#"{{"timestamp":"2026-04-11T00:00:00Z","type":"turn_context","payload":{{"model":"gpt-5"}}}}"#
    )
    .expect("write context");
    writeln!(
        file,
        r#"{{"timestamp":"2026-04-11T00:00:01Z","type":"event_msg","payload":{{"type":"task_started"}}}}"#
    )
    .expect("write start");
    writeln!(
        file,
        r#"{{"timestamp":"2026-04-11T00:00:02Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hello"}}]}}}}"#
    )
    .expect("write user");
    writeln!(
        file,
        r#"{{"timestamp":"2026-04-11T00:00:05Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":120}}}}}}}}"#
    )
    .expect("write tokens");
    writeln!(
        file,
        r#"{{"timestamp":"2026-04-11T00:00:06Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"hi"}}]}}}}"#
    )
    .expect("write assistant");
    writeln!(
        file,
        r#"{{"timestamp":"2026-04-11T00:00:06Z","type":"event_msg","payload":{{"type":"task_complete"}}}}"#
    )
    .expect("write complete");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(
        scan.events[0].session.started_at.to_rfc3339(),
        "2026-04-11T00:00:01+00:00"
    );
    assert_eq!(
        scan.events[0]
            .session
            .ended_at
            .expect("ended_at")
            .to_rfc3339(),
        "2026-04-11T00:00:06+00:00"
    );
    assert_eq!(scan.events[0].session.duration_seconds, Some(5));
    let runtime = scan.events[0].runtime.as_ref().expect("runtime");
    assert_eq!(runtime.latency_ms, Some(5000));
    assert_eq!(runtime.latency_source, Some(LatencySource::Inferred));
    assert_eq!(runtime.time_to_first_token_ms, None);
    assert_eq!(runtime.total_messages, Some(2));
    assert_eq!(runtime.user_messages, Some(1));
    assert_eq!(runtime.assistant_messages, Some(1));
    assert_eq!(runtime.developer_messages, Some(0));
}

#[test]
fn codex_path_independent_turn_dedupe_keeps_distinct_projects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    let workspace_a = dir.path().join("workspace-a").join("ai-stats");
    let workspace_b = dir.path().join("workspace-b").join("ai-stats");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::create_dir_all(&workspace_a).expect("workspace a");
    std::fs::create_dir_all(&workspace_b).expect("workspace b");
    write_git_fixture(
        &workspace_a,
        "git@github.com:example-org/example-workspace.git",
        "main",
    );
    write_git_fixture(
        &workspace_b,
        "git@github.com:example-org/example-workspace.git",
        "main",
    );

    for (name, workspace) in [("a.jsonl", &workspace_a), ("b.jsonl", &workspace_b)] {
        let mut file = File::create(sessions.join(name)).expect("fixture");
        writeln!(
            file,
            r#"{{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{{"cwd":"{}","git":{{"repository_url":"git@github.com:example-org/example-workspace.git","branch":"main"}}}}}}"#,
            workspace.display()
        )
        .expect("write session meta");
        writeln!(
            file,
            r#"{{"timestamp":"2026-06-01T08:00:00Z","type":"turn_context","payload":{{"model":"gpt-5"}}}}"#
        )
        .expect("write context");
        writeln!(
            file,
            r#"{{"timestamp":"2026-06-01T08:00:01Z","type":"event_msg","payload":{{"type":"task_started","started_at":"2026-06-01T08:00:01Z"}}}}"#
        )
        .expect("write start");
        writeln!(
            file,
            r#"{{"timestamp":"2026-06-01T08:00:03Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}}}}}}}}"#
        )
        .expect("write tokens");
        writeln!(
            file,
            r#"{{"timestamp":"2026-06-01T08:00:04Z","type":"event_msg","payload":{{"type":"task_complete","completed_at":"2026-06-01T08:00:04Z","duration_ms":3000}}}}"#
        )
        .expect("write complete");
    }

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 2);
    assert_eq!(scan.diagnostics.duplicate_events, 0);

    let mut project_paths = scan
        .events
        .iter()
        .map(|event| {
            event
                .project
                .as_ref()
                .and_then(|project| project.path_label.clone())
                .expect("project path")
        })
        .collect::<Vec<_>>();
    project_paths.sort();

    assert_eq!(
        project_paths,
        vec![
            workspace_a.to_string_lossy().to_string(),
            workspace_b.to_string_lossy().to_string(),
        ]
    );
}

#[test]
fn codex_path_independent_usage_dedupe_keeps_distinct_branches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    let workspace = dir.path().join("workspace").join("ai-stats");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "git@github.com:example-org/example-workspace.git",
        "main",
    );

    for (name, branch_name) in [("main.jsonl", "main"), ("feature.jsonl", "feature-x")] {
        let mut file = File::create(sessions.join(name)).expect("fixture");
        writeln!(
            file,
            r#"{{"timestamp":"2026-06-03T08:00:00Z","type":"session_meta","payload":{{"cwd":"{}","git":{{"repository_url":"git@github.com:example-org/example-workspace.git","branch":"{}"}}}}}}"#,
            workspace.display(),
            branch_name
        )
        .expect("write session meta");
        writeln!(
            file,
            r#"{{"timestamp":"2026-06-03T08:00:01Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}}}}}}}}"#
        )
        .expect("write usage");
    }

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 2);
    assert_eq!(scan.diagnostics.duplicate_events, 0);

    let mut branches = scan
        .events
        .iter()
        .map(|event| {
            event
                .project
                .as_ref()
                .and_then(|project| project.branch_label.clone())
                .expect("branch")
        })
        .collect::<Vec<_>>();
    branches.sort();

    assert_eq!(branches, vec!["feature-x".to_string(), "main".to_string()]);
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

#[test]
fn codex_turn_context_reasoning_effort_propagates_with_precedence_and_fallback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("reasoning.jsonl")).expect("fixture");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:00Z","type":"turn_context","payload":{{"model":"gpt-5","collaboration_mode":{{"settings":{{"reasoning_effort":"high"}}}},"effort":"low"}}}}"#
    )
    .expect("write first context");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:01Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":120}},"total_token_usage":{{"input_tokens":80,"cached_input_tokens":20,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":120}}}}}}}}"#
    )
    .expect("write first usage");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:02Z","type":"turn_context","payload":{{"model":"gpt-5.4","effort":"xhigh"}}}}"#
    )
    .expect("write second context");
    writeln!(
        file,
        r#"{{"timestamp":"2026-05-01T00:00:03Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":60,"cached_input_tokens":10,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":95}},"total_token_usage":{{"input_tokens":140,"cached_input_tokens":30,"output_tokens":60,"reasoning_output_tokens":15,"total_tokens":215}}}}}}}}"#
    )
    .expect("write second usage");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 2);
    assert_eq!(
        scan.events[0]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level),
        Some(ReasoningLevel::High)
    );
    assert_eq!(
        scan.events[0]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level_raw.as_deref()),
        Some("high")
    );
    assert_eq!(
        scan.events[1]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level),
        Some(ReasoningLevel::Xhigh)
    );
    assert_eq!(
        scan.events[1]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level_raw.as_deref()),
        Some("xhigh")
    );
}

#[test]
fn codex_non_usage_event_messages_do_not_override_turn_model() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("model.jsonl")).expect("fixture");
    file.write_all(
        br#"{"timestamp":"2026-05-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.4"}}
"#,
    )
    .expect("write context");
    file.write_all(
        br#"{"timestamp":"2026-05-01T00:00:01Z","type":"event_msg","payload":{"type":"agent_message","model":"gpt-incorrect"}}
"#,
    )
    .expect("write unrelated event");
    file.write_all(
        br#"{"timestamp":"2026-05-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":80,"output_tokens":40,"total_tokens":120},"total_token_usage":{"input_tokens":80,"output_tokens":40,"total_tokens":120}}}}
"#,
    )
    .expect("write usage");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(
        scan.events[0]
            .model
            .as_ref()
            .and_then(|model| model.normalized_name.as_deref()),
        Some("gpt-5.4")
    );
}
