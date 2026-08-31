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
fn codex_quota_parser_is_anchored_and_preserves_modern_status_fields() {
    let adapter = CodexAdapter;
    let source = codex_source_for_root(
        &adapter,
        Path::new("/tmp/codex-home"),
        LocationOrigin::Configured,
    );
    let observed_at = Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap();
    let value = serde_json::json!({
        "timestamp": observed_at,
        "type": "event_msg",
        "payload": {
            "type": "token_count",
            "info": {"last_token_usage": {"total_tokens": 0}},
            "rate_limits": {
                "limit_id": "codex_subscription",
                "plan_type": "pro",
                "individual_limit": null,
                "spend_control_state": "allowed",
                "reached_type": "weekly",
                "primary": {
                    "used_percent": 12.5,
                    "window_minutes": 10080,
                    "resets_at": 1787832000
                },
                "credits": {
                    "has_credits": true,
                    "unlimited": false,
                    "balance": "0012.5000"
                }
            }
        }
    });
    let record = codex_quota_observation(
        &source,
        Path::new("/tmp/codex-home/sessions/thread.jsonl"),
        7,
        observed_at,
        Some(UsageCounts::default()),
        &value,
    )
    .expect("quota observation");

    assert_eq!(record.windows.len(), 1);
    assert_eq!(record.windows[0].provider_slot, "primary");
    assert_eq!(record.windows[0].window_minutes, 10_080);
    assert_eq!(
        record.windows[0].limit_id.as_deref(),
        Some("codex_subscription")
    );
    assert_eq!(record.observation.status.plan_type.as_deref(), Some("pro"));
    assert_eq!(
        record.observation.status.credits.balance.as_deref(),
        Some("12.5")
    );
    assert_eq!(
        record.observation.status.credits.balance_raw,
        Some(Value::String("0012.5000".to_string()))
    );
    assert_eq!(record.observation.usage_link_kind, QuotaUsageLinkKind::None);

    let nested_as_text = serde_json::json!({
        "type": "event_msg",
        "payload": {
            "type": "user_message",
            "message": value.to_string()
        }
    });
    assert!(codex_quota_observation(
        &source,
        Path::new("/tmp/thread.jsonl"),
        1,
        observed_at,
        None,
        &nested_as_text,
    )
    .is_none());
}

#[test]
fn codex_quota_parser_requires_integer_reset_epochs_and_leniently_reads_balances() {
    let adapter = CodexAdapter;
    let source = codex_source_for_root(
        &adapter,
        Path::new("/tmp/codex-home"),
        LocationOrigin::Configured,
    );
    let observed_at = Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap();
    for (balance, normalized) in [
        (serde_json::json!(14.25), Some("14.25")),
        (serde_json::json!("1.25e-3"), Some("0.00125")),
        (Value::Null, None),
    ] {
        let value = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "primary": {
                        "used_percent": 1,
                        "window_minutes": 300,
                        "resets_at": "1787832000"
                    },
                    "credits": {"balance": balance}
                }
            }
        });
        let record = codex_quota_observation(
            &source,
            Path::new("/tmp/thread.jsonl"),
            1,
            observed_at,
            None,
            &value,
        )
        .expect("structural quota payload");
        assert!(record.windows.is_empty(), "string epochs are invalid");
        assert_eq!(
            record.observation.status.credits.balance.as_deref(),
            normalized
        );
    }
    assert!(codex_quota_observation(
        &source,
        Path::new("/tmp/thread.jsonl"),
        1,
        observed_at,
        None,
        &serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "token_count", "rate_limits": "malformed"}
        }),
    )
    .is_none());
}

