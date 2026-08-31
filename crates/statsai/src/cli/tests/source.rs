use super::support::*;
use super::*;

struct AccountEvidenceTrackingAdapter {
    source: SourceLocation,
    candidate: ScanCandidateFile,
    event: UsageEvent,
    collect_calls: Arc<Mutex<u64>>,
}

impl ProviderAdapter for AccountEvidenceTrackingAdapter {
    fn id(&self) -> &'static str {
        "test-account-evidence"
    }

    fn version(&self) -> &'static str {
        "0"
    }

    fn provider(&self) -> &'static str {
        "codex"
    }

    fn discover(&self) -> Vec<SourceLocation> {
        vec![self.source.clone()]
    }

    fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        Ok(vec![self.candidate.clone()])
    }

    fn collect_account_evidence(
        &self,
        _source: &SourceLocation,
        _checkpoints: &[statsai_core::AccountEvidenceCheckpointV1],
    ) -> Result<AccountEvidenceScan> {
        *self.collect_calls.lock().expect("collect calls") += 1;
        Ok(AccountEvidenceScan::default())
    }

    fn scan(
        &self,
        _source: &SourceLocation,
        _options: &ScanOptions,
    ) -> Result<statsai_adapters::AdapterScan> {
        Ok(statsai_adapters::AdapterScan {
            events: vec![self.event.clone()],
            ..statsai_adapters::AdapterScan::default()
        })
    }
}

fn seed_test_account_evidence(store: &Store, source: &SourceLocation, observed_at: DateTime<Utc>) {
    let account_id = ProviderAccountId("account-source-cleanup".to_string());
    store
        .upsert_account_identity_observations(&[statsai_core::AccountIdentityObservationV1 {
            schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: "identity-source-cleanup".to_string(),
            provider: source.provider.clone(),
            source_id: source.source_id.clone(),
            provider_account_id: Some(account_id.clone()),
            provider_user_id_hash: Some("a".repeat(64)),
            email_hash: None,
            conversation_id_hash: Some("b".repeat(64)),
            turn_id_hash: None,
            observed_at,
            evidence_kind: statsai_core::AccountEvidenceKind::TelemetryIdentity,
            confidence: Confidence::High,
            auth_mode: Some("chatgpt".to_string()),
            application_version: None,
            parser_version: "test.v1".to_string(),
            artifact_kind: "test".to_string(),
            artifact_path_hash: "c".repeat(64),
            record_fingerprint: "d".repeat(64),
        }])
        .expect("identity evidence");
    store
        .upsert_account_plan_observations(&[statsai_core::AccountPlanObservationV1 {
            schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: "plan-source-cleanup".to_string(),
            provider: source.provider.clone(),
            source_id: source.source_id.clone(),
            provider_account_id: Some(account_id.clone()),
            raw_plan_name: "pro".to_string(),
            plan_name: "Pro".to_string(),
            observed_at,
            active_from: None,
            active_until: None,
            is_current_snapshot: false,
            evidence_kind: statsai_core::AccountEvidenceKind::QuotaStatus,
            confidence: Confidence::High,
            parser_version: "test.v1".to_string(),
            artifact_path_hash: "c".repeat(64),
            record_fingerprint: "e".repeat(64),
        }])
        .expect("plan evidence");
    store
        .upsert_conversation_account_bindings(&[statsai_core::ConversationAccountBindingV1 {
            schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
            binding_id: "binding-source-cleanup".to_string(),
            provider: source.provider.clone(),
            source_id: source.source_id.clone(),
            provider_account_id: account_id,
            conversation_id_hash: "b".repeat(64),
            turn_id_hash: None,
            observed_at,
            evidence_kind: statsai_core::AccountEvidenceKind::ResetHistory,
            confidence: Confidence::High,
        }])
        .expect("conversation evidence");
}

#[test]
fn configured_claude_projects_path_normalizes_to_config_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(&projects).expect("projects");

    let normalized =
        normalize_configured_source_path("claude_code", &projects).expect("normalized path");

    assert_eq!(
        normalized,
        dir.path().canonicalize().expect("canonical dir")
    );
}

