use super::*;

/// Importing an archive may trade durability for speed because a lost
/// commit is simply collected again. The rest of the store holds work that
/// no local file can reproduce, so the trade must not outlive the import.
#[test]
fn relaxed_durability_is_scoped_to_the_bulk_import() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("statsai.sqlite")).expect("store");
    let durability = || {
        store
            .conn
            .query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
            .expect("read durability")
    };

    let opened_with = durability();
    assert_ne!(
        opened_with, 0,
        "a store must never open with durability disabled"
    );

    {
        let _relaxed = store
            .relax_durability_for_bulk_import()
            .expect("relax durability");
        assert_eq!(durability(), 1, "the import did not get NORMAL durability");
    }

    assert_eq!(
        durability(),
        opened_with,
        "durability stayed relaxed after the import"
    );
}

#[test]
#[cfg(unix)]
fn open_restricts_store_directory_and_database_permissions() {
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    let dir = tempdir().expect("tempdir");
    let store_dir = dir.path().join(".statsai");
    let db_path = store_dir.join("statsai.sqlite");

    let store = Store::open(&db_path).expect("open store");
    drop(store);

    let dir_mode = std::fs::metadata(&store_dir)
        .expect("dir metadata")
        .permissions()
        .mode()
        & 0o777;
    let file_mode = std::fs::metadata(&db_path)
        .expect("file metadata")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(dir_mode, 0o700);
    assert_eq!(file_mode, 0o600);
}

#[test]
#[cfg(unix)]
fn open_preserves_existing_parent_directory_permissions() {
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    let dir = tempdir().expect("tempdir");
    let shared_dir = dir.path().join("shared");
    std::fs::create_dir(&shared_dir).expect("create shared dir");
    std::fs::set_permissions(&shared_dir, std::fs::Permissions::from_mode(0o750))
        .expect("set shared dir mode");

    let store = Store::open(&shared_dir.join("statsai.sqlite")).expect("open store");
    drop(store);

    let mode = std::fs::metadata(&shared_dir)
        .expect("shared dir metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o750);
}

#[test]
fn reopen_uses_an_independent_connection_to_the_same_file_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("statsai.sqlite");
    let store = Store::open(&db_path).expect("open store");
    store
        .conn
        .execute(
            "INSERT INTO local_metadata (key, value, updated_at)
                 VALUES ('reopen-test', 'visible', '2026-07-25T00:00:00Z')",
            [],
        )
        .expect("insert metadata");

    let reopened = store.reopen().expect("reopen file store");
    let value = reopened
        .conn
        .query_row(
            "SELECT value FROM local_metadata WHERE key = 'reopen-test'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read metadata from independent connection");

    assert_eq!(value, "visible");
}

#[test]
fn reopen_rejects_an_in_memory_store() {
    let store = Store::in_memory().expect("store");

    let error = store
        .reopen()
        .err()
        .expect("in-memory store cannot be reopened");

    assert!(error.to_string().contains("in-memory"));
}
