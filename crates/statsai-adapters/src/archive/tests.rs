use super::*;
use crate::{GrokBuildAdapter, OpenCodeAdapter, ProviderAdapter};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::Value;
use statsai_core::{ArchiveContentKind, ArchiveItemKind, ArchiveRole, LocationOrigin};
use std::fs::File;
use std::io::Write;
use tempfile::tempdir;
use url::Url;

fn source(provider: &str, path: &Path) -> SourceLocation {
    SourceLocation::local_adapter(provider, "test", "1", path, LocationOrigin::Configured)
}

#[test]
fn claude_subagents_with_a_shared_session_remain_distinct_conversations() {
    let dir = tempdir().unwrap();
    let subagents = dir
        .path()
        .join("projects")
        .join("workspace")
        .join("parent-session")
        .join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();

    for (agent_id, text) in [("alpha", "alpha work"), ("beta", "beta work")] {
        let path = subagents.join(format!("agent-{agent_id}.jsonl"));
        std::fs::write(
            path,
            serde_json::json!({
                "sessionId": "parent-session",
                "isSidechain": true,
                "agentId": agent_id,
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": text}]
                }
            })
            .to_string()
                + "\n",
        )
        .unwrap();
    }

    let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
    assert_eq!(scan.conversations.len(), 2);
    let mut native_ids = scan
        .conversations
        .iter()
        .map(|conversation| conversation.native_conversation_id.as_str())
        .collect::<Vec<_>>();
    native_ids.sort_unstable();
    assert_eq!(
        native_ids,
        ["parent-session:agent:alpha", "parent-session:agent:beta"]
    );
    assert_ne!(
        scan.conversations[0].conversation_id,
        scan.conversations[1].conversation_id
    );
    let parent_id = archive_conversation_id(CLAUDE_CODE_PROVIDER, "parent-session");
    assert!(scan
        .conversations
        .iter()
        .all(|conversation| { conversation.superseded_conversation_ids == [parent_id.clone()] }));
    assert_eq!(
        claude_archive_native_id(&serde_json::json!({}), "main-session", "fallback", false),
        "main-session"
    );
}

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
fn codex_collects_visible_reasoning_and_exact_embedded_image() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(file, r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{{"id":"thread-1","thread_name":"Image work"}}}}"#).unwrap();
    writeln!(file, r#"{{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{{"id":"m1","type":"message","role":"user","content":[{{"type":"input_text","text":"inspect this"}},{{"type":"input_image","image_url":"data:image/png;base64,AAEC/w=="}}]}}}}"#).unwrap();
    writeln!(file, r#"{{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{{"id":"r1","type":"reasoning","summary":[{{"type":"summary_text","text":"The image is readable."}}],"encrypted_content":"opaque"}}}}"#).unwrap();

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
    assert_eq!(scan.conversations.len(), 1);
    let conversation = &scan.conversations[0];
    assert_eq!(conversation.completeness, ArchiveCompleteness::Complete);
    assert_eq!(conversation.project, None);
    assert!(conversation
        .items
        .iter()
        .any(|item| item.kind == ArchiveItemKind::ReasoningSummary));
    let image = conversation
        .items
        .iter()
        .flat_map(|item| &item.parts)
        .find(|part| part.kind == ArchiveContentKind::Image)
        .unwrap();
    assert_eq!(
        BASE64.decode(image.data_base64.as_ref().unwrap()).unwrap(),
        [0, 1, 2, 255]
    );
}

#[test]
fn codex_keeps_tool_call_and_result_with_the_same_call_id() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(
        file,
        r#"{{"type":"session_meta","payload":{{"id":"thread-1"}}}}"#
    )
    .unwrap();
    writeln!(file, r#"{{"type":"response_item","payload":{{"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{{\"path\":\"README.md\"}}"}}}}"#).unwrap();
    writeln!(file, r#"{{"type":"response_item","payload":{{"type":"function_call_output","call_id":"call-1","output":"file contents"}}}}"#).unwrap();

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
    let items = &scan.conversations[0].items;
    assert_eq!(items.len(), 2);
    assert_ne!(items[0].item_id, items[1].item_id);
    assert!(items
        .iter()
        .any(|item| item.kind == ArchiveItemKind::ToolCall));
    assert!(items
        .iter()
        .any(|item| item.kind == ArchiveItemKind::ToolResult));
}

#[test]
fn codex_malformed_json_record_makes_trace_coverage_partial() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(
        file,
        r#"{{"type":"session_meta","payload":{{"id":"thread-1"}}}}"#
    )
    .unwrap();
    writeln!(file, "{{malformed json").unwrap();

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();

    assert_eq!(scan.diagnostics.invalid_records, 1);
    assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
}

#[test]
fn claude_invalid_utf8_record_makes_trace_coverage_partial() {
    let dir = tempdir().unwrap();
    let projects = dir.path().join("projects/example");
    std::fs::create_dir_all(&projects).unwrap();
    let path = projects.join("session.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "sessionId":"session-1",
            "message":{"role":"user","content":"hello"}
        })
    )
    .unwrap();
    file.write_all(&[0xff, b'\n']).unwrap();

    let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();

    assert_eq!(scan.diagnostics.invalid_records, 1);
    assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
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

