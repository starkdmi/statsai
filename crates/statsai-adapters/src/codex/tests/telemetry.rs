use super::*;

#[test]
fn codex_auth_json_exposes_verified_source_state_without_stamping_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "email": "existing@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-real",
                "chatgpt_plan_type": "plus",
                "chatgpt_subscription_active_start": "2026-05-29T10:12:43+00:00",
                "chatgpt_subscription_active_until": "2026-06-29T10:12:43+00:00",
                "chatgpt_subscription_last_checked": "2026-05-29T10:14:56.058278+00:00"
            }
        })
        .to_string(),
    )
    .expect("auth");
    let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
    writeln!(
        file,
        "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}"
    )
    .expect("write");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    let verified = scan
        .verified_source_state
        .as_ref()
        .expect("verified source state");
    assert_eq!(verified.provider_user_id.as_deref(), Some("acct-real"));
    assert_eq!(verified.email.as_deref(), Some("existing@example.com"));
    assert_eq!(verified.plan_name.as_deref(), Some("Plus"));
    assert!(verified.authenticated_at.is_some());
    assert_eq!(
        verified.verified_at.map(|value| value.to_rfc3339()),
        Some("2026-05-29T10:14:56.058278+00:00".to_string())
    );
    assert!(verified.subscription.is_none());
    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");
    let plan = evidence.plan_observations.first().expect("plan evidence");
    assert_eq!(plan.raw_plan_name, "plus");
    assert_eq!(plan.plan_name, "Plus");
    assert_eq!(
        plan.active_from.expect("active from").to_rfc3339(),
        "2026-05-29T10:12:43+00:00"
    );
    assert_eq!(
        plan.active_until.map(|value| value.to_rfc3339()),
        Some("2026-06-29T10:12:43+00:00".to_string())
    );
    assert_eq!(scan.events[0].provider_account_id, None);
    assert_ne!(
        scan.events[0]
            .parse_evidence
            .as_ref()
            .map(|evidence| evidence.account_identity_source.clone()),
        Some(IdentitySource::LocalAuth)
    );
}

#[test]
fn codex_auth_identity_is_dated_by_the_login_not_the_subscription_check() {
    // `auth_time` is 2026-06-10; the embedded subscription claims were last
    // revalidated on 2026-05-01. Signing into a different account rewrites
    // the account id without touching that older stamp, so dating the
    // identity by it would claim this source was already `acct-b` five
    // weeks before the login — and `AuthSnapshot` ends a source's account
    // interval, so the previous account would lose those five weeks.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "tokens": {
                "id_token": "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6InBlcnNvbkBleGFtcGxlLmNvbSIsImF1dGhfdGltZSI6MTc4MTA0OTYwMCwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoIjp7ImNoYXRncHRfYWNjb3VudF9pZCI6ImFjY3QtYiIsImNoYXRncHRfcGxhbl90eXBlIjoicGx1cyIsImNoYXRncHRfc3Vic2NyaXB0aW9uX2xhc3RfY2hlY2tlZCI6IjIwMjYtMDUtMDFUMDA6MDA6MDArMDA6MDAifX0."
            }
        })
        .to_string(),
    )
    .expect("auth");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    let identity = evidence
        .identity_observations
        .iter()
        .find(|item| item.evidence_kind == AccountEvidenceKind::AuthSnapshot)
        .expect("auth snapshot identity");
    assert_eq!(
        identity.observed_at.to_rfc3339(),
        "2026-06-10T00:00:00+00:00"
    );
    let plan = evidence.plan_observations.first().expect("plan evidence");
    assert_eq!(plan.observed_at.to_rfc3339(), "2026-05-01T00:00:00+00:00");
}

