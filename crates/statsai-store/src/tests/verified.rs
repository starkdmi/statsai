use super::support::*;
use super::*;

#[test]
fn blocked_auth_reattributes_usage_from_the_evidence_boundary() {
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-delayed-auth-block"),
        LocationOrigin::Configured,
    );
    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
        .single()
        .expect("assignment start");
    let blocked_since = Utc
        .with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
        .single()
        .expect("auth block boundary");
    let before_block = Utc
        .with_ymd_and_hms(2026, 5, 10, 0, 0, 0)
        .single()
        .expect("event before block");
    let after_block = Utc
        .with_ymd_and_hms(2026, 5, 20, 0, 0, 0)
        .single()
        .expect("event after block");
    store.upsert_source(&source).expect("source");
    apply_verified_source_state(
        &store,
        &source,
        Some(&VerifiedSourceState {
            provider_user_id: Some("oauth-account".to_string()),
            email: Some("oauth@example.com".to_string()),
            account_label: None,
            plan_name: None,
            authenticated_at: Some(started_at),
            verified_at: Some(started_at),
            subscription: None,
        }),
    )
    .expect("verified source state");
    let mut events = vec![
        test_store_event(&source, before_block, "before-block"),
        test_store_event(&source, after_block, "after-block"),
    ];
    apply_source_account_resolution(&store, &source, &mut events, &mut [])
        .expect("initial account resolution");
    assert!(events
        .iter()
        .all(|event| event.provider_account_id.is_some()));
    store.insert_events(&events).expect("usage events");
    let observation = VerifiedSourceObservation::AttributionBlocked {
        blocked_since: Some(blocked_since),
    };
    let next_hash = verified_source_observation_hash(&observation).expect("observation hash");

    reconcile_verified_source_state(&store, &mut source, &observation, next_hash)
        .expect("reconcile blocked auth");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].ended_at, Some(blocked_since));
    let events = store
        .events_for_source(&source.source_id)
        .expect("reattributed events");
    assert!(events[0].provider_account_id.is_some());
    assert_eq!(events[1].provider_account_id, None);
}

#[test]
fn cached_profile_inference_backfills_without_interpreting_the_previous_block_hash() {
    let store = Store::in_memory().expect("store");
    let authenticated_at = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("authenticated at");
    let usage_at = authenticated_at + chrono::Duration::days(1);
    let mut source = SourceLocation::local_adapter(
        "claude_code",
        "claude-code-local-jsonl",
        "0.3.3",
        Path::new("/tmp/claude-broken-profile-block-migration"),
        LocationOrigin::Default,
    );
    source.verified_state_hash =
        verified_source_observation_hash(&VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        })
        .expect("blocked observation hash");
    assert_eq!(
            source.verified_state_hash.as_deref(),
            Some(
                "attribution_blocked.v2:8fee6869306fd2707a21c0aa54affa2d1b1c726dd6dd23a20e61edbf7891e860"
            )
        );
    store.upsert_source(&source).expect("source");
    store
        .insert_event(&test_store_event(
            &source,
            usage_at,
            "unassigned-claude-usage",
        ))
        .expect("unassigned usage");

    let inferred_observation = VerifiedSourceObservation::Inferred {
        identity: Box::new(VerifiedSourceState {
            provider_user_id: Some("claude-account".to_string()),
            email: Some("claude@example.com".to_string()),
            account_label: None,
            plan_name: None,
            authenticated_at: Some(authenticated_at),
            verified_at: Some(authenticated_at),
            subscription: None,
        }),
        basis: SourceIdentityInference::CachedLocalProfile,
        settings_modified_at: None,
    };
    let inferred_hash =
        verified_source_observation_hash(&inferred_observation).expect("inferred observation hash");

    reconcile_verified_source_state(&store, &mut source, &inferred_observation, inferred_hash)
        .expect("inferred profile reconciliation");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].started_at, authenticated_at);
    assert_eq!(assignments[0].record_source, IdentitySource::SourceConfig);
    let accounts = store.list_accounts().expect("accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].identity_source, IdentitySource::SourceConfig);
    assert_eq!(accounts[0].confidence, Confidence::Medium);
    let events = store
        .events_for_source(&source.source_id)
        .expect("reattributed events");
    assert!(events[0].provider_account_id.is_some());
}

#[test]
fn repaired_settings_bound_cached_profile_inference_after_legacy_block() {
    let store = Store::in_memory().expect("store");
    let blocked_observed_at = Utc
        .with_ymd_and_hms(2026, 8, 5, 0, 0, 0)
        .single()
        .expect("blocked observation");
    let settings_repaired_at = blocked_observed_at + chrono::Duration::days(1);
    let authenticated_at = blocked_observed_at - chrono::Duration::days(10);
    let mut source = SourceLocation::local_adapter(
        "claude_code",
        "claude-code-local-jsonl",
        "0.3.3",
        Path::new("/tmp/claude-repaired-settings-inference"),
        LocationOrigin::Default,
    );
    source.updated_at = blocked_observed_at;
    source.verified_state_hash =
        verified_source_observation_hash(&VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        })
        .expect("blocked observation hash");
    store.upsert_source(&source).expect("source");
    store
        .insert_events(&[
            test_store_event(
                &source,
                settings_repaired_at - chrono::Duration::hours(1),
                "before-settings-repair",
            ),
            test_store_event(
                &source,
                settings_repaired_at + chrono::Duration::hours(1),
                "after-settings-repair",
            ),
        ])
        .expect("unassigned usage");
    let observation = VerifiedSourceObservation::Inferred {
        identity: Box::new(VerifiedSourceState {
            provider_user_id: Some("claude-account".to_string()),
            email: Some("claude@example.com".to_string()),
            account_label: None,
            plan_name: None,
            authenticated_at: Some(authenticated_at),
            verified_at: Some(authenticated_at),
            subscription: None,
        }),
        basis: SourceIdentityInference::CachedLocalProfile,
        settings_modified_at: Some(settings_repaired_at),
    };
    let next_hash =
        verified_source_observation_hash(&observation).expect("inferred observation hash");

    reconcile_verified_source_state(&store, &mut source, &observation, next_hash)
        .expect("inferred profile reconciliation");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].started_at, settings_repaired_at);
    let events = store
        .events_for_source(&source.source_id)
        .expect("reattributed events");
    assert_eq!(events[0].provider_account_id, None);
    assert!(events[1].provider_account_id.is_some());
}

