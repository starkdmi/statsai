use super::support::*;
use super::*;

#[test]
fn subscription_add_uses_canonical_provider_for_account_id() {
    let store = Store::in_memory().expect("store");

    subscription(
        SubscriptionCommand {
            command: SubscriptionSubcommand::Add {
                provider: "claude".to_string(),
                provider_account_id: None,
                provider_user_id: None,
                email: Some("personal@example.com".to_string()),
                label: None,
                plan: "Pro".to_string(),
                price: "20.00".parse().expect("price"),
                currency: "USD".parse().expect("currency"),
                paid_at: Some("2026-05-15".to_string()),
                started_at: "2026-05-15".to_string(),
                ended_at: None,
            },
        },
        &store,
    )
    .expect("subscription");

    let subscriptions = store.list_subscriptions().expect("subscriptions");
    assert_eq!(subscriptions.len(), 1);
    assert_eq!(subscriptions[0].provider, "claude_code");
    assert_eq!(
        subscriptions[0].provider_account_id,
        provider_account_id_from_identity("claude_code", None, Some("personal@example.com"))
            .expect("account id")
    );
}

#[test]
fn subscription_price_parses_exact_decimal_cents() {
    for (value, expected_cents) in [
        ("0", 0),
        ("20", 2_000),
        ("20.5", 2_050),
        ("20.05", 2_005),
        ("1000000.00", MAX_SUBSCRIPTION_PRICE_CENTS),
    ] {
        assert_eq!(
            value
                .parse::<SubscriptionPrice>()
                .expect("valid price")
                .cents(),
            expected_cents
        );
    }
}

#[test]
fn subscription_price_rejects_invalid_or_excessive_values() {
    for value in [
        "",
        "-1",
        "+1",
        "NaN",
        "inf",
        "1e3",
        ".50",
        "1.",
        "1.001",
        "1000000.01",
        "999999999999999999999999999999999999999999",
    ] {
        assert!(
            value.parse::<SubscriptionPrice>().is_err(),
            "price should be rejected: {value}"
        );
    }
}

#[test]
fn subscription_currency_normalizes_three_letter_codes() {
    assert_eq!(
        "usd"
            .parse::<CurrencyCode>()
            .expect("currency")
            .into_string(),
        "USD"
    );
    for value in ["", "US", "USDD", "U1D", "💵"] {
        assert!(
            value.parse::<CurrencyCode>().is_err(),
            "currency should be rejected: {value}"
        );
    }
}

#[test]
fn subscription_change_requires_active_existing_subscription() {
    let store = Store::in_memory().expect("store");
    let account = upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "codex",
            provider_user_id: None,
            email: Some("change@example.com"),
            label: None,
            plan_name: None,
            identity_source: Some(IdentitySource::UserConfigured),
            verified_at: None,
        },
    )
    .expect("account");

    let error = subscription(
        SubscriptionCommand {
            command: SubscriptionSubcommand::Change {
                provider: "codex".to_string(),
                provider_account_id: Some(account.provider_account_id.0.clone()),
                provider_user_id: None,
                email: None,
                label: None,
                plan: "Pro".to_string(),
                price: "200.00".parse().expect("price"),
                currency: "USD".parse().expect("currency"),
                paid_at: None,
                started_at: "2026-06-01".to_string(),
            },
        },
        &store,
    )
    .expect_err("missing active subscription");

    assert!(error
        .to_string()
        .contains("subscription change requires an active subscription"));
    assert!(store
        .list_subscriptions()
        .expect("subscriptions")
        .is_empty());
    assert_eq!(store.list_accounts().expect("accounts").len(), 1);
}

#[test]
fn subscription_change_closes_existing_period() {
    let store = Store::in_memory().expect("store");

    subscription(
        SubscriptionCommand {
            command: SubscriptionSubcommand::Add {
                provider: "codex".to_string(),
                provider_account_id: None,
                provider_user_id: None,
                email: Some("personal@example.com".to_string()),
                label: None,
                plan: "Plus".to_string(),
                price: "20.00".parse().expect("price"),
                currency: "USD".parse().expect("currency"),
                paid_at: Some("2026-05-01".to_string()),
                started_at: "2026-05-01".to_string(),
                ended_at: None,
            },
        },
        &store,
    )
    .expect("subscription add");

    subscription(
        SubscriptionCommand {
            command: SubscriptionSubcommand::Change {
                provider: "codex".to_string(),
                provider_account_id: None,
                provider_user_id: None,
                email: Some("personal@example.com".to_string()),
                label: None,
                plan: "Pro".to_string(),
                price: "200.00".parse().expect("price"),
                currency: "USD".parse().expect("currency"),
                paid_at: Some("2026-06-01".to_string()),
                started_at: "2026-06-01".to_string(),
            },
        },
        &store,
    )
    .expect("subscription change");

    let subscriptions = store.list_subscriptions().expect("subscriptions");
    assert_eq!(subscriptions.len(), 2);
    assert!(subscriptions
        .iter()
        .any(|subscription| subscription.plan_name == "Plus"
            && subscription.ended_at
                == Some(
                    Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
                        .single()
                        .expect("end")
                )));
    assert!(subscriptions
        .iter()
        .any(|subscription| subscription.plan_name == "Pro" && subscription.ended_at.is_none()));
}

#[test]
fn active_subscription_treats_legacy_verified_cycle_rows_as_current_periods() {
    let store = Store::in_memory().expect("store");
    let account_id = provider_account_id("codex", "verified@example.com");
    let started_at = Utc
        .with_ymd_and_hms(2026, 4, 30, 7, 43, 17)
        .single()
        .expect("started_at");
    let period_end = Utc
        .with_ymd_and_hms(2026, 5, 30, 7, 43, 17)
        .single()
        .expect("period_end");
    store
        .upsert_subscription(&Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: subscription_id("codex", &account_id, "Plus", started_at),
            provider: "codex".to_string(),
            provider_account_id: account_id.clone(),
            plan_name: "Plus".to_string(),
            price: 2000,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: Some(started_at),
            renewal_day: Some(30),
            started_at,
            ended_at: Some(period_end),
            current_period_ends_at: Some(period_end),
            status: SubscriptionStatus::Active,
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(
                Utc.with_ymd_and_hms(2026, 5, 3, 10, 54, 50)
                    .single()
                    .expect("verified_at"),
            ),
            notes: None,
        })
        .expect("legacy subscription");

    let active = active_subscription(
        &store,
        "codex",
        &account_id,
        Some("Plus"),
        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("lookup"),
    )
    .expect("active subscription");

    assert_eq!(active.provider_account_id, account_id);
    assert_eq!(active.plan_name, "Plus");
}
