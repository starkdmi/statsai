use rusqlite::Connection;
use statsai_store::{Store, CURRENT_SCHEMA_VERSION};

#[test]
fn store_refuses_to_open_database_from_a_newer_binary() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("future.sqlite");
    let connection = Connection::open(&path).expect("create future database");
    let future_schema_version = CURRENT_SCHEMA_VERSION + 1;
    connection
        .execute_batch(
            r#"
            CREATE TABLE schema_migrations (
              version INTEGER PRIMARY KEY,
              applied_at TEXT NOT NULL
            );
            "#,
        )
        .expect("create migration table");
    connection
        .execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            (future_schema_version, "2026-08-23T00:00:00Z"),
        )
        .expect("record future schema");
    drop(connection);

    let error = match Store::open(&path) {
        Ok(_) => panic!(
            "schema {CURRENT_SCHEMA_VERSION} binary must reject schema {future_schema_version} database"
        ),
        Err(error) => error,
    };

    assert!(error
        .to_string()
        .contains(&format!(
            "database schema version {future_schema_version} is newer than this StatsAI binary supports ({CURRENT_SCHEMA_VERSION})"
        )));
    let connection = Connection::open(&path).expect("reopen rejected database");
    assert_eq!(
        connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("read future schema marker"),
        future_schema_version
    );
}