#[test]
fn clearing_auth_override_and_later_profile_changes_preserve_the_blocked_interval() {
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-auth-override-recovery"),
        LocationOrigin::Configured,
    );
    let recovery_probe_at = Utc::now();
    let authenticated_at = recovery_probe_at - chrono::Duration::days(30);
    let blocked_since = recovery_probe_at - chrono::Duration::days(20);
    let override_usage_at = recovery_probe_at - chrono::Duration::days(10);
    store.upsert_source(&source).expect("source");

    let verified_state = VerifiedSourceState {
        provider_user_id: Some("oauth-account".to_string()),
        email: Some("oauth@example.com".to_string()),
        account_label: None,
        plan_name: None,
        authenticated_at: Some(authenticated_at),
        verified_at: Some(authenticated_at),
        subscription: None,
    };
    let initial_observation = VerifiedSourceObservation::Verified(Box::new(verified_state.clone()));
    let initial_hash =
        verified_source_observation_hash(&initial_observation).expect("initial hash");
    reconcile_verified_source_state(&store, &mut source, &initial_observation, initial_hash)
        .expect("initial OAuth reconciliation");

    let mut events = vec![test_store_event(
        &source,
        override_usage_at,
        "override-usage",
    )];
    apply_source_account_resolution(&store, &source, &mut events, &mut [])
        .expect("initial account resolution");
    assert!(events[0].provider_account_id.is_some());
    store.insert_events(&events).expect("usage event");

    let blocked_observation = VerifiedSourceObservation::AttributionBlocked {
        blocked_since: Some(blocked_since),
    };
    let blocked_hash =
        verified_source_observation_hash(&blocked_observation).expect("blocked hash");
    reconcile_verified_source_state(&store, &mut source, &blocked_observation, blocked_hash)
        .expect("blocked auth reconciliation");

    let refreshed_during_block = blocked_since + chrono::Duration::days(5);
    let refreshed_state = VerifiedSourceState {
        authenticated_at: Some(refreshed_during_block),
        verified_at: Some(refreshed_during_block),
        ..verified_state
    };
    let clear_observation = VerifiedSourceObservation::Verified(Box::new(refreshed_state.clone()));
    let clear_hash = verified_source_observation_hash(&clear_observation).expect("clear hash");
    let recovery_not_before = Utc::now();
    reconcile_verified_source_state(&store, &mut source, &clear_observation, clear_hash)
        .expect("cleared auth reconciliation");
    let recovery_not_after = Utc::now();

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 2);
    assert_eq!(assignments[0].started_at, authenticated_at);
    assert_eq!(assignments[0].ended_at, Some(blocked_since));
    assert!(assignments[1].started_at >= recovery_not_before);
    assert!(assignments[1].started_at <= recovery_not_after);
    assert_eq!(assignments[1].ended_at, None);
    let recovered_at = assignments[1].started_at;

    let events = store
        .events_for_source(&source.source_id)
        .expect("reattributed events");
    assert_eq!(events[0].provider_account_id, None);

    let changed_profile_observation =
        VerifiedSourceObservation::Verified(Box::new(VerifiedSourceState {
            account_label: Some("Personal".to_string()),
            ..refreshed_state
        }));
    let changed_profile_hash = verified_source_observation_hash(&changed_profile_observation)
        .expect("changed profile hash");
    reconcile_verified_source_state(
        &store,
        &mut source,
        &changed_profile_observation,
        changed_profile_hash,
    )
    .expect("changed profile reconciliation");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments after profile change");
    assert_eq!(assignments.len(), 2);
    assert_eq!(assignments[0].ended_at, Some(blocked_since));
    assert_eq!(assignments[1].started_at, recovered_at);
    assert_eq!(assignments[1].ended_at, None);
    let events = store
        .events_for_source(&source.source_id)
        .expect("events after profile change");
    assert_eq!(events[0].provider_account_id, None);
}

#[test]
fn clearing_current_and_legacy_unknown_auth_blocks_preserves_invalidated_history() {
    for use_legacy_hash in [false, true] {
        let store = Store::in_memory().expect("store");
        let source_path = format!("/tmp/claude-unknown-auth-recovery-{use_legacy_hash}");
        let mut source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new(&source_path),
            LocationOrigin::Configured,
        );
        let recovery_probe_at = Utc::now();
        let authenticated_at = recovery_probe_at - chrono::Duration::days(30);
        let usage_at = recovery_probe_at - chrono::Duration::days(10);
        store.upsert_source(&source).expect("source");

        let verified_state = VerifiedSourceState {
            provider_user_id: Some("oauth-account".to_string()),
            email: Some("oauth@example.com".to_string()),
            account_label: None,
            plan_name: None,
            authenticated_at: Some(authenticated_at),
            verified_at: Some(authenticated_at),
            subscription: None,
        };
        let initial_observation =
            VerifiedSourceObservation::Verified(Box::new(verified_state.clone()));
        let initial_hash =
            verified_source_observation_hash(&initial_observation).expect("initial hash");
        reconcile_verified_source_state(&store, &mut source, &initial_observation, initial_hash)
            .expect("initial OAuth reconciliation");
        let mut events = vec![test_store_event(&source, usage_at, "uncertain-usage")];
        apply_source_account_resolution(&store, &source, &mut events, &mut [])
            .expect("initial account resolution");
        store.insert_events(&events).expect("usage event");

        let blocked_observation = VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        };
        let blocked_hash = if use_legacy_hash {
            let legacy_payload = serde_json::to_string(&(
                "verified_source_observation.attribution_blocked.v2",
                Option::<DateTime<Utc>>::None,
            ))
            .expect("legacy blocked payload");
            Some(hash_text(&legacy_payload))
        } else {
            verified_source_observation_hash(&blocked_observation).expect("blocked hash")
        };
        reconcile_verified_source_state(&store, &mut source, &blocked_observation, blocked_hash)
            .expect("unknown auth block reconciliation");
        assert!(store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("blocked assignments")
            .is_empty());

        let clear_observation = VerifiedSourceObservation::Verified(Box::new(verified_state));
        let clear_hash = verified_source_observation_hash(&clear_observation).expect("clear hash");
        let recovery_not_before = Utc::now();
        reconcile_verified_source_state(&store, &mut source, &clear_observation, clear_hash)
            .expect("cleared auth reconciliation");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("recovered assignments");
        assert_eq!(assignments.len(), 1);
        assert!(assignments[0].started_at >= recovery_not_before);
        let events = store
            .events_for_source(&source.source_id)
            .expect("reattributed events");
        assert_eq!(events[0].provider_account_id, None);
    }
}

