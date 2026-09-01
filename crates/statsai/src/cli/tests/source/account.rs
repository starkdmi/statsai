use super::*;

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
