use crate::*;

pub(crate) const CODEX_ACCOUNT_EVIDENCE_PARSER_VERSION: &str = "codex-account-evidence.v2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexTelemetryCursor {
    pub(crate) maximum_row_id: i64,
    pub(crate) checkpoint_row_fingerprint: Option<String>,
    pub(crate) database_size: i64,
    pub(crate) database_modified_nanos: i64,
    pub(crate) wal_size: i64,
    pub(crate) wal_modified_nanos: i64,
}

pub(crate) fn codex_telemetry_file_state(
    path: &Path,
    maximum_row_id: i64,
    checkpoint_row_fingerprint: Option<String>,
) -> CodexTelemetryCursor {
    let metadata = std::fs::metadata(path).ok();
    let wal_metadata = std::fs::metadata(path.with_extension("sqlite-wal")).ok();
    let modified_nanos = |metadata: Option<&std::fs::Metadata>| {
        metadata
            .and_then(|value| value.modified().ok())
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |value| {
                i64::try_from(value.as_nanos()).unwrap_or(i64::MAX)
            })
    };
    CodexTelemetryCursor {
        maximum_row_id,
        checkpoint_row_fingerprint,
        database_size: metadata
            .as_ref()
            .map_or(0, |value| i64::try_from(value.len()).unwrap_or(i64::MAX)),
        database_modified_nanos: modified_nanos(metadata.as_ref()),
        wal_size: wal_metadata
            .as_ref()
            .map_or(0, |value| i64::try_from(value.len()).unwrap_or(i64::MAX)),
        wal_modified_nanos: modified_nanos(wal_metadata.as_ref()),
    }
}

pub(crate) fn codex_telemetry_cursor(
    checkpoint: &AccountEvidenceCheckpointV1,
) -> CodexTelemetryCursor {
    CodexTelemetryCursor {
        maximum_row_id: checkpoint.maximum_row_id,
        checkpoint_row_fingerprint: checkpoint.checkpoint_row_fingerprint.clone(),
        database_size: checkpoint.database_size,
        database_modified_nanos: checkpoint.database_modified_nanos,
        wal_size: checkpoint.wal_size,
        wal_modified_nanos: checkpoint.wal_modified_nanos,
    }
}

