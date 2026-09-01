use super::*;

#[test]
fn opencode_recovers_missing_session_model_from_messages() {
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
                "Recovered session",
                Option::<String>::None,
                0.0_f64,
                1_000_000_i64,
                1_000_000_i64,
                0_i64,
                1_000_000_i64,
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
                "msg_test",
                "ses_test",
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                serde_json::json!({
                    "providerID": "google",
                    "modelID": "antigravity-claude-opus-4-5-thinking"
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
    assert_eq!(
        event.model.as_ref().and_then(|model| model.name.as_deref()),
        Some("google/antigravity-claude-opus-4-5-thinking")
    );
    assert_eq!(event.cost.estimated_api_equivalent_usd, Some(3050));
    assert_eq!(scan.diagnostics.model_fallbacks, 0);
}

#[test]
fn opencode_recovers_missing_session_model_from_alternative_message_shape() {
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
                "Recovered alt session",
                Option::<String>::None,
                0.0_f64,
                1_000_000_i64,
                1_000_000_i64,
                0_i64,
                1_000_000_i64,
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
                "msg_test",
                "ses_test",
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                serde_json::json!({
                    "provider_id": "openai",
                    "id": "gpt-5.2-codex"
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
    assert_eq!(
        event.model.as_ref().and_then(|model| model.name.as_deref()),
        Some("openai/gpt-5.2-codex")
    );
    assert_eq!(event.cost.estimated_api_equivalent_usd, Some(1593));
    assert_eq!(scan.diagnostics.model_fallbacks, 0);
}

#[test]
fn opencode_recovers_single_model_from_prior_message_context() {
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
                "Mixed metadata session",
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
    connection
        .execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "msg_a",
                "ses_test",
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                serde_json::json!({
                    "providerID": "google",
                    "modelID": "antigravity-claude-opus-4-5-thinking",
                    "tokens": {
                        "input": 60,
                        "output": 0,
                        "reasoning": 0,
                        "cache": { "read": 10, "write": 0 }
                    }
                })
                .to_string(),
            ],
        )
        .expect("insert message a");
    connection
        .execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "msg_b",
                "ses_test",
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                serde_json::json!({
                    "tokens": {
                        "input": 40,
                        "output": 20,
                        "reasoning": 0,
                        "cache": { "read": 20, "write": 0 }
                    }
                })
                .to_string(),
            ],
        )
        .expect("insert message b");
    let source = SourceLocation::local_adapter(
        OPENCODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_opencode_source(&OpenCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 2);
    assert!(scan.events.iter().all(|event| {
        event
            .model
            .as_ref()
            .and_then(|model| model.provider_model_id.as_deref())
            == Some("google/antigravity-claude-opus-4-5-thinking")
    }));
}

#[test]
fn opencode_partial_multi_model_reconstruction_keeps_residual_aggregate_usage() {
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
                "Partial session",
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
    for (id, provider, model, input, output, cache_read) in [
        (
            "msg_a",
            "google",
            "antigravity-claude-opus-4-5-thinking",
            60,
            0,
            10,
        ),
        ("msg_b", "openai", "gpt-5.2-codex", 0, 0, 0),
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
                            "input": input,
                            "output": output,
                            "reasoning": 0,
                            "cache": {
                                "read": cache_read,
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
    let known = scan
        .events
        .iter()
        .find(|event| event.model.is_some())
        .expect("known event");
    let residual = scan
        .events
        .iter()
        .find(|event| event.model.is_none())
        .expect("residual event");
    assert_eq!(known.usage.input_tokens, Some(60));
    assert_eq!(known.usage.cache_read_tokens, Some(10));
    assert_eq!(residual.usage.input_tokens, Some(40));
    assert_eq!(residual.usage.output_tokens, Some(20));
    assert_eq!(residual.usage.cache_read_tokens, Some(20));
}

#[test]
fn opencode_partial_multi_model_reconstruction_preserves_residual_provider_cost() {
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
                "Residual cost session",
                Option::<String>::None,
                3.0_f64,
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
    connection
        .execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "msg_a",
                "ses_test",
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                serde_json::json!({
                    "providerID": "google",
                    "modelID": "antigravity-claude-opus-4-5-thinking",
                    "cost": 1.25,
                    "tokens": {
                        "input": 60,
                        "output": 0,
                        "reasoning": 0,
                        "cache": { "read": 10, "write": 0 }
                    }
                })
                .to_string(),
            ],
        )
        .expect("insert message a");
    connection
        .execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "msg_b",
                "ses_test",
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                serde_json::json!({
                    "providerID": "openai",
                    "modelID": "gpt-5.2-codex",
                    "tokens": {
                        "input": 0,
                        "output": 0,
                        "reasoning": 0,
                        "cache": { "read": 0, "write": 0 }
                    }
                })
                .to_string(),
            ],
        )
        .expect("insert message b");
    let source = SourceLocation::local_adapter(
        OPENCODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_opencode_source(&OpenCodeAdapter, &source, &options()).expect("scan");

    let residual = scan
        .events
        .iter()
        .find(|event| event.model.is_none())
        .expect("residual event");
    assert_eq!(residual.cost.provider_reported_usd, Some(175));
}