#[test]
fn codex_quota_links_consumed_samples_to_turn_events_and_preserves_zero_samples() {
    let directory = tempfile::tempdir().expect("tempdir");
    let root = directory.path().join("codex");
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let path = sessions.join("thread.jsonl");
    let mut fixture = File::create(&path).expect("fixture");
    for value in [
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:00Z",
            "type": "event_msg",
            "payload": {"type": "task_started"}
        }),
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:01Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {"input_tokens": 10, "output_tokens": 5},
                    "total_token_usage": {"input_tokens": 10, "output_tokens": 5}
                },
                "rate_limits": {
                    "primary": {
                        "used_percent": 10,
                        "window_minutes": 10080,
                        "resets_at": 1787832000
                    }
                }
            }
        }),
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:02Z",
            "type": "event_msg",
            "payload": {"type": "task_complete"}
        }),
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:03Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {"total_tokens": 0},
                    "total_token_usage": {"input_tokens": 10, "output_tokens": 5}
                },
                "rate_limits": {
                    "primary": {
                        "used_percent": 11,
                        "window_minutes": 10080,
                        "resets_at": 1787832000
                    }
                }
            }
        }),
    ] {
        writeln!(fixture, "{value}").expect("write fixture");
    }
    drop(fixture);
    let source = codex_source_for_root(&CodexAdapter, &root, LocationOrigin::Configured);
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.quota_observations.len(), 2);
    assert_eq!(
        scan.quota_observations[0].observation.usage_link_kind,
        QuotaUsageLinkKind::TurnEvent
    );
    assert_eq!(
        scan.quota_observations[0].observation.usage_event_id,
        Some(scan.events[0].event_id.clone())
    );
    assert_eq!(
        scan.quota_observations[1]
            .observation
            .usage_sample
            .as_ref()
            .map(UsageCounts::computed_total),
        Some(0)
    );
    assert_eq!(
        scan.quota_observations[1].observation.usage_link_kind,
        QuotaUsageLinkKind::None
    );
    assert!(scan.quota_observations[1]
        .observation
        .usage_event_id
        .is_none());
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
fn codex_task_title_extracts_user_request_from_transcript_delta_prompt() {
    let (title, title_source, is_meta) = codex_task_title(
        None,
        Some(
            ">>> TRANSCRIPT DELTA START [167] user: Code review Found one actionable issue: \
             ::code-comment{title=\"[P2] Concurrent filter changes can overwrite each \
             other\" body=\"Each update derives from the last rendered searchParams\"}",
        ),
    );

    assert_eq!(title, "Code review");
    assert_eq!(title_source, "user_prompt");
    assert!(!is_meta);
}

#[test]
fn codex_task_title_rejects_tool_result_transcript_delta_prompt() {
    let (title, title_source, is_meta) = codex_task_title(
        None,
        Some(
            ">>> TRANSCRIPT DELTA START [288] tool exec_command result: Chunk ID: 84e62e \
             Wall time: 1.0006 seconds Process running with session ID 32988 Original \
             token count: 30 Output:",
        ),
    );

    assert_eq!(title, "Codex task");
    assert_eq!(title_source, "default");
    assert!(is_meta);
}

#[test]
fn codex_task_title_rejects_metric_report_prompt_without_intent() {
    let (title, title_source, is_meta) = codex_task_title(
        None,
        Some("Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34"),
    );

    assert_eq!(title, "Codex task");
    assert_eq!(title_source, "default");
    assert!(is_meta);
}

#[test]
fn codex_task_title_skips_instructional_preamble_and_keeps_request() {
    let (title, title_source, is_meta) = codex_task_title(
        None,
        Some(
            "This is NOT the Next.js you know. This version may differ from your training \
             data. Read the relevant guide before writing code. I need device renaming on \
             web and api.",
        ),
    );

    assert_eq!(title, "I need device renaming on web and api");
    assert_eq!(title_source, "user_prompt");
    assert!(!is_meta);
}

#[test]
fn codex_task_title_prefers_prompt_over_weak_thread_name_banner() {
    let (title, title_source, is_meta) = codex_task_title(
        Some("This is NOT the framework you know"),
        Some(
            "# This is NOT the framework you know\n\
             Read the relevant guide before writing code.\n\
             I need device renaming on web and api.",
        ),
    );

    assert_eq!(title, "I need device renaming on web and api");
    assert_eq!(title_source, "user_prompt");
    assert!(!is_meta);
}

#[test]
fn choose_best_task_preview_ignores_generic_wrapper_fallback() {
    let previews = vec![CodexPromptPreview {
        text: "Code review guidelines".to_string(),
        source: CodexPromptPreviewSource::ResponseItemUser,
    }];

    assert_eq!(choose_best_task_preview(&previews), None);
}

#[test]
fn codex_user_message_preview_skips_wrapped_response_item_user_content() {
    let value = serde_json::json!({
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [
                {
                    "type": "input_text",
                    "text": "<environment_context>\n<cwd>/tmp/example</cwd>\n</environment_context>"
                }
            ]
        }
    });

    let preview = codex_user_message_preview(&value).expect("candidate");
    assert_eq!(preview.source, CodexPromptPreviewSource::ResponseItemUser);
    assert!(materialize_codex_task_previews(&[preview]).is_empty());
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
fn codex_json_string_prefix_decodes_unicode_without_losing_boundaries() {
    let line = r#"{"timestamp":"2026-06-03T09:36:25.000Z","type":"event_msg","payload":{"type":"user_message","message":"A\u2019éB"}}"#;

    assert_eq!(
        codex_event_user_message_preview_from_line(line, CODEX_TASK_PREVIEW_RAW_BYTES).as_deref(),
        Some("A’éB")
    );
}