#[test]
fn configured_codex_sessions_path_normalizes_to_codex_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");

    let normalized = normalize_configured_source_path("codex", &sessions).expect("normalized path");

    assert_eq!(
        normalized,
        dir.path().canonicalize().expect("canonical dir")
    );
}

#[test]
fn configured_opencode_db_path_normalizes_to_data_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("opencode.db");
    std::fs::write(&db, "").expect("db");

    let normalized = normalize_configured_source_path("opencode", &db).expect("normalized path");

    assert_eq!(
        normalized,
        dir.path().canonicalize().expect("canonical dir")
    );
}

#[test]
fn configured_grok_sessions_path_normalizes_to_home_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");

    let normalized =
        normalize_configured_source_path("grok-build", &sessions).expect("normalized path");

    assert_eq!(
        normalized,
        dir.path().canonicalize().expect("canonical dir")
    );
}

#[test]
fn persist_source_upserts_into_store() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-preview-source"),
        LocationOrigin::Configured,
    );

    persist_source_after_preview(&store, &source).expect("persist");

    assert_eq!(store.list_sources().expect("sources").len(), 1);
}

#[test]
fn configured_source_overrides_discovered_source_for_same_path() {
    let discovered = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-merge"),
        LocationOrigin::Default,
    );
    let configured = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-merge"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![discovered],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let sources = scan_sources_for_adapter(&adapter, &[configured]);

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].location_origin, LocationOrigin::Configured);
}

#[test]
fn disabled_configured_source_suppresses_matching_discovered_source() {
    let matching = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-disabled"),
        LocationOrigin::Default,
    );
    let unrelated = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-enabled"),
        LocationOrigin::Default,
    );
    let mut disabled = SourceLocation::local_adapter(
        "claude",
        "test",
        "0",
        Path::new("/tmp/claude-disabled"),
        LocationOrigin::Configured,
    );
    disabled.enabled = false;
    let adapter = TestAdapter {
        provider: "claude_code",
        discovered: vec![matching, unrelated.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let sources = scan_sources_for_adapter(&adapter, &[disabled]);

    assert_eq!(sources, vec![unrelated]);
}

#[test]
fn configured_parent_source_suppresses_discovered_child_source() {
    let discovered = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/statsai-claude/projects"),
        LocationOrigin::Default,
    );
    let configured = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/statsai-claude"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "claude_code",
        discovered: vec![discovered],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let sources = scan_sources_for_adapter(&adapter, &[configured]);

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].location_origin, LocationOrigin::Configured);
    assert_eq!(
        sources[0].path_label.as_deref(),
        Some("/tmp/statsai-claude")
    );
}

#[test]
fn codex_nested_source_is_not_shadowed_by_parent_source() {
    let discovered_parent = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex"),
        LocationOrigin::Env,
    );
    let configured_child = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex/.codex"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![discovered_parent.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let mut sources = scan_sources_for_adapter(&adapter, std::slice::from_ref(&configured_child));
    sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
    assert_eq!(
        sources[1].path_label.as_deref(),
        Some("/tmp/statsai-codex/.codex")
    );
}

#[test]
fn codex_nested_sessions_source_is_shadowed_by_parent_source() {
    let discovered_parent = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex"),
        LocationOrigin::Env,
    );
    let configured_child = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex/sessions"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![discovered_parent.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let sources = scan_sources_for_adapter(&adapter, &[configured_child]);

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
}

#[test]
fn codex_source_under_nested_codex_root_is_not_shadowed_by_parent_source() {
    let discovered_parent = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex"),
        LocationOrigin::Env,
    );
    let configured_child = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex/.codex/sessions"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![discovered_parent.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let mut sources = scan_sources_for_adapter(&adapter, &[configured_child]);
    sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
    assert_eq!(
        sources[1].path_label.as_deref(),
        Some("/tmp/statsai-codex/.codex/sessions")
    );
}

#[test]
fn codex_custom_named_nested_root_is_not_shadowed_by_parent_source() {
    let discovered_parent = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex"),
        LocationOrigin::Env,
    );
    let configured_child = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex/project-codex-home"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![discovered_parent.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let mut sources = scan_sources_for_adapter(&adapter, &[configured_child]);
    sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
    assert_eq!(
        sources[1].path_label.as_deref(),
        Some("/tmp/statsai-codex/project-codex-home")
    );
}