#[cfg(unix)]
#[test]
fn codex_trace_record_ids_use_the_canonical_archive_path() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let real_root = dir.path().join("real");
    let sessions = real_root.join("sessions");
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
                "output":"Done!"
            }
        }),
    ] {
        writeln!(file, "{value}").unwrap();
    }

    let linked_root = dir.path().join("linked");
    symlink(&real_root, &linked_root).unwrap();
    let scan = collect_codex(&source(CODEX_PROVIDER, &linked_root), None).unwrap();

    assert_eq!(scan.trace_edits.len(), 1);
    assert!(scan.trace_edits[0]
        .source_record_id
        .starts_with(&format!("{}:", canonical_display(&path))));
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

#[test]
fn external_artifact_marks_archive_partial_instead_of_silently_dropping_it() {
    let content = serde_json::json!({
        "type": "input_image",
        "image_url": {"url": "https://example.test/image.png"}
    });
    let (item, missing) = item_from_value(ItemInput {
        provider: "test",
        conversation_native_id: "c1",
        native_item_id: "i1",
        source_record_id: "line:1",
        ordinal: 1,
        kind: ArchiveItemKind::Message,
        role: Some(ArchiveRole::User),
        created_at: None,
        model: None,
        tool_name: None,
        tool_call_id: None,
        status: None,
        usage: None,
        content: &content,
    });
    assert_eq!(missing, 1);
    assert_eq!(
        item.parts[0].external_uri.as_deref(),
        Some("https://example.test/image.png")
    );
}

#[test]
fn unmaterialized_artifacts_are_missing_for_messages_and_tools() {
    for kind in [ArchiveItemKind::Message, ArchiveItemKind::ToolResult] {
        let content = serde_json::json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png"}
        });
        let (item, missing) = item_from_value(ItemInput {
            provider: "test",
            conversation_native_id: "c1",
            native_item_id: kind.as_str(),
            source_record_id: "line:1",
            ordinal: 1,
            kind,
            role: Some(ArchiveRole::Assistant),
            created_at: None,
            model: None,
            tool_name: None,
            tool_call_id: None,
            status: None,
            usage: None,
            content: &content,
        });

        assert_eq!(missing, 1);
        assert!(item.parts.iter().any(|part| part.text.is_some()));
        if kind == ArchiveItemKind::Message {
            assert!(item.parts.iter().any(|part| part
                .text
                .as_deref()
                .is_some_and(|text| text.contains("omitted_content"))));
        }
        assert!(item.parts.iter().all(|part| part.data_base64.is_none()));
    }
}

