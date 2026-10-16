//! A scan keeps the usage of a provider log that is compressed, moved or
//! deleted: the store is what keeps the history once the provider stops.

use serde_json::Value;
use statsai_store::Store;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::tempdir;

fn run_statsai(store: &Path, codex_home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_statsai"))
        .arg("--store")
        .arg(store)
        .arg("--device-id")
        .arg("retention-device")
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

/// A rollout with one usage record carrying a quota sample.
fn write_rollout(path: &Path, timestamp: &str, tokens: u64) {
    let mut file = File::create(path).expect("rollout");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": tokens,
                        "output_tokens": 5,
                        "total_tokens": tokens + 5
                    },
                    "total_token_usage": {
                        "input_tokens": tokens,
                        "output_tokens": 5,
                        "total_tokens": tokens + 5
                    }
                },
                "rate_limits": {
                    "plan_type": "pro",
                    "primary": {
                        "used_percent": 25,
                        "window_minutes": 10080,
                        "resets_at": 1787832000
                    }
                }
            }
        })
    )
    .expect("write rollout");
}

/// Replaces `path` with `path.zst`, as Codex does to older rollouts.
fn compress_in_place(path: &Path) -> PathBuf {
    let mut compressed = path.as_os_str().to_owned();
    compressed.push(".zst");
    let compressed = PathBuf::from(compressed);
    let plain = std::fs::read(path).expect("read rollout");
    std::fs::write(
        &compressed,
        ruzstd::encoding::compress_to_vec(
            plain.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        ),
    )
    .expect("write compressed rollout");
    std::fs::remove_file(path).expect("remove plain rollout");
    compressed
}

/// (usage events, quota observations) in `store`.
fn counts(store: &Path, codex_home: &Path) -> (u64, u64) {
    let events = Store::open(store)
        .expect("open store")
        .event_count()
        .expect("event count");
    let status: Value = serde_json::from_str(&stdout(run_statsai(
        store,
        codex_home,
        &["quota", "status", "--json"],
    )))
    .expect("quota status JSON");
    let observations = status["total_observations"]
        .as_u64()
        .expect("observation count");
    (events, observations)
}

#[test]
fn scans_keep_usage_of_compressed_moved_and_deleted_rollouts() {
    let directory = tempdir().expect("temp dir");
    let codex_home = directory.path().join("codex");
    let sessions = codex_home.join("sessions/2026/08/20");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let rollout_a = sessions.join("rollout-a.jsonl");
    let rollout_b = sessions.join("rollout-b.jsonl");
    write_rollout(&rollout_a, "2026-08-20T12:00:00Z", 10);
    write_rollout(&rollout_b, "2026-08-20T13:00:00Z", 20);
    let store = directory.path().join("statsai.sqlite3");
    let scan = |args: &[&str]| {
        let mut command = vec!["scan", "--provider", "codex"];
        command.extend_from_slice(args);
        stdout(run_statsai(&store, &codex_home, &command))
    };

    scan(&[]);
    assert_eq!(counts(&store, &codex_home), (2, 2), "first scan");

    // Compressed in place: the same file, read again from the `.zst`.
    let compressed_a = compress_in_place(&rollout_a);
    let rescan = scan(&[]);
    assert!(rescan.contains("usage_events=1"), "{rescan}");
    assert_eq!(counts(&store, &codex_home), (2, 2), "after compression");

    // Archived: moved, so its rows are replaced by the ones read at the new
    // path rather than kept under both.
    let archived = codex_home.join("archived_sessions");
    std::fs::create_dir_all(&archived).expect("archived sessions");
    std::fs::rename(&compressed_a, archived.join("rollout-a.jsonl.zst")).expect("archive");
    scan(&[]);
    assert_eq!(counts(&store, &codex_home), (2, 2), "after archiving");

    // Deleted: its usage stays.
    std::fs::remove_file(&rollout_b).expect("delete rollout");
    scan(&[]);
    assert_eq!(counts(&store, &codex_home), (2, 2), "after deletion");

    // A full reread replaces what it reads, and keeps the rest.
    scan(&["--no-cache"]);
    assert_eq!(counts(&store, &codex_home), (2, 2), "after --no-cache");

    // A new store sees only what is on disk: the archived, compressed rollout.
    let fresh = directory.path().join("fresh.sqlite3");
    stdout(run_statsai(
        &fresh,
        &codex_home,
        &["scan", "--provider", "codex"],
    ));
    assert_eq!(counts(&fresh, &codex_home), (1, 1), "fresh store");

    // `--replace` is the one destructive rebuild.
    scan(&["--replace"]);
    assert_eq!(counts(&store, &codex_home), (1, 1), "after --replace");
}