#[test]
fn non_local_sources_are_ignored_for_adapter_scans() {
    let configured_local = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-local"),
        LocationOrigin::Configured,
    );
    let configured_manual = SourceLocation::reported_usage(
        "codex",
        SourceKind::Manual,
        "reported-usage-summary",
        "0",
        "manual-note",
        None,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: Vec::new(),
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let sources =
        scan_sources_for_adapter(&adapter, &[configured_local.clone(), configured_manual]);

    assert_eq!(sources, vec![configured_local]);
}

#[test]
fn connect_source_to_account_closes_existing_open_connection() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-connect"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let first_start = Utc
        .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
        .single()
        .expect("first");
    let second_start = Utc
        .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("second");

    connect_source_to_account(
        &store,
        ConnectSourceToAccountInput {
            source_id: &source.source_id,
            provider_account_id_value: None,
            provider_user_id: None,
            email: Some("first@example.com"),
            label: None,
            started_at: first_start,
            ended_at: None,
        },
    )
    .expect("first connect");
    connect_source_to_account(
        &store,
        ConnectSourceToAccountInput {
            source_id: &source.source_id,
            provider_account_id_value: None,
            provider_user_id: None,
            email: Some("second@example.com"),
            label: None,
            started_at: second_start,
            ended_at: None,
        },
    )
    .expect("second connect");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 2);
    assert_eq!(assignments[0].ended_at, Some(second_start));
    assert_eq!(assignments[1].started_at, second_start);
}

#[test]
fn manual_source_reassignment_rebuilds_quota_plan_evidence() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-quota-plan-reassignment"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let mut quota = test_unattributed_quota_record(&source.source_id.0);
    quota.observation.status.plan_type = Some("pro".to_string());
    store
        .upsert_quota_observations(&[quota])
        .expect("quota observation");
    let first_start = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("first start");
    let second_start = Utc
        .with_ymd_and_hms(2026, 8, 15, 0, 0, 0)
        .single()
        .expect("second start");

    let first = connect_source_to_account(
        &store,
        ConnectSourceToAccountInput {
            source_id: &source.source_id,
            provider_account_id_value: None,
            provider_user_id: None,
            email: Some("first-quota@example.com"),
            label: None,
            started_at: first_start,
            ended_at: None,
        },
    )
    .expect("first connection");
    store
        .rebuild_quota_plan_observations_for_source(&source.source_id)
        .expect("seed quota plan evidence");
    assert_eq!(
        store.account_plan_observations().expect("initial plan")[0].provider_account_id,
        Some(first.provider_account_id)
    );

    let second = connect_source_to_account(
        &store,
        ConnectSourceToAccountInput {
            source_id: &source.source_id,
            provider_account_id_value: None,
            provider_user_id: None,
            email: Some("second-quota@example.com"),
            label: None,
            started_at: second_start,
            ended_at: None,
        },
    )
    .expect("second connection");

    let observations = store.account_plan_observations().expect("reassigned plan");
    assert_eq!(observations.len(), 1);
    assert_eq!(
        observations[0].provider_account_id,
        Some(second.provider_account_id)
    );
    assert_eq!(
        observations[0].evidence_kind,
        statsai_core::AccountEvidenceKind::QuotaStatus
    );
}

