use super::*;

/// A resumed session records its parent's identifier before its own, so the
/// conversation this file becomes is known only at the end. Every edit must
/// still reference it, or the store rejects the import outright.
#[test]
fn claude_resumed_session_binds_every_edit_to_the_written_conversation() {
    let dir = tempdir().unwrap();
    let project = dir.path().join("projects").join("workspace");
    std::fs::create_dir_all(&project).unwrap();
    let path = project.join("resumed.jsonl");
    let mut file = File::create(&path).unwrap();
    // The parent session's edit is reconstructed before the record that
    // reveals this file is really the resumed session.
    for record in [
        serde_json::json!({
            "sessionId": "parent-session",
            "type": "assistant",
            "message": {"role": "assistant", "content": [{
                "type": "tool_use",
                "id": "call-1",
                "name": "Write",
                "input": {"file_path": "src/lib.rs", "content": "one\ntwo\n"}
            }]}
        }),
        serde_json::json!({
            "sessionId": "parent-session",
            "type": "user",
            "message": {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "call-1",
                "content": "File created successfully at: src/lib.rs"
            }]}
        }),
        serde_json::json!({
            "sessionId": "resumed-session",
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": "carry on"}]}
        }),
    ] {
        writeln!(file, "{record}").unwrap();
    }
    drop(file);

    let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();

    assert_eq!(scan.conversations.len(), 1);
    let conversation = &scan.conversations[0];
    assert_eq!(conversation.native_conversation_id, "resumed-session");
    assert!(!scan.trace_edits.is_empty(), "the write must be measured");
    // Every edit references the conversation that was actually written, and
    // no two edits collapsed onto one identifier while being re-bound.
    assert!(scan
        .trace_edits
        .iter()
        .all(|edit| edit.conversation_id == conversation.conversation_id));
    let distinct_ids = scan
        .trace_edits
        .iter()
        .map(|edit| edit.trace_edit_id.as_str())
        .collect::<HashSet<_>>();
    assert_eq!(distinct_ids.len(), scan.trace_edits.len());
    // Re-reading the same file reproduces the identifiers, so a re-import
    // replaces the edits instead of duplicating them.
    let repeated = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
    assert_eq!(repeated.trace_edits, scan.trace_edits);
}

#[test]
fn codex_counts_original_patch_before_archive_tool_argument_truncation() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-01T10:00:00Z",
            "type": "session_meta",
            "payload": {"id": "thread-1", "cwd": dir.path()}
        })
    )
    .unwrap();
    let mut patch = "*** Begin Patch\n*** Add File: src/generated.rs\n".to_string();
    for index in 0..5_000 {
        patch.push_str(&format!("+line {index}\n"));
    }
    patch.push_str("*** End Patch\n");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-01T10:00:01Z",
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "call_id": "call-1",
                "name": "apply_patch",
                "arguments": patch,
            }
        })
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-01T10:00:02Z",
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call-1",
                "output": "Done!",
            }
        })
    )
    .unwrap();

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
    assert_eq!(scan.trace_coverage, CoverageStatus::Complete);
    assert_eq!(scan.trace_edits.len(), 1);
    assert_eq!(scan.trace_edits[0].counts.source_additions, 5_000);
    let call = scan.conversations[0]
        .items
        .iter()
        .find(|item| item.kind == ArchiveItemKind::ToolCall)
        .unwrap();
    assert!(call.parts.iter().any(|part| part.truncated));
}

#[test]
fn codex_custom_tool_records_collect_successful_apply_patch_edits() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    for value in [
        serde_json::json!({
            "timestamp":"2026-08-01T10:00:00Z",
            "type":"session_meta",
            "payload":{"id":"thread-1","cwd":dir.path()}
        }),
        serde_json::json!({
            "timestamp":"2026-08-01T10:00:01Z",
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "call_id":"call-1",
                "name":"apply_patch",
                "input":"*** Begin Patch\n*** Add File: src/lib.rs\n+pub fn added() {}\n*** End Patch\n"
            }
        }),
        serde_json::json!({
            "timestamp":"2026-08-01T10:00:02Z",
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":"call-1",
                "output":"Done!"
            }
        }),
    ] {
        writeln!(file, "{value}").unwrap();
    }

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();

    assert_eq!(scan.trace_coverage, CoverageStatus::Complete);
    assert_eq!(scan.trace_edits.len(), 1);
    assert_eq!(scan.trace_edits[0].counts.source_additions, 1);
    assert_eq!(scan.diagnostics.applied_mutations, 1);
    let items = &scan.conversations[0].items;
    assert!(items
        .iter()
        .any(|item| item.kind == ArchiveItemKind::ToolCall));
    assert!(items
        .iter()
        .any(|item| item.kind == ArchiveItemKind::ToolResult));
}