#[test]
fn codex_auth_json_reads_nested_tokens_id_token_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6ImV4aXN0aW5nQGV4YW1wbGUuY29tIiwiaWF0IjoxNzQ4NTEzNTYzLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC1yZWFsIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJwbHVzIiwiY2hhdGdwdF9zdWJzY3JpcHRpb25fYWN0aXZlX3N0YXJ0IjoiMjAyNi0wNS0yOVQxMDoxMjo0MyswMDowMCIsImNoYXRncHRfc3Vic2NyaXB0aW9uX2FjdGl2ZV91bnRpbCI6IjIwMjYtMDYtMjlUMTA6MTI6NDMrMDA6MDAiLCJjaGF0Z3B0X3N1YnNjcmlwdGlvbl9sYXN0X2NoZWNrZWQiOiIyMDI2LTA1LTI5VDEwOjE0OjU2LjA1ODI3OCswMDowMCJ9fQ.",
                "access_token": "unused",
                "refresh_token": "unused",
                "account_id": "00000000-0000-4000-8000-000000000001"
            },
            "last_refresh": "2026-05-19T19:56:03.481816Z"
        })
        .to_string(),
    )
    .expect("auth");
    let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
    writeln!(
        file,
        "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}"
    )
    .expect("write");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    let verified = scan
        .verified_source_state
        .as_ref()
        .expect("verified source state");
    assert_eq!(verified.provider_user_id.as_deref(), Some("acct-real"));
    assert_eq!(verified.email.as_deref(), Some("existing@example.com"));
    assert_eq!(verified.plan_name.as_deref(), Some("Plus"));
    assert!(verified.authenticated_at.is_some());
    assert_eq!(
        verified.verified_at.map(|value| value.to_rfc3339()),
        Some("2026-05-29T10:14:56.058278+00:00".to_string())
    );
    assert!(verified.subscription.is_none());
    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");
    let plan = evidence.plan_observations.first().expect("plan evidence");
    assert_eq!(plan.raw_plan_name, "plus");
    assert_eq!(
        plan.active_from.expect("active from").to_rfc3339(),
        "2026-05-29T10:12:43+00:00"
    );
    assert_eq!(
        plan.active_until.map(|value| value.to_rfc3339()),
        Some("2026-06-29T10:12:43+00:00".to_string())
    );
    assert_eq!(scan.events[0].provider_account_id, None);
}

#[test]
fn codex_auth_refresh_does_not_mark_cached_plan_as_newly_verified() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "email": "existing@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-real",
                "chatgpt_plan_type": "plus",
                "chatgpt_subscription_active_start": "2026-05-29T10:12:43+00:00",
                "chatgpt_subscription_active_until": "2026-06-29T10:12:43+00:00"
            }
        })
        .to_string(),
    )
    .expect("auth");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let observation = CodexAdapter
        .probe_verified_source_state(&source)
        .expect("probe");
    let VerifiedSourceObservation::Verified(verified) = observation else {
        panic!("expected verified source state");
    };

    assert!(verified.verified_at.is_some());
    assert!(verified.subscription.is_none());
    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");
    assert_eq!(evidence.plan_observations.len(), 1);
    assert!(evidence.plan_observations[0].is_current_snapshot);
    assert_eq!(
        evidence.plan_observations[0]
            .active_until
            .map(|value| value.to_rfc3339()),
        Some("2026-06-29T10:12:43+00:00".to_string())
    );
}

