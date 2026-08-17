use serde_json::Value;
use statsai_core::{
    archive_conversation_id, ArchiveCompleteness, ArchiveConversation, SourceId,
    ARCHIVE_CONVERSATION_SCHEMA_VERSION,
};
use statsai_store::Store;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

fn run_statsai(store: &Path, codex_home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_statsai"))
        .arg("--store")
        .arg(store)
        .args(args)
        .env("CODEX_HOME", codex_home)
        // Pinned so every invocation in a test refreshes metrics as the same
        // device. Metric replacement is deliberately scoped to one device, so an
        // identity that changed between two runs would leave the first run's rows
        // untouched and read as a pruning failure.
        .env("STATSAI_DEVICE_ID", "conversation-cli-test")
        .output()
        .expect("run statsai")
}

fn stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 stdout")
}

#[test]
fn conversation_archive_cli_collects_searches_and_round_trips_artifacts() {
    let dir = tempdir().expect("temp dir");
    let codex_home = dir.path().join("codex");
    let sessions = codex_home.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut fixture = File::create(sessions.join("thread.jsonl")).expect("fixture");
    writeln!(
        fixture,
        r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{{"id":"thread-cli","thread_name":"CLI archive test"}}}}"#
    )
    .unwrap();
    writeln!(
        fixture,
        r#"{{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{{"id":"m1","type":"message","role":"user","content":[{{"type":"input_text","text":"find the lighthouse phrase"}},{{"type":"input_image","image_url":"data:image/png;base64,AAEC/w=="}}]}}}}"#
    )
    .unwrap();
    let store = dir.path().join("statsai.sqlite3");

    let first = stdout(run_statsai(
        &store,
        &codex_home,
        &[
            "conversation",
            "collect",
            "--provider",
            "codex",
            "--verbose",
        ],
    ));
    assert!(first.contains("conversations=1"), "{first}");
    assert!(first.contains("binary_bytes=4"), "{first}");

    let list: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["conversation", "list", "--json"],
    )))
    .expect("list JSON");
    let conversation_id = list[0]["conversation_id"]
        .as_str()
        .expect("conversation id");

    let shown: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["conversation", "show", conversation_id, "--json"],
    )))
    .expect("show JSON");
    let binary = shown["items"][0]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|part| part["data_base64"].as_str())
        .expect("embedded image");
    assert_eq!(binary, "AAEC/w==");

    let search: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["conversation", "search", "lighthouse", "--json"],
    )))
    .expect("search JSON");
    assert_eq!(search.as_array().unwrap().len(), 1);

    let second = stdout(run_statsai(
        &store,
        &codex_home,
        &[
            "conversation",
            "collect",
            "--provider",
            "codex",
            "--verbose",
        ],
    ));
    assert!(second.contains("archive unchanged"), "{second}");
    assert!(second.contains("conversations=0"), "{second}");
}

#[test]
fn conversation_collect_reports_partial_coverage_for_unmeasurable_mutations() {
    let dir = tempdir().expect("temp dir");
    let codex_home = dir.path().join("codex");
    let sessions = codex_home.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut fixture = File::create(sessions.join("thread.jsonl")).expect("fixture");
    for value in [
        serde_json::json!({
            "timestamp":"2026-08-01T10:00:00Z",
            "type":"session_meta",
            "payload":{"id":"thread-partial-coverage","cwd":dir.path()}
        }),
        serde_json::json!({
            "timestamp":"2026-08-01T10:00:01Z",
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "call_id":"call-applied",
                "name":"apply_patch",
                "input":"*** Begin Patch\n*** Add File: src/lib.rs\n+pub fn added() {}\n*** End Patch\n"
            }
        }),
        serde_json::json!({
            "timestamp":"2026-08-01T10:00:02Z",
            "type":"response_item",
            "payload":{"type":"custom_tool_call_output","call_id":"call-applied","output":"Done!"}
        }),
        serde_json::json!({
            "timestamp":"2026-08-01T10:00:03Z",
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "call_id":"call-rejected",
                "name":"apply_patch",
                "input":"*** Begin Patch\n*** Update File: src/other.rs\n@@\n-missing\n+replacement\n*** End Patch\n"
            }
        }),
        serde_json::json!({
            "timestamp":"2026-08-01T10:00:04Z",
            "type":"response_item",
            // The exact shape Codex records for a rejected patch.
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":"call-rejected",
                "output":"apply_patch verification failed: Failed to read file to update src/other.rs: No such file or directory (os error 2)"
            }
        }),
    ] {
        writeln!(fixture, "{value}").expect("write fixture");
    }
    drop(fixture);
    let store_path = dir.path().join("statsai.sqlite3");

    let collected = stdout(run_statsai(
        &store_path,
        &codex_home,
        &["conversation", "collect", "--provider", "codex"],
    ));

    // The applied edit is still counted; the rejected one only lowers coverage.
    assert!(collected.contains("trace_edits=1"), "{collected}");
    assert!(collected.contains("trace_coverage=Partial"), "{collected}");
}