#[test]
fn failed_mutations_reduce_coverage_without_counting_lines() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    for value in [
        serde_json::json!({"type":"session_meta","payload":{"id":"thread-1"}}),
        serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"function_call",
                "call_id":"call-1",
                "name":"apply_patch",
                "arguments":"*** Begin Patch\n*** Add File: src/lib.rs\n+line\n*** End Patch\n"
            }
        }),
        serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"function_call_output",
                "call_id":"call-1",
                "output":"patch rejected"
            }
        }),
    ] {
        writeln!(file, "{value}").unwrap();
    }

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
    assert!(scan.trace_edits.is_empty());
    assert_eq!(scan.diagnostics.failed_mutations, 1);
    assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
}

#[test]
fn unparseable_successful_mutation_is_not_applied_or_complete() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    for value in [
        serde_json::json!({"type":"session_meta","payload":{"id":"thread-1"}}),
        serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"function_call",
                "call_id":"call-1",
                "name":"apply_patch",
                "arguments":"*** Begin Patch\n*** End Patch\n"
            }
        }),
        serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"function_call_output",
                "call_id":"call-1",
                "output":"Done!"
            }
        }),
    ] {
        writeln!(file, "{value}").unwrap();
    }

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();

    assert!(scan.trace_edits.is_empty());
    assert_eq!(scan.diagnostics.applied_mutations, 0);
    assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
}

#[test]
fn codex_invalid_context_patch_is_not_counted_as_applied() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    for value in [
        serde_json::json!({"type":"session_meta","payload":{"id":"thread-1"}}),
        serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"function_call",
                "call_id":"call-1",
                "name":"apply_patch",
                "arguments":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch\n"
            }
        }),
        serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"function_call_output",
                "call_id":"call-1",
                "output":"Invalid Context 0:\nold"
            }
        }),
    ] {
        writeln!(file, "{value}").unwrap();
    }

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
    assert!(scan.trace_edits.is_empty());
    assert_eq!(scan.diagnostics.failed_mutations, 1);
    assert_eq!(scan.diagnostics.applied_mutations, 0);
    assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
}

#[test]
fn codex_shell_execution_makes_trace_coverage_partial() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({"type":"session_meta","payload":{"id":"thread-1"}})
    )
    .unwrap();
    for (index, command) in [
        "git apply changes.patch",
        "python -c \"from pathlib import Path; Path('x').write_text('x')\"",
        "truncate -s 0 generated.txt",
        "printf x>generated.txt",
    ]
    .into_iter()
    .enumerate()
    {
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "call_id":format!("call-{index}"),
                    "name":"exec_command",
                    "arguments":{"cmd":command}
                }
            })
        )
        .unwrap();
    }

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
    assert_eq!(scan.diagnostics.unsupported_mutations, 4);
    assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
}

#[test]
fn read_only_shell_execution_keeps_trace_coverage_complete() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({"type":"session_meta","payload":{"id":"thread-1"}})
    )
    .unwrap();
    for (index, arguments) in [
        serde_json::json!({"cmd": "ls -la src"}),
        serde_json::json!({"cmd": "git status --short && git diff --stat"}),
        serde_json::json!({"cmd": "cargo test -p statsai-core 2>&1 | tail -20"}),
        serde_json::json!({"cmd": "RUST_LOG=debug cargo clippy --all-targets"}),
        serde_json::json!({"command": ["bash", "-lc", "rg --files-with-matches TODO crates"]}),
        serde_json::json!({"command": ["ls", "-la"]}),
    ]
    .into_iter()
    .enumerate()
    {
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "call_id":format!("call-{index}"),
                    "name":"exec_command",
                    "arguments":arguments
                }
            })
        )
        .unwrap();
    }

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
    assert_eq!(scan.diagnostics.unsupported_mutations, 0);
    assert_eq!(scan.trace_coverage, CoverageStatus::Complete);
}

#[test]
fn providers_without_mutation_parsing_report_unavailable_trace_coverage() {
    let dir = tempdir().unwrap();
    let scan =
        collect_provider_archive("unwired-provider", &source("unwired", dir.path()), None).unwrap();
    assert_eq!(scan.trace_coverage, CoverageStatus::Unavailable);
}