#[test]
fn connect_source_to_account_preserves_tail_when_replacing_finite_window() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-connect-tail"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let period_start = Utc
        .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
        .single()
        .expect("period start");
    let split_at = Utc
        .with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
        .single()
        .expect("split");
    let period_end = Utc
        .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("period end");
    let before_split = Utc
        .with_ymd_and_hms(2026, 5, 10, 12, 0, 0)
        .single()
        .expect("before split");
    let after_split = Utc
        .with_ymd_and_hms(2026, 5, 20, 12, 0, 0)
        .single()
        .expect("after split");
    store
        .insert_event(&test_event(
            "codex",
            &source,
            before_split,
            None,
            TokenParts::total(1),
        ))
        .expect("before event");
    store
        .insert_event(&test_event(
            "codex",
            &source,
            after_split,
            None,
            TokenParts::total(1),
        ))
        .expect("after event");

    connect_source_to_account(
        &store,
        ConnectSourceToAccountInput {
            source_id: &source.source_id,
            provider_account_id_value: None,
            provider_user_id: None,
            email: Some("first@example.com"),
            label: None,
            started_at: period_start,
            ended_at: Some(period_end),
        },
    )
    .expect("first connect");
    connect_source_to_account(
        &store,
        ConnectSourceToAccountInput {
            source_id: &source.source_id,
            provider_account_id_value: None,
            provider_user_id: None,
            email: Some("second@example.com"),
            label: None,
            started_at: period_start,
            ended_at: Some(split_at),
        },
    )
    .expect("second connect");

    let first_account = provider_account_id_from_identity("codex", None, Some("first@example.com"))
        .expect("first account");
    let second_account =
        provider_account_id_from_identity("codex", None, Some("second@example.com"))
            .expect("second account");
    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 2);
    assert!(assignments.iter().any(|assignment| {
        assignment.provider_account_id == second_account
            && assignment.started_at == period_start
            && assignment.ended_at == Some(split_at)
    }));
    assert!(assignments.iter().any(|assignment| {
        assignment.provider_account_id == first_account
            && assignment.started_at == split_at
            && assignment.ended_at == Some(period_end)
    }));

    let events = store
        .events_for_source(&source.source_id)
        .expect("source events");
    assert_eq!(events.len(), 2);
    let before = events
        .iter()
        .find(|event| event.session.started_at == before_split)
        .expect("before event");
    let after = events
        .iter()
        .find(|event| event.session.started_at == after_split)
        .expect("after event");
    assert_eq!(before.provider_account_id, Some(second_account));
    assert_eq!(after.provider_account_id, Some(first_account));
}

#[test]
fn connect_source_to_account_merges_same_account_and_backfills_boundary_events() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-connect-merge"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let original_start = Utc
        .with_ymd_and_hms(2026, 5, 28, 11, 31, 9)
        .single()
        .expect("original start");
    let extended_start = Utc
        .with_ymd_and_hms(2026, 5, 28, 0, 0, 0)
        .single()
        .expect("extended start");
    let boundary_event_at = Utc
        .with_ymd_and_hms(2026, 5, 28, 7, 23, 28)
        .single()
        .expect("boundary event");

    let event = test_event(
        "codex",
        &source,
        boundary_event_at,
        None,
        TokenParts::total(1),
    );
    store.insert_event(&event).expect("event");

    connect_source_to_account(
        &store,
        ConnectSourceToAccountInput {
            source_id: &source.source_id,
            provider_account_id_value: None,
            provider_user_id: None,
            email: Some("same-account@example.com"),
            label: None,
            started_at: original_start,
            ended_at: None,
        },
    )
    .expect("initial connect");

    connect_source_to_account(
        &store,
        ConnectSourceToAccountInput {
            source_id: &source.source_id,
            provider_account_id_value: None,
            provider_user_id: None,
            email: Some("same-account@example.com"),
            label: None,
            started_at: extended_start,
            ended_at: None,
        },
    )
    .expect("extended connect");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].started_at, extended_start);

    let events = store
        .events_for_source(&source.source_id)
        .expect("source events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].provider_account_id,
        provider_account_id_from_identity("codex", None, Some("same-account@example.com"))
    );
}