#[test]
fn codex_message_content_preview_text_truncates_large_first_part() {
    let large = "é".repeat(70_000);
    let value = serde_json::json!([{
        "type": "input_text",
        "text": large,
    }]);

    let preview = codex_message_content_preview_text(Some(&value), CODEX_TASK_PREVIEW_RAW_BYTES)
        .expect("preview");
    let expected_source = "é".repeat(70_000);
    assert!(preview.len() <= CODEX_TASK_PREVIEW_RAW_BYTES);
    assert_eq!(
        preview,
        codex_prefix_at_char_boundary(expected_source.as_str(), CODEX_TASK_PREVIEW_RAW_BYTES)
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
fn codex_auth_json_exposes_verified_source_state_without_stamping_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "email": "existing@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-real",
                "chatgpt_plan_type": "plus",
                "chatgpt_subscription_active_start": "2026-05-29T10:12:43+00:00",
                "chatgpt_subscription_active_until": "2026-06-29T10:12:43+00:00",
                "chatgpt_subscription_last_checked": "2026-05-29T10:14:56.058278+00:00"
            }
        })
        .to_string(),
    )
    .expect("auth");
    let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
    writeln!(
        file,
        "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}"
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

    let verified = scan
        .verified_source_state
        .as_ref()
        .expect("verified source state");
    assert_eq!(verified.provider_user_id.as_deref(), Some("acct-real"));
    assert_eq!(verified.email.as_deref(), Some("existing@example.com"));
    assert_eq!(verified.plan_name.as_deref(), Some("Plus"));
    assert!(verified.authenticated_at.is_some());
    assert_eq!(
        verified.verified_at.map(|value| value.to_rfc3339()),
        Some("2026-05-29T10:14:56.058278+00:00".to_string())
    );
    assert!(verified.subscription.is_none());
    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");
    let plan = evidence.plan_observations.first().expect("plan evidence");
    assert_eq!(plan.raw_plan_name, "plus");
    assert_eq!(plan.plan_name, "Plus");
    assert_eq!(
        plan.active_from.expect("active from").to_rfc3339(),
        "2026-05-29T10:12:43+00:00"
    );
    assert_eq!(
        plan.active_until.map(|value| value.to_rfc3339()),
        Some("2026-06-29T10:12:43+00:00".to_string())
    );
    assert_eq!(scan.events[0].provider_account_id, None);
    assert_ne!(
        scan.events[0]
            .parse_evidence
            .as_ref()
            .map(|evidence| evidence.account_identity_source.clone()),
        Some(IdentitySource::LocalAuth)
    );
}

#[test]
fn codex_auth_identity_is_dated_by_the_login_not_the_subscription_check() {
    // `auth_time` is 2026-06-10; the embedded subscription claims were last
    // revalidated on 2026-05-01. Signing into a different account rewrites
    // the account id without touching that older stamp, so dating the
    // identity by it would claim this source was already `acct-b` five
    // weeks before the login — and `AuthSnapshot` ends a source's account
    // interval, so the previous account would lose those five weeks.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "tokens": {
                "id_token": "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6InBlcnNvbkBleGFtcGxlLmNvbSIsImF1dGhfdGltZSI6MTc4MTA0OTYwMCwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoIjp7ImNoYXRncHRfYWNjb3VudF9pZCI6ImFjY3QtYiIsImNoYXRncHRfcGxhbl90eXBlIjoicGx1cyIsImNoYXRncHRfc3Vic2NyaXB0aW9uX2xhc3RfY2hlY2tlZCI6IjIwMjYtMDUtMDFUMDA6MDA6MDArMDA6MDAifX0."
            }
        })
        .to_string(),
    )
    .expect("auth");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    let identity = evidence
        .identity_observations
        .iter()
        .find(|item| item.evidence_kind == AccountEvidenceKind::AuthSnapshot)
        .expect("auth snapshot identity");
    assert_eq!(
        identity.observed_at.to_rfc3339(),
        "2026-06-10T00:00:00+00:00"
    );
    let plan = evidence.plan_observations.first().expect("plan evidence");
    assert_eq!(plan.observed_at.to_rfc3339(), "2026-05-01T00:00:00+00:00");
}

