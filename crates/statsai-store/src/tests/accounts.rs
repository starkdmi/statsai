use super::support::*;
use super::*;
use statsai_core::{
    account_plan_observation_id, AccountEvidenceKind, AccountPlanObservationV1,
    ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION,
};

#[test]
fn reads_legacy_subscription_payloads_with_missing_account_and_start() {
    let store = Store::in_memory().expect("store");
    let payload = r#"{
            "schema_version":"subscription.v1",
            "subscription_id":"sub_legacy",
            "provider":"codex",
            "plan_name":"Plus",
            "price":20.0,
            "currency":"USD",
            "billing_period":"monthly",
            "paid_at":"2026-05-01T00:00:00Z",
            "status":"active"
        }"#;
    store
            .conn
            .execute(
                "INSERT INTO subscriptions (subscription_id, provider, provider_account_id, payload) VALUES (?1, ?2, ?3, ?4)",
                params!["sub_legacy", "codex", Option::<String>::None, payload],
            )
            .expect("insert legacy subscription");

    let subscription = store
        .subscription(&SubscriptionId("sub_legacy".to_string()))
        .expect("read legacy subscription")
        .expect("subscription exists");
    let subscriptions = store
        .list_subscriptions()
        .expect("list legacy subscriptions");

    assert_eq!(subscriptions, vec![subscription.clone()]);
    assert_eq!(subscription.provider, "codex");
    assert_eq!(
        subscription.provider_account_id,
        provider_account_id("codex", "legacy_subscription:sub_legacy")
    );
    assert_eq!(
        subscription.started_at,
        Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
            .single()
            .expect("started_at")
    );
}

#[test]
fn migrates_only_codex_local_auth_subscriptions_into_plan_evidence() {
    let store = Store::in_memory().expect("store");
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("started_at");
    let active_until = Utc
        .with_ymd_and_hms(2026, 7, 1, 0, 0, 0)
        .single()
        .expect("active_until");
    let account_id = provider_account_id("codex", "provider-user");
    let synthetic = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: SubscriptionId("synthetic-codex-plan".to_string()),
        provider: "codex".to_string(),
        provider_account_id: account_id.clone(),
        plan_name: "future_ultra".to_string(),
        price: 20_00,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: Some(started_at),
        renewal_day: Some(1),
        started_at,
        ended_at: None,
        current_period_ends_at: Some(active_until),
        status: SubscriptionStatus::Active,
        record_source: IdentitySource::LocalAuth,
        verified_at: Some(started_at),
        notes: None,
    };
    let manual = Subscription {
        subscription_id: SubscriptionId("manual-codex-billing".to_string()),
        record_source: IdentitySource::UserConfigured,
        price: 12_34,
        ..synthetic.clone()
    };
    store.upsert_subscription(&synthetic).expect("synthetic");
    store.upsert_subscription(&manual).expect("manual");

    assert_eq!(
        store
            .migrate_legacy_codex_local_auth_subscriptions_to_plan_evidence()
            .expect("migration"),
        1
    );
    assert_eq!(
        store
            .migrate_legacy_codex_local_auth_subscriptions_to_plan_evidence()
            .expect("repeat migration"),
        0
    );

    assert_eq!(store.list_subscriptions().expect("billing"), vec![manual]);
    let observations = store.account_plan_observations().expect("plan evidence");
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].provider_account_id, Some(account_id));
    assert_eq!(observations[0].raw_plan_name, "future_ultra");
    assert_eq!(observations[0].plan_name, "Future Ultra");
    assert_eq!(observations[0].active_from, Some(started_at));
    assert_eq!(observations[0].active_until, Some(active_until));
    assert_eq!(
        observations[0].evidence_kind,
        statsai_core::AccountEvidenceKind::LegacyLocalAuth
    );
}

#[test]
fn an_unreadable_legacy_payload_does_not_make_the_store_unopenable() {
    let path = tempfile::tempdir()
        .expect("tempdir")
        .keep()
        .join("store.db");
    {
        let store = Store::open(&path).expect("initial store");
        store
            .conn
            .execute(
                "INSERT INTO subscriptions
                       (subscription_id, provider, provider_account_id, payload)
                     VALUES (?1, ?2, ?3, ?4)",
                params![
                    "corrupt-subscription",
                    "codex",
                    "codex:corrupt",
                    "{ this is not json",
                ],
            )
            .expect("corrupt subscription row");
        store
            .conn
            .execute(
                "INSERT INTO provider_accounts
                       (provider_account_id, provider, payload, updated_at)
                     VALUES (?1, ?2, ?3, ?4)",
                params![
                    "codex:corrupt-account",
                    "codex",
                    "{ also not json",
                    Utc::now().to_rfc3339(),
                ],
            )
            .expect("corrupt account row");
        store
            .conn
            .execute(
                "DELETE FROM local_metadata WHERE key = ?1",
                params![LEGACY_CODEX_PLAN_CONVERSION_METADATA_KEY],
            )
            .expect("clear conversion flag");
    }

    // The conversion runs inside `migrate`, so a hard error here would roll
    // back before the completion flag is written and fail every later open.
    let reopened = Store::open(&path).expect("a corrupt legacy row must not brick the store");
    assert_eq!(
        reopened
            .metadata_value(LEGACY_CODEX_PLAN_CONVERSION_METADATA_KEY)
            .expect("conversion flag")
            .as_deref(),
        Some("1"),
        "the one-shot conversion must be recorded as done, not retried forever"
    );
    Store::open(&path).expect("a second open still succeeds");
}

