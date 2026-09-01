use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Utc};
use serde_json::json;
use statsai_core::{
    periods_overlap, subscription_id, timestamp_in_period, BillingPeriod, IdentitySource,
    ProviderAccountId, Subscription, SubscriptionId, SubscriptionStatus,
    SUBSCRIPTION_SCHEMA_VERSION,
};
use statsai_store::Store;

use super::args::{SubscriptionCommand, SubscriptionSubcommand};
use super::format::{parse_date, print_subscription_json};
use super::source::{
    canonical_provider, resolve_existing_provider_account, resolve_or_create_provider_account,
};

pub(crate) fn subscription(command: SubscriptionCommand, store: &Store) -> Result<()> {
    match command.command {
        SubscriptionSubcommand::Add {
            provider,
            provider_account_id,
            provider_user_id,
            email,
            label,
            plan,
            price,
            currency,
            paid_at,
            started_at,
            ended_at,
        } => {
            let provider = canonical_provider(&provider)?;
            let price_cents = price.cents();
            let currency = currency.into_string();
            let account = resolve_or_create_provider_account(
                store,
                &provider,
                provider_account_id.as_deref(),
                provider_user_id.as_deref(),
                email.as_deref(),
                label,
            )?;
            let started_at = parse_date(&started_at)?;
            let ended_at = ended_at.as_deref().map(parse_date).transpose()?;
            validate_time_window(started_at, ended_at, "subscription")?;
            validate_subscription_overlap(
                store,
                &provider,
                &account.provider_account_id,
                started_at,
                ended_at,
                None,
            )?;
            let paid_at = paid_at
                .as_deref()
                .map(parse_date)
                .transpose()?
                .or(Some(started_at));
            let subscription = Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id(
                    &provider,
                    &account.provider_account_id,
                    &plan,
                    started_at,
                ),
                provider: provider.clone(),
                provider_account_id: account.provider_account_id.clone(),
                plan_name: plan,
                price: price_cents,
                currency,
                billing_period: BillingPeriod::Monthly,
                paid_at,
                renewal_day: paid_at.and_then(subscription_renewal_day),
                started_at,
                ended_at,
                current_period_ends_at: None,
                status: SubscriptionStatus::Active,
                record_source: IdentitySource::UserConfigured,
                verified_at: None,
                notes: None,
            };
            store.upsert_subscription(&subscription)?;
            print_subscription_json(&subscription)?;
        }
        SubscriptionSubcommand::Change {
            provider,
            provider_account_id,
            provider_user_id,
            email,
            label,
            plan,
            price,
            currency,
            paid_at,
            started_at,
        } => {
            let provider = canonical_provider(&provider)?;
            let price_cents = price.cents();
            let currency = currency.into_string();
            let account = resolve_existing_provider_account(
                store,
                &provider,
                provider_account_id.as_deref(),
                provider_user_id.as_deref(),
                email.as_deref(),
                label,
            )?;
            let started_at = parse_date(&started_at)?;
            let paid_at = paid_at
                .as_deref()
                .map(parse_date)
                .transpose()?
                .or(Some(started_at));
            if close_active_subscription(
                store,
                &provider,
                &account.provider_account_id,
                started_at,
            )?
            .is_none()
            {
                bail!(
                    "subscription change requires an active subscription for account {} at {}",
                    account.provider_account_id.0,
                    started_at.to_rfc3339()
                );
            }
            let subscription = Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id(
                    &provider,
                    &account.provider_account_id,
                    &plan,
                    started_at,
                ),
                provider: provider.clone(),
                provider_account_id: account.provider_account_id.clone(),
                plan_name: plan,
                price: price_cents,
                currency,
                billing_period: BillingPeriod::Monthly,
                paid_at,
                renewal_day: paid_at.and_then(subscription_renewal_day),
                started_at,
                ended_at: None,
                current_period_ends_at: None,
                status: SubscriptionStatus::Active,
                record_source: IdentitySource::UserConfigured,
                verified_at: None,
                notes: None,
            };
            store.upsert_subscription(&subscription)?;
            print_subscription_json(&subscription)?;
        }
        SubscriptionSubcommand::End {
            provider,
            provider_account_id,
            provider_user_id,
            email,
            ended_at,
        } => {
            let provider = canonical_provider(&provider)?;
            let account = resolve_existing_provider_account(
                store,
                &provider,
                provider_account_id.as_deref(),
                provider_user_id.as_deref(),
                email.as_deref(),
                None,
            )?;
            let subscription = end_active_subscription(
                store,
                &provider,
                &account.provider_account_id,
                ended_at
                    .as_deref()
                    .map(parse_date)
                    .transpose()?
                    .unwrap_or_else(Utc::now),
            )?;
            print_subscription_json(&subscription)?;
        }
        SubscriptionSubcommand::Remove {
            provider,
            provider_account_id,
            provider_user_id,
            email,
            plan,
            started_at,
            current,
        } => {
            if current == started_at.is_some() {
                bail!("pass either --started-at or --current");
            }
            let provider = canonical_provider(&provider)?;
            let account = resolve_existing_provider_account(
                store,
                &provider,
                provider_account_id.as_deref(),
                provider_user_id.as_deref(),
                email.as_deref(),
                None,
            )?;
            let subscription = if current {
                active_subscription(
                    store,
                    &provider,
                    &account.provider_account_id,
                    plan.as_deref(),
                    Utc::now(),
                )?
            } else {
                let started_at = parse_date(
                    started_at
                        .as_deref()
                        .with_context(|| "missing --started-at")?,
                )?;
                subscription_for_period(
                    store,
                    &provider,
                    &account.provider_account_id,
                    started_at,
                    plan.as_deref(),
                )?
            };
            let deleted = store.delete_subscription(&subscription.subscription_id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "subscription_id": subscription.subscription_id.0,
                    "deleted": deleted,
                    "subscription": subscription
                }))?
            );
        }
        SubscriptionSubcommand::List => println!(
            "{}",
            serde_json::to_string_pretty(&store.list_subscriptions()?)?
        ),
    }
    Ok(())
}