#[test]
fn clearing_legacy_timestamped_auth_block_without_history_starts_at_recovery() {
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-legacy-timestamped-auth-recovery"),
        LocationOrigin::Configured,
    );
    let authenticated_at = Utc::now() - chrono::Duration::days(30);
    let blocked_since = authenticated_at + chrono::Duration::days(10);
    let legacy_payload = serde_json::to_string(&(
        "verified_source_observation.attribution_blocked.v2",
        Some(blocked_since),
    ))
    .expect("legacy blocked payload");
    source.verified_state_hash = Some(hash_text(&legacy_payload));
    store.upsert_source(&source).expect("source");

    let clear_observation = VerifiedSourceObservation::Verified(Box::new(VerifiedSourceState {
        provider_user_id: Some("oauth-account".to_string()),
        email: Some("oauth@example.com".to_string()),
        account_label: None,
        plan_name: None,
        authenticated_at: Some(authenticated_at),
        verified_at: Some(authenticated_at),
        subscription: None,
    }));
    let clear_hash = verified_source_observation_hash(&clear_observation).expect("clear hash");
    let recovery_not_before = Utc::now();

    reconcile_verified_source_state(&store, &mut source, &clear_observation, clear_hash)
        .expect("cleared auth reconciliation");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("recovered assignments");
    assert_eq!(assignments.len(), 1);
    assert!(assignments[0].started_at >= recovery_not_before);
}

#[test]
fn migrating_legacy_verified_hash_preserves_active_assignment_continuity() {
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-legacy-verified-hash-migration"),
        LocationOrigin::Configured,
    );
    let authenticated_at = Utc::now() - chrono::Duration::days(30);
    let verified_state = VerifiedSourceState {
        provider_user_id: Some("oauth-account".to_string()),
        email: Some("oauth@example.com".to_string()),
        account_label: None,
        plan_name: None,
        authenticated_at: Some(authenticated_at),
        verified_at: Some(authenticated_at),
        subscription: None,
    };
    source.verified_state_hash =
        verified_source_state_hash(Some(&verified_state)).expect("legacy verified hash");
    store.upsert_source(&source).expect("source");
    apply_verified_source_state(&store, &source, Some(&verified_state))
        .expect("legacy verified assignment");

    let observation = VerifiedSourceObservation::Verified(Box::new(verified_state));
    let typed_hash = verified_source_observation_hash(&observation).expect("typed hash");
    reconcile_verified_source_state(&store, &mut source, &observation, typed_hash.clone())
        .expect("verified hash migration");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].started_at, authenticated_at);
    assert_eq!(assignments[0].ended_at, None);
    assert_eq!(source.verified_state_hash, typed_hash);
}

#[test]
fn earlier_blocked_auth_boundary_shortens_closed_assignment_and_reattributes_usage() {
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-corrected-auth-block"),
        LocationOrigin::Configured,
    );
    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
        .single()
        .expect("assignment start");
    let earlier_boundary = Utc
        .with_ymd_and_hms(2026, 5, 10, 0, 0, 0)
        .single()
        .expect("earlier auth block boundary");
    let first_boundary = Utc
        .with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
        .single()
        .expect("first auth block boundary");
    let before_earlier_boundary = Utc
        .with_ymd_and_hms(2026, 5, 8, 0, 0, 0)
        .single()
        .expect("event before earlier boundary");
    let between_boundaries = Utc
        .with_ymd_and_hms(2026, 5, 12, 0, 0, 0)
        .single()
        .expect("event between boundaries");
    let later_assignment_start = Utc
        .with_ymd_and_hms(2026, 5, 20, 0, 0, 0)
        .single()
        .expect("later assignment start");
    let later_usage = Utc
        .with_ymd_and_hms(2026, 5, 21, 0, 0, 0)
        .single()
        .expect("later usage");
    store.upsert_source(&source).expect("source");
    apply_verified_source_state(
        &store,
        &source,
        Some(&VerifiedSourceState {
            provider_user_id: Some("oauth-account".to_string()),
            email: Some("oauth@example.com".to_string()),
            account_label: None,
            plan_name: None,
            authenticated_at: Some(started_at),
            verified_at: Some(started_at),
            subscription: None,
        }),
    )
    .expect("verified source state");
    let mut events = vec![
        test_store_event(&source, before_earlier_boundary, "before-earlier-boundary"),
        test_store_event(&source, between_boundaries, "between-boundaries"),
    ];
    apply_source_account_resolution(&store, &source, &mut events, &mut [])
        .expect("initial account resolution");
    store.insert_events(&events).expect("usage events");

    let first_observation = VerifiedSourceObservation::AttributionBlocked {
        blocked_since: Some(first_boundary),
    };
    let first_hash =
        verified_source_observation_hash(&first_observation).expect("first observation hash");
    reconcile_verified_source_state(&store, &mut source, &first_observation, first_hash)
        .expect("first blocked auth reconciliation");
    apply_verified_source_state(
        &store,
        &source,
        Some(&VerifiedSourceState {
            provider_user_id: Some("later-oauth-account".to_string()),
            email: Some("later-oauth@example.com".to_string()),
            account_label: None,
            plan_name: None,
            authenticated_at: Some(later_assignment_start),
            verified_at: Some(later_assignment_start),
            subscription: None,
        }),
    )
    .expect("later verified source state");
    let mut later_events = vec![test_store_event(&source, later_usage, "later-usage")];
    apply_source_account_resolution(&store, &source, &mut later_events, &mut [])
        .expect("later account resolution");
    assert!(later_events[0].provider_account_id.is_some());
    store
        .insert_events(&later_events)
        .expect("later usage event");
    let corrected_observation = VerifiedSourceObservation::AttributionBlocked {
        blocked_since: Some(earlier_boundary),
    };
    let corrected_hash = verified_source_observation_hash(&corrected_observation)
        .expect("corrected observation hash");

    reconcile_verified_source_state(&store, &mut source, &corrected_observation, corrected_hash)
        .expect("corrected blocked auth reconciliation");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].ended_at, Some(earlier_boundary));
    let events = store
        .events_for_source(&source.source_id)
        .expect("reattributed events");
    assert!(events[0].provider_account_id.is_some());
    assert_eq!(events[1].provider_account_id, None);
    assert_eq!(events[2].provider_account_id, None);
}

#[test]
fn blocked_auth_without_evidence_invalidates_the_uncertain_assignment_interval() {
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-unknown-auth-block-boundary"),
        LocationOrigin::Configured,
    );
    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
        .single()
        .expect("assignment start");
    store.upsert_source(&source).expect("source");
    apply_verified_source_state(
        &store,
        &source,
        Some(&VerifiedSourceState {
            provider_user_id: Some("oauth-account".to_string()),
            email: Some("oauth@example.com".to_string()),
            account_label: None,
            plan_name: None,
            authenticated_at: Some(started_at),
            verified_at: Some(started_at),
            subscription: None,
        }),
    )
    .expect("verified source state");
    let mut events = vec![test_store_event(&source, started_at, "uncertain-event")];
    apply_source_account_resolution(&store, &source, &mut events, &mut [])
        .expect("initial account resolution");
    assert!(events[0].provider_account_id.is_some());
    store.insert_events(&events).expect("usage event");
    let observation = VerifiedSourceObservation::AttributionBlocked {
        blocked_since: None,
    };
    let next_hash = verified_source_observation_hash(&observation).expect("observation hash");

    reconcile_verified_source_state(&store, &mut source, &observation, next_hash)
        .expect("reconcile blocked auth");

    assert!(store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments")
        .is_empty());
    let events = store
        .events_for_source(&source.source_id)
        .expect("reattributed events");
    assert_eq!(events[0].provider_account_id, None);
}