#[test]
fn codex_auth_json_reads_nested_tokens_id_token_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6ImV4aXN0aW5nQGV4YW1wbGUuY29tIiwiaWF0IjoxNzQ4NTEzNTYzLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC1yZWFsIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJwbHVzIiwiY2hhdGdwdF9zdWJzY3JpcHRpb25fYWN0aXZlX3N0YXJ0IjoiMjAyNi0wNS0yOVQxMDoxMjo0MyswMDowMCIsImNoYXRncHRfc3Vic2NyaXB0aW9uX2FjdGl2ZV91bnRpbCI6IjIwMjYtMDYtMjlUMTA6MTI6NDMrMDA6MDAiLCJjaGF0Z3B0X3N1YnNjcmlwdGlvbl9sYXN0X2NoZWNrZWQiOiIyMDI2LTA1LTI5VDEwOjE0OjU2LjA1ODI3OCswMDowMCJ9fQ.",
                "access_token": "unused",
                "refresh_token": "unused",
                "account_id": "00000000-0000-4000-8000-000000000001"
            },
            "last_refresh": "2026-05-19T19:56:03.481816Z"
        })
        .to_string(),
    )
    .expect("auth");
    let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
    writeln!(
        file,
        "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}"
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

    let verified = scan
        .verified_source_state
        .as_ref()
        .expect("verified source state");
    assert_eq!(verified.provider_user_id.as_deref(), Some("acct-real"));
    assert_eq!(verified.email.as_deref(), Some("existing@example.com"));
    assert_eq!(verified.plan_name.as_deref(), Some("Plus"));
    assert!(verified.authenticated_at.is_some());
    assert_eq!(
        verified.verified_at.map(|value| value.to_rfc3339()),
        Some("2026-05-29T10:14:56.058278+00:00".to_string())
    );
    assert!(verified.subscription.is_none());
    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");
    let plan = evidence.plan_observations.first().expect("plan evidence");
    assert_eq!(plan.raw_plan_name, "plus");
    assert_eq!(
        plan.active_from.expect("active from").to_rfc3339(),
        "2026-05-29T10:12:43+00:00"
    );
    assert_eq!(
        plan.active_until.map(|value| value.to_rfc3339()),
        Some("2026-06-29T10:12:43+00:00".to_string())
    );
    assert_eq!(scan.events[0].provider_account_id, None);
}

#[test]
fn codex_auth_refresh_does_not_mark_cached_plan_as_newly_verified() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "email": "existing@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-real",
                "chatgpt_plan_type": "plus",
                "chatgpt_subscription_active_start": "2026-05-29T10:12:43+00:00",
                "chatgpt_subscription_active_until": "2026-06-29T10:12:43+00:00"
            }
        })
        .to_string(),
    )
    .expect("auth");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let observation = CodexAdapter
        .probe_verified_source_state(&source)
        .expect("probe");
    let VerifiedSourceObservation::Verified(verified) = observation else {
        panic!("expected verified source state");
    };

    assert!(verified.verified_at.is_some());
    assert!(verified.subscription.is_none());
    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");
    assert_eq!(evidence.plan_observations.len(), 1);
    assert!(evidence.plan_observations[0].is_current_snapshot);
    assert_eq!(
        evidence.plan_observations[0]
            .active_until
            .map(|value| value.to_rfc3339()),
        Some("2026-06-29T10:12:43+00:00".to_string())
    );
}

