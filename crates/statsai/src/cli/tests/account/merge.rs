use super::*;

#[test]
fn merge_provider_accounts_moves_source_records_and_prunes_alias() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "codex-local-jsonl",
        "0",
        Path::new("/tmp/.codex-work"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc
        .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
        .single()
        .expect("now");
    let alias = test_account("codex", Some("work"), None, None, None, now);
    let canonical = test_account(
        "codex",
        None,
        Some("verified@example.com"),
        Some("11111111-2222-4333-8444-555555555555"),
        Some("Plus"),
        now,
    );
    store.upsert_account(&alias).expect("alias account");
    store.upsert_account(&canonical).expect("canonical account");
    let assignment = test_assignment(
        &source,
        &alias.provider_account_id,
        now - Duration::days(40),
        None,
        now,
    );
    store
        .upsert_source_account_assignment(&assignment)
        .expect("assignment");

    let mut event = test_event(
        "codex",
        &source,
        now - Duration::days(2),
        Some(alias.provider_account_id.clone()),
        TokenParts::total(120),
    );
    event.parse_evidence = Some(statsai_core::ParseEvidence {
        event_key_version: "test".to_string(),
        source_file_path_hash: source.path_hash.clone(),
        source_line_number: None,
        source_record_id: Some("event".to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::Unknown,
    });
    let mut summary = test_summary(
        "codex",
        &source,
        now,
        300,
        Some(alias.provider_account_id.clone()),
    );
    summary.parse_evidence = Some(statsai_core::ParseEvidence {
        event_key_version: "test".to_string(),
        source_file_path_hash: source.path_hash.clone(),
        source_line_number: None,
        source_record_id: Some("summary".to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::Unknown,
    });
    store.insert_event(&event).expect("event");
    store.upsert_summary(&summary).expect("summary");
    let identity = statsai_core::AccountIdentityObservationV1 {
        schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
        observation_id: "identity-merge-alias".to_string(),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: Some(alias.provider_account_id.clone()),
        provider_user_id_hash: Some("a".repeat(64)),
        email_hash: None,
        conversation_id_hash: Some("b".repeat(64)),
        turn_id_hash: None,
        observed_at: now,
        evidence_kind: statsai_core::AccountEvidenceKind::TelemetryIdentity,
        confidence: Confidence::High,
        auth_mode: Some("chatgpt".to_string()),
        application_version: None,
        parser_version: "test.v1".to_string(),
        artifact_kind: "test".to_string(),
        artifact_path_hash: "c".repeat(64),
        record_fingerprint: "d".repeat(64),
    };
    let plan = statsai_core::AccountPlanObservationV1 {
        schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
        observation_id: account_plan_observation_id(
            &source.source_id,
            Some(&alias.provider_account_id),
            "plus",
            now,
            statsai_core::AccountEvidenceKind::AuthSnapshot,
        ),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: Some(alias.provider_account_id.clone()),
        raw_plan_name: "plus".to_string(),
        plan_name: "Plus".to_string(),
        observed_at: now,
        active_from: None,
        active_until: None,
        is_current_snapshot: true,
        evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
        confidence: Confidence::High,
        parser_version: "test.v1".to_string(),
        artifact_path_hash: "c".repeat(64),
        record_fingerprint: "e".repeat(64),
    };
    let binding = statsai_core::ConversationAccountBindingV1 {
        schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
        binding_id: conversation_account_binding_id(
            &source.source_id,
            &"b".repeat(64),
            None,
            &alias.provider_account_id,
        ),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: alias.provider_account_id.clone(),
        conversation_id_hash: "b".repeat(64),
        turn_id_hash: None,
        observed_at: now,
        evidence_kind: statsai_core::AccountEvidenceKind::ResetHistory,
        confidence: Confidence::High,
    };
    store
        .upsert_account_identity_observations(std::slice::from_ref(&identity))
        .expect("identity evidence");
    store
        .upsert_account_plan_observations(std::slice::from_ref(&plan))
        .expect("plan evidence");
    store
        .upsert_conversation_account_bindings(std::slice::from_ref(&binding))
        .expect("conversation binding");

    let target = "https://api.example.com/api/sync/batches";
    store
        .record_sources_synced("http", target, &[sanitize_source_for_sync(source.clone())])
        .expect("sync source");
    store
        .record_accounts_synced(
            "http",
            target,
            &[
                sanitize_account_for_sync(alias.clone()),
                sanitize_account_for_sync(canonical.clone()),
            ],
        )
        .expect("sync accounts");
    store
        .record_source_account_assignments_synced(
            "http",
            target,
            &[sanitize_source_account_assignment_for_sync(
                assignment.clone(),
            )],
        )
        .expect("sync assignments");
    store
        .record_sync_success("http", target, "batch_1", &[], &[], None)
        .expect("sync success");

    let report = merge_provider_accounts(&store, "codex", "work", "verified@example.com", false)
        .expect("merge");

    assert_eq!(report.moved_source_account_assignments, 1);
    assert_eq!(report.moved_subscriptions, 0);
    assert_eq!(report.moved_events, 1);
    assert_eq!(report.moved_summaries, 1);
    assert_eq!(report.moved_identity_observations, 1);
    assert_eq!(report.moved_plan_observations, 1);
    assert_eq!(report.moved_conversation_bindings, 1);
    assert!(report.deleted_source_account);
    assert!(report.reset_local_sync_tracking);
    assert_eq!(report.remaining_references.total(), 0);

    let accounts = store.list_accounts().expect("accounts");
    assert!(!accounts
        .iter()
        .any(|account| account.provider_account_id == alias.provider_account_id));
    assert!(accounts
        .iter()
        .any(|account| account.provider_account_id == canonical.provider_account_id));

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(
        assignments[0].provider_account_id,
        canonical.provider_account_id
    );

    let events = store.events_for_source(&source.source_id).expect("events");
    assert_eq!(
        events[0].provider_account_id,
        Some(canonical.provider_account_id.clone())
    );
    let summaries = store
        .summaries_for_source(&source.source_id)
        .expect("summaries");
    assert_eq!(
        summaries[0].provider_account_id,
        Some(canonical.provider_account_id.clone())
    );
    assert!(store
        .account_identity_observations(None)
        .expect("identity evidence")
        .iter()
        .all(|observation| {
            observation.provider_account_id.as_ref() == Some(&canonical.provider_account_id)
        }));
    let plans = store.account_plan_observations().expect("plan evidence");
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].provider_account_id.as_ref(),
        Some(&canonical.provider_account_id)
    );
    assert_ne!(plans[0].observation_id, plan.observation_id);
    let bindings = store
        .conversation_account_bindings(None)
        .expect("conversation bindings");
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        bindings[0].provider_account_id,
        canonical.provider_account_id
    );
    assert_ne!(bindings[0].binding_id, binding.binding_id);

    assert!(store.list_sync_states().expect("sync states").is_empty());
    let sync_accounts: Vec<_> = store
        .list_accounts()
        .expect("accounts after merge")
        .into_iter()
        .map(sanitize_account_for_sync)
        .collect();
    let pending = store
        .pending_accounts_for_sync("http", target, &sync_accounts)
        .expect("pending accounts");
    assert_eq!(pending.len(), sync_accounts.len());
}