fn is_verified_subscription_source(source: &IdentitySource) -> bool {
    matches!(
        source,
        IdentitySource::LocalAuth
            | IdentitySource::ProviderAuth
            | IdentitySource::ProviderApi
            | IdentitySource::CookieOauth
            | IdentitySource::CliProbe
    )
}

pub(crate) fn active_subscription(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    plan: Option<&str>,
    timestamp: DateTime<Utc>,
) -> Result<Subscription> {
    store
        .list_subscriptions()?
        .into_iter()
        .filter(|subscription| {
            subscription.provider == provider
                && subscription.provider_account_id == *provider_account_id
                && plan
                    .map(|plan_name| subscription.plan_name.eq_ignore_ascii_case(plan_name))
                    .unwrap_or(true)
                && timestamp_in_period(
                    timestamp,
                    subscription.started_at,
                    effective_subscription_ended_at(subscription),
                )
        })
        .max_by(|left, right| left.started_at.cmp(&right.started_at))
        .with_context(|| {
            let plan_suffix = plan
                .map(|plan_name| format!(" plan {}", plan_name))
                .unwrap_or_default();
            format!(
                "no active{} subscription found for account {} at {}",
                plan_suffix,
                provider_account_id.0,
                timestamp.to_rfc3339()
            )
        })
}

fn subscription_renewal_day(timestamp: DateTime<Utc>) -> Option<u8> {
    u8::try_from(timestamp.day()).ok()
}

fn subscription_for_period(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    plan: Option<&str>,
) -> Result<Subscription> {
    store
        .list_subscriptions()?
        .into_iter()
        .find(|subscription| {
            subscription.provider == provider
                && subscription.provider_account_id == *provider_account_id
                && subscription.started_at == started_at
                && plan
                    .map(|plan_name| subscription.plan_name == plan_name)
                    .unwrap_or(true)
        })
        .with_context(|| {
            format!(
                "unknown subscription period for account {} starting {}",
                provider_account_id.0,
                started_at.to_rfc3339()
            )
        })
}

fn close_active_subscription(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    ended_at: DateTime<Utc>,
) -> Result<Option<Subscription>> {
    let active = store
        .list_subscriptions()?
        .into_iter()
        .find(|subscription| {
            subscription.provider == provider
                && subscription.provider_account_id == *provider_account_id
                && timestamp_in_period(
                    ended_at,
                    subscription.started_at,
                    effective_subscription_ended_at(subscription),
                )
        });
    let Some(mut subscription) = active else {
        return Ok(None);
    };
    validate_time_window(subscription.started_at, Some(ended_at), "subscription")?;
    subscription.ended_at = Some(ended_at);
    store.upsert_subscription(&subscription)?;
    Ok(Some(subscription))
}

fn effective_subscription_ended_at(subscription: &Subscription) -> Option<DateTime<Utc>> {
    if is_verified_subscription_source(&subscription.record_source)
        && subscription.status == SubscriptionStatus::Active
        && subscription.ended_at.is_some()
        && subscription.ended_at == subscription.current_period_ends_at
    {
        return None;
    }
    subscription.ended_at
}

fn end_active_subscription(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    ended_at: DateTime<Utc>,
) -> Result<Subscription> {
    close_active_subscription(store, provider, provider_account_id, ended_at)?.with_context(|| {
        format!(
            "no active subscription found for account {} at {}",
            provider_account_id.0,
            ended_at.to_rfc3339()
        )
    })
}

pub(crate) fn validate_time_window(
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    label: &str,
) -> Result<()> {
    if ended_at.is_some_and(|ended_at| ended_at <= started_at) {
        bail!("{label} ended_at must be after started_at");
    }
    Ok(())
}

pub(crate) fn validate_subscription_overlap(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    ignore_subscription_id: Option<&SubscriptionId>,
) -> Result<()> {
    for subscription in store.list_subscriptions()? {
        if ignore_subscription_id == Some(&subscription.subscription_id) {
            continue;
        }
        if subscription.provider != provider {
            continue;
        }
        if &subscription.provider_account_id != provider_account_id {
            continue;
        }
        if periods_overlap(
            started_at,
            ended_at,
            subscription.started_at,
            subscription.ended_at,
        ) {
            bail!(
                "subscription overlaps existing subscription {} for account {}",
                subscription.subscription_id.0,
                provider_account_id.0
            );
        }
    }
    Ok(())
}