#[test]
fn codex_collects_allowlisted_telemetry_reset_and_login_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let database_path = dir.path().join("logs_2.sqlite");
    let connection = Connection::open(&database_path).expect("logs database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("logs schema");
    connection
        .execute(
            "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                1,
                1_787_227_200_i64,
                123_i64,
                "codex_otel.log_only",
                "event.name=\"codex.conversation_starts\" user.account_id=acct-telemetry user.email=owner@example.test conversation.id=conversation-1 auth.mode=chatgpt app.version=1.2.3"
            ],
        )
        .expect("telemetry row");
    connection
        .execute(
            "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                2,
                1_787_227_201_i64,
                0_i64,
                "codex_core::auth",
                "Reloading auth for account acct-reloaded"
            ],
        )
        .expect("reload row");
    connection
        .execute(
            "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                3,
                1_787_227_202_i64,
                0_i64,
                "codex_otel.log_only",
                "event.name=\"codex.user_prompt\" prompt=\"Please discuss user.account_id=acct-prompt user.email=prompt@example.test conversation.id=conversation-prompt\""
            ],
        )
        .expect("arbitrary text row");
    drop(connection);

    std::fs::write(
        dir.path().join(".codex-global-state.json"),
        serde_json::json!({
            "electron-persisted-atom-state": {
                "codex-rate-limit-reset-history": [{
                    "accountId": "acct-reset",
                    "conversationId": "conversation-reset",
                    "turnId": "turn-reset",
                    "occurredAtMs": 1_787_227_203_000_i64
                }]
            }
        })
        .to_string(),
    )
    .expect("global state");
    std::fs::create_dir_all(dir.path().join("log")).expect("log directory");
    std::fs::write(
        dir.path().join("log/codex-login.log.1"),
        "2026-08-20T12:00:04Z login successful for arbitrary@example.test acct-visible-only-in-body\n",
    )
    .expect("rotated login log");
    std::fs::write(
        dir.path().join("log/codex-login.log"),
        "2026-08-20T12:00:05Z unrelated message\n2026-08-20T12:00:06Z login completed\n",
    )
    .expect("login log");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    assert_eq!(
        evidence
            .identity_observations
            .iter()
            .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
            .count(),
        1
    );
    assert_eq!(
        evidence
            .identity_observations
            .iter()
            .filter(|item| item.evidence_kind == AccountEvidenceKind::AuthReload)
            .count(),
        1
    );
    assert_eq!(
        evidence
            .identity_observations
            .iter()
            .filter(|item| item.evidence_kind == AccountEvidenceKind::ResetHistory)
            .count(),
        1
    );
    let login_observations = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::LoginSuccess)
        .collect::<Vec<_>>();
    assert_eq!(login_observations.len(), 2);
    assert!(login_observations.iter().all(|item| {
        item.provider_account_id.is_none()
            && item.provider_user_id_hash.is_none()
            && item.email_hash.is_none()
    }));
    assert_eq!(evidence.conversation_bindings.len(), 2);
    assert!(evidence
        .conversation_bindings
        .iter()
        .all(|binding| binding.conversation_id_hash.len() == 64));
    assert!(evidence
        .conversation_bindings
        .iter()
        .all(|binding| binding.conversation_id_hash != "conversation-1"));

    let retry_before_ack = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("retry account evidence before acknowledgement");
    assert_eq!(
        retry_before_ack
            .identity_observations
            .iter()
            .filter(|item| matches!(
                item.evidence_kind,
                AccountEvidenceKind::TelemetryIdentity | AccountEvidenceKind::AuthReload
            ))
            .count(),
        2,
        "telemetry must remain retryable until the caller commits the evidence"
    );
    let committed_checkpoints = evidence.checkpoints.clone();

    let repeated = CodexAdapter
        .collect_account_evidence(&source, &committed_checkpoints)
        .expect("repeat account evidence after checkpoint commit");
    assert_eq!(
        repeated
            .identity_observations
            .iter()
            .filter(|item| matches!(
                item.evidence_kind,
                AccountEvidenceKind::TelemetryIdentity | AccountEvidenceKind::AuthReload
            ))
            .count(),
        0,
        "an unchanged telemetry database must not be rescanned"
    );

    let connection = Connection::open(&database_path).expect("reopen logs database");
    connection
        .execute(
            "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                4,
                1_787_227_204_i64,
                0_i64,
                "codex_otel.trace_safe",
                "event.name=\"codex.conversation_starts\" user.account_id=acct-appended"
            ],
        )
        .expect("append telemetry row");
    drop(connection);
    let appended = CodexAdapter
        .collect_account_evidence(&source, &committed_checkpoints)
        .expect("incremental account evidence");
    assert_eq!(
        appended
            .identity_observations
            .iter()
            .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
            .count(),
        1,
        "only the appended telemetry range should be parsed"
    );
    assert_eq!(appended.checkpoints.len(), 1);

    std::fs::rename(&database_path, dir.path().join("logs_2.previous.sqlite"))
        .expect("archive replaced telemetry database");
    let mut replacement = Connection::open(&database_path).expect("replacement logs database");
    replacement
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("replacement logs schema");
    let transaction = replacement.transaction().expect("replacement transaction");
    for row_id in 1..=100_i64 {
        let body = match row_id {
            1 => "event.name=\"codex.conversation_starts\" user.account_id=acct-replacement-early"
                .to_string(),
            100 => "event.name=\"codex.conversation_starts\" user.account_id=acct-replacement-late"
                .to_string(),
            _ => format!("replacement filler row {row_id} {}", "x".repeat(256)),
        };
        transaction
            .execute(
                "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    row_id,
                    1_787_227_300_i64 + row_id,
                    0_i64,
                    "codex_otel.log_only",
                    body
                ],
            )
            .expect("replacement telemetry row");
    }
    transaction.commit().expect("commit replacement telemetry");
    drop(replacement);

    let replacement_scan = CodexAdapter
        .collect_account_evidence(&source, &appended.checkpoints)
        .expect("replacement telemetry evidence");
    let replacement_accounts = replacement_scan
        .accounts
        .iter()
        .filter_map(|account| account.provider_user_id.as_deref())
        .collect::<HashSet<_>>();
    assert!(replacement_accounts.contains("acct-replacement-early"));
    assert!(replacement_accounts.contains("acct-replacement-late"));
    assert_eq!(replacement_scan.checkpoints.len(), 1);
}