#[test]
fn blocked_auth_hash_includes_the_evidence_boundary() {
    let first_boundary = Utc
        .with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
        .single()
        .expect("first boundary");
    let second_boundary = Utc
        .with_ymd_and_hms(2026, 5, 16, 0, 0, 0)
        .single()
        .expect("second boundary");

    let first_hash =
        verified_source_observation_hash(&VerifiedSourceObservation::AttributionBlocked {
            blocked_since: Some(first_boundary),
        })
        .expect("first hash");
    let second_hash =
        verified_source_observation_hash(&VerifiedSourceObservation::AttributionBlocked {
            blocked_since: Some(second_boundary),
        })
        .expect("second hash");

    assert_ne!(first_hash, second_hash);
}

#[test]
fn direct_conversation_evidence_overrides_only_that_event_and_preserves_manual_interval() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-direct-account-binding"),
        LocationOrigin::Configured,
    );
    let observed_at = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("observed_at");
    let manual_account = ProviderAccountId("manual-account".to_string());
    let directly_bound_account = ProviderAccountId("direct-account".to_string());
    store.upsert_source(&source).expect("source");
    let manual_assignment = SourceAccountAssignment {
        schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
        assignment_id: SourceAccountAssignmentId("manual-assignment".to_string()),
        source_id: source.source_id.clone(),
        provider: "codex".to_string(),
        provider_account_id: manual_account.clone(),
        started_at: observed_at - chrono::Duration::days(1),
        ended_at: None,
        record_source: IdentitySource::UserConfigured,
        verified_at: None,
        created_at: observed_at,
        updated_at: observed_at,
    };
    store
        .upsert_source_account_assignment(&manual_assignment)
        .expect("manual assignment");
    let direct_binding = statsai_core::ConversationAccountBindingV1 {
        schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
        binding_id: "direct-binding".to_string(),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: directly_bound_account.clone(),
        conversation_id_hash: "same-session".to_string(),
        turn_id_hash: None,
        active_from: None,
        observed_until: None,
        certain_until: None,
        observed_at,
        evidence_kind: statsai_core::AccountEvidenceKind::ResetHistory,
        confidence: Confidence::High,
    };
    assert_eq!(
        store
            .upsert_conversation_account_bindings(std::slice::from_ref(&direct_binding))
            .expect("direct binding"),
        1
    );
    assert_eq!(
        store
            .upsert_conversation_account_bindings(&[direct_binding])
            .expect("repeat direct binding"),
        0
    );
    store
        .upsert_account_identity_observations(&[statsai_core::AccountIdentityObservationV1 {
            schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: "direct-identity".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: Some(directly_bound_account.clone()),
            provider_user_id_hash: Some("provider-id-hash".to_string()),
            email_hash: None,
            conversation_id_hash: Some("same-session".to_string()),
            turn_id_hash: None,
            observed_at,
            evidence_kind: statsai_core::AccountEvidenceKind::ResetHistory,
            confidence: Confidence::High,
            auth_mode: None,
            application_version: None,
            parser_version: "test.v1".to_string(),
            artifact_kind: "test".to_string(),
            artifact_path_hash: "path-hash".to_string(),
            record_fingerprint: "record-hash".to_string(),
        }])
        .expect("identity observation");
    let mut directly_bound = test_store_event(&source, observed_at, "direct");
    directly_bound.provider_account_id = Some(manual_account.clone());
    let mut unrelated = test_store_event(&source, observed_at, "unrelated");
    unrelated.session.local_session_id_hash = Some("other-session".to_string());
    unrelated.provider_account_id = Some(manual_account.clone());
    let mut events = vec![directly_bound, unrelated];

    store
        .apply_conversation_account_bindings(&source.source_id, &mut events)
        .expect("apply binding");

    assert_eq!(events[0].provider_account_id, Some(directly_bound_account));
    assert_eq!(events[1].provider_account_id, Some(manual_account.clone()));
    assert_eq!(
        store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("manual interval"),
        vec![manual_assignment]
    );
    let summaries = store
        .account_evidence_summaries("device")
        .expect("evidence summary");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].directly_bound_conversations, 1);
    assert_eq!(summaries[0].uncovered_gap_count, 0);
    assert_eq!(summaries[0].conflict_count, 1);
}

#[test]
fn confirmed_auth_reload_boundaries_repair_switches_conservatively() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-account-evidence-switch"),
        LocationOrigin::Configured,
    );
    let base = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("base");
    let account_a = ProviderAccountId("account-a".to_string());
    let account_b = ProviderAccountId("account-b".to_string());
    store.upsert_source(&source).expect("source");
    store
        .upsert_source_account_assignment(&SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: SourceAccountAssignmentId("broad-account-a".to_string()),
            source_id: source.source_id.clone(),
            provider: "codex".to_string(),
            provider_account_id: account_a.clone(),
            started_at: base,
            ended_at: None,
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(base),
            created_at: base,
            updated_at: base,
        })
        .expect("broad assignment");
    let observation = |id: &str,
                       account: ProviderAccountId,
                       observed_at: chrono::DateTime<Utc>,
                       kind: statsai_core::AccountEvidenceKind| {
        statsai_core::AccountIdentityObservationV1 {
            schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: id.to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: Some(account),
            provider_user_id_hash: Some(format!("hash-{id}")),
            email_hash: None,
            conversation_id_hash: None,
            turn_id_hash: None,
            observed_at,
            evidence_kind: kind,
            confidence: Confidence::High,
            auth_mode: Some("chatgpt".to_string()),
            application_version: None,
            parser_version: "test.v1".to_string(),
            artifact_kind: "test".to_string(),
            artifact_path_hash: "path-hash".to_string(),
            record_fingerprint: format!("fingerprint-{id}"),
        }
    };
    let account_a_reload = base + chrono::Duration::days(1);
    let account_a_confirmation = base + chrono::Duration::days(2);
    let account_b_reload = base + chrono::Duration::days(3);
    let account_b_confirmation = base + chrono::Duration::days(4);
    store
        .upsert_account_identity_observations(&[
            observation(
                "a-reload",
                account_a.clone(),
                account_a_reload,
                statsai_core::AccountEvidenceKind::AuthReload,
            ),
            observation(
                "a-confirm",
                account_a.clone(),
                account_a_confirmation,
                statsai_core::AccountEvidenceKind::TelemetryIdentity,
            ),
            observation(
                "b-reload",
                account_b.clone(),
                account_b_reload,
                statsai_core::AccountEvidenceKind::AuthReload,
            ),
            observation(
                "b-confirm",
                account_b.clone(),
                account_b_confirmation,
                statsai_core::AccountEvidenceKind::AuthSnapshot,
            ),
        ])
        .expect("identity evidence");

    assert!(
        store
            .reconcile_source_account_evidence_assignments(&source.source_id)
            .expect("repair intervals")
            > 0
    );
    assert_eq!(
        store
            .reconcile_source_account_evidence_assignments(&source.source_id)
            .expect("repeat repair"),
        0
    );
    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 2);
    let repaired_a = assignments
        .iter()
        .find(|assignment| assignment.provider_account_id == account_a)
        .expect("account a interval");
    assert_eq!(repaired_a.started_at, base);
    assert_eq!(repaired_a.ended_at, Some(account_b_reload));
    let repaired_b = assignments
        .iter()
        .find(|assignment| assignment.provider_account_id == account_b)
        .expect("account b interval");
    assert_eq!(repaired_b.started_at, account_b_reload);
    assert_eq!(repaired_b.ended_at, None);
}

