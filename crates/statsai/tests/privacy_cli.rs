use std::path::Path;
use std::process::{Command, Output};

use chrono::{TimeZone, Utc};
use serde_json::{json, Value};
use statsai::privacy::{inspect_runtime, pseudonym_key_verifier, save_runtime};
use statsai_core::{
    ArchiveCompleteness, ArchiveContentKind, ArchiveContentPart, ArchiveConversation, ArchiveItem,
    ArchiveItemKind, ArchiveRole, SourceId, ARCHIVE_CONVERSATION_SCHEMA_VERSION,
};
use statsai_privacy::{
    archive_privacy_input_fingerprint, privacy_policy_fingerprint, DeterministicDetector,
    FilteredConversation, KingfisherDetector, MlxDetector, PrivacyDetector,
    FILTERED_CONVERSATION_SCHEMA_VERSION,
};
use statsai_store::{FilteredConversationRecord, Store};

fn run_statsai(store: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_statsai"))
        .arg("--store")
        .arg(store)
        .args(args)
        .env("STATSAI_DEVICE_ID", "privacy-cli-test")
        .output()
        .expect("run statsai")
}

fn scoped_privacy_key_path(directory: &Path) -> std::path::PathBuf {
    let paths = std::fs::read_dir(directory)
        .expect("read privacy directory")
        .map(|entry| entry.expect("privacy directory entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("privacy-pseudonym-") && name.ends_with(".key")
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(paths.len(), 1, "expected one store-scoped privacy key");
    paths.into_iter().next().expect("scoped privacy key")
}

fn archive(id: &str, day: u32) -> ArchiveConversation {
    let timestamp = Utc
        .with_ymd_and_hms(2026, 1, day, 12, 34, 56)
        .single()
        .expect("timestamp");
    ArchiveConversation {
        schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
        conversation_id: id.to_string(),
        provider: "codex".to_string(),
        source_id: SourceId(format!("source-{id}")),
        native_conversation_id: format!("native-{id}"),
        title: Some("non-private title".to_string()),
        project: None,
        started_at: Some(timestamp),
        updated_at: None,
        completeness: ArchiveCompleteness::Complete,
        missing_content_count: 0,
        missing_content_scope_id: None,
        discarded_source_record_ids: Vec::new(),
        superseded_conversation_ids: Vec::new(),
        items: vec![ArchiveItem {
            item_id: format!("item-{id}"),
            native_item_id: Some(format!("native-item-{id}")),
            source_record_id: Some(format!("record-{id}")),
            ordinal: 0,
            kind: ArchiveItemKind::Message,
            role: Some(ArchiveRole::User),
            created_at: Some(timestamp),
            model: None,
            tool_name: None,
            tool_call_id: None,
            status: None,
            usage: None,
            parts_authoritative: true,
            parts: vec![ArchiveContentPart::text(
                format!("content-{id}"),
                0,
                ArchiveContentKind::Text,
                "non-private body".to_string(),
            )],
        }],
    }
}

#[test]
fn privacy_cli_exports_current_rows_deterministically_and_fails_closed_when_stale() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store_path = directory.path().join("statsai.sqlite");
    let export_path = directory.path().join("filtered.jsonl");
    let mlx_server = directory.path().join("opf-mlx-server");
    let mlx_model = directory.path().join("model.mlxfn");
    let calibration = directory.path().join("viterbi_calibration.json");
    let kingfisher = directory.path().join("statsai-kingfisher");
    for (path, contents) in [
        (&mlx_server, "server"),
        (&mlx_model, "model"),
        (&calibration, "calibration"),
        (&kingfisher, "kingfisher"),
    ] {
        std::fs::write(path, contents).expect("write runtime fixture");
    }
    let config = inspect_runtime(&mlx_server, &mlx_model, &kingfisher).expect("inspect runtime");
    save_runtime(&store_path, &config).expect("save runtime");

    let key = [9u8; 32];
    let legacy_key_path = directory.path().join("privacy-pseudonym.key");
    std::fs::write(&legacy_key_path, key).expect("write pseudonym key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&legacy_key_path, std::fs::Permissions::from_mode(0o600))
            .expect("private pseudonym key");
    }
    let mut kingfisher_metadata = KingfisherDetector::qualified_metadata();
    kingfisher_metadata.implementation_version = format!(
        "{}+binary.{}",
        kingfisher_metadata.implementation_version, config.kingfisher_sha256
    );
    let metadata = vec![
        MlxDetector::metadata_for_revision(config.model_revision()),
        kingfisher_metadata,
        DeterministicDetector::default().metadata(),
    ];
    let policy_fingerprint = privacy_policy_fingerprint(&metadata);

    let first = archive("raw-first-id", 2);
    let second = archive("raw-second-id", 1);
    let store = Store::open(&store_path).expect("store");
    store
        .ensure_privacy_key_verifier(&pseudonym_key_verifier(&key))
        .expect("initialize privacy key verifier");
    store
        .upsert_archive_conversations(&[first.clone(), second.clone()])
        .expect("archive conversations");
    for (conversation, dataset_key) in [(&first, "dataset_z"), (&second, "dataset_a")] {
        let filtered = FilteredConversation {
            schema_version: FILTERED_CONVERSATION_SCHEMA_VERSION.to_string(),
            dataset_key: dataset_key.to_string(),
            provider: "codex".to_string(),
            day: conversation
                .started_at
                .map(|timestamp| timestamp.format("%Y-%m-%d").to_string()),
            title: Some("filtered title".to_string()),
            project: None,
            items: Vec::new(),
        };
        store
            .write_filtered_conversation(
                &FilteredConversationRecord {
                    conversation_id: conversation.conversation_id.clone(),
                    dataset_key: dataset_key.to_string(),
                    input_fingerprint: archive_privacy_input_fingerprint(conversation)
                        .expect("input fingerprint"),
                    policy_fingerprint: policy_fingerprint.clone(),
                    payload: serde_json::to_string(&filtered).expect("filtered payload"),
                    finding_count: 0,
                    succeeded_at: Utc::now(),
                },
                &[],
            )
            .expect("write filtered conversation");
    }
    assert_eq!(
        store
            .archive_conversation_for_privacy(&first.conversation_id)
            .expect("read first archive")
            .expect("first archive exists")
            .completeness,
        ArchiveCompleteness::Complete
    );
    drop(store);

    let status = run_statsai(&store_path, &["privacy", "status", "--json"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status["dataset"]["current"], 2);
    assert_eq!(status["dataset"]["stale"], 0);
    let key_path = scoped_privacy_key_path(directory.path());

    let output = run_statsai(
        &store_path,
        &[
            "privacy",
            "export",
            "--output",
            export_path.to_str().expect("export path"),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let exported = std::fs::read_to_string(&export_path).expect("exported JSONL");
    let lines = exported.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 3);
    let manifest: Value = serde_json::from_str(lines[0]).expect("manifest");
    let first_payload: Value = serde_json::from_str(lines[1]).expect("first payload");
    let second_payload: Value = serde_json::from_str(lines[2]).expect("second payload");
    assert_eq!(manifest["conversations"], 2);
    assert_eq!(first_payload["dataset_key"], "dataset_a");
    assert_eq!(second_payload["dataset_key"], "dataset_z");
    for forbidden in [
        "raw-first-id",
        "raw-second-id",
        "native-raw-first-id",
        mlx_server.to_str().expect("server path"),
    ] {
        assert!(!exported.contains(forbidden), "export contains {forbidden}");
    }

    let before_wrong_key_export = std::fs::read(&export_path).expect("read valid export");
    std::fs::write(&key_path, [8u8; 32]).expect("replace pseudonym key fixture");
    let wrong_key_status = run_statsai(&store_path, &["privacy", "status", "--json"]);
    assert!(!wrong_key_status.status.success());
    assert!(String::from_utf8_lossy(&wrong_key_status.stderr).contains("does not match"));
    let wrong_key = run_statsai(
        &store_path,
        &[
            "privacy",
            "export",
            "--output",
            export_path.to_str().expect("export path"),
        ],
    );
    assert!(!wrong_key.status.success());
    assert!(String::from_utf8_lossy(&wrong_key.stderr).contains("does not match"));
    assert_eq!(
        std::fs::read(&export_path).expect("preserved export after wrong key"),
        before_wrong_key_export
    );
    std::fs::write(&key_path, key).expect("restore pseudonym key fixture");

    let unchanged = run_statsai(
        &store_path,
        &["privacy", "filter", "--conversation", "raw-first-id"],
    );
    assert!(
        unchanged.status.success(),
        "{}",
        String::from_utf8_lossy(&unchanged.stderr)
    );
    assert!(String::from_utf8_lossy(&unchanged.stdout).contains("unchanged=1"));

    let startup_failed = run_statsai(
        &store_path,
        &[
            "privacy",
            "filter",
            "--conversation",
            "raw-first-id",
            "--force",
        ],
    );
    assert!(!startup_failed.status.success());
    assert!(String::from_utf8_lossy(&startup_failed.stdout).contains("failed=1"));
    let store = Store::open(&store_path).expect("store after startup failure");
    let mut first_record = store
        .filtered_conversation("raw-first-id")
        .expect("read first filtered row")
        .expect("first filtered row exists");
    let second_record = store
        .filtered_conversation("raw-second-id")
        .expect("read second filtered row")
        .expect("second filtered row exists");
    assert!(store
        .filtered_conversation_has_newer_failure("raw-first-id", first_record.succeeded_at)
        .expect("first startup failure"));
    assert!(!store
        .filtered_conversation_has_newer_failure("raw-second-id", second_record.succeeded_at)
        .expect("second remains current"));
    let startup_export = run_statsai(
        &store_path,
        &[
            "privacy",
            "export",
            "--output",
            export_path.to_str().expect("export path"),
        ],
    );
    assert!(!startup_export.status.success());
    assert!(String::from_utf8_lossy(&startup_export.stderr).contains("newer failed attempt"));
    assert_eq!(
        std::fs::read(&export_path).expect("preserved export after startup failure"),
        before_wrong_key_export
    );
    first_record.succeeded_at = Utc::now();
    store
        .write_filtered_conversation(&first_record, &[])
        .expect("restore first filtered row after startup failure");
    drop(store);

    let store = Store::open(&store_path).expect("reopen store");
    let mut stale = first;
    stale.title = Some("archive changed".to_string());
    store
        .upsert_archive_conversations(std::slice::from_ref(&stale))
        .expect("update archive");
    drop(store);
    let before_failed_export = std::fs::read(&export_path).expect("read valid export");
    let failed = run_statsai(
        &store_path,
        &[
            "privacy",
            "export",
            "--output",
            export_path.to_str().expect("export path"),
        ],
    );
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("stale"));
    assert_eq!(
        std::fs::read(&export_path).expect("preserved export"),
        before_failed_export
    );

    let store = Store::open(&store_path).expect("reopen store after stale export");
    let mut refreshed = store
        .filtered_conversation(&stale.conversation_id)
        .expect("read stale filtered row")
        .expect("stale filtered row exists");
    refreshed.input_fingerprint =
        archive_privacy_input_fingerprint(&stale).expect("updated input fingerprint");
    refreshed.succeeded_at = Utc::now();
    store
        .write_filtered_conversation(&refreshed, &[])
        .expect("refresh filtered row");
    let mut partial = second.clone();
    partial.completeness = ArchiveCompleteness::Partial;
    partial.missing_content_count = 1;
    partial.missing_content_scope_id = Some("partial-regression".to_string());
    store
        .upsert_archive_conversations(&[partial])
        .expect("mark archive partial");
    drop(store);

    let status = run_statsai(&store_path, &["privacy", "status", "--json"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).expect("partial status JSON");
    assert_eq!(status["dataset"]["current"], 1);
    assert_eq!(status["dataset"]["stale"], 1);

    let failed = run_statsai(
        &store_path,
        &[
            "privacy",
            "export",
            "--output",
            export_path.to_str().expect("export path"),
        ],
    );
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("partial"));
    assert_eq!(
        std::fs::read(&export_path).expect("preserved export after partial archive"),
        before_failed_export
    );

    let shown = run_statsai(&store_path, &["privacy", "show", "raw-second-id", "--json"]);
    assert!(shown.status.success());
    let shown: Value = serde_json::from_slice(&shown.stdout).expect("shown JSON");
    assert_eq!(shown["dataset_key"], json!("dataset_a"));
}