#[test]
fn source_explain_distinguishes_inferred_blocked_and_unavailable_auth() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-source-explain-auth"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let blocked = explain_source_with_observation(
        &store,
        &source,
        Some(&VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        }),
    )
    .expect("blocked explanation");
    let unavailable = explain_source_with_observation(
        &store,
        &source,
        Some(&VerifiedSourceObservation::Unavailable),
    )
    .expect("unavailable explanation");
    let inferred = explain_source_with_observation(
        &store,
        &source,
        Some(&VerifiedSourceObservation::Inferred {
            identity: Box::new(VerifiedSourceState {
                provider_user_id: Some("cached-account".to_string()),
                email: Some("cached@example.com".to_string()),
                account_label: None,
                plan_name: None,
                authenticated_at: None,
                verified_at: None,
                subscription: None,
            }),
            basis: SourceIdentityInference::CachedLocalProfile,
            settings_modified_at: None,
        }),
    )
    .expect("inferred explanation");

    assert_eq!(
        blocked.pointer("/detected_auth_state/status"),
        Some(&json!("attribution_blocked"))
    );
    assert_eq!(
        unavailable.pointer("/detected_auth_state/status"),
        Some(&json!("unavailable"))
    );
    assert_eq!(
        inferred.pointer("/detected_auth_state/status"),
        Some(&json!("inferred"))
    );
    assert_eq!(
        inferred.pointer("/detected_auth_state/state/basis"),
        Some(&json!("cached_local_profile"))
    );
}

#[test]
fn manual_only_source_ignores_verified_state_mutations() {
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-manual-only"),
        LocationOrigin::Configured,
    );
    source.verification_mode = SourceVerificationMode::ManualOnly;
    store.upsert_source(&source).expect("source");

    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("started_at");
    let verified_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
        .single()
        .expect("verified_at");
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: Some(VerifiedSourceState {
            provider_user_id: Some("acct-manual-only".to_string()),
            email: Some("manual-only@example.com".to_string()),
            account_label: None,
            plan_name: Some("Plus".to_string()),
            authenticated_at: Some(started_at),
            verified_at: Some(verified_at),
            subscription: Some(VerifiedSubscriptionState {
                plan_name: "Plus".to_string(),
                price: 2000,
                currency: "USD".to_string(),
                billing_period: BillingPeriod::Monthly,
                paid_at: Some(started_at),
                started_at,
                ended_at: None,
                current_period_ends_at: None,
                status: SubscriptionStatus::Active,
                verified_at: Some(verified_at),
            }),
        }),
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    assert!(store.list_accounts().expect("accounts").is_empty());
    assert!(store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments")
        .is_empty());
    assert!(store
        .list_subscriptions()
        .expect("subscriptions")
        .is_empty());
}

