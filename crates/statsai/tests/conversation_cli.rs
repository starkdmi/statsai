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