#[test]
fn local_artifact_reads_require_bounded_regular_files() {
    let dir = tempdir().unwrap();
    let small_path = dir.path().join("small.bin");
    std::fs::write(&small_path, [0, 1, 2, 255]).unwrap();
    assert_eq!(
        read_explicit_local_artifact(small_path.to_str().unwrap()),
        Some(vec![0, 1, 2, 255])
    );
    assert_eq!(
        read_explicit_local_artifact(dir.path().to_str().unwrap()),
        None
    );

    let spaced_path = dir.path().join("My Image.bin");
    std::fs::write(&spaced_path, [4, 5, 6]).unwrap();
    let file_url = Url::from_file_path(&spaced_path).unwrap().to_string();
    assert!(file_url.contains("My%20Image.bin"));
    assert_eq!(
        explicit_local_artifact_path(&file_url),
        Some(spaced_path.clone())
    );
    assert_eq!(read_explicit_local_artifact(&file_url), Some(vec![4, 5, 6]));
    let encoded_content = serde_json::json!({
        "type": "file",
        "url": file_url
    });
    let (encoded_item, encoded_missing) = item_from_value(ItemInput {
        provider: "test",
        conversation_native_id: "c1",
        native_item_id: "encoded-file",
        source_record_id: "line:encoded",
        ordinal: 2,
        kind: ArchiveItemKind::Message,
        role: Some(ArchiveRole::User),
        created_at: None,
        model: None,
        tool_name: None,
        tool_call_id: None,
        status: None,
        usage: None,
        content: &encoded_content,
    });
    assert_eq!(encoded_missing, 0);
    assert_eq!(
        BASE64
            .decode(encoded_item.parts[0].data_base64.as_ref().unwrap())
            .unwrap(),
        [4, 5, 6]
    );

    let oversized_path = dir.path().join("oversized.bin");
    File::create(&oversized_path)
        .unwrap()
        .set_len(MAX_ARTIFACT_BYTES + 1)
        .unwrap();
    assert_eq!(
        read_explicit_local_artifact(oversized_path.to_str().unwrap()),
        None
    );

    let artifact = oversized_path.to_string_lossy().into_owned();
    let content = serde_json::json!({"type": "file", "url": artifact.clone()});
    let (item, missing) = item_from_value(ItemInput {
        provider: "test",
        conversation_native_id: "c1",
        native_item_id: "i1",
        source_record_id: "line:1",
        ordinal: 1,
        kind: ArchiveItemKind::Message,
        role: Some(ArchiveRole::User),
        created_at: None,
        model: None,
        tool_name: None,
        tool_call_id: None,
        status: None,
        usage: None,
        content: &content,
    });
    assert_eq!(missing, 1);
    assert_eq!(
        item.parts[0].external_uri.as_deref(),
        Some(artifact.as_str())
    );
}

#[test]
fn local_image_paths_keep_image_kind_without_provider_mime_metadata() {
    let dir = tempdir().unwrap();
    let png_path = dir.path().join("photo.png");
    std::fs::write(&png_path, [0x89, b'P', b'N', b'G']).unwrap();
    let content = serde_json::json!({
        "type": "input_image",
        "image_url": png_path.to_str().unwrap()
    });
    let (item, missing) = item_from_value(ItemInput {
        provider: "test",
        conversation_native_id: "c1",
        native_item_id: "image-1",
        source_record_id: "line:1",
        ordinal: 1,
        kind: ArchiveItemKind::Message,
        role: Some(ArchiveRole::User),
        created_at: None,
        model: None,
        tool_name: None,
        tool_call_id: None,
        status: None,
        usage: None,
        content: &content,
    });
    assert_eq!(missing, 0);
    assert_eq!(item.parts[0].kind, ArchiveContentKind::Image);
    assert_eq!(item.parts[0].mime_type.as_deref(), Some("image/png"));

    let extensionless_path = dir.path().join("attachment");
    std::fs::write(&extensionless_path, [1, 2, 3]).unwrap();
    let extensionless = serde_json::json!({
        "type": "image",
        "source": extensionless_path.to_str().unwrap()
    });
    let mut parts = Vec::new();
    let mut binary_missing = 0;
    extract_binary_content_parts(
        &extensionless,
        "item-binary",
        &mut parts,
        &mut binary_missing,
        true,
    );
    assert_eq!(binary_missing, 0);
    assert_eq!(parts[0].kind, ArchiveContentKind::Image);
    assert_eq!(parts[0].mime_type.as_deref(), Some("image/unknown"));
}

#[test]
fn codex_tool_results_cannot_materialize_local_files() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "do not archive").unwrap();
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session_meta",
            "payload": {"id": "thread-1"}
        })
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call-1",
                "output": {
                    "type": "file",
                    "source": secret.to_str().unwrap()
                }
            }
        })
    )
    .unwrap();

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
    assert!(scan.artifact_dependencies.is_empty());
    assert_eq!(scan.conversations[0].missing_content_count, 1);
    let result = scan.conversations[0]
        .items
        .iter()
        .find(|item| item.kind == ArchiveItemKind::ToolResult)
        .unwrap();
    assert!(result.parts.iter().all(|part| part.data_base64.is_none()));
    assert!(result
        .parts
        .iter()
        .any(|part| part.external_uri.as_deref() == secret.to_str()));
}