#[test]
fn opencode_variant_only_residual_keeps_recovered_session_model() {
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
                "ses_variant_only",
                "Variant aggregate only",
                Option::<String>::None,
                0.0_f64,
                90_i64,
                20_i64,
                5_i64,
                10_i64,
                0_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert session");
    for (id, data) in [
        (
            "msg_user_a",
            serde_json::json!({
                "role": "user",
                "model": {
                    "providerID": "openai",
                    "modelID": "gpt-5.5",
                    "variant": "high"
                }
            }),
        ),
        (
            "msg_assistant_a",
            serde_json::json!({
                "role": "assistant",
                "model": {
                    "providerID": "openai",
                    "modelID": "gpt-5.5"
                }
            }),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    "ses_variant_only",
                    1_767_225_600_000_i64,
                    1_767_225_660_000_i64,
                    data.to_string(),
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
    let model = event.model.as_ref().expect("model");
    assert_eq!(model.provider_model_id.as_deref(), Some("openai/gpt-5.5"));
    assert_eq!(model.reasoning_level, Some(ReasoningLevel::High));
    assert_eq!(model.reasoning_level_raw.as_deref(), Some("high"));
    assert!(
        !event
            .parse_evidence
            .as_ref()
            .expect("evidence")
            .model_inferred
    );
}

#[test]
fn opencode_variant_only_residual_falls_back_to_session_row_model() {
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
                "ses_variant_session_model",
                "Variant session row model",
                "openai/gpt-5.5",
                0.0_f64,
                90_i64,
                20_i64,
                5_i64,
                10_i64,
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
                "msg_user_a",
                "ses_variant_session_model",
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                serde_json::json!({
                    "role": "user",
                    "variant": "high"
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
    let model = event.model.as_ref().expect("model");
    assert_eq!(model.provider_model_id.as_deref(), Some("openai/gpt-5.5"));
    assert_eq!(model.reasoning_level, None);
    assert_eq!(model.reasoning_level_raw, None);
    assert!(
        !event
            .parse_evidence
            .as_ref()
            .expect("evidence")
            .model_inferred
    );
}

#[test]
fn opencode_variant_residual_with_missing_message_model_uses_session_row_model() {
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
                "ses_variant_missing_model",
                "Variant missing message model",
                "openai/gpt-5.5",
                0.0_f64,
                90_i64,
                20_i64,
                5_i64,
                10_i64,
                0_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert session");
    for (id, data) in [
        (
            "msg_user_a",
            serde_json::json!({
                "role": "user",
                "variant": "high"
            }),
        ),
        (
            "msg_assistant_a",
            serde_json::json!({
                "role": "assistant",
                "tokens": {
                    "input": 60,
                    "output": 10,
                    "reasoning": 5,
                    "cache": { "read": 0, "write": 0 }
                }
            }),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    "ses_variant_missing_model",
                    1_767_225_600_000_i64,
                    1_767_225_660_000_i64,
                    data.to_string(),
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
    let model = event.model.as_ref().expect("model");
    assert_eq!(model.provider_model_id.as_deref(), Some("openai/gpt-5.5"));
    assert_eq!(model.reasoning_level, None);
    assert_eq!(model.reasoning_level_raw, None);
    assert!(
        !event
            .parse_evidence
            .as_ref()
            .expect("evidence")
            .model_inferred
    );
}

#[test]
fn opencode_ambiguous_usage_still_detects_late_variant_conflict_for_residuals() {
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
                "ses_variant_ambiguous_conflict",
                "Variant ambiguous conflict",
                Option::<String>::None,
                0.0_f64,
                100_i64,
                20_i64,
                5_i64,
                10_i64,
                0_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert session");
    for (id, created_at, data) in [
        (
            "msg_user_low",
            1_767_225_600_000_i64,
            serde_json::json!({
                "role": "user",
                "model": {
                    "providerID": "openai",
                    "modelID": "gpt-5.5",
                    "variant": "low"
                }
            }),
        ),
        (
            "msg_assistant_usage",
            1_767_225_601_000_i64,
            serde_json::json!({
                "role": "assistant",
                "tokens": {
                    "input": 60,
                    "output": 10,
                    "reasoning": 5,
                    "cache": { "read": 0, "write": 0 }
                }
            }),
        ),
        (
            "msg_user_high",
            1_767_225_602_000_i64,
            serde_json::json!({
                "role": "user",
                "model": {
                    "providerID": "openai",
                    "modelID": "gpt-5.5",
                    "variant": "high"
                }
            }),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    "ses_variant_ambiguous_conflict",
                    created_at,
                    created_at,
                    data.to_string(),
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
    let reconstructed = scan
        .events
        .iter()
        .find(|event| event.usage.input_tokens == Some(60))
        .expect("reconstructed event");
    let reconstructed_model = reconstructed.model.as_ref().expect("reconstructed model");
    assert_eq!(
        reconstructed_model.provider_model_id.as_deref(),
        Some("openai/gpt-5.5")
    );
    assert_eq!(
        reconstructed_model.reasoning_level,
        Some(ReasoningLevel::Low)
    );
    assert_eq!(
        reconstructed_model.reasoning_level_raw.as_deref(),
        Some("low")
    );

    let residual = scan
        .events
        .iter()
        .find(|event| event.usage.input_tokens == Some(40))
        .expect("residual event");
    assert!(residual.model.is_none());
    assert!(
        residual
            .parse_evidence
            .as_ref()
            .expect("evidence")
            .model_inferred
    );
}

#[test]
fn opencode_variant_only_residual_stays_model_less_when_variants_conflict() {
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
                "ses_variant_conflict",
                "Variant conflict aggregate only",
                Option::<String>::None,
                0.0_f64,
                90_i64,
                20_i64,
                5_i64,
                10_i64,
                0_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert session");
    for (id, data) in [
        (
            "msg_user_a",
            serde_json::json!({
                "role": "user",
                "model": {
                    "providerID": "openai",
                    "modelID": "gpt-5.5",
                    "variant": "low"
                }
            }),
        ),
        (
            "msg_user_b",
            serde_json::json!({
                "role": "user",
                "model": {
                    "providerID": "openai",
                    "modelID": "gpt-5.5",
                    "variant": "high"
                }
            }),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    "ses_variant_conflict",
                    1_767_225_600_000_i64,
                    1_767_225_660_000_i64,
                    data.to_string(),
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
    assert!(event.model.is_none());
    assert!(
        event
            .parse_evidence
            .as_ref()
            .expect("evidence")
            .model_inferred
    );
}

#[test]
fn opencode_variant_sessions_reconstruct_usage_from_nested_model_context() {
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
                "ses_variant",
                "Variant session",
                Option::<String>::None,
                0.0_f64,
                90_i64,
                20_i64,
                9_i64,
                0_i64,
                0_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert session");
    for (id, created_at, data) in [
        (
            "msg_user_a",
            1_767_225_600_000_i64,
            serde_json::json!({
                "role": "user",
                "model": {
                    "providerID": "openai",
                    "modelID": "gpt-5.4-mini",
                    "variant": "low"
                }
            }),
        ),
        (
            "msg_assistant_a",
            1_767_225_601_000_i64,
            serde_json::json!({
                "role": "assistant",
                "tokens": {
                    "input": 60,
                    "output": 10,
                    "reasoning": 4,
                    "cache": { "read": 0, "write": 0 }
                }
            }),
        ),
        (
            "msg_user_b",
            1_767_225_602_000_i64,
            serde_json::json!({
                "role": "user",
                "model": {
                    "providerID": "openai",
                    "modelID": "gpt-5.5",
                    "variant": "high"
                }
            }),
        ),
        (
            "msg_assistant_b",
            1_767_225_603_000_i64,
            serde_json::json!({
                "role": "assistant",
                "tokens": {
                    "input": 30,
                    "output": 10,
                    "reasoning": 5,
                    "cache": { "read": 0, "write": 0 }
                }
            }),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![id, "ses_variant", created_at, created_at, data.to_string(),],
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
    assert!(scan.events.iter().all(|event| event.model.is_some()));
    assert!(scan.events.iter().any(|event| {
        event
            .model
            .as_ref()
            .and_then(|model| model.provider_model_id.as_deref())
            == Some("openai/gpt-5.4-mini")
            && event.model.as_ref().and_then(|model| model.reasoning_level)
                == Some(ReasoningLevel::Low)
    }));
    assert!(scan.events.iter().any(|event| {
        event
            .model
            .as_ref()
            .and_then(|model| model.provider_model_id.as_deref())
            == Some("openai/gpt-5.5")
            && event.model.as_ref().and_then(|model| model.reasoning_level)
                == Some(ReasoningLevel::High)
    }));
}

#[test]
fn opencode_model_switch_without_variant_clears_inherited_reasoning() {
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
                "ses_switch",
                "Switch session",
                Option::<String>::None,
                0.0_f64,
                100_i64,
                20_i64,
                5_i64,
                0_i64,
                0_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert session");
    for (id, created_at, data) in [
        (
            "msg_user_variant",
            1_767_225_600_000_i64,
            serde_json::json!({
                "role": "user",
                "model": {
                    "providerID": "openai",
                    "modelID": "gpt-5.4-mini",
                    "variant": "low"
                }
            }),
        ),
        (
            "msg_assistant_same",
            1_767_225_601_000_i64,
            serde_json::json!({
                "role": "assistant",
                "tokens": {
                    "input": 60,
                    "output": 10,
                    "reasoning": 5,
                    "cache": { "read": 0, "write": 0 }
                }
            }),
        ),
        (
            "msg_assistant_switch",
            1_767_225_602_000_i64,
            serde_json::json!({
                "role": "assistant",
                "providerID": "openai",
                "modelID": "gpt-5.5",
                "tokens": {
                    "input": 40,
                    "output": 10,
                    "reasoning": 0,
                    "cache": { "read": 0, "write": 0 }
                }
            }),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![id, "ses_switch", created_at, created_at, data.to_string(),],
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
    let retained = scan
        .events
        .iter()
        .find(|event| {
            event
                .model
                .as_ref()
                .and_then(|model| model.provider_model_id.as_deref())
                == Some("openai/gpt-5.4-mini")
        })
        .expect("retained event");
    assert_eq!(
        retained
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level),
        Some(ReasoningLevel::Low)
    );
    let cleared = scan
        .events
        .iter()
        .find(|event| {
            event
                .model
                .as_ref()
                .and_then(|model| model.provider_model_id.as_deref())
                == Some("openai/gpt-5.5")
        })
        .expect("cleared event");
    assert_eq!(
        cleared
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level),
        None
    );
    assert_eq!(
        cleared
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level_raw.as_deref()),
        None
    );
}

#[test]
fn opencode_same_model_without_variant_clears_inherited_reasoning() {
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
                "ses_same_model",
                "Same model session",
                Option::<String>::None,
                0.0_f64,
                120_i64,
                25_i64,
                5_i64,
                0_i64,
                0_i64,
                1_767_225_600_000_i64,
                1_767_225_660_000_i64,
                dir.path().to_string_lossy().to_string(),
            ],
        )
        .expect("insert session");
    for (id, created_at, data) in [
        (
            "msg_user_variant",
            1_767_225_600_000_i64,
            serde_json::json!({
                "role": "user",
                "model": {
                    "providerID": "openai",
                    "modelID": "gpt-5.5",
                    "variant": "high"
                }
            }),
        ),
        (
            "msg_assistant_inherit",
            1_767_225_601_000_i64,
            serde_json::json!({
                "role": "assistant",
                "tokens": {
                    "input": 50,
                    "output": 10,
                    "reasoning": 5,
                    "cache": { "read": 0, "write": 0 }
                }
            }),
        ),
        (
            "msg_assistant_clear",
            1_767_225_602_000_i64,
            serde_json::json!({
                "role": "assistant",
                "providerID": "openai",
                "modelID": "gpt-5.5",
                "tokens": {
                    "input": 70,
                    "output": 15,
                    "reasoning": 0,
                    "cache": { "read": 0, "write": 0 }
                }
            }),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    "ses_same_model",
                    created_at,
                    created_at,
                    data.to_string(),
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
    let inherited = scan
        .events
        .iter()
        .find(|event| event.usage.input_tokens == Some(50))
        .expect("inherited event");
    assert_eq!(
        inherited
            .model
            .as_ref()
            .and_then(|model| model.provider_model_id.as_deref()),
        Some("openai/gpt-5.5")
    );
    assert_eq!(
        inherited
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level),
        Some(ReasoningLevel::High)
    );
    assert_eq!(
        inherited
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level_raw.as_deref()),
        Some("high")
    );

    let cleared = scan
        .events
        .iter()
        .find(|event| event.usage.input_tokens == Some(70))
        .expect("cleared event");
    assert_eq!(
        cleared
            .model
            .as_ref()
            .and_then(|model| model.provider_model_id.as_deref()),
        Some("openai/gpt-5.5")
    );
    assert_eq!(
        cleared
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level),
        None
    );
    assert_eq!(
        cleared
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level_raw.as_deref()),
        None
    );
}