#[test]
fn migrates_legacy_codex_account_plan_without_subscription() {
    let store = Store::in_memory().expect("store");
    let observed_at = Utc
        .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("observed at");
    let account_id = provider_account_id("codex", "legacy-plan-account");
    store
        .upsert_account(&ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: account_id.clone(),
            provider: "codex".to_string(),
            identity_source: IdentitySource::LocalAuth,
            provider_user_id: None,
            provider_user_id_hash: Some("a".repeat(64)),
            email: None,
            email_hash: None,
            org_id_hash: None,
            account_label: Some("Codex account".to_string()),
            plan_name: Some("future_ultra".to_string()),
            confidence: Confidence::High,
            verified_at: Some(observed_at),
            created_at: observed_at,
            updated_at: observed_at,
        })
        .expect("legacy account");

    assert_eq!(
        store
            .migrate_legacy_codex_local_auth_subscriptions_to_plan_evidence()
            .expect("migration"),
        0
    );

    let account = store
        .account(&account_id)
        .expect("account")
        .expect("exists");
    assert_eq!(account.plan_name, None);
    assert_eq!(account.account_label.as_deref(), Some("Codex account"));
    let observations = store.account_plan_observations().expect("plan evidence");
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].provider_account_id, Some(account_id));
    assert_eq!(observations[0].raw_plan_name, "future_ultra");
    assert_eq!(observations[0].plan_name, "Future Ultra");
    assert_eq!(observations[0].active_from, None);
    assert_eq!(observations[0].active_until, None);
    assert_eq!(
        observations[0].evidence_kind,
        statsai_core::AccountEvidenceKind::LegacyLocalAuth
    );
    assert_eq!(
        store
            .migrate_legacy_codex_local_auth_subscriptions_to_plan_evidence()
            .expect("repeat migration"),
        0
    );
    assert_eq!(
        store
            .account_plan_observations()
            .expect("repeated plan evidence")
            .len(),
        1
    );
}

#[test]
fn completed_legacy_plan_conversion_is_not_repeated_on_migrate() {
    let store = Store::in_memory().expect("store");
    store
        .conn
        .execute(
            r#"
                INSERT INTO account_plan_observations (
                  observation_id, provider, source_id, provider_account_id,
                  observed_at, active_from, active_until, plan_name, evidence_kind, payload
                ) VALUES (?1, ?2, ?3, NULL, ?4, NULL, NULL, ?5, ?6, ?7)
                "#,
            params![
                "post-conversion-malformed-observation",
                "codex",
                "source-after-conversion",
                "2026-08-23T00:00:00Z",
                "Pro",
                "quota_status",
                "not-json"
            ],
        )
        .expect("insert evidence that must not be rescanned");

    store.migrate().expect("completed conversion stays skipped");
}

#[test]
fn migration_rekeys_plan_observations_onto_the_canonical_plan_identity() {
    let store = Store::in_memory().expect("store");
    let source_id = SourceId("rekey-source".to_string());
    let account_id = ProviderAccountId("rekey-account".to_string());
    let observed_at = Utc
        .with_ymd_and_hms(2026, 8, 23, 0, 0, 0)
        .single()
        .expect("timestamp");
    // The identity an earlier build wrote, before the canonical plan joined it.
    let legacy_observation_id = format!(
        "plan_observation_{}",
        &hash_text(&format!(
            "account_plan_observation.v1:{}:{}:{}:{}:{:?}",
            source_id.0,
            account_id.0,
            "claude_max",
            observed_at.to_rfc3339(),
            AccountEvidenceKind::AuthSnapshot
        ))[..32]
    );
    let observation = AccountPlanObservationV1 {
        schema_version: ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
        observation_id: legacy_observation_id.clone(),
        provider: "claude_code".to_string(),
        source_id: source_id.clone(),
        provider_account_id: Some(account_id.clone()),
        raw_plan_name: "claude_max".to_string(),
        plan_name: "Max 20x".to_string(),
        observed_at,
        active_from: None,
        active_until: None,
        is_current_snapshot: true,
        evidence_kind: AccountEvidenceKind::AuthSnapshot,
        confidence: Confidence::Medium,
        parser_version: "claude-account-evidence.v1".to_string(),
        artifact_path_hash: "a".repeat(64),
        record_fingerprint: "b".repeat(64),
    };
    store
        .upsert_account_plan_observations(std::slice::from_ref(&observation))
        .expect("seed legacy observation");

    crate::migrations::apply_migration_023(&store.conn).expect("re-key plan observations");

    let current_observation_id = account_plan_observation_id(
        &source_id,
        Some(&account_id),
        &observation.raw_plan_name,
        &observation.plan_name,
        observed_at,
        AccountEvidenceKind::AuthSnapshot,
    );
    let stored = store.account_plan_observations().expect("observations");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].observation_id, current_observation_id);
    assert_ne!(current_observation_id, legacy_observation_id);

    // The next scan re-emits the same observation under the current identity, so
    // a migrated store must recognize it instead of appending a duplicate.
    let mut rescanned = observation;
    rescanned.observation_id = current_observation_id;
    store
        .upsert_account_plan_observations(std::slice::from_ref(&rescanned))
        .expect("rescan");
    assert_eq!(
        store
            .account_plan_observations()
            .expect("observations")
            .len(),
        1
    );
}
