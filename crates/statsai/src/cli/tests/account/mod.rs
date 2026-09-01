pub(super) use super::support::*;
pub(crate) use super::*;

mod merge;

#[test]
fn canonicalization_skips_accounts_without_surviving_evidence() {
    let store = Store::in_memory().expect("store");
    let mut evidence = AccountEvidenceScan::default();
    evidence
        .accounts
        .push(statsai_adapters::ObservedProviderAccount {
            provider_user_id: Some("unchanged-provider-user".to_string()),
            email: Some("unchanged@example.test".to_string()),
            plan_name: None,
            observed_at: Utc
                .with_ymd_and_hms(2026, 8, 23, 10, 12, 43)
                .single()
                .expect("date"),
        });

    retain_accounts_referenced_by_account_evidence("codex", &HashMap::new(), &mut evidence);
    canonicalize_account_evidence(&store, "codex", &mut evidence)
        .expect("canonicalize account evidence");

    assert!(evidence.accounts.is_empty());
    assert!(store.list_accounts().expect("accounts").is_empty());
}

fn plan_observation_fixture(
    source_id: &SourceId,
    account_id: Option<&ProviderAccountId>,
    plan: &str,
    observed_at: DateTime<Utc>,
    evidence_kind: statsai_core::AccountEvidenceKind,
) -> statsai_core::AccountPlanObservationV1 {
    statsai_core::AccountPlanObservationV1 {
        schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
        observation_id: account_plan_observation_id(
            source_id,
            account_id,
            plan,
            observed_at,
            evidence_kind,
        ),
        provider: "codex".to_string(),
        source_id: source_id.clone(),
        provider_account_id: account_id.cloned(),
        raw_plan_name: plan.to_ascii_lowercase(),
        plan_name: plan.to_string(),
        observed_at,
        active_from: None,
        active_until: None,
        is_current_snapshot: false,
        evidence_kind,
        confidence: Confidence::High,
        parser_version: "test.v1".to_string(),
        artifact_path_hash: "a".repeat(64),
        record_fingerprint: "b".repeat(64),
    }
}

#[test]
fn account_plans_report_the_newest_observation_per_account() {
    let store = Store::in_memory().expect("store");
    let source_id = SourceId("plans-source".to_string());
    let first = ProviderAccountId("acct-first".to_string());
    let second = ProviderAccountId("acct-second".to_string());
    let at = |day: u32| {
        Utc.with_ymd_and_hms(2026, 8, day, 12, 0, 0)
            .single()
            .expect("date")
    };
    // An older snapshot that still claims to be current: the newest observation must win over
    // it, which is the whole reason this reports `latest_observation` and not a derived plan.
    let mut stale_current_snapshot = plan_observation_fixture(
        &source_id,
        Some(&first),
        "Free",
        at(1),
        statsai_core::AccountEvidenceKind::AuthSnapshot,
    );
    stale_current_snapshot.is_current_snapshot = true;
    // A legacy row that kept the subscription's own provider casing.
    let mut non_canonical_provider = plan_observation_fixture(
        &source_id,
        Some(&second),
        "Pro",
        at(5),
        statsai_core::AccountEvidenceKind::LegacyLocalAuth,
    );
    non_canonical_provider.provider = "Codex".to_string();
    store
        .upsert_account_plan_observations(&[
            stale_current_snapshot,
            plan_observation_fixture(
                &source_id,
                Some(&first),
                "Plus",
                at(9),
                statsai_core::AccountEvidenceKind::QuotaStatus,
            ),
            non_canonical_provider,
            // Evidence that never resolved to an account still has to be visible: dropping it
            // would hide plan history the operator can still act on.
            plan_observation_fixture(
                &source_id,
                None,
                "Team",
                at(3),
                statsai_core::AccountEvidenceKind::AuthSnapshot,
            ),
        ])
        .expect("seed plan observations");

    let report =
        account_plan_evidence_report(&store, Some("codex"), None, false).expect("plan report");

    assert_eq!(report.len(), 3);
    let plan_for = |account: Option<&str>| {
        report
            .iter()
            .find(|entry| entry["provider_account_id"].as_str() == account)
            .map(|entry| entry["latest_observation"]["plan_name"].as_str().unwrap())
    };
    // The newest observation wins over an older one that still claims to be the current
    // snapshot, which is what `latest_observation` promises and a derived plan would not.
    assert_eq!(plan_for(Some("acct-first")), Some("Plus"));
    // Reached only through case-insensitive matching: this account's sole observation is
    // stored as `Codex`.
    assert_eq!(plan_for(Some("acct-second")), Some("Pro"));
    assert_eq!(plan_for(None), Some("Team"));
    let second_entry = report
        .iter()
        .find(|entry| entry["provider_account_id"] == "acct-second")
        .expect("second account entry");
    assert_eq!(
        second_entry["provider"], "codex",
        "a non-canonical stored provider is reported under its canonical name"
    );
    assert_eq!(
        second_entry["observation_count"], 1,
        "the case variant groups with its canonical provider rather than forming its own entry"
    );
    let first_entry = report
        .iter()
        .find(|entry| entry["provider_account_id"] == "acct-first")
        .expect("first account entry");
    assert_eq!(first_entry["observation_count"], 2);
    // Without `--all` the payload stays a summary.
    assert!(first_entry.get("observations").is_none());

    let detailed = account_plan_evidence_report(&store, Some("codex"), Some(&first), true)
        .expect("detailed plan report");
    assert_eq!(detailed.len(), 1, "the account filter selects one account");
    let observations = detailed[0]["observations"]
        .as_array()
        .expect("observations array");
    assert_eq!(observations.len(), 2);
    // Oldest first, so the newest is what `latest_observation` reports.
    assert_eq!(observations[0]["plan_name"], "Free");
    assert_eq!(observations[1]["plan_name"], "Plus");

    assert!(
        account_plan_evidence_report(&store, Some("claude_code"), None, false)
            .expect("other provider")
            .is_empty(),
        "the provider filter excludes other providers"
    );
}