#[test]
fn codex_scan_tracks_explicit_local_artifact_dependencies() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let artifact = dir.path().join("missing.png");
    let path = sessions.join("thread.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session_meta",
            "payload": {"id": "thread-1"}
        })
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "response_item",
            "payload": {
                "id": "m1",
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "image_url": artifact.to_str().unwrap()
                }]
            }
        })
    )
    .unwrap();

    let selected = HashSet::from([canonical_display(&path)]);
    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), Some(&selected)).unwrap();
    assert_eq!(scan.artifact_dependencies.len(), 1);
    assert_eq!(
        scan.artifact_dependencies[0].cache_key,
        canonical_display(&path)
    );
    assert_eq!(scan.artifact_dependencies[0].path, artifact);
    assert_eq!(scan.artifact_dependencies[0].metadata_signature, "missing");
}

#[test]
fn codex_streaming_collection_uses_late_session_metadata() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("fallback-name.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"id":"m1","type":"message","role":"user","content":[{{"type":"input_text","text":"before metadata"}}]}}}}"#
        )
        .unwrap();
    writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"thread-late","thread_name":"Late metadata"}}}}"#
        )
        .unwrap();

    let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
    assert_eq!(scan.conversations.len(), 1);
    let conversation = &scan.conversations[0];
    assert_eq!(conversation.native_conversation_id, "thread-late");
    assert_eq!(conversation.title.as_deref(), Some("Late metadata"));
    assert_eq!(conversation.items.len(), 1);
    assert_eq!(
        conversation.items[0].parts[0].text.as_deref(),
        Some("before metadata")
    );
}

#[test]
fn selected_jsonl_paths_skip_discovery_for_explicit_cache_keys() {
    let path = PathBuf::from("selected.jsonl");
    let selected = HashSet::from([path.to_string_lossy().into_owned()]);
    let discovery_called = std::cell::Cell::new(false);

    let paths = selected_jsonl_paths(Some(&selected), || {
        discovery_called.set(true);
        vec![PathBuf::from("discovered.jsonl")]
    });

    assert_eq!(paths, vec![path]);
    assert!(!discovery_called.get());
}

#[test]
fn embedded_artifact_decode_is_bounded_and_marks_missing() {
    let encoded = "AAEC/w==";
    assert_eq!(decoded_base64_len(encoded), Some(4));

    let mut parts = Vec::new();
    let mut missing = 0;
    push_binary_part_with_limit(
        "item-1",
        "image/png",
        None,
        encoded,
        3,
        &mut parts,
        &mut missing,
    );
    assert!(parts.is_empty());
    assert_eq!(missing, 1);

    push_binary_part_with_limit(
        "item-1",
        "image/png",
        None,
        encoded,
        4,
        &mut parts,
        &mut missing,
    );
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].original_bytes, 4);
    assert_eq!(missing, 1);

    push_binary_part_with_limit(
        "item-1",
        "image/png",
        None,
        "invalid!",
        4,
        &mut parts,
        &mut missing,
    );
    assert_eq!(parts.len(), 1);
    assert_eq!(missing, 2);
}

#[test]
fn tool_results_are_bounded_but_keep_full_hash_and_size() {
    let original = "a".repeat(MAX_TOOL_RESULT_TEXT_BYTES + 1024);
    let content = Value::String(original.clone());
    let (item, missing) = item_from_value(ItemInput {
        provider: "test",
        conversation_native_id: "c1",
        native_item_id: "i1",
        source_record_id: "line:1",
        ordinal: 1,
        kind: ArchiveItemKind::ToolResult,
        role: Some(ArchiveRole::Tool),
        created_at: None,
        model: None,
        tool_name: Some("exec"),
        tool_call_id: None,
        status: None,
        usage: None,
        content: &content,
    });
    assert_eq!(missing, 0);
    assert!(!item.parts_authoritative);
    assert!(item.parts[0].truncated);
    assert_eq!(item.parts[0].original_bytes, original.len() as u64);
    assert_eq!(item.parts[0].content_hash, hash_text(&original));
    assert!(item.parts[0].text.as_ref().unwrap().contains("truncated"));
    assert!(item.parts[0].text.as_ref().unwrap().len() <= MAX_TOOL_RESULT_TEXT_BYTES);
}