#[test]
fn merge_provider_accounts_moves_orphan_summary_rows() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "codex-local-jsonl",
        "0",
        Path::new("/tmp/.codex-legacy-alias"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc
        .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
        .single()
        .expect("now");
    let alias = test_account("codex", Some("legacy-alias"), None, None, None, now);
    let canonical = test_account(
        "codex",
        None,
        Some("canonical@example.com"),
        Some("stable-provider-id"),
        Some("Plus"),
        now,
    );
    store.upsert_account(&alias).expect("alias account");
    store.upsert_account(&canonical).expect("canonical account");

    let mut summary = test_summary(
        "codex",
        &source,
        now - Duration::days(10),
        512,
        Some(alias.provider_account_id.clone()),
    );
    summary.parse_evidence = Some(statsai_core::ParseEvidence {
        event_key_version: "test".to_string(),
        source_file_path_hash: source.path_hash.clone(),
        source_line_number: None,
        source_record_id: Some("summary".to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::Unknown,
    });
    store.upsert_summary(&summary).expect("summary");

    let report = merge_provider_accounts(
        &store,
        "codex",
        "legacy-alias",
        "canonical@example.com",
        false,
    )
    .expect("merge");

    assert_eq!(report.moved_source_account_assignments, 0);
    assert_eq!(report.moved_subscriptions, 0);
    assert_eq!(report.moved_events, 0);
    assert_eq!(report.moved_summaries, 1);
    assert!(report.deleted_source_account);
    assert_eq!(report.remaining_references.total(), 0);

    let summaries = store.summaries().expect("summaries");
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].provider_account_id,
        Some(canonical.provider_account_id.clone())
    );
    assert_eq!(
        summaries[0]
            .parse_evidence
            .as_ref()
            .map(|evidence| evidence.account_identity_source.clone()),
        Some(IdentitySource::UserConfigured)
    );
    assert!(store
        .list_accounts()
        .expect("accounts")
        .into_iter()
        .all(|account| account.provider_account_id != alias.provider_account_id));
}