#[test]
fn conflicting_bindings_keep_a_manual_account_assignment() {
    let store = Store::in_memory().expect("store");
    let now = Utc::now();
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        std::path::Path::new("/tmp/binding-conflict"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let manual = ProviderAccountId("account-manual".to_string());

    // The same conversation is bound to two different accounts.
    for (index, account) in ["account-one", "account-two"].iter().enumerate() {
        store
            .upsert_conversation_account_bindings(&[statsai_core::ConversationAccountBindingV1 {
                schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION
                    .to_string(),
                binding_id: format!("binding-{index}"),
                provider: "codex".to_string(),
                source_id: source.source_id.clone(),
                provider_account_id: ProviderAccountId((*account).to_string()),
                conversation_id_hash: "same-session".to_string(),
                turn_id_hash: None,
                active_from: None,
                observed_until: None,
                certain_until: None,
                observed_at: now,
                evidence_kind: statsai_core::AccountEvidenceKind::TelemetryIdentity,
                confidence: Confidence::High,
            }])
            .expect("binding");
    }

    let mut manual_event = test_store_event(&source, now, "manual-record");
    manual_event.provider_account_id = Some(manual.clone());
    manual_event.parse_evidence = Some(ParseEvidence {
        event_key_version: "test".to_string(),
        source_file_path_hash: None,
        source_line_number: None,
        source_record_id: None,
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::UserConfigured,
    });
    let mut derived_event = test_store_event(&source, now, "derived-record");
    derived_event.provider_account_id = Some(ProviderAccountId("account-one".to_string()));
    derived_event.parse_evidence = Some(ParseEvidence {
        account_identity_source: IdentitySource::LocalAuth,
        ..manual_event
            .parse_evidence
            .clone()
            .expect("evidence template")
    });

    let mut events = vec![manual_event, derived_event];
    store
        .apply_conversation_account_bindings(&source.source_id, &mut events)
        .expect("apply bindings");

    assert_eq!(
        events[0].provider_account_id.as_ref(),
        Some(&manual),
        "a conflict between derived bindings must not discard a manual assignment"
    );
    assert_eq!(
        events[0]
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
        Some(&IdentitySource::UserConfigured),
        "the recorded identity source must still describe the account on the event"
    );
    assert_eq!(
        events[1].provider_account_id, None,
        "a derived attribution is still cleared by a genuine conflict"
    );
}

#[test]
fn reset_history_alone_does_not_truncate_a_source_assignment() {
    let store = Store::in_memory().expect("store");
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("started at");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        std::path::Path::new("/tmp/reset-history-truncation"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let bound = ProviderAccountId("account-bound".to_string());
    let assignment = statsai_core::SourceAccountAssignment {
        schema_version: statsai_core::SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
        assignment_id: statsai_core::SourceAccountAssignmentId("assignment-1".to_string()),
        source_id: source.source_id.clone(),
        provider: "codex".to_string(),
        provider_account_id: bound.clone(),
        started_at,
        ended_at: None,
        record_source: IdentitySource::LocalAuth,
        verified_at: Some(started_at),
        created_at: started_at,
        updated_at: started_at,
    };
    store
        .upsert_source_account_assignment(&assignment)
        .expect("assignment");

    // An auth snapshot corroborates the assignment, then a single
    // conversation-scoped reset-history entry names a different account.
    for (index, (kind, account, offset_days)) in [
        (
            statsai_core::AccountEvidenceKind::AuthSnapshot,
            "account-bound",
            1_i64,
        ),
        (
            statsai_core::AccountEvidenceKind::ResetHistory,
            "account-other",
            2,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        store
            .upsert_account_identity_observations(&[statsai_core::AccountIdentityObservationV1 {
                schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION
                    .to_string(),
                observation_id: format!("identity-{index}"),
                provider: "codex".to_string(),
                source_id: source.source_id.clone(),
                provider_account_id: Some(ProviderAccountId(account.to_string())),
                provider_user_id_hash: None,
                email_hash: None,
                conversation_id_hash: Some("f".repeat(64)),
                turn_id_hash: Some("a".repeat(64)),
                observed_at: started_at + chrono::Duration::days(offset_days),
                evidence_kind: kind,
                confidence: Confidence::High,
                auth_mode: None,
                application_version: None,
                parser_version: "test.v1".to_string(),
                artifact_kind: "test".to_string(),
                artifact_path_hash: "c".repeat(64),
                record_fingerprint: format!("{index}").repeat(64),
            }])
            .expect("identity observation");
    }

    store
        .reconcile_source_account_evidence_assignments(&source.source_id)
        .expect("reconcile");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].provider_account_id, bound);
    assert_eq!(
        assignments[0].ended_at, None,
        "a per-conversation reset-history entry cannot end a source-wide assignment, \
             because nothing downstream is able to reopen one"
    );
}

#[test]
fn reset_history_does_not_bound_a_reopened_auth_reload_interval() {
    let store = Store::in_memory().expect("store");
    let base = Utc
        .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("base");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        std::path::Path::new("/tmp/reload-interval-bounds"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let reloaded = ProviderAccountId("account-reloaded".to_string());

    // reload A, telemetry A, then a turn-scoped reset-history entry naming B.
    for (index, (kind, account, offset_days)) in [
        (
            statsai_core::AccountEvidenceKind::AuthReload,
            "account-reloaded",
            1_i64,
        ),
        (
            statsai_core::AccountEvidenceKind::TelemetryIdentity,
            "account-reloaded",
            2,
        ),
        (
            statsai_core::AccountEvidenceKind::ResetHistory,
            "account-other",
            3,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        store
            .upsert_account_identity_observations(&[statsai_core::AccountIdentityObservationV1 {
                schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION
                    .to_string(),
                observation_id: format!("reload-identity-{index}"),
                provider: "codex".to_string(),
                source_id: source.source_id.clone(),
                provider_account_id: Some(ProviderAccountId(account.to_string())),
                provider_user_id_hash: None,
                email_hash: None,
                conversation_id_hash: Some("f".repeat(64)),
                turn_id_hash: Some("a".repeat(64)),
                observed_at: base + chrono::Duration::days(offset_days),
                evidence_kind: kind,
                confidence: Confidence::High,
                auth_mode: None,
                application_version: None,
                parser_version: "test.v1".to_string(),
                artifact_kind: "test".to_string(),
                artifact_path_hash: "c".repeat(64),
                record_fingerprint: format!("{index}").repeat(64),
            }])
            .expect("identity observation");
    }

    store
        .reconcile_source_account_evidence_assignments(&source.source_id)
        .expect("reconcile");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].provider_account_id, reloaded);
    assert_eq!(
        assignments[0].ended_at, None,
        "reset history must not close the interval an auth reload opened"
    );
}

#[test]
fn refreshing_verified_at_does_not_rewrite_unchanged_source_records() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/verified-at-only-refresh"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let account_id = ProviderAccountId("account-verified".to_string());
    let authenticated_at = Utc::now() - chrono::Duration::days(1);
    upsert_verified_source_assignment(
        &store,
        &source,
        &account_id,
        authenticated_at,
        Some(authenticated_at),
        None,
        IdentitySource::LocalAuth,
    )
    .expect("initial verification");
    let mut event = test_store_event(&source, authenticated_at, "verified-event");
    event.provider_account_id = Some(account_id.clone());
    store.insert_events(&[event]).expect("event");
    store
        .conn
        .execute_batch(
            r#"
                CREATE TABLE event_update_audit (count INTEGER NOT NULL);
                INSERT INTO event_update_audit VALUES (0);
                CREATE TRIGGER count_event_updates
                AFTER UPDATE ON usage_events
                BEGIN
                  UPDATE event_update_audit SET count = count + 1;
                END;
                "#,
        )
        .expect("audit trigger");

    let refreshed_at = authenticated_at + chrono::Duration::hours(1);
    upsert_verified_source_assignment(
        &store,
        &source,
        &account_id,
        authenticated_at,
        Some(refreshed_at),
        None,
        IdentitySource::LocalAuth,
    )
    .expect("refresh verification");

    let update_count: i64 = store
        .conn
        .query_row("SELECT count FROM event_update_audit", [], |row| row.get(0))
        .expect("audit count");
    assert_eq!(update_count, 0);
    let assignment = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignment")
        .pop()
        .expect("verified assignment");
    assert_eq!(assignment.verified_at, Some(refreshed_at));
}