#[test]
fn tool_calls_preserve_complete_structured_arguments() {
    let content = serde_json::json!({
        "path": "/tmp/a",
        "content": "replacement",
        "options": {"recursive": true, "limit": 25}
    });
    let (item, missing) = item_from_value(ItemInput {
        provider: "test",
        conversation_native_id: "c1",
        native_item_id: "i1",
        source_record_id: "line:1",
        ordinal: 1,
        kind: ArchiveItemKind::ToolCall,
        role: Some(ArchiveRole::Assistant),
        created_at: None,
        model: None,
        tool_name: Some("replace"),
        tool_call_id: Some("call-1"),
        status: None,
        usage: None,
        content: &content,
    });

    assert_eq!(missing, 0);
    assert!(item.parts_authoritative);
    assert_eq!(item.parts.len(), 1);
    let archived = serde_json::from_str::<Value>(item.parts[0].text.as_ref().unwrap()).unwrap();
    assert_eq!(archived, content);
    assert_eq!(
        item.parts[0].original_bytes,
        content.to_string().len() as u64
    );
    assert!(!item.parts[0].truncated);
}

#[test]
fn claude_collects_text_thinking_and_embedded_images() {
    let dir = tempdir().unwrap();
    let projects = dir.path().join("projects/project-a");
    std::fs::create_dir_all(&projects).unwrap();
    let path = projects.join("session.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(
            file,
            r#"{{"sessionId":"claude-1","type":"assistant","timestamp":"2026-01-01T00:00:00Z","message":{{"role":"assistant","content":[{{"type":"text","text":"answer"}},{{"type":"thinking","thinking":"Readable reasoning"}},{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"AAEC/w=="}}}}]}}}}"#
        )
        .unwrap();

    let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
    let conversation = &scan.conversations[0];
    let parts = conversation
        .items
        .iter()
        .flat_map(|item| &item.parts)
        .collect::<Vec<_>>();
    assert!(parts
        .iter()
        .any(|part| part.text.as_deref() == Some("answer")));
    assert!(parts
        .iter()
        .any(|part| part.text.as_deref() == Some("Readable reasoning")));
    let image = parts
        .iter()
        .find(|part| part.kind == ArchiveContentKind::Image)
        .unwrap();
    assert_eq!(
        BASE64.decode(image.data_base64.as_ref().unwrap()).unwrap(),
        [0, 1, 2, 255]
    );
}

#[test]
fn claude_discards_redacted_and_encrypted_only_thinking_blocks() {
    let dir = tempdir().unwrap();
    let projects = dir.path().join("projects/project-a");
    std::fs::create_dir_all(&projects).unwrap();
    let path = projects.join("session.jsonl");
    let record = serde_json::json!({
        "sessionId": "claude-1",
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "visible answer"},
                {"type": "redacted_thinking", "data": "opaque-redacted-payload"},
                {"type": "thinking", "data": "opaque-encrypted-payload"},
                {"type": "thinking", "thinking": "readable reasoning"}
            ]
        }
    });
    std::fs::write(&path, record.to_string() + "\n").unwrap();

    let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
    let conversation = &scan.conversations[0];
    let archived_text = conversation
        .items
        .iter()
        .flat_map(|item| &item.parts)
        .filter_map(|part| part.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(archived_text.contains("visible answer"));
    assert!(archived_text.contains("readable reasoning"));
    assert!(!archived_text.contains("opaque-redacted-payload"));
    assert!(!archived_text.contains("opaque-encrypted-payload"));
    assert_eq!(conversation.items.len(), 2);
    assert_eq!(conversation.discarded_source_record_ids.len(), 2);
    assert_eq!(conversation.completeness, ArchiveCompleteness::Complete);
}

