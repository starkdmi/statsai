#![cfg(target_os = "macos")]

use rusqlite::Connection;
use statsai_store::{clone_database_to, database_schema_version, Store, CURRENT_SCHEMA_VERSION};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn clone_contains_committed_wal_data_and_is_independent_after_writes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let source = directory.path().join("production.sqlite");
    let destination = directory.path().join("development.sqlite");
    drop(Store::open(&source).expect("initialize source store"));
    let source_connection = Connection::open(&source).expect("open source connection");
    source_connection
        .execute_batch(
            r#"
            PRAGMA wal_autocheckpoint = 0;
            CREATE TABLE clone_probe (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO clone_probe (value) VALUES ('committed-in-wal');
            "#,
        )
        .expect("write source WAL data");

    let cloned = clone_database_to(&source, &destination).expect("clone live WAL database");

    assert_eq!(cloned.schema_version, CURRENT_SCHEMA_VERSION);
    assert_eq!(
        database_schema_version(&destination).expect("read clone schema"),
        Some(CURRENT_SCHEMA_VERSION)
    );
    let clone = Connection::open(&destination).expect("open cloned database");
    assert_eq!(
        clone
            .query_row("SELECT value FROM clone_probe", [], |row| {
                row.get::<_, String>(0)
            })
            .expect("read cloned row"),
        "committed-in-wal"
    );
    assert_eq!(
        clone
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .expect("check cloned database"),
        "ok"
    );

    source_connection
        .execute("UPDATE clone_probe SET value = 'production-only'", [])
        .expect("update production after clone");
    clone
        .execute("UPDATE clone_probe SET value = 'development-only'", [])
        .expect("update development after clone");
    assert_eq!(
        source_connection
            .query_row("SELECT value FROM clone_probe", [], |row| {
                row.get::<_, String>(0)
            })
            .expect("read production row"),
        "production-only"
    );
    assert_eq!(
        clone
            .query_row("SELECT value FROM clone_probe", [], |row| {
                row.get::<_, String>(0)
            })
            .expect("read development row"),
        "development-only"
    );
}

#[test]
fn clone_waits_for_an_active_writer_and_captures_its_commit() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let source = directory.path().join("production.sqlite");
    let destination = directory.path().join("development.sqlite");
    drop(Store::open(&source).expect("initialize source store"));
    let source_connection = Connection::open(&source).expect("open source connection");
    source_connection
        .execute("CREATE TABLE clone_probe (value TEXT NOT NULL)", [])
        .expect("create probe table");
    source_connection
        .execute_batch("BEGIN IMMEDIATE; INSERT INTO clone_probe VALUES ('committed');")
        .expect("hold source write transaction");

    let (started_tx, started_rx) = mpsc::channel();
    let clone_source = source.clone();
    let clone_destination = destination.clone();
    let worker = std::thread::spawn(move || {
        started_tx.send(()).expect("signal clone start");
        clone_database_to(clone_source, clone_destination)
    });
    started_rx.recv().expect("wait for clone start");
    std::thread::sleep(Duration::from_millis(100));
    source_connection
        .execute_batch("COMMIT")
        .expect("commit held source transaction");

    worker
        .join()
        .expect("join clone worker")
        .expect("clone after writer commit");
    let clone = Connection::open(&destination).expect("open cloned database");
    assert_eq!(
        clone
            .query_row("SELECT value FROM clone_probe", [], |row| {
                row.get::<_, String>(0)
            })
            .expect("read committed clone row"),
        "committed"
    );
}

#[test]
fn failed_publication_preserves_existing_destination_wal() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let source = directory.path().join("production.sqlite");
    let destination = directory.path().join("development.sqlite");
    drop(Store::open(&source).expect("initialize source store"));
    write_database_with_committed_wal(&destination);
    let destination_wal = path_with_suffix(&destination, "-wal");
    assert!(destination_wal.is_file());
    let immutable = ImmutableFile::new(&destination);

    let error = clone_database_to(&source, &destination)
        .expect_err("immutable destination must reject publication");
    drop(immutable);

    assert!(error
        .to_string()
        .contains("atomically replace database clone"));
    assert!(destination_wal.is_file());
    let connection = Connection::open(&destination).expect("reopen preserved destination");
    assert_eq!(
        connection
            .query_row("SELECT value FROM clone_probe", [], |row| {
                row.get::<_, String>(0)
            })
            .expect("read committed WAL row after failed publication"),
        "destination-committed-in-wal"
    );
}

fn write_database_with_committed_wal(destination: &Path) {
    let seed = destination.with_extension("seed.sqlite");
    let connection = Connection::open(&seed).expect("create WAL seed database");
    connection
        .execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA wal_autocheckpoint = 0;
            CREATE TABLE clone_probe (value TEXT NOT NULL);
            PRAGMA wal_checkpoint(TRUNCATE);
            INSERT INTO clone_probe VALUES ('destination-committed-in-wal');
            "#,
        )
        .expect("create committed WAL fixture");
    let seed_wal = path_with_suffix(&seed, "-wal");
    let database_bytes = fs::read(&seed).expect("read WAL fixture database");
    let wal_bytes = fs::read(&seed_wal).expect("read WAL fixture sidecar");
    assert!(wal_bytes.len() > 32, "WAL fixture must contain a frame");

    fs::write(destination, database_bytes).expect("write destination database fixture");
    fs::write(path_with_suffix(destination, "-wal"), wal_bytes)
        .expect("write destination WAL fixture");
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

struct ImmutableFile {
    path: PathBuf,
}

impl ImmutableFile {
    fn new(path: &Path) -> Self {
        set_immutable(path, true);
        Self {
            path: path.to_path_buf(),
        }
    }
}

impl Drop for ImmutableFile {
    fn drop(&mut self) {
        set_immutable(&self.path, false);
    }
}

fn set_immutable(path: &Path, immutable: bool) {
    let flag = if immutable { "uchg" } else { "nouchg" };
    let status = Command::new("chflags")
        .args([flag])
        .arg(path)
        .status()
        .expect("run chflags");
    assert!(status.success(), "chflags {flag} must succeed");
}