pub(crate) fn codex_telemetry_checkpoint_row_fingerprint(
    connection: &Connection,
    row_id: i64,
) -> Option<String> {
    if row_id == 0 {
        return Some(hash_text("codex-telemetry-checkpoint-row.v1:empty"));
    }
    let (seconds, nanos, body) = connection
        .query_row(
            "SELECT ts, ts_nanos, feedback_log_body FROM logs WHERE id = ?1",
            [row_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok()?;
    Some(hash_text(&format!(
        "codex-telemetry-checkpoint-row.v1:{row_id}:{seconds}:{nanos}:{}",
        body.as_deref()
            .map_or_else(|| "none".to_string(), hash_text,)
    )))
}

pub(crate) fn collect_codex_telemetry_evidence(
    source: &SourceLocation,
    root: &Path,
    checkpoints: &[AccountEvidenceCheckpointV1],
    scan: &mut AccountEvidenceScan,
) -> Result<()> {
    let account_attribute = Regex::new(
        r#"(?i)(?:\"?user\.account_id\"?|\"?user_account_id\"?)\s*[:=]\s*[\"']?([a-z0-9_:\-]{3,256})"#,
    )?;
    let email_attribute = Regex::new(
        r#"(?i)\"?user\.email\"?\s*[:=]\s*[\"']?([a-z0-9.!#$%&'*+/=?^_`{|}~\-]+@[a-z0-9.\-]+\.[a-z]{2,})"#,
    )?;
    let conversation_attribute = Regex::new(
        r#"(?i)(?:\"?conversation\.id\"?|\"?conversation_id\"?|\"?conversationId\"?)\s*[:=]\s*[\"']?([a-z0-9_:\-]{3,256})"#,
    )?;
    let auth_mode_attribute = Regex::new(
        r#"(?i)(?:\"?auth\.mode\"?|\"?auth_mode\"?)\s*[:=]\s*[\"']?([a-z0-9_:\-]{2,64})"#,
    )?;
    let app_version_attribute = Regex::new(
        r#"(?i)(?:\"?app\.version\"?|\"?application_version\"?|\"?cli_version\"?)\s*[:=]\s*[\"']?([a-z0-9.+_\-]{1,64})"#,
    )?;
    let event_name = Regex::new(r#"(?:^|\s)event\.name\s*=\s*\"([a-z0-9._-]+)\""#)?;
    // Newer builds emit the reload line at the end of a tracing span context
    // (`app_server.request{...}: Reloading auth for account <id>`), so the
    // message is accepted either at the start of the body or right after a
    // span close, and nothing may follow the account id. Requiring the id to
    // end the body keeps quoted copies of this phrase inside logged user
    // content from ever being read as a reload.
    let auth_reload =
        Regex::new(r#"(?i)(?:^\s*|\}:\s*)Reloading auth for account\s+([a-z0-9_:\-]{3,256})\s*$"#)?;
    // Anything a user typed can reach this body verbatim inside one of these
    // fields. Identity attributes are only read from the structured prefix that
    // precedes the first of them, so a prompt quoting `user.account_id=...`
    // cannot mint a binding for an account this device never used.
    let free_text_attribute = Regex::new(
        r#"(?i)(?:^|\s)(?:prompt|message|text|content|body|input|output|arguments|args|command|reasoning|response|error)\s*[:=]"#,
    )?;

    for database_path in [
        root.join("logs_2.sqlite"),
        root.join("sqlite/logs_2.sqlite"),
    ] {
        if !database_path.is_file() {
            continue;
        }
        let Ok(connection) = open_sqlite_readonly(&database_path) else {
            continue;
        };
        let Ok(has_logs_table) = sqlite_table_exists(&connection, "logs") else {
            continue;
        };
        if !has_logs_table {
            continue;
        }
        let Ok(maximum_row_id) =
            connection.query_row("SELECT COALESCE(MAX(id), 0) FROM logs", [], |row| {
                row.get::<_, i64>(0)
            })
        else {
            continue;
        };
        let checkpoint_row_fingerprint =
            codex_telemetry_checkpoint_row_fingerprint(&connection, maximum_row_id);
        let current_state =
            codex_telemetry_file_state(&database_path, maximum_row_id, checkpoint_row_fingerprint);
        let path_hash = hash_text(&canonical_display(&database_path));
        let previous_state = checkpoints
            .iter()
            .find(|checkpoint| {
                checkpoint.source_id == source.source_id
                    && checkpoint.artifact_path_hash == path_hash
                    && checkpoint.parser_version == CODEX_ACCOUNT_EVIDENCE_PARSER_VERSION
            })
            .map(codex_telemetry_cursor);
        if previous_state.as_ref() == Some(&current_state) {
            continue;
        }
        let database_generation_matches = previous_state.as_ref().is_some_and(|previous| {
            previous
                .checkpoint_row_fingerprint
                .as_ref()
                .is_some_and(|expected| {
                    codex_telemetry_checkpoint_row_fingerprint(&connection, previous.maximum_row_id)
                        .as_ref()
                        == Some(expected)
                })
        });
        let minimum_row_id = previous_state
            .as_ref()
            .filter(|previous| {
                database_generation_matches
                    && maximum_row_id > previous.maximum_row_id
                    && current_state.database_size >= previous.database_size
            })
            .map_or(i64::MIN, |previous| previous.maximum_row_id);
        let Ok(mut statement) = connection.prepare(
            r#"
            SELECT id, ts, ts_nanos, target, feedback_log_body
            FROM logs
            WHERE feedback_log_body IS NOT NULL
              AND id > ?1
              AND (
                (
                  target IN ('codex_otel.log_only', 'codex_otel.trace_safe')
                  AND (
                    -- Both spellings the identity regex accepts have to appear
                    -- here. Selecting only the dotted attribute discarded rows
                    -- that carry `user_account_id` without an email before the
                    -- parser ever saw them, so that telemetry produced no
                    -- evidence at all.
                    instr(feedback_log_body, 'user.account_id') > 0
                    OR instr(feedback_log_body, 'user_account_id') > 0
                    OR instr(feedback_log_body, 'user.email') > 0
                  )
                )
                OR (
                  (
                    target LIKE 'codex_core::auth%'
                    OR target = 'codex_login::auth::manager'
                  )
                  AND instr(feedback_log_body, 'Reloading auth for account') > 0
                )
              )
            ORDER BY id
            "#,
        ) else {
            continue;
        };
        let Ok(rows) = statement.query_map([minimum_row_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        }) else {
            continue;
        };
        let mut read_complete = true;
        for row in rows {
            let Ok((row_id, seconds, nanos, target, body)) = row else {
                read_complete = false;
                break;
            };
            let Some(observed_at) = Utc
                .timestamp_opt(seconds, nanos.clamp(0, 999_999_999) as u32)
                .single()
            else {
                continue;
            };
            // Only the structured attribute prefix is trusted; everything from
            // the first free-text field onward is user content.
            let attributes = free_text_attribute
                .find(&body)
                .map_or(body.as_str(), |free_text| &body[..free_text.start()]);
            // A body naming the event twice is not a body this parser
            // understands. Reading only the first match let a second, injected
            // `event.name` sit unnoticed beside the real one.
            let mut event_names = event_name
                .captures_iter(attributes)
                .filter_map(|captures| captures.get(1).map(|value| value.as_str()));
            let declared_event_name = event_names.next();
            // Identity is not tied to one event name: this Codex generation
            // carries `user.account_id`/`user.email` on ordinary telemetry
            // events (`codex.turn_ttft`, `codex.tool_decision`, ...) and emits
            // `codex.conversation_starts` rarely or never. Any single declared
            // `codex.*` event in the trusted prefix qualifies; the free-text
            // cut above and the sole-attribute rule below still decide what
            // may be read from it.
            let structured_identity_event = matches!(
                target.as_str(),
                "codex_otel.log_only" | "codex_otel.trace_safe"
            ) && declared_event_name
                .is_some_and(|name| name.starts_with("codex."))
                && event_names.next().is_none();
            let reload_account_id = (target.starts_with("codex_core::auth")
                || target == "codex_login::auth::manager")
                .then(|| {
                    auth_reload
                        .captures(&body)
                        .and_then(|captures| captures.get(1))
                        // A reload line sitting at or past the first free-text
                        // field is quoted content, not an auth event.
                        .filter(|account| {
                            free_text_attribute
                                .find(&body)
                                .is_none_or(|free_text| account.start() < free_text.start())
                        })
                        .map(|value| value.as_str().to_string())
                })
                .flatten();
            // A repeated attribute is ambiguous evidence, not two facts, so an
            // event that states an identity twice states it for nobody.
            let sole_attribute = |pattern: &Regex| -> Option<String> {
                let mut matches = pattern
                    .captures_iter(attributes)
                    .filter_map(|captures| captures.get(1).map(|value| value.as_str().to_string()));
                let first = matches.next()?;
                matches.next().is_none().then_some(first)
            };
            let provider_user_id = reload_account_id.clone().or_else(|| {
                structured_identity_event.then(|| sole_attribute(&account_attribute))?
            });
            let email = structured_identity_event
                .then(|| sole_attribute(&email_attribute))
                .flatten()
                .map(|value| normalize_email(&value));
            if provider_user_id.is_none() && email.is_none() {
                continue;
            }
            let conversation_id = structured_identity_event
                .then(|| sole_attribute(&conversation_attribute))
                .flatten();
            let auth_mode = structured_identity_event
                .then(|| sole_attribute(&auth_mode_attribute))
                .flatten();
            let application_version = structured_identity_event
                .then(|| sole_attribute(&app_version_attribute))
                .flatten();
            let provider_account_id = provider_account_id_from_identity(
                CODEX_PROVIDER,
                provider_user_id.as_deref(),
                email.as_deref(),
            );
            let Some(provider_account_id) = provider_account_id else {
                continue;
            };
            let evidence_kind = if reload_account_id.is_some() {
                AccountEvidenceKind::AuthReload
            } else {
                AccountEvidenceKind::TelemetryIdentity
            };
            let conversation_id_hash = conversation_id.as_deref().map(hash_text);
            let record_fingerprint = hash_text(&format!(
                "codex-telemetry-identity.v1:{row_id}:{}:{}:{}:{}",
                provider_user_id.as_deref().unwrap_or("none"),
                email.as_deref().unwrap_or("none"),
                conversation_id.as_deref().unwrap_or("none"),
                observed_at.to_rfc3339()
            ));
            scan.accounts.push(ObservedProviderAccount {
                provider_user_id: provider_user_id.clone(),
                email: email.clone(),
                plan_name: None,
                observed_at,
            });
            scan.identity_observations
                .push(AccountIdentityObservationV1 {
                    schema_version: ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
                    observation_id: account_identity_observation_id(
                        &source.source_id,
                        evidence_kind,
                        observed_at,
                        &record_fingerprint,
                    ),
                    provider: CODEX_PROVIDER.to_string(),
                    source_id: source.source_id.clone(),
                    provider_account_id: Some(provider_account_id.clone()),
                    provider_user_id_hash: provider_user_id.as_deref().map(hash_text),
                    email_hash: email.as_deref().map(hash_text),
                    conversation_id_hash: conversation_id_hash.clone(),
                    turn_id_hash: None,
                    observed_at,
                    evidence_kind,
                    confidence: Confidence::High,
                    auth_mode,
                    application_version,
                    parser_version: CODEX_ACCOUNT_EVIDENCE_PARSER_VERSION.to_string(),
                    artifact_kind: "logs_2_sqlite".to_string(),
                    artifact_path_hash: path_hash.clone(),
                    record_fingerprint,
                });
            if let Some(conversation_id_hash) = conversation_id_hash {
                scan.conversation_bindings
                    .push(ConversationAccountBindingV1 {
                        schema_version: CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
                        binding_id: conversation_account_binding_id(
                            &source.source_id,
                            &conversation_id_hash,
                            None,
                            &provider_account_id,
                        ),
                        provider: CODEX_PROVIDER.to_string(),
                        source_id: source.source_id.clone(),
                        provider_account_id,
                        conversation_id_hash,
                        turn_id_hash: None,
                        observed_at,
                        evidence_kind,
                        confidence: Confidence::High,
                    });
            }
        }
        if read_complete {
            scan.checkpoints.push(AccountEvidenceCheckpointV1 {
                schema_version: ACCOUNT_EVIDENCE_CHECKPOINT_SCHEMA_VERSION.to_string(),
                source_id: source.source_id.clone(),
                artifact_path_hash: path_hash,
                parser_version: CODEX_ACCOUNT_EVIDENCE_PARSER_VERSION.to_string(),
                maximum_row_id: current_state.maximum_row_id,
                checkpoint_row_fingerprint: current_state.checkpoint_row_fingerprint.clone(),
                database_size: current_state.database_size,
                database_modified_nanos: current_state.database_modified_nanos,
                wal_size: current_state.wal_size,
                wal_modified_nanos: current_state.wal_modified_nanos,
            });
        }
    }
    Ok(())
}

pub(crate) fn collect_codex_reset_history_evidence(
    source: &SourceLocation,
    root: &Path,
    scan: &mut AccountEvidenceScan,
) {
    let state_path = root.join(".codex-global-state.json");
    let Some(value) = read_json_file(&state_path) else {
        return;
    };
    let Some(entries) = value
        .pointer("/electron-persisted-atom-state/codex-rate-limit-reset-history")
        .and_then(Value::as_array)
    else {
        return;
    };
    let artifact_path_hash = hash_text(&canonical_display(&state_path));
    for entry in entries {
        let Some(provider_user_id) = string_at_any(entry, &["accountId", "account_id"]) else {
            continue;
        };
        let Some(conversation_id) = string_at_any(entry, &["conversationId", "conversation_id"])
        else {
            continue;
        };
        let turn_id = string_at_any(entry, &["turnId", "turn_id"]);
        let Some(observed_at) = entry
            .get("occurredAtMs")
            .or_else(|| entry.get("occurred_at_ms"))
            .and_then(timestamp_from_scalar)
        else {
            continue;
        };
        let Some(provider_account_id) =
            provider_account_id_from_identity(CODEX_PROVIDER, Some(&provider_user_id), None)
        else {
            continue;
        };
        let conversation_id_hash = hash_text(&conversation_id);
        let turn_id_hash = turn_id.as_deref().map(hash_text);
        let record_fingerprint = hash_text(&format!(
            "codex-reset-history.v1:{provider_user_id}:{conversation_id}:{}:{}",
            turn_id.as_deref().unwrap_or("none"),
            observed_at.to_rfc3339()
        ));
        scan.accounts.push(ObservedProviderAccount {
            provider_user_id: Some(provider_user_id.clone()),
            email: None,
            plan_name: None,
            observed_at,
        });
        scan.identity_observations
            .push(AccountIdentityObservationV1 {
                schema_version: ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
                observation_id: account_identity_observation_id(
                    &source.source_id,
                    AccountEvidenceKind::ResetHistory,
                    observed_at,
                    &record_fingerprint,
                ),
                provider: CODEX_PROVIDER.to_string(),
                source_id: source.source_id.clone(),
                provider_account_id: Some(provider_account_id.clone()),
                provider_user_id_hash: Some(hash_text(&provider_user_id)),
                email_hash: None,
                conversation_id_hash: Some(conversation_id_hash.clone()),
                turn_id_hash: turn_id_hash.clone(),
                observed_at,
                evidence_kind: AccountEvidenceKind::ResetHistory,
                confidence: Confidence::High,
                auth_mode: None,
                application_version: None,
                parser_version: CODEX_ACCOUNT_EVIDENCE_PARSER_VERSION.to_string(),
                artifact_kind: "global_state_reset_history".to_string(),
                artifact_path_hash: artifact_path_hash.clone(),
                record_fingerprint,
            });
        scan.conversation_bindings
            .push(ConversationAccountBindingV1 {
                schema_version: CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
                binding_id: conversation_account_binding_id(
                    &source.source_id,
                    &conversation_id_hash,
                    turn_id_hash.as_deref(),
                    &provider_account_id,
                ),
                provider: CODEX_PROVIDER.to_string(),
                source_id: source.source_id.clone(),
                provider_account_id,
                conversation_id_hash,
                turn_id_hash,
                observed_at,
                evidence_kind: AccountEvidenceKind::ResetHistory,
                confidence: Confidence::High,
            });
    }
}

pub(crate) fn collect_codex_login_evidence(
    source: &SourceLocation,
    root: &Path,
    scan: &mut AccountEvidenceScan,
) -> Result<()> {
    let log_directory = root.join("log");
    let mut login_paths = std::fs::read_dir(&log_directory)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            (name == "codex-login.log" || name.starts_with("codex-login.log."))
                .then(|| entry.path())
        })
        .collect::<Vec<_>>();
    login_paths.sort();
    for login_path in login_paths {
        let Ok(file) = File::open(&login_path) else {
            continue;
        };
        let artifact_path_hash = hash_text(&canonical_display(&login_path));
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            match read_bounded_jsonl_line(&mut reader, &mut line, 64 * 1024)? {
                BoundedLineRead::Eof => break,
                BoundedLineRead::Oversized => continue,
                BoundedLineRead::Complete => {}
            }
            let Ok(text) = std::str::from_utf8(&line) else {
                continue;
            };
            let lower = text.to_ascii_lowercase();
            if ![
                "successfully logged in",
                "login successful",
                "authentication successful",
                "login completed",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
            {
                continue;
            }
            let Some(timestamp) = text.split_whitespace().next() else {
                continue;
            };
            let Ok(observed_at) = DateTime::parse_from_rfc3339(timestamp) else {
                continue;
            };
            let observed_at = observed_at.with_timezone(&Utc);
            // Identify the login by what it says and when, never by where it
            // currently sits. Hashing the file and line number meant the same
            // login was a new observation the moment `codex-login.log` rotated
            // to `codex-login.log.1`, and these rows are append-only, so every
            // rotation permanently doubled the login history.
            let record_fingerprint = hash_text(&format!(
                "codex-login-success.v1:{}:{}",
                observed_at.to_rfc3339(),
                text.trim()
            ));
            scan.identity_observations
                .push(AccountIdentityObservationV1 {
                    schema_version: ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
                    observation_id: account_identity_observation_id(
                        &source.source_id,
                        AccountEvidenceKind::LoginSuccess,
                        observed_at,
                        &record_fingerprint,
                    ),
                    provider: CODEX_PROVIDER.to_string(),
                    source_id: source.source_id.clone(),
                    provider_account_id: None,
                    provider_user_id_hash: None,
                    email_hash: None,
                    conversation_id_hash: None,
                    turn_id_hash: None,
                    observed_at,
                    evidence_kind: AccountEvidenceKind::LoginSuccess,
                    confidence: Confidence::Low,
                    auth_mode: None,
                    application_version: None,
                    parser_version: CODEX_ACCOUNT_EVIDENCE_PARSER_VERSION.to_string(),
                    artifact_kind: "codex_login_log".to_string(),
                    artifact_path_hash: artifact_path_hash.clone(),
                    record_fingerprint,
                });
        }
    }
    Ok(())
}