#[test]
fn claude_classifies_and_bounds_tool_blocks() {
    let dir = tempdir().unwrap();
    let projects = dir.path().join("projects/project-a");
    std::fs::create_dir_all(&projects).unwrap();
    let path = projects.join("session.jsonl");
    let output = "x".repeat(MAX_TOOL_RESULT_TEXT_BYTES + 1024);
    let record = serde_json::json!({
        "sessionId": "claude-1",
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [
                {
                    "type": "tool_use",
                    "id": "tool-1",
                    "name": "shell",
                    "input": {"command": "build"}
                },
                {
                    "type": "tool_result",
                    "tool_use_id": "tool-1",
                    "content": [
                        {"type": "text", "text": output},
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "AAEC/w=="
                            }
                        }
                    ]
                }
            ]
        }
    });
    let mut file = File::create(&path).unwrap();
    writeln!(file, "{record}").unwrap();

    let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
    let conversation = &scan.conversations[0];
    assert!(conversation
        .items
        .iter()
        .any(|item| item.kind == ArchiveItemKind::ToolCall));
    let result = conversation
        .items
        .iter()
        .find(|item| item.kind == ArchiveItemKind::ToolResult)
        .unwrap();
    assert_eq!(result.role, Some(ArchiveRole::Tool));
    assert_eq!(result.tool_call_id.as_deref(), Some("tool-1"));
    assert!(result.parts[0].truncated);
    assert!(result.parts[0].original_bytes > MAX_TOOL_RESULT_TEXT_BYTES as u64);
    assert!(result.parts[0].text.as_ref().unwrap().contains("truncated"));
    assert!(result.parts[0].text.as_ref().unwrap().len() <= MAX_TOOL_RESULT_TEXT_BYTES);
    let image = result
        .parts
        .iter()
        .find(|part| part.kind == ArchiveContentKind::Image)
        .unwrap();
    assert_eq!(
        BASE64.decode(image.data_base64.as_ref().unwrap()).unwrap(),
        [0, 1, 2, 255]
    );
}

#[test]
fn grok_collects_readable_chat_history() {
    let dir = tempdir().unwrap();
    let session = dir.path().join("sessions/grok-1");
    std::fs::create_dir_all(&session).unwrap();
    let mut file = File::create(session.join("chat_history.jsonl")).unwrap();
    writeln!(file, r#"{{"id":"u1","type":"user","content":"hello"}}"#).unwrap();
    writeln!(
        file,
        r#"{{"id":"r1","type":"reasoning","summary":"thinking"}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"id":"a1","type":"assistant","content":"world"}}"#
    )
    .unwrap();

    let scan = collect_grok(&source(GROK_BUILD_PROVIDER, dir.path()), None).unwrap();
    let conversation = &scan.conversations[0];
    assert_eq!(conversation.items.len(), 3);
    let reasoning = conversation
        .items
        .iter()
        .find(|item| item.kind == ArchiveItemKind::ReasoningSummary)
        .expect("reasoning summary");
    assert_eq!(reasoning.parts.len(), 1);
    assert_eq!(reasoning.parts[0].text.as_deref(), Some("thinking"));
    assert_eq!(conversation.completeness, ArchiveCompleteness::Complete);
    assert_eq!(scan.trace_coverage, CoverageStatus::Unavailable);
}

#[test]
fn grok_archive_candidates_include_chat_without_summary() {
    let dir = tempdir().unwrap();
    let session = dir.path().join("sessions/grok-1");
    std::fs::create_dir_all(&session).unwrap();
    let chat_path = session.join("chat_history.jsonl");
    let mut file = File::create(&chat_path).unwrap();
    writeln!(file, r#"{{"id":"u1","type":"user","content":"hello"}}"#).unwrap();

    let source = source(GROK_BUILD_PROVIDER, dir.path());
    let adapter = GrokBuildAdapter;
    let candidates = adapter.archive_scan_candidates(&source).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].path, chat_path);

    let selected = candidates
        .into_iter()
        .map(|candidate| candidate.cache_key)
        .collect::<HashSet<_>>();
    let scan = adapter.collect_archive(&source, Some(&selected)).unwrap();
    assert_eq!(scan.conversations.len(), 1);
    assert_eq!(scan.conversations[0].items.len(), 1);
}

