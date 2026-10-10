//! `statsai scan` and `statsai sync` wait for a running scanner, then skip
//! without failing.

use statsai_store::{Store, SyncPreferences};
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

fn run_statsai(home: &Path, store: &Path, args: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_statsai"))
        .arg("--store")
        .arg(store)
        .args(args)
        // Nothing to collect, and nothing outside the test directory to find.
        .env("HOME", home)
        .env("CODEX_HOME", home.join("codex"))
        .env("STATSAI_DEVICE_ID", "scan-lock-cli-test")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run statsai")
}

fn finish(child: std::process::Child) -> (Output, String) {
    let output = child.wait_with_output().expect("statsai output");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "statsai failed: {stderr}");
    (output, stderr)
}

#[test]
fn scan_and_sync_skip_cleanly_while_another_scan_holds_the_lock() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = directory.path().join("data").join("statsai.sqlite");
    let lock = statsai::scan_lock_path(&store);
    let _other_scanner = statsai::try_acquire_scan_lock(&lock)
        .expect("acquire")
        .expect("uncontended lock");

    // Both wait out the same few seconds, so run them side by side.
    let scan = run_statsai(directory.path(), &store, &["scan", "--provider", "codex"]);
    let batch = directory.path().join("batch.json");
    let sync = run_statsai(
        directory.path(),
        &store,
        &[
            "sync",
            "--sink",
            "file",
            "--output",
            batch.to_str().expect("utf-8 path"),
        ],
    );
    let busy = format!(
        "Another statsai scan is running (lock: {});",
        lock.display()
    );
    for (name, child, expected) in [
        ("scan", scan, format!("{busy} skipping.")),
        (
            "sync",
            sync,
            format!("{busy} skipping this sync. Nothing was sent."),
        ),
    ] {
        let (output, stderr) = finish(child);
        assert!(
            stderr.contains(&expected),
            "{name} did not report the busy lock: {stderr}"
        );
        assert!(output.stdout.is_empty(), "{name} wrote output");
    }
    assert!(
        !store.exists(),
        "a skipped command must not have opened (and created) the store"
    );
    assert!(!batch.exists(), "a skipped sync must not have sent a batch");
}

#[test]
fn an_invalid_sync_is_an_error_even_while_another_scan_holds_the_lock() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = directory.path().join("statsai.sqlite");
    let _other_scanner = statsai::try_acquire_scan_lock(&statsai::scan_lock_path(&store))
        .expect("acquire")
        .expect("uncontended lock");

    let output = run_statsai(
        directory.path(),
        &store,
        &["sync", "--since-last", "--full"],
    )
    .wait_with_output()
    .expect("statsai output");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "an invalid sync succeeded: {stderr}"
    );
    assert!(
        stderr.contains("--since-last cannot be combined with --full or --rebuild-rollups"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Another statsai scan is running"),
        "{stderr}"
    );
}

#[test]
fn a_skipped_sync_still_records_a_preference_opt_out() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store_path = directory.path().join("statsai.sqlite");
    Store::open(&store_path)
        .expect("create store")
        .set_sync_preferences(SyncPreferences {
            include_projects: true,
            ..SyncPreferences::default()
        })
        .expect("opt in to project sync");
    let lock = statsai::scan_lock_path(&store_path);
    let _other_scanner = statsai::try_acquire_scan_lock(&lock)
        .expect("acquire")
        .expect("uncontended lock");

    let sync = run_statsai(
        directory.path(),
        &store_path,
        &["sync", "--sink", "stdout", "--exclude-projects"],
    );
    let (output, stderr) = finish(sync);

    assert!(
        stderr.contains(&format!(
            "Another statsai scan is running (lock: {}); skipping this sync. Nothing was sent.",
            lock.display()
        )),
        "{stderr}"
    );
    assert!(output.stdout.is_empty(), "a skipped sync sent a batch");
    let preferences = Store::open(&store_path)
        .expect("reopen store")
        .sync_preferences()
        .expect("sync preferences");
    assert!(
        !preferences.include_projects,
        "the --exclude-projects opt-out was lost: {preferences:?}"
    );
}

#[test]
fn scan_waits_for_a_short_scan_to_finish_and_then_runs() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = directory.path().join("statsai.sqlite");
    let lock = statsai::scan_lock_path(&store);
    let other_scanner = statsai::try_acquire_scan_lock(&lock)
        .expect("acquire")
        .expect("uncontended lock");

    let scan = run_statsai(directory.path(), &store, &["scan", "--provider", "codex"]);
    std::thread::sleep(Duration::from_millis(500));
    drop(other_scanner);
    let (_, stderr) = finish(scan);

    assert!(
        !stderr.contains("Another statsai scan is running"),
        "scan skipped instead of waiting: {stderr}"
    );
    assert!(store.exists(), "the scan did not run");
    // The scan released the lock when it exited.
    assert!(statsai::try_acquire_scan_lock(&lock)
        .expect("try")
        .is_some());
}
