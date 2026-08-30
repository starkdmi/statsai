use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::Value;
use statsai_core::{
    display_account_identity, normalize_email, normalize_provider_user_id, subscription_id,
    IdentitySource, ProviderAccount, ProviderAccountId, Subscription,
};
use statsai_store::{QuotaQuery, Store};
use std::collections::{BTreeMap, HashMap};

use super::args::{AccountCommand, AccountSubcommand};

use super::source::{
    canonical_provider, canonical_provider_name, connect_source_to_account,
    ConnectSourceToAccountInput,
};
use crate::validate_subscription_overlap;

pub(crate) fn account(command: AccountCommand, store: &Store) -> Result<()> {
    match command.command {
        AccountSubcommand::List => {
            println!("{}", serde_json::to_string_pretty(&store.list_accounts()?)?);
        }
        AccountSubcommand::Plans {
            provider,
            account,
            all,
        } => {
            let provider = provider.as_deref().map(canonical_provider).transpose()?;
            let selected_account = match (account.as_deref(), provider.as_deref()) {
                (Some(selector), Some(provider)) => Some(
                    resolve_existing_provider_account_selector(store, provider, selector)?
                        .provider_account_id,
                ),
                (Some(_), None) => {
                    bail!("--account needs --provider so the selector resolves in one provider")
                }
                (None, _) => None,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&account_plan_evidence_report(
                    store,
                    provider.as_deref(),
                    selected_account.as_ref(),
                    all,
                )?)?
            );
        }
        AccountSubcommand::Merge {
            provider,
            from,
            to,
            dry_run,
        } => {
            let report = merge_provider_accounts(
                store,
                &canonical_provider(&provider)?,
                &from,
                &to,
                dry_run,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        AccountSubcommand::Remove {
            provider,
            account,
            dry_run,
        } => {
            let report = remove_orphan_provider_account(
                store,
                &canonical_provider(&provider)?,
                &account,
                dry_run,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub(crate) struct AccountReferenceCounts {
    pub(crate) source_account_assignments: usize,
    pub(crate) subscriptions: usize,
    pub(crate) events: usize,
    pub(crate) summaries: usize,
    pub(crate) quota_observations: usize,
    pub(crate) identity_observations: usize,
    pub(crate) plan_observations: usize,
    pub(crate) conversation_bindings: usize,
}

impl AccountReferenceCounts {
    pub(crate) fn total(&self) -> usize {
        self.source_account_assignments
            + self.subscriptions
            + self.events
            + self.summaries
            + self.quota_observations
            + self.identity_observations
            + self.plan_observations
            + self.conversation_bindings
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct AccountMergeReport {
    pub(crate) provider: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) from_provider_account_id: String,
    pub(crate) to_provider_account_id: String,
    pub(crate) moved_source_account_assignments: usize,
    pub(crate) moved_subscriptions: usize,
    pub(crate) moved_events: usize,
    pub(crate) moved_summaries: usize,
    pub(crate) moved_identity_observations: usize,
    pub(crate) moved_plan_observations: usize,
    pub(crate) moved_conversation_bindings: usize,
    pub(crate) deleted_source_account: bool,
    pub(crate) remaining_references: AccountReferenceCounts,
    pub(crate) reset_local_sync_tracking: bool,
    pub(crate) dry_run: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct AccountRemoveReport {
    pub(crate) provider: String,
    pub(crate) account: String,
    pub(crate) provider_account_id: String,
    pub(crate) deleted: bool,
    pub(crate) remaining_references: AccountReferenceCounts,
    pub(crate) reset_local_sync_tracking: bool,
    pub(crate) dry_run: bool,
}

pub(crate) fn merge_provider_accounts(
    store: &Store,
    provider: &str,
    from_selector: &str,
    to_selector: &str,
    dry_run: bool,
) -> Result<AccountMergeReport> {
    let from = resolve_existing_provider_account_selector(store, provider, from_selector)?;
    let to = resolve_existing_provider_account_selector(store, provider, to_selector)?;
    if from.provider_account_id == to.provider_account_id {
        bail!("source and destination accounts are the same");
    }

    let assignments_to_move: Vec<_> = store
        .list_source_account_assignments()?
        .into_iter()
        .filter(|assignment| assignment.provider == provider)
        .filter(|assignment| assignment.provider_account_id == from.provider_account_id)
        .collect();
    let subscriptions_to_move: Vec<_> = store
        .list_subscriptions()?
        .into_iter()
        .filter(|subscription| subscription.provider == provider)
        .filter(|subscription| subscription.provider_account_id == from.provider_account_id)
        .collect();
    let direct_events_to_move = store
        .events()?
        .into_iter()
        .filter(|event| event.provider == provider)
        .filter(|event| event.provider_account_id.as_ref() == Some(&from.provider_account_id))
        .count();
    let direct_summaries_to_move = store
        .summaries()?
        .into_iter()
        .filter(|summary| summary.provider == provider)
        .filter(|summary| summary.provider_account_id.as_ref() == Some(&from.provider_account_id))
        .count();
    let evidence_to_move =
        store.account_evidence_reference_counts(provider, &from.provider_account_id)?;

    if !dry_run {
        for assignment in &assignments_to_move {
            connect_source_to_account(
                store,
                ConnectSourceToAccountInput {
                    source_id: &assignment.source_id,
                    provider_account_id_value: Some(&to.provider_account_id.0),
                    provider_user_id: None,
                    email: None,
                    label: None,
                    started_at: assignment.started_at,
                    ended_at: assignment.ended_at,
                },
            )?;
        }
        for subscription in &subscriptions_to_move {
            move_subscription_to_account(store, subscription, &to.provider_account_id)?;
        }
        move_direct_account_records(
            store,
            provider,
            &from.provider_account_id,
            &to.provider_account_id,
        )?;
    }

    let remaining_references =
        account_reference_counts(store, &from.provider_account_id, Some(provider))?;
    let deleted_source_account = if !dry_run && remaining_references.total() == 0 {
        store.delete_account(&from.provider_account_id)?
    } else {
        false
    };
    if !dry_run {
        store.clear_sync_tracking()?;
    }

    Ok(AccountMergeReport {
        provider: provider.to_string(),
        from: display_account_identity(&from),
        to: display_account_identity(&to),
        from_provider_account_id: from.provider_account_id.0,
        to_provider_account_id: to.provider_account_id.0,
        moved_source_account_assignments: assignments_to_move.len(),
        moved_subscriptions: subscriptions_to_move.len(),
        moved_events: direct_events_to_move,
        moved_summaries: direct_summaries_to_move,
        moved_identity_observations: evidence_to_move.identity_observations,
        moved_plan_observations: evidence_to_move.plan_observations,
        moved_conversation_bindings: evidence_to_move.conversation_bindings,
        deleted_source_account,
        remaining_references,
        reset_local_sync_tracking: !dry_run,
        dry_run,
    })
}

pub(crate) fn remove_orphan_provider_account(
    store: &Store,
    provider: &str,
    selector: &str,
    dry_run: bool,
) -> Result<AccountRemoveReport> {
    let account = resolve_existing_provider_account_selector(store, provider, selector)?;
    let remaining_references =
        account_reference_counts(store, &account.provider_account_id, Some(provider))?;
    if remaining_references.total() > 0 {
        bail!(
            "account {} still has references: {} source assignments, {} subscriptions, {} events, {} summaries, {} quota observations, {} identity observations, {} plan observations, {} conversation bindings",
            display_account_identity(&account),
            remaining_references.source_account_assignments,
            remaining_references.subscriptions,
            remaining_references.events,
            remaining_references.summaries,
            remaining_references.quota_observations,
            remaining_references.identity_observations,
            remaining_references.plan_observations,
            remaining_references.conversation_bindings
        );
    }
    let deleted = if dry_run {
        false
    } else {
        store.delete_account(&account.provider_account_id)?
    };
    if !dry_run {
        store.clear_sync_tracking()?;
    }

    Ok(AccountRemoveReport {
        provider: provider.to_string(),
        account: display_account_identity(&account),
        provider_account_id: account.provider_account_id.0,
        deleted,
        remaining_references,
        reset_local_sync_tracking: !dry_run,
        dry_run,
    })
}

/// Groups stored plan observations by account so the CLI can show detected plans.
///
/// The dashboard derives a plan *timeline* from these rows -- segments, gaps, and a current plan --
/// and that derivation lives in the hosted API. Reimplementing it here would create a second
/// answer to the same question that could disagree with the first, so this reports what is stored
/// instead: the newest observation per account, and every observation behind `--all`. The field is
/// named `latest_observation` rather than `current_plan` for exactly that reason -- it is the most
/// recent thing this machine recorded, not a derived verdict about what the account is on now.
pub(crate) fn account_plan_evidence_report(
    store: &Store,
    provider: Option<&str>,
    provider_account_id: Option<&ProviderAccountId>,
    include_all_observations: bool,
) -> Result<Vec<Value>> {
    let accounts = store
        .list_accounts()?
        .into_iter()
        .map(|account| (account.provider_account_id.clone(), account))
        .collect::<HashMap<_, _>>();
    let mut grouped: BTreeMap<
        (String, Option<String>),
        Vec<statsai_core::AccountPlanObservationV1>,
    > = BTreeMap::new();
    for observation in store.account_plan_observations()? {
        // Stored observations do not all carry the canonical spelling: the legacy subscription
        // migration matches `codex` case-insensitively and then copies the subscription's own
        // provider string through, and nothing normalizes a payload on read. Comparing and
        // grouping on the raw value would hide a `Codex` row from `--provider codex` and list its
        // account a second time under its own heading.
        // `canonical_provider_name` matches exact lowercase aliases, so the stored value is folded
        // before the lookup. A provider this build does not recognize keeps its stored spelling
        // rather than being silently relabelled.
        let canonical_provider =
            canonical_provider_name(&observation.provider.to_ascii_lowercase())
                .map(str::to_string)
                .unwrap_or_else(|| observation.provider.clone());
        if provider.is_some_and(|provider| !canonical_provider.eq_ignore_ascii_case(provider)) {
            continue;
        }
        if provider_account_id
            .is_some_and(|account_id| observation.provider_account_id.as_ref() != Some(account_id))
        {
            continue;
        }
        let key = (
            canonical_provider,
            observation
                .provider_account_id
                .as_ref()
                .map(|account_id| account_id.0.clone()),
        );
        grouped.entry(key).or_default().push(observation);
    }

    let describe = |observation: &statsai_core::AccountPlanObservationV1| {
        serde_json::json!({
            "plan_name": observation.plan_name,
            "raw_plan_name": observation.raw_plan_name,
            "observed_at": observation.observed_at,
            "active_from": observation.active_from,
            "active_until": observation.active_until,
            "is_current_snapshot": observation.is_current_snapshot,
            "evidence_kind": observation.evidence_kind,
            "confidence": observation.confidence,
            "source_id": observation.source_id,
        })
    };

    let mut report = Vec::new();
    for ((provider, account_id), mut observations) in grouped {
        // Newest last, with the id breaking ties so two observations sharing a timestamp -- which
        // happens when one artifact yields both a snapshot and a login -- always order the same way.
        observations.sort_by(|left, right| {
            left.observed_at
                .cmp(&right.observed_at)
                .then_with(|| left.observation_id.cmp(&right.observation_id))
        });
        let account = account_id
            .as_ref()
            .and_then(|account_id| accounts.get(&ProviderAccountId(account_id.clone())));
        let mut entry = serde_json::json!({
            "provider": provider,
            "provider_account_id": account_id,
            "email": account.and_then(|account| account.email.clone()),
            "account_label": account.and_then(|account| account.account_label.clone()),
            "observation_count": observations.len(),
            "latest_observation": observations.last().map(&describe),
        });
        if include_all_observations {
            entry["observations"] = Value::Array(observations.iter().map(&describe).collect());
        }
        report.push(entry);
    }
    Ok(report)
}

pub(crate) fn resolve_existing_provider_account_selector(
    store: &Store,
    provider: &str,
    selector: &str,
) -> Result<ProviderAccount> {
    let selector = selector.trim();
    if selector.is_empty() {
        bail!("account selector cannot be empty");
    }
    let normalized_email = normalize_email(selector);
    let normalized_provider_user_id = normalize_provider_user_id(selector);
    let normalized_label = selector.to_ascii_lowercase();

    let matches: Vec<_> = store
        .list_accounts()?
        .into_iter()
        .filter(|account| account.provider == provider)
        .filter(|account| {
            account.provider_account_id.0 == selector
                || account.email.as_deref().map(normalize_email).as_deref()
                    == Some(normalized_email.as_str())
                || account
                    .provider_user_id
                    .as_deref()
                    .map(normalize_provider_user_id)
                    .as_deref()
                    == Some(normalized_provider_user_id.as_str())
                || account
                    .account_label
                    .as_deref()
                    .map(|label| label.trim().to_ascii_lowercase())
                    .as_deref()
                    == Some(normalized_label.as_str())
        })
        .collect();

    match matches.len() {
        0 => bail!("no {provider} account matched '{selector}'"),
        1 => Ok(matches.into_iter().next().expect("single account")),
        _ => bail!("multiple {provider} accounts matched '{selector}'"),
    }
}

pub(crate) fn move_subscription_to_account(
    store: &Store,
    subscription: &Subscription,
    target_provider_account_id: &ProviderAccountId,
) -> Result<Subscription> {
    let moved_subscription_id = subscription_id(
        &subscription.provider,
        target_provider_account_id,
        &subscription.plan_name,
        subscription.started_at,
    );
    if moved_subscription_id != subscription.subscription_id {
        if let Some(existing) = store.subscription(&moved_subscription_id)? {
            if existing.provider == subscription.provider
                && existing.provider_account_id == *target_provider_account_id
                && existing.plan_name == subscription.plan_name
                && existing.price == subscription.price
                && existing.currency == subscription.currency
                && existing.billing_period == subscription.billing_period
                && existing.paid_at == subscription.paid_at
                && existing.renewal_day == subscription.renewal_day
                && existing.started_at == subscription.started_at
                && existing.ended_at == subscription.ended_at
                && existing.current_period_ends_at == subscription.current_period_ends_at
                && existing.status == subscription.status
            {
                store.delete_subscription(&subscription.subscription_id)?;
                return Ok(existing);
            }
            bail!(
                "subscription {} would collide with existing subscription {} on {}",
                subscription.subscription_id.0,
                moved_subscription_id.0,
                target_provider_account_id.0
            );
        }
    }

    validate_subscription_overlap(
        store,
        &subscription.provider,
        target_provider_account_id,
        subscription.started_at,
        subscription.ended_at,
        Some(&subscription.subscription_id),
    )?;

    let moved = Subscription {
        subscription_id: moved_subscription_id,
        provider_account_id: target_provider_account_id.clone(),
        ..subscription.clone()
    };
    if moved.subscription_id != subscription.subscription_id {
        store.delete_subscription(&subscription.subscription_id)?;
    }
    store.upsert_subscription(&moved)?;
    Ok(moved)
}

pub(crate) fn move_direct_account_records(
    store: &Store,
    provider: &str,
    from_provider_account_id: &ProviderAccountId,
    target_provider_account_id: &ProviderAccountId,
) -> Result<()> {
    let mut events_to_move: Vec<_> = store
        .events()?
        .into_iter()
        .filter(|event| event.provider == provider)
        .filter(|event| event.provider_account_id.as_ref() == Some(from_provider_account_id))
        .collect();
    for event in &mut events_to_move {
        event.provider_account_id = Some(target_provider_account_id.clone());
        if let Some(evidence) = event.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::UserConfigured;
        }
    }
    if !events_to_move.is_empty() {
        store.rewrite_events(&events_to_move)?;
    }

    let mut summaries_to_move: Vec<_> = store
        .summaries()?
        .into_iter()
        .filter(|summary| summary.provider == provider)
        .filter(|summary| summary.provider_account_id.as_ref() == Some(from_provider_account_id))
        .collect();
    for summary in &mut summaries_to_move {
        summary.provider_account_id = Some(target_provider_account_id.clone());
        if let Some(evidence) = summary.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::UserConfigured;
        }
    }
    if !summaries_to_move.is_empty() {
        store.rewrite_summaries(&summaries_to_move)?;
    }

    store.rekey_account_evidence(
        provider,
        from_provider_account_id,
        target_provider_account_id,
    )?;

    Ok(())
}

pub(crate) fn account_reference_counts(
    store: &Store,
    provider_account_id: &ProviderAccountId,
    provider: Option<&str>,
) -> Result<AccountReferenceCounts> {
    let provider_matches =
        |row_provider: &str| provider.map(|value| value == row_provider).unwrap_or(true);
    let source_account_assignments = store
        .list_source_account_assignments()?
        .into_iter()
        .filter(|assignment| assignment.provider_account_id == *provider_account_id)
        .filter(|assignment| provider_matches(&assignment.provider))
        .count();
    let subscriptions = store
        .list_subscriptions()?
        .into_iter()
        .filter(|subscription| subscription.provider_account_id == *provider_account_id)
        .filter(|subscription| provider_matches(&subscription.provider))
        .count();
    let events = store
        .events()?
        .into_iter()
        .filter(|event| event.provider_account_id.as_ref() == Some(provider_account_id))
        .filter(|event| provider_matches(&event.provider))
        .count();
    let summaries = store
        .summaries()?
        .into_iter()
        .filter(|summary| summary.provider_account_id.as_ref() == Some(provider_account_id))
        .filter(|summary| provider_matches(&summary.provider))
        .count();
    let quota_observations = store
        .quota_observations(&QuotaQuery::default(), false)?
        .into_iter()
        .filter(|record| {
            record.observation.provider_account_id.as_ref() == Some(provider_account_id)
        })
        .filter(|record| provider_matches(&record.observation.provider))
        .count();
    let evidence = if let Some(provider) = provider {
        store.account_evidence_reference_counts(provider, provider_account_id)?
    } else {
        let identity_observations = store
            .account_identity_observations(None)?
            .into_iter()
            .filter(|observation| {
                observation.provider_account_id.as_ref() == Some(provider_account_id)
            })
            .count();
        let plan_observations = store
            .account_plan_observations()?
            .into_iter()
            .filter(|observation| {
                observation.provider_account_id.as_ref() == Some(provider_account_id)
            })
            .count();
        let conversation_bindings = store
            .conversation_account_bindings(None)?
            .into_iter()
            .filter(|binding| binding.provider_account_id == *provider_account_id)
            .count();
        statsai_store::AccountEvidenceReferenceCounts {
            identity_observations,
            plan_observations,
            conversation_bindings,
        }
    };

    Ok(AccountReferenceCounts {
        source_account_assignments,
        subscriptions,
        events,
        summaries,
        quota_observations,
        identity_observations: evidence.identity_observations,
        plan_observations: evidence.plan_observations,
        conversation_bindings: evidence.conversation_bindings,
    })
}
