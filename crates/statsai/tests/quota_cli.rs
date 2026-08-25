use serde_json::Value;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

fn run_statsai(store: &Path, codex_home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_statsai"))
        .arg("--store")
        .arg(store)
        .arg("--device-id")
        .arg("quota-device")
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
fn quota_cli_scans_attributes_reconstructs_and_exports_sync_projection() {
    let directory = tempdir().expect("temp dir");
    let codex_home = directory.path().join("codex");
    let sessions = codex_home.join("sessions/2026/08/20");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut fixture = File::create(sessions.join("thread.jsonl")).expect("fixture");
    writeln!(
        fixture,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:00Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": 10,
                        "output_tokens": 5,
                        "total_tokens": 15
                    },
                    "total_token_usage": {
                        "input_tokens": 10,
                        "output_tokens": 5,
                        "total_tokens": 15
                    }
                },
                "rate_limits": {
                    "limit_id": "weekly,\"special\"",
                    "plan_type": "pro",
                    "primary": {
                        "used_percent": 25,
                        "window_minutes": 10080,
                        "resets_at": 1787832000
                    },
                    "secondary": {
                        "used_percent": 5,
                        "window_minutes": 300,
                        "resets_at": 1787245200
                    },
                    "credits": {"balance": "10.50"}
                }
            }
        })
    )
    .expect("write fixture");
    writeln!(
        fixture,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-20T12:01:00Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "limit_id": "weekly,\"special\"",
                    "plan_type": "pro",
                    "primary": {
                        "used_percent": 6,
                        "window_minutes": 300,
                        "resets_at": 1787245320
                    }
                }
            }
        })
    )
    .expect("write current single-window fixture");
    drop(fixture);
    let store = directory.path().join("statsai.sqlite3");

    let scan = stdout(run_statsai(
        &store,
        &codex_home,
        &["scan", "--provider", "codex"],
    ));
    assert!(scan.contains("quota_observations=2"), "{scan}");

    let status: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["quota", "status", "--json"],
    )))
    .expect("status JSON");
    assert_eq!(status["total_observations"], 2);
    assert_eq!(status["unattributed_observations"], 2);

    let history: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["quota", "history", "--raw", "--json"],
    )))
    .expect("raw history JSON");
    assert_eq!(
        history["observations"][0]["observation"]["usage_link_kind"],
        "record_event"
    );
    assert!(history["observations"][0]["observation"]["usage_event_id"].is_string());

    let windows: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["quota", "windows", "--json"],
    )))
    .expect("windows JSON");
    assert_eq!(windows.as_array().expect("windows").len(), 2);
    assert!(windows
        .as_array()
        .expect("windows")
        .iter()
        .all(|window| window["usage_totals"].is_null()));
    let filtered_windows: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &[
            "quota",
            "windows",
            "--from",
            "2026-08-20T12:01:00Z",
            "--json",
        ],
    )))
    .expect("filtered windows JSON");
    assert_eq!(
        filtered_windows.as_array().expect("filtered windows").len(),
        1
    );
    let filtered_window_id = filtered_windows[0]["window_id"]
        .as_str()
        .expect("filtered window id");
    let full_window_id = windows
        .as_array()
        .expect("windows")
        .iter()
        .find(|window| window["window_minutes"] == 300)
        .and_then(|window| window["window_id"].as_str())
        .expect("full window id");
    assert_eq!(filtered_window_id, full_window_id);
    let filtered_history: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &[
            "quota",
            "history",
            "--window-id",
            filtered_window_id,
            "--json",
        ],
    )))
    .expect("filtered window history JSON");
    assert_eq!(filtered_history["window_id"], filtered_window_id);
    let human_windows = stdout(run_statsai(&store, &codex_home, &["quota", "windows"]));
    assert!(
        human_windows.contains("usage unavailable"),
        "{human_windows}"
    );

    let no_windows: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["quota", "windows", "--limit", "0", "--json"],
    )))
    .expect("zero-limit windows JSON");
    assert_eq!(no_windows, serde_json::json!([]));

    let current: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["quota", "current", "--json"],
    )))
    .expect("current JSON");
    assert_eq!(current.as_array().expect("current").len(), 1);
    assert_eq!(current[0]["window_minutes"], 10_080);

    stdout(run_statsai(
        &store,
        &codex_home,
        &[
            "source",
            "connect",
            "--path",
            codex_home.to_str().expect("path"),
            "--provider-user-id",
            "account-cli",
            "--started-at",
            "2026-08-01",
        ],
    ));

    let attributed_windows: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["quota", "windows", "--json"],
    )))
    .expect("attributed windows JSON");
    let weekly_window = attributed_windows
        .as_array()
        .expect("attributed windows")
        .iter()
        .find(|window| window["window_minutes"] == 10_080)
        .expect("weekly window");
    assert_eq!(weekly_window["usage_totals"]["total_tokens"], 15);

    let projections: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &[
            "quota",
            "export",
            "--level",
            "sync-windows",
            "--format",
            "json",
        ],
    )))
    .expect("projection JSON");
    assert_eq!(projections.as_array().expect("projections").len(), 1);
    assert_eq!(projections[0]["device_id"], "quota-device");
    assert!(projections[0].get("source_id").is_none());
    assert!(projections[0].get("total_tokens").is_none());

    let csv = stdout(run_statsai(
        &store,
        &codex_home,
        &["quota", "export", "--level", "windows", "--format", "csv"],
    ));
    let mut reader = csv::Reader::from_reader(csv.as_bytes());
    let rows = reader
        .records()
        .collect::<csv::Result<Vec<_>>>()
        .expect("standards-compliant CSV");
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|row| row.get(5) == Some("weekly,\"special\"")));
}