#[test]
fn codex_collects_allowlisted_telemetry_reset_and_login_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let database_path = dir.path().join("logs_2.sqlite");
    let connection = Connection::open(&database_path).expect("logs database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("logs schema");
    connection
        .execute(
            "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                1,
                1_787_227_200_i64,
                123_i64,
                "codex_otel.log_only",
                "event.name=\"codex.conversation_starts\" user.account_id=acct-telemetry user.email=owner@example.test conversation.id=conversation-1 auth.mode=chatgpt app.version=1.2.3"
            ],
        )
        .expect("telemetry row");
    connection
        .execute(
            "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                2,
                1_787_227_201_i64,
                0_i64,
                "codex_core::auth",
                "Reloading auth for account acct-reloaded"
            ],
        )
        .expect("reload row");
    connection
        .execute(
            "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                3,
                1_787_227_202_i64,
                0_i64,
                "codex_otel.log_only",
                "event.name=\"codex.user_prompt\" prompt=\"Please discuss user.account_id=acct-prompt user.email=prompt@example.test conversation.id=conversation-prompt\""
            ],
        )
        .expect("arbitrary text row");
    drop(connection);

    std::fs::write(
        dir.path().join(".codex-global-state.json"),
        serde_json::json!({
            "electron-persisted-atom-state": {
                "codex-rate-limit-reset-history": [{
                    "accountId": "acct-reset",
                    "conversationId": "conversation-reset",
                    "turnId": "turn-reset",
                    "occurredAtMs": 1_787_227_203_000_i64
                }]
            }
        })
        .to_string(),
    )
    .expect("global state");
    std::fs::create_dir_all(dir.path().join("log")).expect("log directory");
    std::fs::write(
        dir.path().join("log/codex-login.log.1"),
        "2026-08-20T12:00:04Z login successful for arbitrary@example.test acct-visible-only-in-body\n",
    )
    .expect("rotated login log");
    std::fs::write(
        dir.path().join("log/codex-login.log"),
        "2026-08-20T12:00:05Z unrelated message\n2026-08-20T12:00:06Z login completed\n",
    )
    .expect("login log");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    assert_eq!(
        evidence
            .identity_observations
            .iter()
            .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
            .count(),
        1
    );
    assert_eq!(
        evidence
            .identity_observations
            .iter()
            .filter(|item| item.evidence_kind == AccountEvidenceKind::AuthReload)
            .count(),
        1
    );
    assert_eq!(
        evidence
            .identity_observations
            .iter()
            .filter(|item| item.evidence_kind == AccountEvidenceKind::ResetHistory)
            .count(),
        1
    );
    let login_observations = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::LoginSuccess)
        .collect::<Vec<_>>();
    assert_eq!(login_observations.len(), 2);
    assert!(login_observations.iter().all(|item| {
        item.provider_account_id.is_none()
            && item.provider_user_id_hash.is_none()
            && item.email_hash.is_none()
    }));
    assert_eq!(evidence.conversation_bindings.len(), 2);
    assert!(evidence
        .conversation_bindings
        .iter()
        .all(|binding| binding.conversation_id_hash.len() == 64));
    assert!(evidence
        .conversation_bindings
        .iter()
        .all(|binding| binding.conversation_id_hash != "conversation-1"));

    let retry_before_ack = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("retry account evidence before acknowledgement");
    assert_eq!(
        retry_before_ack
            .identity_observations
            .iter()
            .filter(|item| matches!(
                item.evidence_kind,
                AccountEvidenceKind::TelemetryIdentity | AccountEvidenceKind::AuthReload
            ))
            .count(),
        2,
        "telemetry must remain retryable until the caller commits the evidence"
    );
    let committed_checkpoints = evidence.checkpoints.clone();

    let repeated = CodexAdapter
        .collect_account_evidence(&source, &committed_checkpoints)
        .expect("repeat account evidence after checkpoint commit");
    assert_eq!(
        repeated
            .identity_observations
            .iter()
            .filter(|item| matches!(
                item.evidence_kind,
                AccountEvidenceKind::TelemetryIdentity | AccountEvidenceKind::AuthReload
            ))
            .count(),
        0,
        "an unchanged telemetry database must not be rescanned"
    );

    let connection = Connection::open(&database_path).expect("reopen logs database");
    connection
        .execute(
            "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                4,
                1_787_227_204_i64,
                0_i64,
                "codex_otel.trace_safe",
                "event.name=\"codex.conversation_starts\" user.account_id=acct-appended"
            ],
        )
        .expect("append telemetry row");
    drop(connection);
    let appended = CodexAdapter
        .collect_account_evidence(&source, &committed_checkpoints)
        .expect("incremental account evidence");
    assert_eq!(
        appended
            .identity_observations
            .iter()
            .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
            .count(),
        1,
        "only the appended telemetry range should be parsed"
    );
    assert_eq!(appended.checkpoints.len(), 1);

    std::fs::rename(&database_path, dir.path().join("logs_2.previous.sqlite"))
        .expect("archive replaced telemetry database");
    let mut replacement = Connection::open(&database_path).expect("replacement logs database");
    replacement
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("replacement logs schema");
    let transaction = replacement.transaction().expect("replacement transaction");
    for row_id in 1..=100_i64 {
        let body = match row_id {
            1 => "event.name=\"codex.conversation_starts\" user.account_id=acct-replacement-early"
                .to_string(),
            100 => "event.name=\"codex.conversation_starts\" user.account_id=acct-replacement-late"
                .to_string(),
            _ => format!("replacement filler row {row_id} {}", "x".repeat(256)),
        };
        transaction
            .execute(
                "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    row_id,
                    1_787_227_300_i64 + row_id,
                    0_i64,
                    "codex_otel.log_only",
                    body
                ],
            )
            .expect("replacement telemetry row");
    }
    transaction.commit().expect("commit replacement telemetry");
    drop(replacement);

    let replacement_scan = CodexAdapter
        .collect_account_evidence(&source, &appended.checkpoints)
        .expect("replacement telemetry evidence");
    let replacement_accounts = replacement_scan
        .accounts
        .iter()
        .filter_map(|account| account.provider_user_id.as_deref())
        .collect::<HashSet<_>>();
    assert!(replacement_accounts.contains("acct-replacement-early"));
    assert!(replacement_accounts.contains("acct-replacement-late"));
    assert_eq!(replacement_scan.checkpoints.len(), 1);
}