#[test]
fn opencode_collects_part_text_and_binary_artifacts() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("opencode.db");
    let connection = rusqlite::Connection::open(&db_path).unwrap();
    connection
            .execute_batch(
                r#"
                CREATE TABLE session (
                  id TEXT PRIMARY KEY,
                  title TEXT,
                  time_created INTEGER NOT NULL,
                  time_updated INTEGER NOT NULL,
                  directory TEXT
                );
                CREATE TABLE message (
                  id TEXT PRIMARY KEY,
                  session_id TEXT NOT NULL,
                  time_created INTEGER NOT NULL,
                  data TEXT NOT NULL
                );
                CREATE TABLE part (
                  id TEXT PRIMARY KEY,
                  message_id TEXT NOT NULL,
                  session_id TEXT NOT NULL,
                  time_created INTEGER NOT NULL,
                  data TEXT NOT NULL
                );
                INSERT INTO session VALUES ('s1', 'OpenCode thread', 1000, 2000, '/tmp/project');
                INSERT INTO message VALUES ('m1', 's1', 1000, '{"role":"user"}');
                INSERT INTO part VALUES ('p1', 'm1', 's1', 1001, '{"type":"text","text":"hello from opencode"}');
                INSERT INTO part VALUES ('p2', 'm1', 's1', 1002, '{"type":"file","url":"data:image/png;base64,AAEC/w=="}');
                "#,
            )
            .unwrap();
    let tool_output = format!(
        "output-head-{}-output-tail",
        "x".repeat(MAX_TOOL_RESULT_TEXT_BYTES + 1024)
    );
    let tool_state = serde_json::json!({
        "type": "tool",
        "callID": "call-1",
        "tool": "shell",
        "state": {
            "status": "completed",
            "input": {"command": "build", "cwd": "/tmp/project"},
            "output": tool_output.clone()
        }
    });
    connection
        .execute(
            "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["p3", "m1", "s1", 1003, tool_state.to_string()],
        )
        .unwrap();
    drop(connection);

    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let noncanonical_root = nested.join("..");
    let mut source = source(OPENCODE_PROVIDER, &noncanonical_root);
    source.path_label = Some(noncanonical_root.to_string_lossy().into_owned());
    let adapter = OpenCodeAdapter;
    let selected = adapter
        .archive_scan_candidates(&source)
        .unwrap()
        .into_iter()
        .map(|candidate| candidate.cache_key)
        .collect::<HashSet<_>>();
    assert_eq!(selected, HashSet::from([canonical_display(&db_path)]));

    let scan = collect_opencode(&source, Some(&selected)).unwrap();
    let conversation = &scan.conversations[0];
    assert_eq!(conversation.items.len(), 4);
    assert!(conversation
        .items
        .iter()
        .flat_map(|item| &item.parts)
        .any(|part| part.text.as_deref() == Some("hello from opencode")));
    assert!(conversation
        .items
        .iter()
        .flat_map(|item| &item.parts)
        .any(|part| part.kind == ArchiveContentKind::Image && part.original_bytes == 4));
    let call = conversation
        .items
        .iter()
        .find(|item| item.kind == ArchiveItemKind::ToolCall)
        .expect("tool call");
    let result = conversation
        .items
        .iter()
        .find(|item| item.kind == ArchiveItemKind::ToolResult)
        .expect("tool result");
    assert_ne!(call.item_id, result.item_id);
    assert_eq!(call.native_item_id.as_deref(), Some("p3"));
    assert_eq!(result.native_item_id.as_deref(), Some("p3:result"));
    assert_eq!(call.tool_name.as_deref(), Some("shell"));
    assert_eq!(call.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(call.status.as_deref(), Some("completed"));
    assert_eq!(
        serde_json::from_str::<Value>(call.parts[0].text.as_ref().unwrap()).unwrap(),
        serde_json::json!({"command": "build", "cwd": "/tmp/project"})
    );
    assert_eq!(result.role, Some(ArchiveRole::Tool));
    assert_eq!(result.tool_name.as_deref(), Some("shell"));
    assert_eq!(result.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(result.status.as_deref(), Some("completed"));
    assert!(result.parts[0].truncated);
    assert_eq!(result.parts[0].original_bytes, tool_output.len() as u64);
    assert_eq!(result.parts[0].content_hash, hash_text(&tool_output));
    let retained_output = result.parts[0].text.as_ref().unwrap();
    assert!(retained_output.starts_with("output-head-"));
    assert!(retained_output.contains("[... truncated ...]"));
    assert!(retained_output.ends_with("-output-tail"));
    assert!(retained_output.len() <= MAX_TOOL_RESULT_TEXT_BYTES);
    assert_eq!(scan.trace_coverage, CoverageStatus::Unavailable);
}