#[test]
fn claude_structured_edit_is_counted_after_successful_tool_result() {
    let dir = tempdir().unwrap();
    let projects = dir.path().join("projects/example");
    std::fs::create_dir_all(&projects).unwrap();
    let path = projects.join("session.jsonl");
    let mut file = File::create(&path).unwrap();
    for value in [
        serde_json::json!({
            "sessionId":"session-1",
            "cwd":dir.path(),
            "message":{"role":"assistant","content":[{
                "type":"tool_use",
                "id":"tool-1",
                "name":"Edit",
                "input":{"file_path":dir.path().join("src/lib.rs"),"old_string":"old","new_string":"new"}
            }]}
        }),
        serde_json::json!({
            "sessionId":"session-1",
            "message":{"role":"user","content":[{
                "type":"tool_result",
                "tool_use_id":"tool-1",
                "content":"updated"
            }]}
        }),
    ] {
        writeln!(file, "{value}").unwrap();
    }

    let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
    assert_eq!(scan.trace_edits.len(), 1);
    assert_eq!(
        scan.trace_edits[0].relative_path,
        PathBuf::from("src/lib.rs")
    );
    assert_eq!(scan.trace_edits[0].counts.source_additions, 1);
    assert_eq!(scan.trace_edits[0].counts.source_deletions, 1);
}

#[test]
fn claude_write_is_counted_as_a_creation_when_the_result_says_so() {
    let write_session = |result: &str| {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects/example");
        std::fs::create_dir_all(&projects).unwrap();
        let mut file = File::create(projects.join("session.jsonl")).unwrap();
        for value in [
            serde_json::json!({
                "sessionId":"session-1",
                "cwd":dir.path(),
                "message":{"role":"assistant","content":[{
                    "type":"tool_use",
                    "id":"tool-1",
                    "name":"Write",
                    "input":{
                        "file_path":dir.path().join("src/lib.rs"),
                        "content":"one\ntwo\nthree\n"
                    }
                }]}
            }),
            serde_json::json!({
                "sessionId":"session-1",
                "message":{"role":"user","content":[{
                    "type":"tool_result",
                    "tool_use_id":"tool-1",
                    "content":result
                }]}
            }),
        ] {
            writeln!(file, "{value}").unwrap();
        }
        let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
        scan.trace_edits.into_iter().next().expect("one trace edit")
    };

    // The Write tool never declares creation in its arguments, so a created
    // file is only distinguishable from an overwrite through the outcome.
    let created = write_session("File created successfully at: /workspace/src/lib.rs");
    assert_eq!(
        created.mutation_kind,
        statsai_core::MutationKind::FileCreation
    );
    assert_eq!(created.counts.source_additions, 3);
    assert_eq!(created.counts.unclassified_lines_written, 0);
    assert_eq!(
        created.added_line_fingerprints.len(),
        3,
        "a creation carries fingerprints, so it can be trace-matched"
    );

    // An overwrite mixes new and replaced lines, which stays unclassified.
    let overwritten = write_session("The file /workspace/src/lib.rs has been updated.");
    assert_eq!(
        overwritten.mutation_kind,
        statsai_core::MutationKind::FileWrite
    );
    assert_eq!(overwritten.counts.source_additions, 0);
    assert_eq!(overwritten.counts.unclassified_lines_written, 3);
}

#[test]
fn claude_multiedit_collects_each_nested_edit_after_successful_result() {
    let dir = tempdir().unwrap();
    let projects = dir.path().join("projects/example");
    std::fs::create_dir_all(&projects).unwrap();
    let path = projects.join("session.jsonl");
    let mut file = File::create(&path).unwrap();
    for value in [
        serde_json::json!({
            "sessionId":"session-1",
            "cwd":dir.path(),
            "message":{"role":"assistant","content":[{
                "type":"tool_use",
                "id":"tool-1",
                "name":"MultiEdit",
                "input":{
                    "file_path":dir.path().join("src/lib.rs"),
                    "edits":[
                        {"old_string":"old","new_string":"new"},
                        {"old_string":"old","new_string":"new"}
                    ]
                }
            }]}
        }),
        serde_json::json!({
            "sessionId":"session-1",
            "message":{"role":"user","content":[{
                "type":"tool_result",
                "tool_use_id":"tool-1",
                "content":"updated"
            }]}
        }),
    ] {
        writeln!(file, "{value}").unwrap();
    }

    let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();

    assert_eq!(scan.trace_coverage, CoverageStatus::Complete);
    assert_eq!(scan.trace_edits.len(), 2);
    assert_eq!(scan.diagnostics.applied_mutations, 1);
    assert_ne!(
        scan.trace_edits[0].trace_edit_id,
        scan.trace_edits[1].trace_edit_id
    );
    assert!(scan.trace_edits.iter().all(|edit| {
        edit.relative_path == Path::new("src/lib.rs")
            && edit.counts.source_additions == 1
            && edit.counts.source_deletions == 1
    }));
}