#[test]
fn known_account_aliases_are_applied_before_evidence_deduplication() {
    let store = Store::in_memory().expect("store");
    let observed_at = Utc
        .with_ymd_and_hms(2026, 8, 23, 10, 12, 43)
        .single()
        .expect("date");
    upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "codex",
            provider_user_id: None,
            email: Some("owner@example.test"),
            label: None,
            plan_name: None,
            identity_source: Some(IdentitySource::LocalAuth),
            verified_at: Some(observed_at),
        },
    )
    .expect("email-only account");
    let source_id = SourceId("alias-dedup-source".to_string());
    let raw_account_id = provider_account_id_from_identity(
        "codex",
        Some("provider-user-1"),
        Some("owner@example.test"),
    )
    .expect("detected account id");
    let raw_plan = statsai_core::AccountPlanObservationV1 {
        schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
        observation_id: account_plan_observation_id(
            &source_id,
            Some(&raw_account_id),
            "plus",
            observed_at,
            statsai_core::AccountEvidenceKind::AuthSnapshot,
        ),
        provider: "codex".to_string(),
        source_id: source_id.clone(),
        provider_account_id: Some(raw_account_id.clone()),
        raw_plan_name: "plus".to_string(),
        plan_name: "Plus".to_string(),
        observed_at,
        active_from: None,
        active_until: None,
        is_current_snapshot: true,
        evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
        confidence: Confidence::High,
        parser_version: "test.v1".to_string(),
        artifact_path_hash: "a".repeat(64),
        record_fingerprint: "b".repeat(64),
    };
    let raw_binding = statsai_core::ConversationAccountBindingV1 {
        schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
        binding_id: conversation_account_binding_id(
            &source_id,
            &"c".repeat(64),
            None,
            &raw_account_id,
        ),
        provider: "codex".to_string(),
        source_id: source_id.clone(),
        provider_account_id: raw_account_id,
        conversation_id_hash: "c".repeat(64),
        turn_id_hash: None,
        observed_at,
        evidence_kind: statsai_core::AccountEvidenceKind::ResetHistory,
        confidence: Confidence::High,
    };
    let raw_scan = AccountEvidenceScan {
        accounts: vec![statsai_adapters::ObservedProviderAccount {
            provider_user_id: Some("provider-user-1".to_string()),
            email: Some("owner@example.test".to_string()),
            plan_name: None,
            observed_at,
        }],
        plan_observations: vec![raw_plan],
        conversation_bindings: vec![raw_binding],
        ..AccountEvidenceScan::default()
    };

    let mut first_scan = raw_scan.clone();
    canonicalize_account_evidence(&store, "codex", &mut first_scan)
        .expect("canonicalize first scan");
    store
        .upsert_account_plan_observations(&first_scan.plan_observations)
        .expect("store canonical plan");
    store
        .upsert_conversation_account_bindings(&first_scan.conversation_bindings)
        .expect("store canonical binding");

    let mut repeated_scan = raw_scan;
    let known_account_aliases =
        canonicalize_known_account_evidence(&store, "codex", &mut repeated_scan)
            .expect("canonicalize known aliases");
    store
        .retain_unseen_account_evidence(
            &source_id,
            &mut repeated_scan.identity_observations,
            &mut repeated_scan.plan_observations,
            &mut repeated_scan.conversation_bindings,
        )
        .expect("filter canonical evidence");
    assert!(repeated_scan.plan_observations.is_empty());
    assert!(repeated_scan.conversation_bindings.is_empty());

    repeated_scan
        .identity_observations
        .push(statsai_core::AccountIdentityObservationV1 {
            schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: "identity-provider-user-enrichment".to_string(),
            provider: "codex".to_string(),
            source_id,
            provider_account_id: known_account_aliases.values().next().cloned(),
            provider_user_id_hash: Some("provider-user-hash".to_string()),
            email_hash: Some("email-hash".to_string()),
            conversation_id_hash: None,
            turn_id_hash: None,
            observed_at,
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence: Confidence::High,
            auth_mode: Some("chatgpt".to_string()),
            application_version: None,
            parser_version: "test.v1".to_string(),
            artifact_kind: "auth_json".to_string(),
            artifact_path_hash: "d".repeat(64),
            record_fingerprint: "e".repeat(64),
        });
    retain_accounts_referenced_by_account_evidence(
        "codex",
        &known_account_aliases,
        &mut repeated_scan,
    );
    assert_eq!(
        repeated_scan.accounts.len(),
        1,
        "the account carrying a newly learned provider ID must survive canonical alias remapping"
    );
    canonicalize_account_evidence(&store, "codex", &mut repeated_scan)
        .expect("enrich canonical account");
    let accounts = store.list_accounts().expect("accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        accounts[0].provider_user_id.as_deref(),
        Some("provider-user-1")
    );
}