#[test]
fn conversation_collect_prunes_code_traces_when_archive_file_is_deleted() {
    let dir = tempdir().expect("temp dir");
    let codex_home = dir.path().join("codex");
    let sessions = codex_home.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let fixture_path = sessions.join("thread.jsonl");
    let mut fixture = File::create(&fixture_path).expect("fixture");
    for value in [
        serde_json::json!({
            "timestamp":"2026-08-01T10:00:00Z",
            "type":"session_meta",
            "payload":{"id":"thread-deleted-trace","cwd":dir.path()}
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
        writeln!(fixture, "{value}").expect("write fixture");
    }
    drop(fixture);
    let store_path = dir.path().join("statsai.sqlite3");

    stdout(run_statsai(
        &store_path,
        &codex_home,
        &["conversation", "collect", "--provider", "codex"],
    ));
    let store = Store::open(&store_path).expect("open collected store");
    assert_eq!(store.list_trace_edits().expect("initial traces").len(), 1);
    assert_eq!(
        store
            .list_code_change_metrics(false)
            .expect("initial metrics")
            .len(),
        1
    );
    drop(store);

    std::fs::remove_file(&fixture_path).expect("delete archive fixture");
    let recollected = stdout(run_statsai(
        &store_path,
        &codex_home,
        &["conversation", "collect", "--provider", "codex"],
    ));

    assert!(recollected.contains("trace_edits=0"), "{recollected}");
    assert!(
        recollected.contains("trace_coverage=Unavailable"),
        "{recollected}"
    );
    let store = Store::open(&store_path).expect("reopen collected store");
    assert!(store
        .list_trace_edits()
        .expect("reconciled traces")
        .is_empty());
    assert!(store
        .list_code_change_metrics(false)
        .expect("reconciled metrics")
        .is_empty());
}

#[test]
fn conversation_collect_keeps_imported_state_when_the_archive_root_is_unavailable() {
    let dir = tempdir().expect("temp dir");
    let codex_home = dir.path().join("codex");
    let sessions = codex_home.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut fixture = File::create(sessions.join("thread.jsonl")).expect("fixture");
    for value in [
        serde_json::json!({
            "timestamp":"2026-08-01T10:00:00Z",
            "type":"session_meta",
            "payload":{"id":"thread-unavailable-root","cwd":dir.path()}
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
            "payload":{"type":"custom_tool_call_output","call_id":"call-1","output":"Done!"}
        }),
    ] {
        writeln!(fixture, "{value}").expect("write fixture");
    }
    drop(fixture);
    let store_path = dir.path().join("statsai.sqlite3");

    stdout(run_statsai(
        &store_path,
        &codex_home,
        &["conversation", "collect", "--provider", "codex"],
    ));
    let store = Store::open(&store_path).expect("open collected store");
    assert_eq!(store.list_trace_edits().expect("initial traces").len(), 1);
    drop(store);

    // An unmounted volume or a renamed home directory: the root the source was
    // configured with no longer resolves.
    std::fs::rename(&codex_home, dir.path().join("codex-moved")).expect("move archive root");
    stdout(run_statsai(
        &store_path,
        &codex_home,
        &["conversation", "collect", "--provider", "codex"],
    ));

    let store = Store::open(&store_path).expect("reopen collected store");
    assert_eq!(
        store.list_trace_edits().expect("retained traces").len(),
        1,
        "an unreachable archive root must not be read as a deletion"
    );
    assert!(!store
        .list_code_change_metrics(false)
        .expect("retained metrics")
        .is_empty());
}

#[test]
fn conversation_collect_retries_when_local_artifact_metadata_changes() {
    let dir = tempdir().expect("temp dir");
    let codex_home = dir.path().join("codex");
    let sessions = codex_home.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let artifact = dir.path().join("referenced-image.bin");
    let mut fixture = File::create(sessions.join("thread.jsonl")).expect("fixture");
    writeln!(
        fixture,
        "{}",
        serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "type": "session_meta",
            "payload": {"id": "thread-local-artifact"}
        })
    )
    .unwrap();
    writeln!(
        fixture,
        "{}",
        serde_json::json!({
            "timestamp": "2026-01-01T00:00:01Z",
            "type": "response_item",
            "payload": {
                "id": "m1",
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "image_url": artifact.to_str().expect("artifact path")
                }]
            }
        })
    )
    .unwrap();
    drop(fixture);
    let store = dir.path().join("statsai.sqlite3");

    let first = stdout(run_statsai(
        &store,
        &codex_home,
        &[
            "conversation",
            "collect",
            "--provider",
            "codex",
            "--verbose",
        ],
    ));
    assert!(first.contains("missing=1"), "{first}");

    let unchanged = stdout(run_statsai(
        &store,
        &codex_home,
        &[
            "conversation",
            "collect",
            "--provider",
            "codex",
            "--verbose",
        ],
    ));
    assert!(unchanged.contains("archive unchanged"), "{unchanged}");

    std::fs::write(&artifact, [0, 1, 2, 255]).expect("create artifact");
    let repaired = stdout(run_statsai(
        &store,
        &codex_home,
        &[
            "conversation",
            "collect",
            "--provider",
            "codex",
            "--verbose",
        ],
    ));
    assert!(repaired.contains("conversations=1"), "{repaired}");
    assert!(repaired.contains("binary_bytes=4"), "{repaired}");
    assert!(repaired.contains("missing=0"), "{repaired}");

    std::fs::write(&artifact, [0, 1, 2, 3, 255]).expect("modify artifact");
    let modified = stdout(run_statsai(
        &store,
        &codex_home,
        &[
            "conversation",
            "collect",
            "--provider",
            "codex",
            "--verbose",
        ],
    ));
    assert!(modified.contains("conversations=1"), "{modified}");
    assert!(modified.contains("binary_bytes=5"), "{modified}");

    let list: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["conversation", "list", "--provider", "codex", "--json"],
    )))
    .expect("list JSON");
    assert_eq!(list[0]["completeness"], "complete");
    assert_eq!(list[0]["missing_content_count"], 0);
}