#[test]
fn conversation_takeover_bindings_split_one_conversation_by_event_time() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-desktop-takeover"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("start");
    let takeover = start + chrono::Duration::hours(2);
    let original_account = ProviderAccountId("original-account".to_string());
    let new_account = ProviderAccountId("new-account".to_string());
    let binding = |account: &ProviderAccountId, active_from| {
        let mut binding = statsai_core::ConversationAccountBindingV1 {
            schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
            binding_id: String::new(),
            provider: "claude_code".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: account.clone(),
            conversation_id_hash: "same-session".to_string(),
            turn_id_hash: None,
            active_from,
            observed_until: None,
            certain_until: None,
            observed_at: active_from.unwrap_or(start),
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence: Confidence::High,
        };
        binding.binding_id = binding.derived_binding_id();
        binding
    };
    let bindings = [
        binding(&original_account, None),
        binding(&new_account, Some(takeover)),
    ];
    assert_ne!(bindings[0].binding_id, bindings[1].binding_id);
    assert_eq!(
        store
            .upsert_conversation_account_bindings(&bindings)
            .expect("bindings"),
        2
    );
    let mut events = vec![
        test_store_event(&source, start, "before"),
        test_store_event(
            &source,
            takeover - chrono::Duration::milliseconds(1),
            "edge",
        ),
        test_store_event(&source, takeover, "at"),
        test_store_event(&source, takeover + chrono::Duration::hours(1), "after"),
    ];

    store
        .apply_conversation_account_bindings(&source.source_id, &mut events)
        .expect("apply bindings");

    let accounts = events
        .iter()
        .map(|event| event.provider_account_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        accounts,
        vec![
            Some(original_account.clone()),
            Some(original_account),
            Some(new_account.clone()),
            Some(new_account),
        ]
    );
}

#[test]
fn conversation_takeover_binding_leaves_earlier_events_to_the_source() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-desktop-late-takeover"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("start");
    let takeover = start + chrono::Duration::hours(2);
    let source_account = ProviderAccountId("source-account".to_string());
    let new_account = ProviderAccountId("new-account".to_string());
    let mut binding = statsai_core::ConversationAccountBindingV1 {
        schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
        binding_id: String::new(),
        provider: "claude_code".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: new_account.clone(),
        conversation_id_hash: "same-session".to_string(),
        turn_id_hash: None,
        active_from: Some(takeover),
        observed_until: None,
        certain_until: None,
        observed_at: takeover,
        evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
        confidence: Confidence::High,
    };
    binding.binding_id = binding.derived_binding_id();
    store
        .upsert_conversation_account_bindings(&[binding])
        .expect("binding");
    let mut before = test_store_event(&source, start, "before");
    before.provider_account_id = Some(source_account.clone());
    let mut after = test_store_event(&source, takeover, "after");
    after.provider_account_id = Some(source_account.clone());
    let mut events = vec![before, after];

    store
        .apply_conversation_account_bindings(&source.source_id, &mut events)
        .expect("apply binding");

    assert_eq!(events[0].provider_account_id, Some(source_account));
    assert_eq!(events[1].provider_account_id, Some(new_account));
}

