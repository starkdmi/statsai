use super::*;

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