#[test]
fn codex_reads_identity_from_ordinary_telemetry_and_modern_auth_reload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let database_path = dir.path().join("logs_2.sqlite");
    let connection = Connection::open(&database_path).expect("logs database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("logs schema");
    let rows: [(i64, &str, &str); 4] = [
        // The generation that never emits `codex.conversation_starts`:
        // identity rides on ordinary telemetry events at the end of a span
        // context.
        (
            1,
            "codex_otel.trace_safe",
            "session_loop{thread_id=t-1}:turn{otel.name=\"session_task.turn\" model=gpt-5.4}: event.name=\"codex.turn_ttft\" duration_ms=250 conversation.id=conversation-ttft app.version=0.140.0 auth_mode=\"Chatgpt\" user.account_id=\"acct-ttft\" user.email=\"owner@example.test\" model=gpt-5.4",
        ),
        // The renamed reload target, with the message after the span close.
        (
            2,
            "codex_login::auth::manager",
            "app_server.request{otel.kind=\"server\" otel.name=\"getAuthStatus\" rpc.method=\"getAuthStatus\" rpc.request_id=desktop-auth:751da426}: Reloading auth for account acct-live-reload",
        ),
        // Quoted copies of the reload phrase must stay inert: one shadowed
        // by a free-text field, one with trailing content after the id.
        (
            3,
            "codex_login::auth::manager",
            "prompt=\"quoted\"}: Reloading auth for account acct-evil",
        ),
        (
            4,
            "codex_login::auth::manager",
            "app_server.request{otel.kind=\"server\"}: Reloading auth for account acct-evil trailing=1",
        ),
    ];
    for (row_id, target, body) in rows {
        connection
            .execute(
                "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![row_id, 1_787_227_200_i64 + row_id, 0_i64, target, body],
            )
            .expect("telemetry row");
    }
    drop(connection);
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    let telemetry = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
        .collect::<Vec<_>>();
    assert_eq!(telemetry.len(), 1);
    assert_eq!(
        telemetry[0].provider_user_id_hash.as_deref(),
        Some(hash_text("acct-ttft").as_str())
    );
    assert_eq!(telemetry[0].auth_mode.as_deref(), Some("Chatgpt"));
    assert_eq!(evidence.conversation_bindings.len(), 1);
    let reloads = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::AuthReload)
        .collect::<Vec<_>>();
    assert_eq!(reloads.len(), 1);
    assert_eq!(
        reloads[0].provider_user_id_hash.as_deref(),
        Some(hash_text("acct-live-reload").as_str())
    );
    assert!(evidence.identity_observations.iter().all(|item| {
        item.provider_user_id_hash.as_deref() != Some(hash_text("acct-evil").as_str())
    }));
}