#[test]
fn manual_only_source_does_not_collect_or_apply_account_evidence() {
    let store = Store::in_memory().expect("store");
    let source_root = "/tmp/codex-manual-only-evidence";
    let file_path = "/tmp/codex-manual-only-evidence/session.jsonl";
    let mut source = SourceLocation::local_adapter(
        "codex",
        "test-account-evidence",
        "0",
        Path::new(source_root),
        LocationOrigin::Configured,
    );
    source.verification_mode = SourceVerificationMode::ManualOnly;
    store.upsert_source(&source).expect("source");
    let observed_at = Utc
        .with_ymd_and_hms(2026, 8, 23, 10, 0, 0)
        .single()
        .expect("observed at");
    seed_test_account_evidence(&store, &source, observed_at);

    let mut event = test_scan_event(&source, file_path, observed_at, "manual-event", 100);
    event.session.local_session_id_hash = Some("b".repeat(64));
    let collect_calls = Arc::new(Mutex::new(0));
    let adapter = AccountEvidenceTrackingAdapter {
        source: source.clone(),
        candidate: test_scan_candidate(file_path, "manual-evidence-v1"),
        event,
        collect_calls: Arc::clone(&collect_calls),
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    assert_eq!(*collect_calls.lock().expect("collect calls"), 0);
    let stored_event = store
        .events()
        .expect("events")
        .into_iter()
        .find(|item| item.source.source_record_id.as_deref() == Some("manual-event"))
        .expect("manual event");
    assert_eq!(stored_event.provider_account_id, None);
}

#[test]
fn disabled_source_mode_closes_verified_linkages() {
    let store = Store::in_memory().expect("store");
    let mut source_location = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-disable-verification"),
        LocationOrigin::Configured,
    );
    source_location.verified_state_hash = Some("verified-state".to_string());
    store.upsert_source(&source_location).expect("source");
    let started_at = Utc::now() - Duration::days(1);
    let account = upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "codex",
            provider_user_id: Some("acct-disable"),
            email: Some("disable@example.com"),
            label: None,
            plan_name: Some("Plus".to_string()),
            identity_source: Some(IdentitySource::LocalAuth),
            verified_at: Some(started_at),
        },
    )
    .expect("account");
    store
        .upsert_source_account_assignment(&SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: source_account_assignment_id(
                &source_location.source_id,
                &account.provider_account_id,
                started_at,
            ),
            source_id: source_location.source_id.clone(),
            provider: "codex".to_string(),
            provider_account_id: account.provider_account_id.clone(),
            started_at,
            ended_at: None,
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(started_at),
            created_at: started_at,
            updated_at: started_at,
        })
        .expect("assignment");
    store
        .upsert_subscription(&Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: subscription_id(
                "codex",
                &account.provider_account_id,
                "Plus",
                started_at,
            ),
            provider: "codex".to_string(),
            provider_account_id: account.provider_account_id.clone(),
            plan_name: "Plus".to_string(),
            price: 2000,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: Some(started_at),
            renewal_day: None,
            started_at,
            ended_at: None,
            current_period_ends_at: None,
            status: SubscriptionStatus::Active,
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(started_at),
            notes: None,
        })
        .expect("subscription");
    seed_test_account_evidence(&store, &source_location, started_at);

    source(
        SourceCommand {
            command: SourceSubcommand::Mode {
                source_id: Some(source_location.source_id.0.clone()),
                path: None,
                mode: "disabled".to_string(),
            },
        },
        &store,
        "device",
    )
    .expect("disable mode");

    let source = store
        .source(&source_location.source_id)
        .expect("source lookup")
        .expect("source exists");
    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    let subscriptions = store.list_subscriptions().expect("subscriptions");

    assert_eq!(source.verification_mode, SourceVerificationMode::Disabled);
    assert_eq!(source.verified_state_hash, None);
    assert_eq!(assignments.len(), 1);
    assert!(assignments[0].ended_at.is_some());
    assert_eq!(subscriptions.len(), 1);
    assert!(subscriptions[0].ended_at.is_some());
    assert!(store
        .account_identity_observations(Some(&source.source_id))
        .expect("identity evidence")
        .is_empty());
    assert!(store
        .account_plan_observations()
        .expect("plan evidence")
        .is_empty());
    assert!(store
        .conversation_account_bindings(Some(&source.source_id))
        .expect("conversation evidence")
        .is_empty());
}

#[test]
fn apply_verified_source_state_does_not_override_conflicting_manual_connection() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-manual-wins"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("started_at");
    let manual = connect_source_to_account(
        &store,
        ConnectSourceToAccountInput {
            source_id: &source.source_id,
            provider_account_id_value: None,
            provider_user_id: None,
            email: Some("manual@example.com"),
            label: Some("manual".to_string()),
            started_at,
            ended_at: None,
        },
    )
    .expect("manual connection");

    apply_verified_source_state(
        &store,
        &source,
        Some(&VerifiedSourceState {
            provider_user_id: Some("chatgpt-account-999".to_string()),
            email: Some("verified@example.com".to_string()),
            account_label: None,
            plan_name: Some("Plus".to_string()),
            authenticated_at: Some(started_at),
            verified_at: Some(started_at),
            subscription: Some(VerifiedSubscriptionState {
                plan_name: "Plus".to_string(),
                price: 2000,
                currency: "USD".to_string(),
                billing_period: BillingPeriod::Monthly,
                paid_at: Some(started_at),
                started_at,
                ended_at: None,
                current_period_ends_at: None,
                status: SubscriptionStatus::Active,
                verified_at: Some(started_at),
            }),
        }),
    )
    .expect("apply verified state");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(
        assignments[0].provider_account_id,
        manual.provider_account_id
    );
    assert_eq!(assignments[0].record_source, IdentitySource::UserConfigured);
}