#[test]
fn apply_verified_source_state_reuses_existing_email_account() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-verified-state"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let existing = upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "codex",
            provider_user_id: None,
            email: Some("existing@example.com"),
            label: Some("existing-alias".to_string()),
            plan_name: None,
            identity_source: Some(IdentitySource::UserConfigured),
            verified_at: None,
        },
    )
    .expect("existing account");
    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("started_at");
    let verified_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
        .single()
        .expect("verified_at");
    let current_period_ends_at = Utc
        .with_ymd_and_hms(2026, 6, 29, 10, 12, 43)
        .single()
        .expect("current_period_ends_at");

    apply_verified_source_state(
        &store,
        &source,
        Some(&VerifiedSourceState {
            provider_user_id: Some("chatgpt-account-123".to_string()),
            email: Some("existing@example.com".to_string()),
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
                ended_at: Some(current_period_ends_at),
                current_period_ends_at: Some(current_period_ends_at),
                status: SubscriptionStatus::Active,
                verified_at: Some(verified_at),
            }),
        }),
    )
    .expect("apply verified state");

    let accounts = store.list_accounts().expect("accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        accounts[0].provider_account_id,
        existing.provider_account_id
    );
    assert_eq!(
        accounts[0].provider_user_id.as_deref(),
        Some("chatgpt-account-123")
    );
    assert_eq!(accounts[0].plan_name, None);
    assert_eq!(accounts[0].verified_at, Some(verified_at));

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(
        assignments[0].provider_account_id,
        existing.provider_account_id
    );
    assert_eq!(assignments[0].record_source, IdentitySource::LocalAuth);
    assert_eq!(assignments[0].verified_at, Some(verified_at));

    assert!(store
        .list_subscriptions()
        .expect("subscriptions")
        .is_empty());
}