#[test]
fn certain_conversation_binding_outranks_one_inferred_from_copied_history() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-desktop-confidence"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("start");
    let original_account = ProviderAccountId("original-account".to_string());
    let copy_account = ProviderAccountId("copy-account".to_string());
    let binding = |conversation: &str, account: &ProviderAccountId, confidence| {
        let mut binding = statsai_core::ConversationAccountBindingV1 {
            schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
            binding_id: String::new(),
            provider: "claude_code".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: account.clone(),
            conversation_id_hash: conversation.to_string(),
            turn_id_hash: None,
            active_from: None,
            observed_until: None,
            certain_until: None,
            observed_at: start,
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence,
        };
        binding.binding_id = binding.derived_binding_id();
        binding
    };
    store
        .upsert_conversation_account_bindings(&[
            binding("ranked", &original_account, Confidence::High),
            binding("ranked", &copy_account, Confidence::Medium),
            binding("tied", &original_account, Confidence::Medium),
            binding("tied", &copy_account, Confidence::Medium),
        ])
        .expect("bindings");
    let mut ranked = test_store_event(&source, start, "ranked");
    ranked.session.local_session_id_hash = Some("ranked".to_string());
    let mut tied = test_store_event(&source, start, "tied");
    tied.session.local_session_id_hash = Some("tied".to_string());
    tied.provider_account_id = Some(original_account.clone());
    let mut events = vec![ranked, tied];

    store
        .apply_conversation_account_bindings(&source.source_id, &mut events)
        .expect("apply bindings");

    assert_eq!(events[0].provider_account_id, Some(original_account));
    assert_eq!(events[1].provider_account_id, None);
}

#[test]
fn certain_range_outranks_a_later_inferred_takeover() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-desktop-certain-range"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("start");
    let hours = chrono::Duration::hours;
    let original_account = ProviderAccountId("original-account".to_string());
    let inferred_account = ProviderAccountId("inferred-account".to_string());
    let binding = |account: &ProviderAccountId, active_from, observed_until, confidence| {
        let mut binding = statsai_core::ConversationAccountBindingV1 {
            schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
            binding_id: String::new(),
            provider: "claude_code".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: account.clone(),
            conversation_id_hash: "same-session".to_string(),
            turn_id_hash: None,
            active_from,
            observed_until,
            // A direct observation is certain through its whole range.
            certain_until: if confidence == Confidence::High {
                observed_until
            } else {
                None
            },
            observed_at: start,
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence,
        };
        binding.binding_id = binding.derived_binding_id();
        binding
    };
    // Seen held by the original account alone through hour 2; a later scan of
    // copied history inferred the other account from a sign-in before the
    // session began.
    store
        .upsert_conversation_account_bindings(&[
            binding(
                &original_account,
                None,
                Some(start + hours(2)),
                Confidence::High,
            ),
            binding(
                &inferred_account,
                Some(start - hours(50)),
                None,
                Confidence::Medium,
            ),
        ])
        .expect("bindings");
    let mut events = vec![
        test_store_event(&source, start + hours(1), "certain"),
        test_store_event(&source, start + hours(2), "certain-edge"),
        test_store_event(&source, start + hours(5), "inferred"),
    ];

    store
        .apply_conversation_account_bindings(&source.source_id, &mut events)
        .expect("apply bindings");

    let accounts = events
        .iter()
        .map(|event| event.provider_account_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        accounts,
        vec![
            Some(original_account.clone()),
            Some(original_account),
            Some(inferred_account),
        ]
    );
}

#[test]
fn rediscovered_binding_extends_its_certain_range_and_never_weakens() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-desktop-range-extension"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("start");
    let hours = chrono::Duration::hours;
    let original_account = ProviderAccountId("original-account".to_string());
    let inferred_account = ProviderAccountId("inferred-account".to_string());
    let binding = |account: &ProviderAccountId, active_from, observed_until, confidence| {
        let mut binding = statsai_core::ConversationAccountBindingV1 {
            schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
            binding_id: String::new(),
            provider: "claude_code".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: account.clone(),
            conversation_id_hash: "same-session".to_string(),
            turn_id_hash: None,
            active_from,
            observed_until,
            // A direct observation is certain through its whole range.
            certain_until: if confidence == Confidence::High {
                observed_until
            } else {
                None
            },
            observed_at: start,
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence,
        };
        binding.binding_id = binding.derived_binding_id();
        binding
    };
    let seen_through = |hour| {
        binding(
            &original_account,
            None,
            Some(start + hours(hour)),
            Confidence::High,
        )
    };
    assert_eq!(
        store
            .upsert_conversation_account_bindings(&[seen_through(2)])
            .expect("first scan"),
        1
    );

    // A later scan saw the original account alone through hour 5.
    let mut extended = vec![seen_through(5)];
    store
        .retain_unseen_account_evidence(
            &source.source_id,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut extended,
        )
        .expect("retain");
    assert_eq!(extended.len(), 1, "a longer certain range is new evidence");
    assert_eq!(
        store
            .upsert_conversation_account_bindings(&extended)
            .expect("extend"),
        1
    );

    // Neither a repeat nor a weaker rediscovery changes it.
    let weaker = binding(&original_account, None, None, Confidence::Medium);
    let mut rediscovered = vec![seen_through(5), seen_through(3), weaker.clone()];
    store
        .retain_unseen_account_evidence(
            &source.source_id,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut rediscovered,
        )
        .expect("retain");
    assert!(rediscovered.is_empty());
    assert_eq!(
        store
            .upsert_conversation_account_bindings(&[weaker])
            .expect("weaker"),
        0
    );

    // Copied history later inferred the other account from hour 3.
    store
        .upsert_conversation_account_bindings(&[binding(
            &inferred_account,
            Some(start + hours(3)),
            None,
            Confidence::Medium,
        )])
        .expect("inferred");
    let mut events = vec![
        test_store_event(&source, start + hours(4), "certain"),
        test_store_event(&source, start + hours(6), "inferred"),
    ];
    store
        .apply_conversation_account_bindings(&source.source_id, &mut events)
        .expect("apply bindings");

    assert_eq!(events[0].provider_account_id, Some(original_account));
    assert_eq!(events[1].provider_account_id, Some(inferred_account));
}

#[test]
fn evidence_summaries_include_accounts_known_only_from_bindings() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-desktop-summary"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("start");
    let original_account = ProviderAccountId("original-account".to_string());
    let desktop_account = ProviderAccountId("desktop-account".to_string());
    let binding = |conversation: &str, account: &ProviderAccountId, active_from| {
        let mut binding = statsai_core::ConversationAccountBindingV1 {
            schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
            binding_id: String::new(),
            provider: "claude_code".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: account.clone(),
            conversation_id_hash: conversation.to_string(),
            turn_id_hash: None,
            active_from,
            observed_until: None,
            certain_until: None,
            observed_at: start,
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence: Confidence::High,
        };
        binding.binding_id = binding.derived_binding_id();
        binding
    };
    // The desktop account took one session over part-way and ran another alone.
    store
        .upsert_conversation_account_bindings(&[
            binding("taken-over", &original_account, None),
            binding(
                "taken-over",
                &desktop_account,
                Some(start + chrono::Duration::hours(2)),
            ),
            binding("own", &desktop_account, None),
        ])
        .expect("bindings");

    let summaries = store
        .account_evidence_summaries("device")
        .expect("summaries");

    let desktop = summaries
        .iter()
        .find(|summary| summary.provider_account_id == desktop_account)
        .expect("binding-only account is summarized");
    assert_eq!(desktop.provider, "claude_code");
    assert_eq!(desktop.strong_observation_count, 0);
    assert_eq!(desktop.directly_bound_conversations, 2);
    assert_eq!(desktop.conflict_count, 0);
    assert_eq!(
        desktop.evidence_kinds,
        vec![statsai_core::AccountEvidenceKind::AuthSnapshot]
    );
    assert!(summaries
        .iter()
        .any(|summary| summary.provider_account_id == original_account));
}