#[test]
fn codex_reads_underscored_account_attribute_without_an_email() {
    let dir = tempfile::tempdir().expect("tempdir");
    let database_path = dir.path().join("logs_2.sqlite");
    let connection = Connection::open(&database_path).expect("logs database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("logs schema");
    connection
        .execute(
            "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                1_i64,
                1_787_227_200_i64,
                0_i64,
                "codex_otel.trace_safe",
                "event.name=\"codex.turn_ttft\" duration_ms=250 conversation.id=conversation-underscored user_account_id=\"acct-underscored\""
            ],
        )
        .expect("telemetry row");
    drop(connection);
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    let telemetry = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
        .collect::<Vec<_>>();
    assert_eq!(
        telemetry.len(),
        1,
        "the row-selection filter must accept every account attribute spelling the parser reads"
    );
    assert_eq!(
        telemetry[0].provider_user_id_hash.as_deref(),
        Some(hash_text("acct-underscored").as_str())
    );
    assert_eq!(evidence.conversation_bindings.len(), 1);
}

#[test]
fn codex_collapses_repeated_telemetry_identity_runs_to_endpoints() {
    let dir = tempfile::tempdir().expect("tempdir");
    let database_path = dir.path().join("logs_2.sqlite");
    let connection = Connection::open(&database_path).expect("logs database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("logs schema");
    // A(x5), B(x1), A(x2): the collapse must keep A's first and last on
    // each side of B — the alternation is the account-switch signal. A's
    // rows alternate between two parallel conversations to pin down that
    // the collapse is conversation-blind: interleaving must not split runs.
    let rows = [
        ("a", "one"),
        ("a", "two"),
        ("a", "one"),
        ("a", "two"),
        ("a", "one"),
        ("b", "three"),
        ("a", "two"),
        ("a", "one"),
    ];
    for (offset, (account, conversation)) in rows.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    offset as i64 + 1,
                    1_787_227_200_i64 + offset as i64,
                    0_i64,
                    "codex_otel.log_only",
                    format!(
                        "event.name=\"codex.turn_ttft\" duration_ms=1 conversation.id=conversation-{conversation} user.account_id=\"acct-{account}\""
                    )
                ],
            )
            .expect("telemetry row");
    }
    drop(connection);
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    let telemetry_accounts = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
        .map(|item| item.provider_user_id_hash.clone().expect("account hash"))
        .collect::<Vec<_>>();
    assert_eq!(
        telemetry_accounts,
        vec![
            hash_text("acct-a"),
            hash_text("acct-a"),
            hash_text("acct-b"),
            hash_text("acct-a"),
            hash_text("acct-a"),
        ],
        "each run keeps exactly its first and last observation"
    );
}