#[test]
fn source_remove_delete_data_retires_committed_metrics_without_any_traces() {
    let repository = tempfile::TempDir::new().expect("temporary repository");
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "test@example.com"],
        &["config", "user.name", "Test"],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repository.path())
            .status()
            .expect("run git");
        assert!(status.success());
    }
    std::fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
    for args in [&["add", "main.rs"][..], &["commit", "-qm", "initial"]] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repository.path())
            .status()
            .expect("run git");
        assert!(status.success());
    }

    let store = Store::in_memory().expect("store");
    let committed_source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-committed-only"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&committed_source).expect("source");

    // Usage carries the project path, which is how committed churn is
    // discovered. This source has no archive and therefore no reconstructed
    // edits at all.
    let now = Utc
        .with_ymd_and_hms(2026, 6, 1, 10, 0, 0)
        .single()
        .expect("now");
    seed_test_account_evidence(&store, &committed_source, now);
    let mut summary = test_summary("codex", &committed_source, now, 100, None);
    summary.project = Some(ProjectInfo {
        project_id: "project-committed-only".to_string(),
        project_label: None,
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: None,
        path_label: Some(repository.path().to_string_lossy().to_string()),
    });
    store.upsert_summaries(&[summary]).expect("summary");

    store
        .refresh_code_changes("device")
        .expect("measure committed churn");
    assert!(store.list_trace_edits().expect("traces").is_empty());
    assert_eq!(
        store
            .list_code_change_metrics(false)
            .expect("metrics before")
            .len(),
        1
    );

    // Removing the source deletes the usage that carried the project path,
    // so nothing references the repository any more. Rebuilding only when
    // traces were dropped left these metrics materialized and the
    // authoritative snapshot republishing them.
    source(
        SourceCommand {
            command: SourceSubcommand::Remove {
                source_id: committed_source.source_id.0.clone(),
                delete_data: true,
            },
        },
        &store,
        "device",
    )
    .expect("remove source");

    assert!(store
        .list_code_change_metrics(false)
        .expect("metrics after")
        .is_empty());
    assert!(store
        .account_identity_observations(Some(&committed_source.source_id))
        .expect("identity evidence after")
        .is_empty());
    assert!(store
        .account_plan_observations()
        .expect("plan evidence after")
        .is_empty());
    assert!(store
        .conversation_account_bindings(Some(&committed_source.source_id))
        .expect("conversation evidence after")
        .is_empty());
}