#[test]
fn binding_stops_after_its_recorded_activity_so_a_resume_elsewhere_keeps_the_source() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-desktop-resume-elsewhere"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("start");
    let desktop_account = ProviderAccountId("desktop-account".to_string());
    let cli_account = ProviderAccountId("cli-account".to_string());
    let last_desktop_activity = start + chrono::Duration::hours(4);
    let mut binding = statsai_core::ConversationAccountBindingV1 {
        schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
        binding_id: String::new(),
        provider: "claude_code".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: desktop_account.clone(),
        conversation_id_hash: "same-session".to_string(),
        turn_id_hash: None,
        active_from: None,
        observed_until: Some(last_desktop_activity),
        certain_until: None,
        observed_at: start,
        evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
        confidence: Confidence::High,
    };
    binding.binding_id = binding.derived_binding_id();
    store
        .upsert_conversation_account_bindings(&[binding])
        .expect("binding");
    let event = |at, record: &str| {
        let mut event = test_store_event(&source, at, record);
        // What the source's own attribution gave it.
        event.provider_account_id = Some(cli_account.clone());
        event
    };
    let mut events = vec![
        event(last_desktop_activity, "desktop"),
        // The app records activity a little before the reply's final usage.
        event(
            last_desktop_activity + chrono::Duration::seconds(40),
            "desktop-lag",
        ),
        // Resumed in the CLI an hour later.
        event(last_desktop_activity + chrono::Duration::hours(1), "cli"),
    ];

    store
        .apply_conversation_account_bindings(&source.source_id, &mut events)
        .expect("apply binding");

    let accounts = events
        .iter()
        .map(|event| event.provider_account_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        accounts,
        vec![
            Some(desktop_account.clone()),
            Some(desktop_account),
            Some(cli_account),
        ]
    );
}

fn desktop_binding(
    source: &SourceLocation,
    account: &ProviderAccountId,
    active_from: Option<chrono::DateTime<Utc>>,
    observed_until: chrono::DateTime<Utc>,
    certain: bool,
) -> statsai_core::ConversationAccountBindingV1 {
    let mut binding = statsai_core::ConversationAccountBindingV1 {
        schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
        binding_id: String::new(),
        provider: "claude_code".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: account.clone(),
        conversation_id_hash: "same-session".to_string(),
        turn_id_hash: None,
        active_from,
        observed_until: Some(observed_until),
        certain_until: certain.then_some(observed_until),
        observed_at: observed_until,
        evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
        confidence: if certain {
            Confidence::High
        } else {
            Confidence::Medium
        },
    };
    binding.binding_id = binding.derived_binding_id();
    binding
}

#[test]
fn an_inferred_reach_never_extends_a_certain_range() {
    let start = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("start");
    let hours = chrono::Duration::hours;
    let original_account = ProviderAccountId("original-account".to_string());
    let other_account = ProviderAccountId("other-account".to_string());
    for certain_first in [true, false] {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-desktop-merged-ranges"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        // Seen alone through hour 2, and inferred to hour 12 from copied history
        // that the other account actually ran from hour 9.
        let certain = desktop_binding(&source, &original_account, None, start + hours(2), true);
        let inferred = desktop_binding(&source, &original_account, None, start + hours(12), false);
        let ordered = if certain_first {
            [certain, inferred]
        } else {
            [inferred, certain]
        };
        for binding in ordered {
            store
                .upsert_conversation_account_bindings(&[binding])
                .expect("binding");
        }
        store
            .upsert_conversation_account_bindings(&[desktop_binding(
                &source,
                &other_account,
                Some(start + hours(9)),
                start + hours(12),
                false,
            )])
            .expect("other binding");
        let mut events = vec![
            test_store_event(&source, start + hours(1), "certain"),
            test_store_event(&source, start + hours(10), "inferred"),
        ];

        store
            .apply_conversation_account_bindings(&source.source_id, &mut events)
            .expect("apply bindings");

        assert_eq!(
            events[0].provider_account_id,
            Some(original_account.clone()),
            "certain first: {certain_first}"
        );
        assert_eq!(
            events[1].provider_account_id,
            Some(other_account.clone()),
            "certain first: {certain_first}"
        );
    }
}

#[test]
fn the_previous_accounts_trailing_reply_stays_with_it_across_a_takeover() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-desktop-trailing-reply"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("start");
    let hours = chrono::Duration::hours;
    let grace = chrono::Duration::seconds(statsai_core::CONVERSATION_ACTIVITY_GRACE_SECONDS);
    let previous_account = ProviderAccountId("previous-account".to_string());
    let next_account = ProviderAccountId("next-account".to_string());
    let previous_end = start + hours(2);
    store
        .upsert_conversation_account_bindings(&[
            desktop_binding(&source, &previous_account, None, previous_end, true),
            desktop_binding(
                &source,
                &next_account,
                Some(previous_end + grace),
                start + hours(20),
                true,
            ),
        ])
        .expect("bindings");
    let mut events = vec![
        test_store_event(
            &source,
            previous_end + chrono::Duration::seconds(40),
            "late",
        ),
        test_store_event(&source, start + hours(3), "next"),
    ];

    store
        .apply_conversation_account_bindings(&source.source_id, &mut events)
        .expect("apply bindings");

    assert_eq!(events[0].provider_account_id, Some(previous_account));
    assert_eq!(events[1].provider_account_id, Some(next_account));
}

#[test]
fn an_expired_binding_does_not_contest_one_still_in_reach() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-desktop-expired-peer"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("start");
    let hours = chrono::Duration::hours;
    let expired_account = ProviderAccountId("expired-account".to_string());
    let current_account = ProviderAccountId("current-account".to_string());
    // Both inferred from the same instant; one saw activity only to hour 1.
    store
        .upsert_conversation_account_bindings(&[
            desktop_binding(&source, &expired_account, None, start + hours(1), false),
            desktop_binding(&source, &current_account, None, start + hours(10), false),
        ])
        .expect("bindings");
    let mut events = vec![
        test_store_event(&source, start, "contested"),
        test_store_event(&source, start + hours(5), "current"),
    ];

    store
        .apply_conversation_account_bindings(&source.source_id, &mut events)
        .expect("apply bindings");

    assert_eq!(events[0].provider_account_id, None);
    assert_eq!(events[1].provider_account_id, Some(current_account));
}