#[test]
fn codex_reads_historical_auth_file_variants_as_dated_snapshots() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6ImV4aXN0aW5nQGV4YW1wbGUuY29tIiwiaWF0IjoxNzQ4NTEzNTYzLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC1yZWFsIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJwbHVzIiwiY2hhdGdwdF9zdWJzY3JpcHRpb25fYWN0aXZlX3N0YXJ0IjoiMjAyNi0wNS0yOVQxMDoxMjo0MyswMDowMCIsImNoYXRncHRfc3Vic2NyaXB0aW9uX2FjdGl2ZV91bnRpbCI6IjIwMjYtMDYtMjlUMTA6MTI6NDMrMDA6MDAiLCJjaGF0Z3B0X3N1YnNjcmlwdGlvbl9sYXN0X2NoZWNrZWQiOiIyMDI2LTA1LTI5VDEwOjE0OjU2LjA1ODI3OCswMDowMCJ9fQ."
            }
        })
        .to_string(),
    )
    .expect("current auth");
    // A swapped-out login kept beside the live one; its claims date to the
    // moment that account was last authenticated here.
    std::fs::write(
        dir.path().join("auth-previous.json"),
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6InByZXZpb3VzQGV4YW1wbGUudGVzdCIsImF1dGhfdGltZSI6MTc3OTc4MjQwMCwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoIjp7ImNoYXRncHRfYWNjb3VudF9pZCI6ImFjY3QtcHJldmlvdXMiLCJjaGF0Z3B0X3BsYW5fdHlwZSI6InBsdXMiLCJjaGF0Z3B0X3N1YnNjcmlwdGlvbl9hY3RpdmVfc3RhcnQiOiIyMDI2LTA0LTI2VDAwOjAwOjAwKzAwOjAwIiwiY2hhdGdwdF9zdWJzY3JpcHRpb25fYWN0aXZlX3VudGlsIjoiMjAyNi0wNS0yNlQwMDowMDowMCswMDowMCIsImNoYXRncHRfc3Vic2NyaXB0aW9uX2xhc3RfY2hlY2tlZCI6IjIwMjYtMDUtMjZUMDk6MDA6MDArMDA6MDAifX0."
            }
        })
        .to_string(),
    )
    .expect("historical auth");
    std::fs::write(dir.path().join("auth-broken.json"), "not json").expect("broken variant");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    // Only the live auth.json may act as a source-wide auth state; the
    // swapped-out variant is a dated login that must never close an
    // interval nothing can reopen.
    let snapshots = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::AuthSnapshot)
        .collect::<Vec<_>>();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        snapshots[0].provider_user_id_hash.as_deref(),
        Some(hash_text("acct-real").as_str())
    );
    let previous_identity = evidence
        .identity_observations
        .iter()
        .find(|item| {
            item.provider_user_id_hash.as_deref() == Some(hash_text("acct-previous").as_str())
        })
        .expect("historical snapshot identity");
    assert_eq!(
        previous_identity.evidence_kind,
        AccountEvidenceKind::LoginSuccess
    );
    assert!(!previous_identity.evidence_kind.ends_source_attribution());
    assert_eq!(
        previous_identity.observed_at.to_rfc3339(),
        "2026-05-26T08:00:00+00:00"
    );
    let current_plan = evidence
        .plan_observations
        .iter()
        .find(|item| item.is_current_snapshot)
        .expect("current plan claim");
    assert_eq!(current_plan.source_id, source.source_id);
    let historical_plan = evidence
        .plan_observations
        .iter()
        .find(|item| !item.is_current_snapshot)
        .expect("historical plan claim");
    assert_eq!(
        historical_plan.observed_at.to_rfc3339(),
        "2026-05-26T09:00:00+00:00"
    );
    assert_eq!(
        historical_plan.active_until.map(|value| value.to_rfc3339()),
        Some("2026-05-26T00:00:00+00:00".to_string())
    );
}

#[test]
fn codex_skips_malformed_or_locked_telemetry_databases() {
    let malformed = tempfile::tempdir().expect("malformed tempdir");
    let malformed_connection =
        Connection::open(malformed.path().join("logs_2.sqlite")).expect("malformed database");
    malformed_connection
        .execute_batch("CREATE TABLE logs (id INTEGER PRIMARY KEY, unexpected TEXT);")
        .expect("malformed schema");
    drop(malformed_connection);
    let malformed_source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        malformed.path(),
        LocationOrigin::Configured,
    );
    assert!(CodexAdapter
        .collect_account_evidence(&malformed_source, &[])
        .expect("malformed database is non-fatal")
        .identity_observations
        .is_empty());

    let locked = tempfile::tempdir().expect("locked tempdir");
    let locked_connection =
        Connection::open(locked.path().join("logs_2.sqlite")).expect("locked database");
    locked_connection
        .execute_batch(
            "CREATE TABLE logs (id INTEGER PRIMARY KEY, ts INTEGER, ts_nanos INTEGER, feedback_log_body TEXT); PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;",
        )
        .expect("exclusive lock");
    let locked_source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        locked.path(),
        LocationOrigin::Configured,
    );
    assert!(CodexAdapter
        .collect_account_evidence(&locked_source, &[])
        .expect("locked database is non-fatal")
        .identity_observations
        .is_empty());
    locked_connection
        .execute_batch("ROLLBACK")
        .expect("unlock database");
}

