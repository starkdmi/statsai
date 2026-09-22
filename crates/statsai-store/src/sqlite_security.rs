use super::*;
use statsai_core::{
    archive_content_id, archive_conversation_id, archive_item_id, ArchiveCompleteness,
    ArchiveContentKind, ArchiveContentPart, ArchiveConversation, ArchiveItem, ArchiveItemKind,
    ArchiveRole, SourceId,
};

const MINIMUM_BUNDLED_SQLITE: i32 = 3_053_002;

fn searchable_conversation(text: &str) -> ArchiveConversation {
    let native_id = "thread-sqlite";
    let conversation_id = archive_conversation_id("codex", native_id);
    let item_id = archive_item_id("codex", native_id, Some("message-1"), 0, text);
    ArchiveConversation {
        schema_version: statsai_core::ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
        conversation_id,
        provider: "codex".to_string(),
        source_id: SourceId("source-sqlite".to_string()),
        native_conversation_id: native_id.to_string(),
        title: Some("SQLite security".to_string()),
        project: None,
        started_at: Some(DateTime::<Utc>::UNIX_EPOCH),
        updated_at: Some(DateTime::<Utc>::UNIX_EPOCH),
        completeness: ArchiveCompleteness::Complete,
        missing_content_count: 0,
        missing_content_scope_id: None,
        discarded_source_record_ids: Vec::new(),
        superseded_conversation_ids: Vec::new(),
        items: vec![ArchiveItem {
            item_id: item_id.clone(),
            native_item_id: Some("message-1".to_string()),
            source_record_id: Some("line:1".to_string()),
            ordinal: 0,
            kind: ArchiveItemKind::Message,
            role: Some(ArchiveRole::User),
            created_at: Some(DateTime::<Utc>::UNIX_EPOCH),
            model: None,
            tool_name: None,
            tool_call_id: None,
            status: None,
            usage: None,
            parts_authoritative: true,
            parts: vec![ArchiveContentPart::text(
                archive_content_id(&item_id, 0),
                0,
                ArchiveContentKind::Text,
                text.to_string(),
            )],
        }],
    }
}

fn integrity_ok(store: &Store) -> bool {
    store
        .connection()
        .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .expect("integrity check")
        == "ok"
}

#[test]
fn bundled_sqlite_meets_the_fts5_fix_floor() {
    assert!(
        rusqlite::version_number() >= MINIMUM_BUNDLED_SQLITE,
        "bundled SQLite {} is older than 3.53.2",
        rusqlite::version()
    );
}

#[test]
fn defensive_mode_rejects_writable_schema_and_keeps_wal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("statsai.sqlite");
    let store = Store::open(&path).expect("open");
    assert_eq!(
        store.schema_version().expect("schema"),
        CURRENT_SCHEMA_VERSION
    );
    assert!(integrity_ok(&store));
    let journal_mode: String = store
        .connection()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode");
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    let writable_schema = store
        .connection()
        .execute_batch("PRAGMA writable_schema=ON");
    let writable_schema_value: i64 = store
        .connection()
        .query_row("PRAGMA writable_schema", [], |row| row.get(0))
        .expect("read writable_schema");
    let unprotected_path = directory.path().join("unprotected.sqlite");
    let unprotected = rusqlite::Connection::open(&unprotected_path).expect("open unprotected");
    unprotected
        .execute_batch("PRAGMA writable_schema=ON")
        .expect("writable_schema is available without defensive mode");
    let unprotected_value: i64 = unprotected
        .query_row("PRAGMA writable_schema", [], |row| row.get(0))
        .expect("read unprotected writable_schema");
    assert_eq!(
        unprotected_value, 1,
        "the control connection must show that writable_schema can be enabled"
    );
    assert!(
        writable_schema.is_err() || writable_schema_value == 0,
        "defensive mode must keep writable_schema disabled, got {writable_schema_value}"
    );
    assert_eq!(writable_schema_value, 0);
    let shadow_write = store.connection().execute_batch(
        "CREATE TABLE defensive_probe(id INTEGER);
         INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql)
         VALUES ('table', 'defensive_probe', 'defensive_probe', 0, 'CREATE TABLE defensive_probe(id INTEGER)');",
    );
    assert!(
        shadow_write.is_err(),
        "defensive mode must reject writes through sqlite_master"
    );
}

#[test]
fn existing_database_reopens_without_rewriting_the_schema() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("statsai.sqlite");
    let store = Store::open(&path).expect("create");
    let conversation = searchable_conversation("original bundled sqlite phrase");
    store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("seed");
    store.checkpoint_wal().expect("checkpoint");
    drop(store);

    let reopened = Store::open(&path).expect("reopen");
    assert_eq!(
        reopened.schema_version().expect("schema"),
        CURRENT_SCHEMA_VERSION
    );
    assert!(integrity_ok(&reopened));
    let hits = reopened
        .search_archive("bundled", 10)
        .expect("search existing database");
    assert_eq!(hits.len(), 1);
    assert_eq!(
        database_schema_version(&path).expect("schema version"),
        Some(CURRENT_SCHEMA_VERSION)
    );
}

#[test]
fn fts_insert_update_delete_and_search_work_under_defensive_mode() {
    let store = Store::in_memory().expect("store");
    let original = searchable_conversation("alpha fts token stays searchable");
    store
        .upsert_archive_conversations(std::slice::from_ref(&original))
        .expect("insert");
    assert_eq!(store.search_archive("alpha", 10).expect("search").len(), 1);

    let updated = searchable_conversation("beta fts token replaces the old one");
    store
        .upsert_archive_conversations(std::slice::from_ref(&updated))
        .expect("update");
    assert!(store.search_archive("alpha", 10).expect("old").is_empty());
    assert_eq!(store.search_archive("beta", 10).expect("new").len(), 1);

    store
        .connection()
        .execute("DELETE FROM archive_content_parts", [])
        .expect("delete content so the FTS delete trigger runs");
    assert!(store
        .search_archive("beta", 10)
        .expect("search after delete")
        .is_empty());
    assert!(integrity_ok(&store));
}

#[test]
fn snapshot_reads_schema_and_clone_stays_compatible_with_defensive_mode() {
    let directory = tempfile::tempdir().expect("tempdir");
    let source = directory.path().join("source.sqlite");
    let destination = directory.path().join("clone.sqlite");
    let store = Store::open(&source).expect("source");
    store.checkpoint_wal().expect("checkpoint");
    drop(store);

    assert_eq!(
        database_schema_version(&source).expect("read schema"),
        Some(CURRENT_SCHEMA_VERSION)
    );
    let clone = clone_database_to(&source, &destination);
    if cfg!(target_os = "macos") {
        let clone = clone.expect("APFS clone");
        assert_eq!(clone.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(integrity_ok(&Store::open(&destination).expect("clone")));
    } else {
        let error = clone.expect_err("non-macOS clone");
        assert!(error.to_string().contains("macOS"));
    }
    assert!(integrity_ok(
        &Store::open(&source).expect("source still opens")
    ));
}
