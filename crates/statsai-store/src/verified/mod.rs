use super::*;

mod accounts;
mod assignments;

pub(crate) use accounts::requires_conservative_verified_recovery;
pub use accounts::{
    find_existing_provider_account, upsert_provider_account, verified_source_observation_hash,
    verified_source_state_hash, UpsertProviderAccountInput,
};
pub(crate) use assignments::is_verified_source_assignment;
#[cfg(test)]
pub(crate) use assignments::upsert_verified_source_assignment;
pub use assignments::{
    apply_source_account_resolution, apply_verified_source_state,
    close_active_verified_source_assignments, close_active_verified_source_linkages,
    reconcile_verified_source_state,
};

pub(crate) fn upsert_verified_subscription(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    verified: &VerifiedSubscriptionState,
) -> Result<()> {
    // Codex authentication exposes provider plan state, not user billing facts. Its adapter
    // persists that information through the plan-evidence ledger instead of this billing table.
    if provider.eq_ignore_ascii_case("codex") {
        return Ok(());
    }
    validate_time_window(verified.started_at, None, "subscription")?;
    let subscriptions: Vec<_> = store
        .list_subscriptions()?
        .into_iter()
        .filter(|subscription| {
            subscription.provider == provider
                && subscription.provider_account_id == *provider_account_id
        })
        .collect();

    if let Some(existing) = subscriptions.iter().find(|subscription| {
        subscription
            .plan_name
            .eq_ignore_ascii_case(&verified.plan_name)
            && periods_overlap(
                verified.started_at,
                None,
                subscription.started_at,
                subscription.ended_at,
            )
    }) {
        let merged = merge_verified_subscription(existing, verified);
        store.upsert_subscription(&merged)?;
        return Ok(());
    }

    if subscriptions.iter().any(|subscription| {
        subscription.record_source == IdentitySource::UserConfigured
            && periods_overlap(
                verified.started_at,
                None,
                subscription.started_at,
                subscription.ended_at,
            )
            && !subscription
                .plan_name
                .eq_ignore_ascii_case(&verified.plan_name)
    }) {
        return Ok(());
    }

    for mut subscription in subscriptions
        .iter()
        .filter(|subscription| {
            is_verified_subscription_source(&subscription.record_source)
                && periods_overlap(
                    verified.started_at,
                    None,
                    subscription.started_at,
                    subscription.ended_at,
                )
                && !subscription
                    .plan_name
                    .eq_ignore_ascii_case(&verified.plan_name)
        })
        .cloned()
    {
        if subscription.started_at < verified.started_at {
            subscription.ended_at = Some(verified.started_at);
            store.upsert_subscription(&subscription)?;
        } else {
            store.delete_subscription(&subscription.subscription_id)?;
        }
    }

    let current_subscriptions: Vec<_> = store
        .list_subscriptions()?
        .into_iter()
        .filter(|subscription| {
            subscription.provider == provider
                && subscription.provider_account_id == *provider_account_id
        })
        .collect();

    if current_subscriptions.iter().any(|subscription| {
        periods_overlap(
            verified.started_at,
            None,
            subscription.started_at,
            subscription.ended_at,
        )
    }) {
        return Ok(());
    }

    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id(
            provider,
            provider_account_id,
            &verified.plan_name,
            verified.started_at,
        ),
        provider: provider.to_string(),
        provider_account_id: provider_account_id.clone(),
        plan_name: verified.plan_name.clone(),
        price: verified.price,
        currency: verified.currency.clone(),
        billing_period: verified.billing_period.clone(),
        paid_at: verified.paid_at.or(Some(verified.started_at)),
        renewal_day: verified
            .current_period_ends_at
            .and_then(subscription_renewal_day),
        started_at: verified.started_at,
        ended_at: None,
        current_period_ends_at: verified.current_period_ends_at,
        status: verified.status.clone(),
        record_source: IdentitySource::LocalAuth,
        verified_at: verified.verified_at,
        notes: None,
    };
    validate_subscription_overlap(
        store,
        provider,
        provider_account_id,
        subscription.started_at,
        subscription.ended_at,
        None,
    )?;
    store.upsert_subscription(&subscription)?;
    Ok(())
}