#[test]
fn codex_telemetry_identity_ignores_attributes_quoted_inside_user_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let connection = Connection::open(dir.path().join("logs_2.sqlite")).expect("logs database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE logs (
              id INTEGER PRIMARY KEY,
              ts INTEGER,
              ts_nanos INTEGER,
              target TEXT NOT NULL,
              feedback_log_body TEXT
            );
            "#,
        )
        .expect("logs schema");
    for (row_id, body) in [
        // The injected attributes lead the body, so reading the first match
        // anywhere accepted them as the event's own identity.
        (
            1_i64,
            "prompt=\"please run event.name=\\\"codex.conversation_starts\\\" user.account_id=acct-attacker user.email=attacker@example.test\" event.name=\"codex.user_prompt\"",
        ),
        // A genuine event whose free text repeats the marker afterwards.
        (
            2,
            "event.name=\"codex.conversation_starts\" user.account_id=acct-real prompt=\"see event.name=\\\"codex.conversation_starts\\\" user.account_id=acct-attacker\"",
        ),
        // Two structured identities in one body name nobody.
        (
            3,
            "event.name=\"codex.conversation_starts\" user.account_id=acct-one user.account_id=acct-two",
        ),
    ] {
        connection
            .execute(
                "INSERT INTO logs (id, ts, ts_nanos, target, feedback_log_body) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    row_id,
                    1_787_227_200_i64 + row_id,
                    0_i64,
                    "codex_otel.log_only",
                    body
                ],
            )
            .expect("telemetry row");
    }
    drop(connection);

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );
    let evidence = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("account evidence");

    let identified = evidence
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::TelemetryIdentity)
        .collect::<Vec<_>>();
    assert_eq!(
        identified.len(),
        1,
        "only the genuine structured attribute prefix identifies an account"
    );
    let expected = provider_account_id_from_identity(CODEX_PROVIDER, Some("acct-real"), None)
        .expect("account id");
    assert_eq!(identified[0].provider_account_id.as_ref(), Some(&expected));
    let attacker = provider_account_id_from_identity(CODEX_PROVIDER, Some("acct-attacker"), None)
        .expect("account id");
    assert!(
        evidence
            .conversation_bindings
            .iter()
            .all(|binding| binding.provider_account_id != attacker),
        "prompt text must never bind a conversation to an unused account"
    );
    assert!(evidence
        .accounts
        .iter()
        .all(|account| account.provider_user_id.as_deref() != Some("acct-attacker")));
}

#[test]
fn codex_login_evidence_survives_log_rotation_without_duplicating() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_directory = dir.path().join("log");
    std::fs::create_dir_all(&log_directory).expect("log directory");
    let entry = "2026-08-20T12:00:00Z successfully logged in\n";
    std::fs::write(log_directory.join("codex-login.log"), entry).expect("login log");
    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let before = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("evidence before rotation");
    let before_ids = before
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::LoginSuccess)
        .map(|item| item.observation_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(before_ids.len(), 1);

    // The same login, now one generation older, plus a fresh empty log.
    std::fs::rename(
        log_directory.join("codex-login.log"),
        log_directory.join("codex-login.log.1"),
    )
    .expect("rotate login log");
    std::fs::write(log_directory.join("codex-login.log"), "").expect("fresh login log");

    let after = CodexAdapter
        .collect_account_evidence(&source, &[])
        .expect("evidence after rotation");
    let after_ids = after
        .identity_observations
        .iter()
        .filter(|item| item.evidence_kind == AccountEvidenceKind::LoginSuccess)
        .map(|item| item.observation_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        after_ids, before_ids,
        "rotation moves a login between files; it is not a second login"
    );
}

#[test]
fn codex_probe_verified_source_state_uses_parent_auth_for_sessions_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::json!({
            "email": "existing@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-real",
                "chatgpt_plan_type": "plus",
                "chatgpt_subscription_active_start": "2026-05-29T10:12:43+00:00",
                "chatgpt_subscription_active_until": "2026-06-29T10:12:43+00:00",
                "chatgpt_subscription_last_checked": "2026-05-29T10:14:56.058278+00:00"
            }
        })
        .to_string(),
    )
    .expect("auth");

    let source = SourceLocation::local_adapter(
        CODEX_PROVIDER,
        "test",
        "0",
        &sessions,
        LocationOrigin::Configured,
    );

    let observation = CodexAdapter
        .probe_verified_source_state(&source)
        .expect("probe");
    let VerifiedSourceObservation::Verified(verified) = observation else {
        panic!("expected verified source state");
    };

    assert_eq!(verified.provider_user_id.as_deref(), Some("acct-real"));
    assert_eq!(verified.email.as_deref(), Some("existing@example.com"));
    assert_eq!(verified.plan_name.as_deref(), Some("Plus"));
}
