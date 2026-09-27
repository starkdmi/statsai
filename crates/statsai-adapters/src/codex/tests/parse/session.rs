use super::*;

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
    let function_call_output = r#"{"timestamp":"2026-06-03T09:36:24.900Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"{}"}}"#;
    let user_message = r#"{"timestamp":"2026-06-03T09:36:25.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#;

    assert_eq!(codex_line_kind(reasoning), CodexLineKind::Irrelevant);
    assert_eq!(
        codex_line_kind(function_call),
        CodexLineKind::ResponseItemToolCall
    );
    assert_eq!(
        codex_line_kind(function_call_output),
        CodexLineKind::ResponseItemToolCall
    );
    assert_eq!(
        codex_line_kind(user_message),
        CodexLineKind::ResponseItemMessage
    );

    let unrelated_event = r#"{"timestamp":"2026-06-03T09:36:26.000Z","type":"event_msg","payload":{"type":"agent_message","model":"gpt-incorrect"}}"#;
    assert_eq!(codex_line_kind(unrelated_event), CodexLineKind::Irrelevant);
    let item_started = r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"event_msg","payload":{"type":"item_started","item":{"type":"CommandExecution"}}}"#;
    assert_eq!(
        codex_line_kind(item_started),
        CodexLineKind::EventItemStarted
    );
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
fn codex_token_usage_record_is_never_classified_as_headless_usage() {
    // `"usage":` on a real record sits past the 256-byte header, under payload.
    // A compact line still carries that key inside the header. Neither shape
    // may fall through to HeadlessUsage.
    let compact = r#"{"timestamp":"2026-09-01T10:00:02.080Z","type":"token_usage_record","payload":{"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
    assert!(compact.len() < 256);
    assert!(compact.contains("\"usage\":"));
    assert_eq!(codex_line_kind(compact), CodexLineKind::TokenUsageRecord);

    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "tests/fixtures/codex/usage-record/mixed/sessions/2026/09/01/rollout-fixture-usage-record.jsonl",
    );
    let realistic = std::fs::read_to_string(fixture)
        .expect("fixture")
        .lines()
        .find(|line| line.contains("\"token_usage_record\""))
        .expect("record line")
        .to_string();
    let usage_at = realistic.find("\"usage\":").expect("usage field");
    assert!(
        usage_at > 256,
        "the fixture must keep usage outside the header window, at {usage_at}"
    );
    assert!(!codex_line_header(&realistic).contains("\"usage\":"));
    assert_eq!(codex_line_kind(&realistic), CodexLineKind::TokenUsageRecord);
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

    // The thread name titles the session even when tasks are not collected.
    let without_tasks =
        scan_codex_source(&CodexAdapter, &source, &options_without_tasks()).expect("scan");
    assert!(without_tasks.task_spans.is_empty());
    assert!(!without_tasks.events.is_empty());
    assert!(without_tasks
        .events
        .iter()
        .all(|event| event.session.title.as_deref() == Some("Fix parser bug")));
}

#[test]
fn codex_subagents_borrow_their_parent_thread_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::write(
        codex_root.join("session_index.jsonl"),
        "{\"id\":\"parent-1\",\"thread_name\":\"Fix parser bug\"}\n",
    )
    .expect("session index");
    for (index, (file, meta)) in [
        (
            "spawned.jsonl",
            r#"{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{"id":"child-spawned","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-1","depth":1,"agent_nickname":"Popper","agent_role":null}}}}}"#,
        ),
        (
            "guardian.jsonl",
            r#"{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{"id":"child-guardian","session_id":"parent-1","source":{"subagent":{"other":"guardian"}}}}"#,
        ),
        (
            "orphan.jsonl",
            r#"{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{"id":"child-orphan","source":{"subagent":{"other":"guardian"}}}}"#,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        // Distinct usage per rollout, or the path-independent dedupe keeps one.
        let input = 60 + index;
        let total = 120 + index;
        let usage = format!(
            r#"{{"timestamp":"2026-06-01T08:00:03Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":{total}}},"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":{total}}}}}}}}}"#
        );
        let mut handle = File::create(sessions.join(file)).expect("rollout");
        writeln!(handle, "{meta}").expect("meta");
        writeln!(
            handle,
            r#"{{"timestamp":"2026-06-01T08:00:00Z","type":"turn_context","payload":{{"model":"gpt-5"}}}}"#
        )
        .expect("context");
        writeln!(handle, "{usage}").expect("usage");
    }

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options_without_tasks()).expect("scan");
    // The parent's name is joined when the session is built, so a rename of
    // the parent reaches its sub-agents; here only the link is reported.
    let link_for = |raw: &str| {
        let hash = statsai_core::hash_text(raw);
        scan.session_names
            .iter()
            .find(|name| name.local_session_id_hash == hash)
            .cloned()
    };
    let parent = Some(statsai_core::hash_text("parent-1"));
    assert_eq!(
        link_for("child-spawned").map(|name| (name.title, name.parent_local_session_id_hash)),
        Some(("Popper".to_string(), parent.clone()))
    );
    assert_eq!(
        link_for("child-guardian").map(|name| (name.title, name.parent_local_session_id_hash)),
        Some(("guardian".to_string(), parent))
    );
    assert_eq!(link_for("child-orphan"), None);
}

#[test]
fn codex_turns_without_a_reported_duration_end_at_their_last_work() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    let sessions = codex_root.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut file = File::create(sessions.join("resumed.jsonl")).expect("rollout");
    for line in [
        r#"{"timestamp":"2026-06-01T08:00:00Z","type":"session_meta","payload":{"id":"resumed-thread"}}"#,
        r#"{"timestamp":"2026-06-01T08:00:00Z","type":"turn_context","payload":{"model":"gpt-5"}}"#,
        r#"{"timestamp":"2026-06-01T08:00:01Z","type":"event_msg","payload":{"type":"task_started","started_at":"2026-06-01T08:00:01Z"}}"#,
        r#"{"timestamp":"2026-06-01T08:02:01Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120},"total_token_usage":{"input_tokens":60,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":120}}}}"#,
        // The next prompt and the completion are written three days later,
        // when the thread runs again, with no duration.
        r#"{"timestamp":"2026-06-04T09:00:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Next step"}]}}"#,
        r#"{"timestamp":"2026-06-04T09:00:00Z","type":"event_msg","payload":{"type":"task_complete","completed_at":"2026-06-04T09:00:00Z"}}"#,
    ] {
        writeln!(file, "{line}").expect("write line");
    }

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options_without_tasks()).expect("scan");
    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.events[0].session.duration_seconds, Some(120));
}

#[test]
fn codex_session_index_names_are_reported_on_every_scan() {
    let dir = tempfile::tempdir().expect("tempdir");
    let codex_root = dir.path().join("codex");
    std::fs::create_dir_all(codex_root.join("sessions")).expect("sessions");
    std::fs::write(
        codex_root.join("session_index.jsonl"),
        "{\"id\":\"thread-1\",\"thread_name\":\"Design tool/plugin\"}\n",
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &codex_root,
        LocationOrigin::Configured,
    );
    let scan = scan_codex_source(&CodexAdapter, &source, &options_without_tasks()).expect("scan");
    // No rollout changed, yet the index name is reported, uncleaned.
    assert_eq!(
        scan.session_names,
        vec![statsai_core::SessionName {
            local_session_id_hash: statsai_core::hash_text("thread-1"),
            title: "Design tool/plugin".to_string(),
            parent_local_session_id_hash: None,
        }]
    );
}
