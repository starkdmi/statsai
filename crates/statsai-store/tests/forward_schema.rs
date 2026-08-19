use rusqlite::Connection;
use statsai_store::{Store, CURRENT_SCHEMA_VERSION};

#[test]
fn store_refuses_to_open_database_from_a_newer_binary() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("future.sqlite");
    let connection = Connection::open(&path).expect("create future database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE schema_migrations (
              version INTEGER PRIMARY KEY,
              applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations (version, applied_at)
            VALUES (18, '2026-08-19T00:00:00Z');
            "#,
        )
        .expect("record future schema");
    drop(connection);

    let error = match Store::open(&path) {
        Ok(_) => panic!("schema 17 binary must reject schema 18 database"),
        Err(error) => error,
    };

    assert_eq!(CURRENT_SCHEMA_VERSION, 17);
    assert!(error
        .to_string()
        .contains("database schema version 18 is newer than this StatsAI binary supports (17)"));
    let connection = Connection::open(&path).expect("reopen rejected database");
    assert_eq!(
        connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("read future schema marker"),
        18
    );
}