#[test]
fn quota_projection_schema_is_available_without_opening_a_store() {
    let directory = tempdir().expect("temp dir");
    let missing_store = directory.path().join("missing/store.sqlite3");
    let schema: Value = serde_json::from_str(&stdout(run_statsai(
        &missing_store,
        directory.path(),
        &["schema", "quota-window-projection"],
    )))
    .expect("schema JSON");
    assert_eq!(schema["properties"]["schema_version"]["type"], "string");
    assert!(!missing_store.exists());
}

#[test]
fn scan_preview_reports_quota_only_sources_and_totals() {
    let directory = tempdir().expect("temp dir");
    let codex_home = directory.path().join("codex");
    let sessions = codex_home.join("sessions/2026/08/20");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let mut fixture = File::create(sessions.join("quota-only.jsonl")).expect("fixture");
    writeln!(
        fixture,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:00Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "primary": {
                        "used_percent": 25,
                        "window_minutes": 10080,
                        "resets_at": 1787832000
                    }
                }
            }
        })
    )
    .expect("write fixture");

    let preview = stdout(run_statsai(
        &directory.path().join("statsai.sqlite3"),
        &codex_home,
        &["scan", "--provider", "codex", "--preview"],
    ));

    assert!(
        preview.contains("usage_events=0 summaries=0 task_spans=0 quota_observations=1"),
        "{preview}"
    );
    assert!(
        preview
            .contains("preview total: sources=1 usage_events=0 summaries=0 quota_observations=1"),
        "{preview}"
    );
}

#[test]
fn conversation_archive_backfills_and_reconciles_quota_observations() {
    let directory = tempdir().expect("temp dir");
    let codex_home = directory.path().join("codex");
    let sessions = codex_home.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let path = sessions.join("archived-thread.jsonl");
    let mut fixture = File::create(&path).expect("fixture");
    writeln!(
        fixture,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:00Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "primary": {
                        "used_percent": 25,
                        "window_minutes": 10080,
                        "resets_at": 1787832000
                    }
                }
            }
        })
    )
    .expect("write fixture");
    writeln!(
        fixture,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-20T12:05:00Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "primary": {
                        "used_percent": 30,
                        "window_minutes": 10080,
                        "resets_at": 1787832000
                    }
                }
            }
        })
    )
    .expect("write second fixture record");
    drop(fixture);
    let store = directory.path().join("statsai.sqlite3");

    stdout(run_statsai(
        &store,
        &codex_home,
        &["conversation", "collect", "--provider", "codex"],
    ));
    let status: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["quota", "status", "--json"],
    )))
    .expect("status JSON");
    assert_eq!(status["total_observations"], 2);

    let mut fixture = File::create(&path).expect("replace fixture");
    writeln!(
        fixture,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:00Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "primary": {
                        "used_percent": 25,
                        "window_minutes": 10080,
                        "resets_at": 1787832000
                    }
                }
            }
        })
    )
    .expect("write shortened fixture");
    drop(fixture);
    stdout(run_statsai(
        &store,
        &codex_home,
        &["conversation", "collect", "--provider", "codex"],
    ));
    let status: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["quota", "status", "--json"],
    )))
    .expect("shortened status JSON");
    assert_eq!(status["total_observations"], 1);

    std::fs::remove_file(path).expect("remove archive fixture");
    stdout(run_statsai(
        &store,
        &codex_home,
        &["conversation", "collect", "--provider", "codex"],
    ));
    let status: Value = serde_json::from_str(&stdout(run_statsai(
        &store,
        &codex_home,
        &["quota", "status", "--json"],
    )))
    .expect("reconciled status JSON");
    assert_eq!(status["total_observations"], 0);
}