#[test]
fn codex_reads_identity_from_ordinary_telemetry_and_modern_auth_reload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let database_path = dir.path().join("logs_2.sqlite");
    let connection = Connection::open(&database_path).expect("logs database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("logs schema");
    let rows: [(i64, &str, &str); 4] = [
        // The generation that never emits `codex.conversation_starts`:
        // identity rides on ordinary telemetry events at the end of a span
        // context.
        (
            1,
            "codex_otel.trace_safe",
            "session_loop{thread_id=t-1}:turn{otel.name=\"session_task.turn\" model=gpt-5.4}: event.name=\"codex.turn_ttft\" duration_ms=250 conversation.id=conversation-ttft app.version=0.140.0 auth_mode=\"Chatgpt\" user.account_id=\"acct-ttft\" user.email=\"owner@example.test\" model=gpt-5.4",
        ),
        // The renamed reload target, with the message after the span close.
        (
            2,
            "codex_login::auth::manager",
            "app_server.request{otel.kind=\"server\" otel.name=\"getAuthStatus\" rpc.method=\"getAuthStatus\" rpc.request_id=desktop-auth:751da426}: Reloading auth for account acct-live-reload",
        ),
        // Quoted copies of the reload phrase must stay inert: one shadowed
        // by a free-text field, one with trailing content after the id.
        (
            3,
            "codex_login::auth::manager",
            "prompt=\"quoted\"}: Reloading auth for account acct-evil",
        ),
        (
            4,
            "codex_login::auth::manager",
            "app_server.request{otel.kind=\"server\"}: Reloading auth for account acct-evil trailing=1",
        ),
    ];
    for (row_id, target, body) in rows {
        connection
            .execute(
                "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![row_id, 1_787_227_200_i64 + row_id, 0_i64, target, body],
            )
            .expect("telemetry row");
    }
    drop(connection);
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    let telemetry = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
        .collect::<Vec<_>>();
    assert_eq!(telemetry.len(), 1);
    assert_eq!(
        telemetry[0].provider_user_id_hash.as_deref(),
        Some(hash_text("acct-ttft").as_str())
    );
    assert_eq!(telemetry[0].auth_mode.as_deref(), Some("Chatgpt"));
    assert_eq!(evidence.conversation_bindings.len(), 1);
    let reloads = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::AuthReload)
        .collect::<Vec<_>>();
    assert_eq!(reloads.len(), 1);
    assert_eq!(
        reloads[0].provider_user_id_hash.as_deref(),
        Some(hash_text("acct-live-reload").as_str())
    );
    assert!(evidence.identity_observations.iter().all(|item| {
        item.provider_user_id_hash.as_deref() != Some(hash_text("acct-evil").as_str())
    }));
}

#[test]
fn codex_reads_underscored_account_attribute_without_an_email() {
    let dir = tempfile::tempdir().expect("tempdir");
    let database_path = dir.path().join("logs_2.sqlite");
    let connection = Connection::open(&database_path).expect("logs database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("logs schema");
    connection
        .execute(
            "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                1_i64,
                1_787_227_200_i64,
                0_i64,
                "codex_otel.trace_safe",
                "event.name=\"codex.turn_ttft\" duration_ms=250 conversation.id=conversation-underscored user_account_id=\"acct-underscored\""
            ],
        )
        .expect("telemetry row");
    drop(connection);
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    let telemetry = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
        .collect::<Vec<_>>();
    assert_eq!(
        telemetry.len(),
        1,
        "the row-selection filter must accept every account attribute spelling the parser reads"
    );
    assert_eq!(
        telemetry[0].provider_user_id_hash.as_deref(),
        Some(hash_text("acct-underscored").as_str())
    );
    assert_eq!(evidence.conversation_bindings.len(), 1);
}

#[test]
fn codex_collapses_repeated_telemetry_identity_runs_to_endpoints() {
    let dir = tempfile::tempdir().expect("tempdir");
    let database_path = dir.path().join("logs_2.sqlite");
    let connection = Connection::open(&database_path).expect("logs database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("logs schema");
    // A(x5), B(x1), A(x2): the collapse must keep A's first and last on
    // each side of B — the alternation is the account-switch signal. A's
    // rows alternate between two parallel conversations to pin down that
    // the collapse is conversation-blind: interleaving must not split runs.
    let rows = [
        ("a", "one"),
        ("a", "two"),
        ("a", "one"),
        ("a", "two"),
        ("a", "one"),
        ("b", "three"),
        ("a", "two"),
        ("a", "one"),
    ];
    for (offset, (account, conversation)) in rows.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    offset as i64 + 1,
                    1_787_227_200_i64 + offset as i64,
                    0_i64,
                    "codex_otel.log_only",
                    format!(
                        "event.name=\"codex.turn_ttft\" duration_ms=1 conversation.id=conversation-{conversation} user.account_id=\"acct-{account}\""
                    )
                ],
            )
            .expect("telemetry row");
    }
    drop(connection);
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    let telemetry_accounts = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
        .map(|item| item.provider_user_id_hash.clone().expect("account hash"))
        .collect::<Vec<_>>();
    assert_eq!(
        telemetry_accounts,
        vec![
            hash_text("acct-a"),
            hash_text("acct-a"),
            hash_text("acct-b"),
            hash_text("acct-a"),
            hash_text("acct-a"),
        ],
        "each run keeps exactly its first and last observation"
    );
}

