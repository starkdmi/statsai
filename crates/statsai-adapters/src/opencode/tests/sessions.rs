use super::*;

#[test]
fn opencode_sqlite_sessions_become_usage_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("opencode.db");
    let connection = Connection::open(&db_path).expect("db");
    connection
        .execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT,
                model TEXT,
                cost REAL,
                tokens_input INTEGER NOT NULL,
                tokens_output INTEGER NOT NULL,
                tokens_reasoning INTEGER NOT NULL,
                tokens_cache_read INTEGER NOT NULL,
                tokens_cache_write INTEGER NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                directory TEXT
            );",
        )
        .expect("schema");
    connection
        .execute(
            "INSERT INTO session VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                "ses_test",
                "Test session",
                r#"{"id":"grok-build-0.1","providerID":"xai"}"#,
                1.23_f64,
                100_i64,
                20_i64,
                5_i64,
                30_i64,
                7_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert");
    let source = SourceLocation::local_adapter(
        OPENCODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_opencode_source(&OpenCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let event = &scan.events[0];
    assert_eq!(event.provider, OPENCODE_PROVIDER);
    assert_eq!(event.session.title.as_deref(), Some("Test session"));
    assert_eq!(event.usage.input_tokens, Some(100));
    assert_eq!(event.usage.cache_creation_tokens, Some(7));
    assert_eq!(event.usage.computed_total(), 162);
    assert_eq!(event.cost.provider_reported_usd, Some(123));
    let project = event.project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(display_path(dir.path()).as_str())
    );
    assert_eq!(
        project.path_hash.as_deref(),
        Some(path_hash(dir.path()).as_str())
    );
    assert_eq!(
        event.model.as_ref().and_then(|model| model.name.as_deref()),
        Some("xai/grok-build-0.1")
    );
    assert_eq!(
        event
            .model
            .as_ref()
            .and_then(|model| model.normalized_name.as_deref()),
        Some("grok-build-0.1")
    );
    assert_eq!(
        event
            .model
            .as_ref()
            .and_then(|model| model.provider_model_id.as_deref()),
        Some("xai/grok-build-0.1")
    );
}

#[test]
fn opencode_splits_multi_model_sessions_into_message_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("opencode.db");
    let connection = Connection::open(&db_path).expect("db");
    connection
        .execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT,
                model TEXT,
                cost REAL,
                tokens_input INTEGER NOT NULL,
                tokens_output INTEGER NOT NULL,
                tokens_reasoning INTEGER NOT NULL,
                tokens_cache_read INTEGER NOT NULL,
                tokens_cache_write INTEGER NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                directory TEXT
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .expect("schema");
    connection
        .execute(
            "INSERT INTO session VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                "ses_test",
                "Ambiguous session",
                Option::<String>::None,
                0.0_f64,
                100_i64,
                20_i64,
                0_i64,
                30_i64,
                0_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert session");
    for (id, provider, model) in [
        ("msg_a", "google", "antigravity-claude-opus-4-5-thinking"),
        ("msg_b", "openai", "gpt-5.2-codex"),
    ] {
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    "ses_test",
                    1_767_225_600_000_i64,
                    1_767_225_660_000_i64,
                    serde_json::json!({
                        "providerID": provider,
                        "modelID": model,
                        "tokens": {
                            "input": if provider == "google" { 100 } else { 0 },
                            "output": if provider == "openai" { 20 } else { 0 },
                            "reasoning": 0,
                            "cache": {
                                "read": if provider == "google" { 30 } else { 0 },
                                "write": 0
                            }
                        }
                    })
                    .to_string(),
                ],
            )
            .expect("insert message");
    }
    let source = SourceLocation::local_adapter(
        OPENCODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_opencode_source(&OpenCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 2);
    assert_eq!(scan.diagnostics.model_fallbacks, 0);
    assert_eq!(scan.diagnostics.candidate_usage_rows, 2);
    assert!(scan.events.iter().any(|event| event
        .model
        .as_ref()
        .and_then(|model| model.name.as_deref())
        == Some("google/antigravity-claude-opus-4-5-thinking")));
    assert!(scan.events.iter().any(|event| event
        .model
        .as_ref()
        .and_then(|model| model.name.as_deref())
        == Some("openai/gpt-5.2-codex")));
}