fn merge_verified_subscription(
    existing: &Subscription,
    verified: &VerifiedSubscriptionState,
) -> Subscription {
    let mut merged = existing.clone();
    if merged.price <= 0 {
        merged.price = verified.price;
    }
    if merged.currency.trim().is_empty() {
        merged.currency = verified.currency.clone();
    }
    merged.billing_period = verified.billing_period.clone();
    merged.paid_at = max_datetime(
        merged.paid_at,
        verified.paid_at.or(Some(verified.started_at)),
    );
    merged.renewal_day = verified
        .current_period_ends_at
        .and_then(subscription_renewal_day)
        .or(merged.renewal_day);
    merged.current_period_ends_at = max_datetime(
        merged.current_period_ends_at,
        verified.current_period_ends_at,
    );
    merged.status = verified.status.clone();
    merged.record_source = merge_identity_source(&merged.record_source, IdentitySource::LocalAuth);
    merged.verified_at = max_datetime(merged.verified_at, verified.verified_at);
    merged
}

fn merge_identity_source(existing: &IdentitySource, incoming: IdentitySource) -> IdentitySource {
    if identity_source_rank(&incoming) >= identity_source_rank(existing) {
        incoming
    } else {
        existing.clone()
    }
}

fn identity_source_rank(source: &IdentitySource) -> u8 {
    match source {
        IdentitySource::UserConfigured => 100,
        IdentitySource::ProviderApi => 90,
        IdentitySource::ProviderAuth => 80,
        IdentitySource::LocalAuth => 70,
        IdentitySource::CliProbe => 60,
        IdentitySource::CookieOauth => 50,
        IdentitySource::SourceConfig => 40,
        IdentitySource::ManualHint => 30,
        IdentitySource::Unresolved => 10,
        IdentitySource::Unknown => 0,
    }
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

fn max_datetime(
    left: Option<DateTime<Utc>>,
    right: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn subscription_renewal_day(timestamp: DateTime<Utc>) -> Option<u8> {
    u8::try_from(timestamp.day()).ok()
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

fn validate_source_assignment_overlap(
    store: &Store,
    source_id: &SourceId,
    _provider_account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    ignore_assignment_id: Option<&SourceAccountAssignmentId>,
) -> Result<()> {
    for assignment in store.list_source_account_assignments_for_source(source_id)? {
        if ignore_assignment_id == Some(&assignment.assignment_id) {
            continue;
        }
        if periods_overlap(
            started_at,
            ended_at,
            assignment.started_at,
            assignment.ended_at,
        ) {
            bail!(
                "source connection overlaps an existing connection for source {}",
                source_id.0
            );
        }
    }
    Ok(())
}

fn validate_subscription_overlap(
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

pub(crate) fn reattribute_source_records(store: &Store, source_id: &SourceId) -> Result<()> {
    if store.source(source_id)?.is_none() {
        return Ok(());
    }
    let assignments = store.list_source_account_assignments_for_source(source_id)?;
    let mut changed_events = Vec::new();
    for mut event in store.events_for_source(source_id)? {
        let previous_account = event.provider_account_id.clone();
        let previous_identity_source = event
            .parse_evidence
            .as_ref()
            .map(|evidence| evidence.account_identity_source.clone());
        apply_account_resolution_to_event(&assignments, &mut event);
        let identity_source = event
            .parse_evidence
            .as_ref()
            .map(|evidence| evidence.account_identity_source.clone());
        if event.provider_account_id != previous_account
            || identity_source != previous_identity_source
        {
            changed_events.push(event);
        }
    }
    let mut changed_summaries = Vec::new();
    for mut summary in store.summaries_for_source(source_id)? {
        let previous_account = summary.provider_account_id.clone();
        let previous_identity_source = summary
            .parse_evidence
            .as_ref()
            .map(|evidence| evidence.account_identity_source.clone());
        apply_account_resolution_to_summary(&assignments, &mut summary);
        let identity_source = summary
            .parse_evidence
            .as_ref()
            .map(|evidence| evidence.account_identity_source.clone());
        if summary.provider_account_id != previous_account
            || identity_source != previous_identity_source
        {
            changed_summaries.push(summary);
        }
    }
    store.rewrite_events(&changed_events)?;
    store.rewrite_summaries(&changed_summaries)?;
    store.reattribute_quota_observations(source_id)?;
    store.rebuild_quota_plan_observations_for_source(source_id)?;
    Ok(())
}

fn apply_account_resolution_to_event(
    assignments: &[SourceAccountAssignment],
    event: &mut UsageEvent,
) {
    if keep_detected_account_identity(
        event.provider_account_id.as_ref(),
        event
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        return;
    }
    let assignment = assignment_for_timestamp(assignments, event.session.started_at);
    if let Some(assignment) = assignment {
        event.provider_account_id = Some(assignment.provider_account_id.clone());
        if let Some(evidence) = event.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::SourceConfig;
        }
    } else if should_clear_resolved_account(
        event.provider_account_id.as_ref(),
        event
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        event.provider_account_id = None;
        if let Some(evidence) = event.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::Unresolved;
        }
    }
}

fn apply_account_resolution_to_summary(
    assignments: &[SourceAccountAssignment],
    summary: &mut UsageSummary,
) {
    if keep_detected_account_identity(
        summary.provider_account_id.as_ref(),
        summary
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        return;
    }
    let timestamp = summary.period_start.unwrap_or(summary.observed_at);
    let assignment = assignment_for_timestamp(assignments, timestamp);
    if let Some(assignment) = assignment {
        summary.provider_account_id = Some(assignment.provider_account_id.clone());
        if let Some(evidence) = summary.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::SourceConfig;
        }
    } else if should_clear_resolved_account(
        summary.provider_account_id.as_ref(),
        summary
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        summary.provider_account_id = None;
        if let Some(evidence) = summary.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::Unresolved;
        }
    }
}