#[test]
fn upsert_provider_account_rejects_conflicting_email_and_provider_user_id() {
    let store = Store::in_memory().expect("store");
    let email_account = upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "codex",
            provider_user_id: None,
            email: Some("conflict@example.com"),
            label: Some("email".to_string()),
            plan_name: None,
            identity_source: Some(IdentitySource::UserConfigured),
            verified_at: None,
        },
    )
    .expect("email account");
    let user_account = upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "codex",
            provider_user_id: Some("acct-conflict"),
            email: None,
            label: Some("user".to_string()),
            plan_name: None,
            identity_source: Some(IdentitySource::UserConfigured),
            verified_at: None,
        },
    )
    .expect("user account");

    let error = upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "codex",
            provider_user_id: Some("acct-conflict"),
            email: Some("conflict@example.com"),
            label: None,
            plan_name: None,
            identity_source: Some(IdentitySource::LocalAuth),
            verified_at: None,
        },
    )
    .expect_err("conflicting identity");

    assert!(error
        .to_string()
        .contains("conflicting provider account identifiers"));
    let accounts = store.list_accounts().expect("accounts");
    assert_eq!(accounts.len(), 2);
    assert!(accounts.iter().any(|account| {
        account.provider_account_id == email_account.provider_account_id
            && account.provider_user_id.is_none()
            && account.email.as_deref() == Some("conflict@example.com")
    }));
    assert!(accounts.iter().any(|account| {
        account.provider_account_id == user_account.provider_account_id
            && account.provider_user_id.as_deref() == Some("acct-conflict")
            && account.email.is_none()
    }));
}

#[test]
fn lookup_provider_account_does_not_create_orphans() {
    let store = Store::in_memory().expect("store");

    let error = resolve_existing_provider_account(
        &store,
        "codex",
        None,
        None,
        Some("typo@example.com"),
        None,
    )
    .expect_err("missing account");

    assert!(error
        .to_string()
        .contains("unknown provider account selector"));
    assert!(store.list_accounts().expect("accounts").is_empty());
}

#[test]
fn provider_account_id_lookup_rejects_wrong_provider() {
    let store = Store::in_memory().expect("store");
    let account = upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "claude_code",
            provider_user_id: None,
            email: Some("claude@example.com"),
            label: None,
            plan_name: None,
            identity_source: Some(IdentitySource::UserConfigured),
            verified_at: None,
        },
    )
    .expect("account");

    let existing_error = resolve_existing_provider_account(
        &store,
        "codex",
        Some(&account.provider_account_id.0),
        None,
        None,
        None,
    )
    .expect_err("wrong existing provider");
    let create_error = resolve_or_create_provider_account(
        &store,
        "codex",
        Some(&account.provider_account_id.0),
        Some("codex-user"),
        None,
        None,
    )
    .expect_err("wrong create provider");

    assert!(existing_error
        .to_string()
        .contains("belongs to claude_code"));
    assert!(create_error.to_string().contains("belongs to claude_code"));
}

#[test]
fn remove_orphan_provider_account_rejects_referenced_account() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "codex-local-jsonl",
        "0",
        Path::new("/tmp/.codex-existing-alias"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc
        .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
        .single()
        .expect("now");
    let alias = test_account("codex", Some("existing-alias"), None, None, None, now);
    store.upsert_account(&alias).expect("alias account");
    let assignment = test_assignment(
        &source,
        &alias.provider_account_id,
        now - Duration::days(1),
        None,
        now,
    );
    store
        .upsert_source_account_assignment(&assignment)
        .expect("assignment");

    let error = remove_orphan_provider_account(&store, "codex", "existing-alias", false)
        .expect_err("referenced account should fail");
    assert!(error.to_string().contains("still has references"));
}

#[test]
fn remove_orphan_provider_account_deletes_account_and_clears_sync_tracking() {
    let store = Store::in_memory().expect("store");
    let now = Utc
        .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
        .single()
        .expect("now");
    let alias = test_account("codex", Some("orphan-alias"), None, None, None, now);
    store.upsert_account(&alias).expect("alias account");
    store
        .record_accounts_synced(
            "http",
            "https://api.example.com/api/sync/batches",
            &[sanitize_account_for_sync(alias.clone())],
        )
        .expect("sync account");
    store
        .record_sync_success(
            "http",
            "https://api.example.com/api/sync/batches",
            "batch_1",
            &[],
            &[],
            None,
        )
        .expect("sync success");

    let report =
        remove_orphan_provider_account(&store, "codex", "orphan-alias", false).expect("remove");
    assert!(report.deleted);
    assert!(report.reset_local_sync_tracking);
    assert!(store.list_sync_states().expect("sync states").is_empty());
    assert!(store
        .list_accounts()
        .expect("accounts")
        .into_iter()
        .all(|account| account.provider_account_id != alias.provider_account_id));
}