#[test]
fn conversation_list_accepts_provider_aliases() {
    let dir = tempdir().expect("temp dir");
    let store_path = dir.path().join("statsai.sqlite3");
    let store = Store::open(&store_path).expect("store");
    let conversation = ArchiveConversation {
        schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
        conversation_id: archive_conversation_id("claude_code", "thread-alias"),
        provider: "claude_code".to_string(),
        source_id: SourceId("source-alias".to_string()),
        native_conversation_id: "thread-alias".to_string(),
        title: Some("Alias filter".to_string()),
        project: None,
        started_at: None,
        updated_at: None,
        completeness: ArchiveCompleteness::MetadataOnly,
        missing_content_count: 0,
        missing_content_scope_id: None,
        discarded_source_record_ids: Vec::new(),
        superseded_conversation_ids: Vec::new(),
        items: Vec::new(),
    };
    store
        .upsert_archive_conversations(&[conversation])
        .expect("archive conversation");
    drop(store);

    let list: Value = serde_json::from_str(&stdout(run_statsai(
        &store_path,
        &dir.path().join("codex"),
        &["conversation", "list", "--provider", "claude", "--json"],
    )))
    .expect("list JSON");
    assert_eq!(list.as_array().expect("list array").len(), 1);
    assert_eq!(list[0]["provider"], "claude_code");
}

#[test]
fn conversation_collect_skips_disabled_discovered_source() {
    let dir = tempdir().expect("temp dir");
    let codex_home = dir.path().join("codex");
    let sessions = codex_home.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut fixture = File::create(sessions.join("thread.jsonl")).expect("fixture");
    writeln!(
        fixture,
        r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{{"id":"thread-disabled"}}}}"#
    )
    .unwrap();
    writeln!(
        fixture,
        r#"{{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{{"id":"m1","type":"message","role":"user","content":[{{"type":"input_text","text":"do not archive"}}]}}}}"#
    )
    .unwrap();
    let store = dir.path().join("statsai.sqlite3");

    let added: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &[
            "source",
            "add",
            "--provider",
            "codex",
            "--path",
            codex_home.to_str().expect("codex path"),
        ],
    )))
    .expect("source JSON");
    let source_id = added["source_id"]["0"]
        .as_str()
        .or_else(|| added["source_id"].as_str())
        .expect("source id");
    stdout(run_statsai(
        &store,
        &codex_home,
        &["source", "disable", "--source-id", source_id],
    ));

    let collected = stdout(run_statsai(
        &store,
        &codex_home,
        &["conversation", "collect", "--provider", "codex"],
    ));
    assert!(collected.contains("sources=0"), "{collected}");
    assert!(collected.contains("conversations=0"), "{collected}");

    let listed: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["conversation", "list", "--json"],
    )))
    .expect("list JSON");
    assert!(listed.as_array().expect("list array").is_empty());
}