#[test]
fn codex_reads_historical_auth_file_variants_as_dated_snapshots() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6ImV4aXN0aW5nQGV4YW1wbGUuY29tIiwiaWF0IjoxNzQ4NTEzNTYzLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC1yZWFsIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJwbHVzIiwiY2hhdGdwdF9zdWJzY3JpcHRpb25fYWN0aXZlX3N0YXJ0IjoiMjAyNi0wNS0yOVQxMDoxMjo0MyswMDowMCIsImNoYXRncHRfc3Vic2NyaXB0aW9uX2FjdGl2ZV91bnRpbCI6IjIwMjYtMDYtMjlUMTA6MTI6NDMrMDA6MDAiLCJjaGF0Z3B0X3N1YnNjcmlwdGlvbl9sYXN0X2NoZWNrZWQiOiIyMDI2LTA1LTI5VDEwOjE0OjU2LjA1ODI3OCswMDowMCJ9fQ."
            }
        })
        .to_string(),
    )
    .expect("current auth");
    // A swapped-out login kept beside the live one; its claims date to the
    // moment that account was last authenticated here.
    std::fs::write(
        dir.path().join("auth-previous.json"),
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6InByZXZpb3VzQGV4YW1wbGUudGVzdCIsImF1dGhfdGltZSI6MTc3OTc4MjQwMCwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoIjp7ImNoYXRncHRfYWNjb3VudF9pZCI6ImFjY3QtcHJldmlvdXMiLCJjaGF0Z3B0X3BsYW5fdHlwZSI6InBsdXMiLCJjaGF0Z3B0X3N1YnNjcmlwdGlvbl9hY3RpdmVfc3RhcnQiOiIyMDI2LTA0LTI2VDAwOjAwOjAwKzAwOjAwIiwiY2hhdGdwdF9zdWJzY3JpcHRpb25fYWN0aXZlX3VudGlsIjoiMjAyNi0wNS0yNlQwMDowMDowMCswMDowMCIsImNoYXRncHRfc3Vic2NyaXB0aW9uX2xhc3RfY2hlY2tlZCI6IjIwMjYtMDUtMjZUMDk6MDA6MDArMDA6MDAifX0."
            }
        })
        .to_string(),
    )
    .expect("historical auth");
    std::fs::write(dir.path().join("auth-broken.json"), "not json").expect("broken variant");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    // Only the live auth.json may act as a source-wide auth state; the
    // swapped-out variant is a dated login that must never close an
    // interval nothing can reopen.
    let snapshots = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::AuthSnapshot)
        .collect::<Vec<_>>();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        snapshots[0].provider_user_id_hash.as_deref(),
        Some(hash_text("acct-real").as_str())
    );
    let previous_identity = evidence
        .identity_observations
        .iter()
        .find(|item| {
            item.provider_user_id_hash.as_deref() == Some(hash_text("acct-previous").as_str())
        })
        .expect("historical snapshot identity");
    assert_eq!(
        previous_identity.evidence_kind,
        AccountEvidenceKind::LoginSuccess
    );
    assert!(!previous_identity.evidence_kind.ends_source_attribution());
    assert_eq!(
        previous_identity.observed_at.to_rfc3339(),
        "2026-05-26T08:00:00+00:00"
    );
    let current_plan = evidence
        .plan_observations
        .iter()
        .find(|item| item.is_current_snapshot)
        .expect("current plan claim");
    assert_eq!(current_plan.source_id, source.source_id);
    let historical_plan = evidence
        .plan_observations
        .iter()
        .find(|item| !item.is_current_snapshot)
        .expect("historical plan claim");
    assert_eq!(
        historical_plan.observed_at.to_rfc3339(),
        "2026-05-26T09:00:00+00:00"
    );
    assert_eq!(
        historical_plan.active_until.map(|value| value.to_rfc3339()),
        Some("2026-05-26T00:00:00+00:00".to_string())
    );
}

#[test]
fn codex_skips_malformed_or_locked_telemetry_databases() {
    let malformed = tempfile::tempdir().expect("malformed tempdir");
    let malformed_connection =
        Connection::open(malformed.path().join("logs_2.sqlite")).expect("malformed database");
    malformed_connection
        .execute_batch("CREATE TABLE logs (id INTEGER PRIMARY KEY, unexpected TEXT);")
        .expect("malformed schema");
    drop(malformed_connection);
    let malformed_source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        malformed.path(),
        LocationOrigin::Configured,
    );
    assert!(CodexAdapter
        .collect_account_evidence(&malformed_source, &[])
        .expect("malformed database is non-fatal")
        .identity_observations
        .is_empty());

    let locked = tempfile::tempdir().expect("locked tempdir");
    let locked_connection =
        Connection::open(locked.path().join("logs_2.sqlite")).expect("locked database");
    locked_connection
        .execute_batch(
            "CREATE TABLE logs (id INTEGER PRIMARY KEY, ts INTEGER, ts_nanos INTEGER, feedback_log_body TEXT); PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;",
        )
        .expect("exclusive lock");
    let locked_source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        locked.path(),
        LocationOrigin::Configured,
    );
    assert!(CodexAdapter
        .collect_account_evidence(&locked_source, &[])
        .expect("locked database is non-fatal")
        .identity_observations
        .is_empty());
    locked_connection
        .execute_batch("ROLLBACK")
        .expect("unlock database");
}