#[test]
fn opencode_session_rows_report_their_message_count_as_requests() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("opencode.db");
    let connection = Connection::open(&db_path).expect("db");
    connection
        .execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT,
                model TEXT,
                cost REAL,
                tokens_input INTEGER NOT NULL,
                tokens_output INTEGER NOT NULL,
                tokens_reasoning INTEGER NOT NULL,
                tokens_cache_read INTEGER NOT NULL,
                tokens_cache_write INTEGER NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                directory TEXT
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .expect("schema");
    connection
        .execute(
            "INSERT INTO session VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                "ses_test",
                "Long session",
                r#"{"id":"grok-4.6","providerID":"xai"}"#,
                0.0_f64,
                150_000_i64,
                10_000_i64,
                0_i64,
                60_000_i64,
                0_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert session");
    for id in ["msg_a", "msg_b"] {
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    "ses_test",
                    1_767_225_600_000_i64,
                    1_767_225_660_000_i64,
                    serde_json::json!({
                        "providerID": "xai",
                        "modelID": "grok-4.6",
                        "tokens": {
                            "input": 75_000,
                            "output": 5_000,
                            "reasoning": 0,
                            "cache": { "read": 30_000, "write": 0 }
                        }
                    })
                    .to_string(),
                ],
            )
            .expect("insert message");
    }
    let source = SourceLocation::local_adapter(
        OPENCODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_opencode_source(&OpenCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let event = &scan.events[0];
    // Two requests of 105k prompt tokens each, not one request of 210k: xAI's
    // long-context tier is per request, so the session total must not trigger it.
    assert_eq!(event.usage.requests, Some(2));
    assert_eq!(event.cost.estimated_api_equivalent_micro_usd, Some(390_000));
}

#[test]
fn opencode_session_rows_keep_per_message_long_context_pricing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("opencode.db");
    let connection = Connection::open(&db_path).expect("db");
    connection
        .execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT,
                model TEXT,
                cost REAL,
                tokens_input INTEGER NOT NULL,
                tokens_output INTEGER NOT NULL,
                tokens_reasoning INTEGER NOT NULL,
                tokens_cache_read INTEGER NOT NULL,
                tokens_cache_write INTEGER NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                directory TEXT
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .expect("schema");
    connection
        .execute(
            "INSERT INTO session VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                "ses_test",
                "Long-context session",
                r#"{"id":"grok-4.6","providerID":"xai"}"#,
                0.0_f64,
                240_000_i64,
                20_000_i64,
                0_i64,
                160_000_i64,
                0_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert session");
    for id in ["msg_a", "msg_b"] {
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    "ses_test",
                    1_767_225_600_000_i64,
                    1_767_225_660_000_i64,
                    serde_json::json!({
                        "providerID": "xai",
                        "modelID": "grok-4.6",
                        "tokens": {
                            "input": 120_000,
                            "output": 10_000,
                            "reasoning": 0,
                            "cache": { "read": 80_000, "write": 0 }
                        }
                    })
                    .to_string(),
                ],
            )
            .expect("insert message");
    }
    let source = SourceLocation::local_adapter(
        OPENCODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_opencode_source(&OpenCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let event = &scan.events[0];
    assert_eq!(event.usage.requests, Some(2));
    // Both requests cross the 200k prompt threshold on their own, so both are
    // billed at the doubled long-context rate: 2 x 680_000.
    assert_eq!(
        event.cost.estimated_api_equivalent_micro_usd,
        Some(1_360_000)
    );
    assert_eq!(
        event.cost.pricing_source.as_deref(),
        Some("xai_api_pricing:grok-4.6:message_usage")
    );
    assert!(statsai_pricing::estimate_priced_from_source_records(
        &event.cost
    ));
}

#[test]
fn opencode_session_usage_beyond_its_messages_stays_untiered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("opencode.db");
    let connection = Connection::open(&db_path).expect("db");
    connection
        .execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT,
                model TEXT,
                cost REAL,
                tokens_input INTEGER NOT NULL,
                tokens_output INTEGER NOT NULL,
                tokens_reasoning INTEGER NOT NULL,
                tokens_cache_read INTEGER NOT NULL,
                tokens_cache_write INTEGER NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                directory TEXT
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .expect("schema");
    connection
        .execute(
            "INSERT INTO session VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                "ses_test",
                "Compacted session",
                r#"{"id":"grok-4.6","providerID":"xai"}"#,
                0.0_f64,
                370_000_i64,
                10_000_i64,
                0_i64,
                80_000_i64,
                0_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert session");
    connection
        .execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "msg_a",
                "ses_test",
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                serde_json::json!({
                    "providerID": "xai",
                    "modelID": "grok-4.6",
                    "tokens": {
                        "input": 120_000,
                        "output": 10_000,
                        "reasoning": 0,
                        "cache": { "read": 80_000, "write": 0 }
                    }
                })
                .to_string(),
            ],
        )
        .expect("insert message");
    let source = SourceLocation::local_adapter(
        OPENCODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_opencode_source(&OpenCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let event = &scan.events[0];
    // 680_000 for the message that crossed the threshold, plus the 250k input
    // tokens no message accounts for, which cannot be shown to be one request.
    assert_eq!(
        event.cost.estimated_api_equivalent_micro_usd,
        Some(1_180_000)
    );
}