#[test]
fn source_remove_delete_data_clears_task_spans_and_rebuilds_surviving_work_items() {
    let store = Store::in_memory().expect("store");
    let source_a = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-source-remove-a"),
        LocationOrigin::Configured,
    );
    let source_b = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-source-remove-b"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source_a).expect("source a");
    store.upsert_source(&source_b).expect("source b");

    let started_at_a = Utc
        .with_ymd_and_hms(2026, 6, 1, 10, 0, 0)
        .single()
        .expect("started_at_a");
    let started_at_b = started_at_a + Duration::days(10);
    let event_a = test_scan_event(
        &source_a,
        "/tmp/codex-source-remove-a/session.jsonl",
        started_at_a,
        "event-a",
        100,
    );
    let event_b = test_scan_event(
        &source_b,
        "/tmp/codex-source-remove-b/session.jsonl",
        started_at_b,
        "event-b",
        120,
    );
    store.insert_event(&event_a).expect("event a");
    store.insert_event(&event_b).expect("event b");

    let mut span_a = test_task_span(
        &source_a,
        "/tmp/codex-source-remove-a/session.jsonl",
        started_at_a,
        "span-a",
        "Implement source delete cleanup alpha",
        &event_a,
    );
    span_a.session_id = Some("session-a".to_string());
    let mut span_b = test_task_span(
        &source_b,
        "/tmp/codex-source-remove-b/session.jsonl",
        started_at_b,
        "span-b",
        "Implement source delete cleanup beta",
        &event_b,
    );
    span_b.session_id = Some("session-b".to_string());
    store
        .upsert_task_spans(&[span_a.clone(), span_b.clone()])
        .expect("task spans");
    store
        .rebuild_task_work_items_for_project_buckets(&BTreeSet::from([span_a
            .project_bucket
            .clone()]))
        .expect("rebuild");

    assert_eq!(store.task_spans().expect("task spans before").len(), 2);
    assert_eq!(store.work_items().expect("work items before").len(), 2);

    // Distinct days so each source contributes its own daily metric rather
    // than merging into one aggregate row.
    for (source_location, path, occurred_at) in [
        (
            &source_a,
            "/tmp/codex-source-remove-a/session.jsonl",
            started_at_a,
        ),
        (
            &source_b,
            "/tmp/codex-source-remove-b/session.jsonl",
            started_at_b,
        ),
    ] {
        let native_id = format!("thread-{}", source_location.source_id.0);
        let conversation_id = statsai_core::archive_conversation_id("codex", &native_id);
        let context = statsai_core::TraceEditContext {
            provider: "codex",
            source_id: &source_location.source_id,
            cache_key: path,
            conversation_id: &conversation_id,
            source_record_id: &format!("{path}:1"),
            occurred_at: Some(occurred_at),
            project: None,
            repository_path: None,
        };
        let edits = statsai_core::parse_full_file_write(
            &context,
            Path::new("src/lib.rs"),
            "one\ntwo\n",
            true,
        )
        .edits;
        store
            .store_archive_scan_with_code_changes(
                &source_location.source_id,
                &[statsai_core::ArchiveConversation {
                    schema_version: statsai_core::ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
                    conversation_id,
                    provider: "codex".to_string(),
                    source_id: source_location.source_id.clone(),
                    native_conversation_id: native_id,
                    title: None,
                    project: None,
                    started_at: Some(occurred_at),
                    updated_at: Some(occurred_at),
                    completeness: statsai_core::ArchiveCompleteness::Complete,
                    missing_content_count: 0,
                    missing_content_scope_id: None,
                    discarded_source_record_ids: Vec::new(),
                    superseded_conversation_ids: Vec::new(),
                    items: Vec::new(),
                }],
                &[statsai_store::ScanFileStateEntry {
                    cache_key: path.to_string(),
                    cache_signature: "signature".to_string(),
                }],
                &[],
                &edits,
                statsai_core::CoverageStatus::Complete,
                &[],
            )
            .expect("seed trace edits");
    }
    store
        .refresh_code_changes("device")
        .expect("build code-change metrics");
    assert_eq!(store.list_trace_edits().expect("traces before").len(), 2);
    assert_eq!(
        store
            .list_code_change_metrics(false)
            .expect("metrics before")
            .len(),
        2
    );

    source(
        SourceCommand {
            command: SourceSubcommand::Remove {
                source_id: source_a.source_id.0.clone(),
                delete_data: true,
            },
        },
        &store,
        "device",
    )
    .expect("remove source");

    assert!(store
        .source(&source_a.source_id)
        .expect("source a lookup")
        .is_none());
    assert!(store
        .source(&source_b.source_id)
        .expect("source b lookup")
        .is_some());

    let spans = store.task_spans().expect("task spans after");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].source_id, source_b.source_id);
    assert_eq!(spans[0].span_id, span_b.span_id);

    let work_items = store.work_items().expect("work items after");
    assert_eq!(work_items.len(), 1);
    assert_eq!(work_items[0].anchor_span_id, span_b.span_id);
    assert_eq!(work_items[0].total_tokens, 120);

    // The deleted source's reconstructed edits are gone, and the metrics
    // built from them are rebuilt now so the authoritative snapshot stops
    // republishing them.
    let traces = store.list_trace_edits().expect("traces after");
    assert_eq!(traces.len(), 1);
    assert_eq!(traces[0].source_id, source_b.source_id);
    // The import state goes with them, so re-adding the source imports
    // again rather than believing its files were already read.
    assert_eq!(
        store
            .archive_import_entry_count(&source_a.source_id)
            .expect("retired import state"),
        0
    );
    assert_eq!(
        store
            .archive_import_entry_count(&source_b.source_id)
            .expect("surviving import state"),
        1
    );
    // Deleting a source's data includes the archived copy of it.
    let conversations = store
        .list_archive_conversations(None, 10)
        .expect("remaining conversations");
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].source_id, source_b.source_id.0);
    assert_eq!(
        store
            .list_code_change_metrics(false)
            .expect("metrics after")
            .len(),
        1
    );
}