fn keep_detected_account_identity(
    provider_account_id: Option<&ProviderAccountId>,
    identity_source: Option<&IdentitySource>,
) -> bool {
    let Some(provider_account_id) = provider_account_id else {
        return false;
    };
    if provider_account_id.0.trim().is_empty() {
        return false;
    }
    let Some(identity_source) = identity_source else {
        return false;
    };
    !matches!(
        identity_source,
        IdentitySource::SourceConfig
            | IdentitySource::UserConfigured
            | IdentitySource::ManualHint
            | IdentitySource::Unknown
            | IdentitySource::Unresolved
    )
}

fn should_clear_resolved_account(
    provider_account_id: Option<&ProviderAccountId>,
    identity_source: Option<&IdentitySource>,
) -> bool {
    let Some(provider_account_id) = provider_account_id else {
        return false;
    };
    if provider_account_id.0.trim().is_empty() {
        return false;
    }
    matches!(
        identity_source,
        None | Some(
            IdentitySource::SourceConfig
                | IdentitySource::UserConfigured
                | IdentitySource::ManualHint
                | IdentitySource::Unknown
                | IdentitySource::Unresolved
        )
    )
}

pub(crate) fn assignment_for_timestamp(
    assignments: &[SourceAccountAssignment],
    timestamp: DateTime<Utc>,
) -> Option<&SourceAccountAssignment> {
    assignments
        .iter()
        .filter(|assignment| {
            timestamp_in_period(timestamp, assignment.started_at, assignment.ended_at)
        })
        .max_by(|left, right| left.started_at.cmp(&right.started_at))
}