#[test]
fn codex_telemetry_identity_ignores_attributes_quoted_inside_user_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let connection = Connection::open(dir.path().join("logs_2.sqlite")).expect("logs database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("logs schema");
    for (row_id, body) in [
        // The injected attributes lead the body, so reading the first match
        // anywhere accepted them as the event's own identity.
        (
            1_i64,
            "prompt=\"please run event.name=\\\"codex.conversation_starts\\\" user.account_id=acct-attacker user.email=attacker@example.test\" event.name=\"codex.user_prompt\"",
        ),
        // A genuine event whose free text repeats the marker afterwards.
        (
            2,
            "event.name=\"codex.conversation_starts\" user.account_id=acct-real prompt=\"see event.name=\\\"codex.conversation_starts\\\" user.account_id=acct-attacker\"",
        ),
        // Two structured identities in one body name nobody.
        (
            3,
            "event.name=\"codex.conversation_starts\" user.account_id=acct-one user.account_id=acct-two",
        ),
    ] {
        connection
            .execute(
                "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    row_id,
                    1_787_227_200_i64 + row_id,
                    0_i64,
                    "codex_otel.log_only",
                    body
                ],
            )
            .expect("telemetry row");
    }
    drop(connection);

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );
    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    let identified = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
        .collect::<Vec<_>>();
    assert_eq!(
        identified.len(),
        1,
        "only the genuine structured attribute prefix identifies an account"
    );
    let expected = provider_account_id_from_identity(CODEX_PROVIDER, Some("acct-real"), None)
        .expect("account id");
    assert_eq!(identified[0].provider_account_id.as_ref(), Some(&expected));
    let attacker = provider_account_id_from_identity(CODEX_PROVIDER, Some("acct-attacker"), None)
        .expect("account id");
    assert!(
        evidence
            .conversation_bindings
            .iter()
            .all(|binding| binding.provider_account_id != attacker),
        "prompt text must never bind a conversation to an unused account"
    );
    assert!(evidence
        .accounts
        .iter()
        .all(|account| account.provider_user_id.as_deref() != Some("acct-attacker")));
}

#[test]
fn codex_login_evidence_survives_log_rotation_without_duplicating() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_directory = dir.path().join("log");
    std::fs::create_dir_all(&log_directory).expect("log directory");
    let entry = "2026-08-20T12:00:00Z successfully logged in\n";
    std::fs::write(log_directory.join("codex-login.log"), entry).expect("login log");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let before = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("evidence before rotation");
    let before_ids = before
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::LoginSuccess)
        .map(|item| item.observation_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(before_ids.len(), 1);

    // The same login, now one generation older, plus a fresh empty log.
    std::fs::rename(
        log_directory.join("codex-login.log"),
        log_directory.join("codex-login.log.1"),
    )
    .expect("rotate login log");
    std::fs::write(log_directory.join("codex-login.log"), "").expect("fresh login log");

    let after = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("evidence after rotation");
    let after_ids = after
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::LoginSuccess)
        .map(|item| item.observation_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        after_ids, before_ids,
        "rotation moves a login between files; it is not a second login"
    );
}

#[test]
fn codex_probe_verified_source_state_uses_parent_auth_for_sessions_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "email": "existing@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-real",
                "chatgpt_plan_type": "plus",
                "chatgpt_subscription_active_start": "2026-05-29T10:12:43+00:00",
                "chatgpt_subscription_active_until": "2026-06-29T10:12:43+00:00",
                "chatgpt_subscription_last_checked": "2026-05-29T10:14:56.058278+00:00"
            }
        })
        .to_string(),
    )
    .expect("auth");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &sessions,
        LocationOrigin::Configured,
    );

    let observation = CodexAdapter
        .probe_verified_source_state(&source)
        .expect("probe");
    let VerifiedSourceObservation::Verified(verified) = observation else {
        panic!("expected verified source state");
    };

    assert_eq!(verified.provider_user_id.as_deref(), Some("acct-real"));
    assert_eq!(verified.email.as_deref(), Some("existing@example.com"));
    assert_eq!(verified.plan_name.as_deref(), Some("Plus"));
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
