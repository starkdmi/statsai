use super::*;

pub fn verified_source_state_hash(
    verified_state: Option<&VerifiedSourceState>,
) -> Result<Option<String>> {
    verified_state
        .map(|verified_state| serde_json::to_string(verified_state).map(|json| hash_text(&json)))
        .transpose()
        .map_err(Into::into)
}

pub fn verified_source_observation_hash(
    observation: &VerifiedSourceObservation,
) -> Result<Option<String>> {
    match observation {
        VerifiedSourceObservation::Unavailable => Ok(None),
        VerifiedSourceObservation::Verified(state) => Ok(verified_source_state_hash(Some(state))?
            .map(|hash| format!("{VERIFIED_SOURCE_OBSERVATION_HASH_PREFIX}{hash}"))),
        VerifiedSourceObservation::Inferred {
            identity,
            basis,
            settings_modified_at,
        } => {
            let payload = serde_json::to_string(&(
                "verified_source_observation.inferred.v1",
                basis,
                identity,
                settings_modified_at,
            ))?;
            Ok(Some(format!(
                "{INFERRED_SOURCE_OBSERVATION_HASH_PREFIX}{}",
                hash_text(&payload)
            )))
        }
        VerifiedSourceObservation::AttributionBlocked { blocked_since } => {
            let payload = serde_json::to_string(&(
                "verified_source_observation.attribution_blocked.v2",
                blocked_since,
            ))?;
            Ok(Some(format!(
                "{ATTRIBUTION_BLOCKED_OBSERVATION_HASH_PREFIX}{}",
                hash_text(&payload)
            )))
        }
    }
}

fn requires_conservative_verified_recovery(hash: &str) -> bool {
    hash.starts_with(ATTRIBUTION_BLOCKED_OBSERVATION_HASH_PREFIX)
        // Before observation hashes were typed, both verified profiles and
        // blocked states (with or without timestamps) were stored as bare
        // digests. A digest cannot reveal which payload produced it, so only
        // an active matching assignment can prove uninterrupted continuity.
        || !hash.starts_with(VERIFIED_SOURCE_OBSERVATION_HASH_PREFIX)
}

pub fn find_existing_provider_account(
    store: &Store,
    provider: &str,
    provider_user_id: Option<&str>,
    email: Option<&str>,
) -> Result<Option<ProviderAccount>> {
    let normalized_provider_user_id = provider_user_id
        .map(normalize_provider_user_id)
        .filter(|provider_user_id| !provider_user_id.is_empty());
    let normalized_email = email.map(normalize_email).filter(|email| !email.is_empty());
    let accounts = store.list_accounts()?;
    let mut matches: Vec<(&'static str, ProviderAccount)> = Vec::new();

    if let Some(email) = normalized_email.as_deref() {
        if let Some(account) = accounts.iter().find(|account| {
            account.provider == provider
                && account.email.as_deref().map(normalize_email).as_deref() == Some(email)
        }) {
            matches.push(("email", account.clone()));
        }
    }
    if let Some(provider_user_id) = normalized_provider_user_id.as_deref() {
        if let Some(account) = accounts.iter().find(|account| {
            account.provider == provider
                && account
                    .provider_user_id
                    .as_deref()
                    .map(normalize_provider_user_id)
                    .as_deref()
                    == Some(provider_user_id)
        }) {
            matches.push(("provider_user_id", account.clone()));
        }
    }
    if let Some(provider_account_id) = provider_account_id_from_identity(
        provider,
        normalized_provider_user_id.as_deref(),
        normalized_email.as_deref(),
    ) {
        if let Some(account) = store.account(&provider_account_id)? {
            matches.push(("provider_account_id", account));
        }
    }

    let mut unique_matches: Vec<(&'static str, ProviderAccount)> = Vec::new();
    for (match_kind, account) in matches {
        if !unique_matches
            .iter()
            .any(|(_, existing)| existing.provider_account_id == account.provider_account_id)
        {
            unique_matches.push((match_kind, account));
        }
    }

    if unique_matches.len() > 1 {
        let details = unique_matches
            .iter()
            .map(|(match_kind, account)| {
                format!("{match_kind} matched {}", account.provider_account_id.0)
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!("conflicting provider account identifiers for {provider}: {details}");
    }

    Ok(unique_matches
        .into_iter()
        .next()
        .map(|(_, account)| account))
}

#[derive(Debug, Clone)]
pub struct UpsertProviderAccountInput<'a> {
    pub provider: &'a str,
    pub provider_user_id: Option<&'a str>,
    pub email: Option<&'a str>,
    pub label: Option<String>,
    pub plan_name: Option<String>,
    pub identity_source: Option<IdentitySource>,
    pub verified_at: Option<DateTime<Utc>>,
}

pub fn upsert_provider_account(
    store: &Store,
    input: UpsertProviderAccountInput<'_>,
) -> Result<ProviderAccount> {
    let UpsertProviderAccountInput {
        provider,
        provider_user_id,
        email,
        label,
        plan_name,
        identity_source,
        verified_at,
    } = input;
    let normalized_provider_user_id = provider_user_id
        .map(normalize_provider_user_id)
        .filter(|provider_user_id| !provider_user_id.is_empty());
    let normalized_email = email.map(normalize_email).filter(|email| !email.is_empty());
    let existing = find_existing_provider_account(
        store,
        provider,
        normalized_provider_user_id.as_deref(),
        normalized_email.as_deref(),
    )?;
    let provider_account_id = existing
        .as_ref()
        .map(|account| account.provider_account_id.clone())
        .or_else(|| {
            provider_account_id_from_identity(
                provider,
                normalized_provider_user_id.as_deref(),
                normalized_email.as_deref(),
            )
        })
        .with_context(|| format!("missing canonical account identity for {provider}"))?;
    let now = Utc::now();
    let mut account = existing.unwrap_or(ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: provider_account_id.clone(),
        provider: provider.to_string(),
        identity_source: identity_source
            .clone()
            .unwrap_or(IdentitySource::UserConfigured),
        provider_user_id: None,
        email: None,
        provider_user_id_hash: None,
        email_hash: None,
        org_id_hash: None,
        account_label: None,
        plan_name: None,
        confidence: Confidence::High,
        verified_at: None,
        created_at: now,
        updated_at: now,
    });
    if let Some(provider_user_id) = normalized_provider_user_id.as_deref() {
        account.provider_user_id = Some(provider_user_id.to_string());
        account.provider_user_id_hash = Some(hash_text(provider_user_id));
    }
    if let Some(email) = normalized_email.as_deref() {
        account.email_hash = Some(hash_text(email));
        account.email = Some(email.to_string());
    }
    if let Some(label) = label {
        account.account_label = Some(label);
    }
    if let Some(plan_name) = plan_name.filter(|plan_name| !plan_name.trim().is_empty()) {
        account.plan_name = Some(plan_name);
    }
    if let Some(identity_source) = identity_source {
        account.identity_source = merge_identity_source(&account.identity_source, identity_source);
    }
    account.verified_at = max_datetime(account.verified_at, verified_at);
    account.provider = provider.to_string();
    account.confidence = if account.provider_user_id.is_some() || account.email.is_some() {
        Confidence::High
    } else {
        account.confidence
    };
    account.updated_at = now;
    store.upsert_account(&account)?;
    Ok(account)
}

pub fn apply_verified_source_state(
    store: &Store,
    source: &SourceLocation,
    verified_state: Option<&VerifiedSourceState>,
) -> Result<()> {
    apply_verified_source_state_with_recovery_boundary(
        store,
        source,
        verified_state,
        None,
        IdentitySource::LocalAuth,
    )
}

fn apply_verified_source_state_with_recovery_boundary(
    store: &Store,
    source: &SourceLocation,
    verified_state: Option<&VerifiedSourceState>,
    recovery: Option<VerifiedAssignmentRecoveryBoundary>,
    record_source: IdentitySource,
) -> Result<()> {
    let Some(verified_state) = verified_state else {
        return Ok(());
    };
    let mut account = upsert_provider_account(
        store,
        UpsertProviderAccountInput {
            provider: &source.provider,
            provider_user_id: verified_state.provider_user_id.as_deref(),
            email: verified_state.email.as_deref(),
            label: verified_state.account_label.clone(),
            plan_name: (!source.provider.eq_ignore_ascii_case("codex"))
                .then(|| verified_state.plan_name.clone())
                .flatten(),
            identity_source: Some(record_source.clone()),
            verified_at: verified_state.verified_at,
        },
    )?;
    if matches!(record_source, IdentitySource::SourceConfig)
        && matches!(account.identity_source, IdentitySource::SourceConfig)
    {
        account.confidence = Confidence::Medium;
        store.upsert_account(&account)?;
    }
    let assignment_started_at = verified_state.authenticated_at.or_else(|| {
        verified_state
            .subscription
            .as_ref()
            .map(|subscription| subscription.started_at)
    });
    if let Some(started_at) = assignment_started_at {
        upsert_verified_source_assignment(
            store,
            source,
            &account.provider_account_id,
            started_at,
            verified_state.verified_at,
            recovery,
            record_source.clone(),
        )?;
    }
    if let Some(subscription) = verified_state.subscription.as_ref() {
        upsert_verified_subscription(
            store,
            &source.provider,
            &account.provider_account_id,
            subscription,
        )?;
    }
    Ok(())
}

pub fn reconcile_verified_source_state(
    store: &Store,
    source: &mut SourceLocation,
    observation: &VerifiedSourceObservation,
    next_verified_state_hash: Option<String>,
) -> Result<()> {
    if !matches!(source.verification_mode, SourceVerificationMode::Auto) {
        return Ok(());
    }
    // A local-auth file can be absent or mid-rewrite. That does not prove the
    // account signed out, nor does it prove its subscription ended.
    if matches!(observation, VerifiedSourceObservation::Unavailable) {
        return Ok(());
    }
    if source.verified_state_hash == next_verified_state_hash {
        return Ok(());
    }

    match observation {
        VerifiedSourceObservation::Unavailable => {}
        VerifiedSourceObservation::Verified(verified_state) => {
            let recovery = source.verified_state_hash.as_deref().map(|previous_hash| {
                VerifiedAssignmentRecoveryBoundary {
                    observed_at: Utc::now(),
                    must_start_at_observed_at: requires_conservative_verified_recovery(
                        previous_hash,
                    ),
                    minimum_started_at: None,
                }
            });
            apply_verified_source_state_with_recovery_boundary(
                store,
                source,
                Some(verified_state),
                recovery,
                IdentitySource::LocalAuth,
            )?;
        }
        VerifiedSourceObservation::Inferred {
            identity,
            settings_modified_at,
            ..
        } => {
            let has_assignment_history = !store
                .list_source_account_assignments_for_source(&source.source_id)?
                .is_empty();
            let repaired_settings_boundary =
                settings_modified_at.filter(|modified_at| *modified_at > source.updated_at);
            let recovery = source.verified_state_hash.as_deref().map(|previous_hash| {
                VerifiedAssignmentRecoveryBoundary {
                    observed_at: Utc::now(),
                    // A current cached-profile inference explicitly enables best-effort
                    // historical attribution when no assignment history exists. Once an
                    // interval exists, blocked/legacy transitions keep their conservative gap.
                    must_start_at_observed_at: has_assignment_history
                        && requires_conservative_verified_recovery(previous_hash),
                    minimum_started_at: repaired_settings_boundary,
                }
            });
            apply_verified_source_state_with_recovery_boundary(
                store,
                source,
                Some(identity),
                recovery,
                IdentitySource::SourceConfig,
            )?;
        }
        VerifiedSourceObservation::AttributionBlocked { blocked_since } => {
            if let Some(blocked_since) = blocked_since {
                close_active_verified_source_assignments(
                    store,
                    &source.source_id,
                    (*blocked_since).min(Utc::now()),
                )?;
            } else {
                invalidate_active_verified_source_assignments(store, &source.source_id)?;
            }
        }
    }
    source.verified_state_hash = next_verified_state_hash;
    source.updated_at = Utc::now();
    Ok(())
}

pub fn apply_source_account_resolution(
    store: &Store,
    source: &SourceLocation,
    events: &mut [UsageEvent],
    summaries: &mut [UsageSummary],
) -> Result<()> {
    let assignments = store.list_source_account_assignments_for_source(&source.source_id)?;
    for event in events {
        apply_account_resolution_to_event(&assignments, event);
    }
    for summary in summaries {
        apply_account_resolution_to_summary(&assignments, summary);
    }
    Ok(())
}

pub(crate) fn upsert_verified_source_assignment(
    store: &Store,
    source: &SourceLocation,
    provider_account_id: &ProviderAccountId,
    authenticated_at: DateTime<Utc>,
    verified_at: Option<DateTime<Utc>>,
    recovery: Option<VerifiedAssignmentRecoveryBoundary>,
    record_source: IdentitySource,
) -> Result<()> {
    let assignments = store.list_source_account_assignments_for_source(&source.source_id)?;
    let started_at = verified_assignment_started_at(
        &assignments,
        provider_account_id,
        authenticated_at,
        verified_at,
        recovery,
    );
    let overlaps: Vec<_> = assignments
        .iter()
        .filter(|assignment| {
            periods_overlap(started_at, None, assignment.started_at, assignment.ended_at)
        })
        .cloned()
        .collect();

    if let Some(existing) = overlaps
        .iter()
        .find(|assignment| assignment.provider_account_id == *provider_account_id)
    {
        let merged_started_at = existing.started_at.min(started_at);
        let merged = SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: source_account_assignment_id(
                &source.source_id,
                provider_account_id,
                merged_started_at,
            ),
            source_id: source.source_id.clone(),
            provider: source.provider.clone(),
            provider_account_id: provider_account_id.clone(),
            started_at: merged_started_at,
            ended_at: None,
            record_source: merge_identity_source(&existing.record_source, record_source.clone()),
            verified_at: max_datetime(existing.verified_at, verified_at),
            created_at: existing.created_at,
            updated_at: Utc::now(),
        };
        let attribution_changed = merged.assignment_id != existing.assignment_id
            || merged.started_at != existing.started_at
            || merged.ended_at != existing.ended_at;
        if merged.assignment_id != existing.assignment_id {
            store.delete_source_account_assignment(&existing.assignment_id)?;
        }
        store.upsert_source_account_assignment(&merged)?;
        if attribution_changed {
            reattribute_source_records(store, &source.source_id)?;
        }
        return Ok(());
    }

    if overlaps
        .iter()
        .any(|assignment| matches!(assignment.record_source, IdentitySource::UserConfigured))
    {
        return Ok(());
    }

    for existing in overlaps
        .iter()
        .filter(|assignment| assignment.provider_account_id != *provider_account_id)
    {
        if existing.started_at <= started_at {
            let mut closed = existing.clone();
            closed.ended_at = Some(started_at);
            closed.updated_at = Utc::now();
            store.upsert_source_account_assignment(&closed)?;
        } else {
            store.delete_source_account_assignment(&existing.assignment_id)?;
        }
    }

    let now = Utc::now();
    let assignment = SourceAccountAssignment {
        schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
        assignment_id: source_account_assignment_id(
            &source.source_id,
            provider_account_id,
            started_at,
        ),
        source_id: source.source_id.clone(),
        provider: source.provider.clone(),
        provider_account_id: provider_account_id.clone(),
        started_at,
        ended_at: None,
        record_source,
        verified_at,
        created_at: now,
        updated_at: now,
    };
    validate_source_assignment_overlap(
        store,
        &source.source_id,
        provider_account_id,
        assignment.started_at,
        assignment.ended_at,
        None,
    )?;
    store.upsert_source_account_assignment(&assignment)?;
    reattribute_source_records(store, &source.source_id)?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct VerifiedAssignmentRecoveryBoundary {
    observed_at: DateTime<Utc>,
    must_start_at_observed_at: bool,
    minimum_started_at: Option<DateTime<Utc>>,
}

fn verified_assignment_started_at(
    assignments: &[SourceAccountAssignment],
    provider_account_id: &ProviderAccountId,
    authenticated_at: DateTime<Utc>,
    verified_at: Option<DateTime<Utc>>,
    recovery: Option<VerifiedAssignmentRecoveryBoundary>,
) -> DateTime<Utc> {
    let Some(recovery) = recovery else {
        return authenticated_at;
    };
    let clamp_to_evidence = |started_at: DateTime<Utc>| {
        recovery
            .minimum_started_at
            .map_or(started_at, |minimum| started_at.max(minimum))
    };
    if let Some(active_started_at) = assignments
        .iter()
        .filter(|assignment| {
            assignment.provider_account_id == *provider_account_id
                && is_active_verified_source_assignment(assignment)
        })
        .map(|assignment| assignment.started_at)
        .max()
    {
        return clamp_to_evidence(authenticated_at.max(active_started_at));
    }
    if recovery.must_start_at_observed_at {
        return clamp_to_evidence(recovery.observed_at);
    }
    let latest_closed_at = assignments
        .iter()
        .filter(|assignment| {
            assignment.provider_account_id == *provider_account_id
                && is_verified_source_assignment(assignment)
        })
        .filter_map(|assignment| assignment.ended_at)
        .max();
    let Some(latest_closed_at) = latest_closed_at else {
        return clamp_to_evidence(authenticated_at);
    };
    if authenticated_at > latest_closed_at {
        return clamp_to_evidence(authenticated_at.min(recovery.observed_at));
    }
    clamp_to_evidence(
        verified_at
            .filter(|verified_at| *verified_at > latest_closed_at)
            .map_or(recovery.observed_at, |verified_at| {
                verified_at.min(recovery.observed_at)
            }),
    )
}

fn close_verified_source_assignments_at_boundary_inner(
    store: &Store,
    source_id: &SourceId,
    ended_at: DateTime<Utc>,
) -> Result<Vec<ProviderAccountId>> {
    let mut closed_account_ids = Vec::new();
    for mut assignment in store.list_source_account_assignments_for_source(source_id)? {
        if !is_verified_source_assignment(&assignment)
            || assignment
                .ended_at
                .is_some_and(|current_end| current_end <= ended_at)
        {
            continue;
        }
        if ended_at <= assignment.started_at {
            store.delete_source_account_assignment(&assignment.assignment_id)?;
            closed_account_ids.push(assignment.provider_account_id);
            continue;
        }
        validate_time_window(assignment.started_at, Some(ended_at), "source connection")?;
        assignment.ended_at = Some(ended_at);
        assignment.updated_at = Utc::now();
        store.upsert_source_account_assignment(&assignment)?;
        closed_account_ids.push(assignment.provider_account_id.clone());
    }
    if !closed_account_ids.is_empty() {
        reattribute_source_records(store, source_id)?;
    }
    Ok(closed_account_ids)
}

fn invalidate_active_verified_source_assignments(
    store: &Store,
    source_id: &SourceId,
) -> Result<()> {
    let mut changed = false;
    for assignment in store.list_source_account_assignments_for_source(source_id)? {
        if !is_active_verified_source_assignment(&assignment) {
            continue;
        }
        store.delete_source_account_assignment(&assignment.assignment_id)?;
        changed = true;
    }
    if changed {
        reattribute_source_records(store, source_id)?;
    }
    Ok(())
}

fn is_active_verified_source_assignment(assignment: &SourceAccountAssignment) -> bool {
    assignment.ended_at.is_none() && is_verified_source_assignment(assignment)
}

pub(crate) fn is_verified_source_assignment(assignment: &SourceAccountAssignment) -> bool {
    matches!(
        assignment.record_source,
        IdentitySource::LocalAuth
            | IdentitySource::SourceConfig
            | IdentitySource::ProviderAuth
            | IdentitySource::ProviderApi
            | IdentitySource::CookieOauth
            | IdentitySource::CliProbe
    )
}

pub fn close_active_verified_source_assignments(
    store: &Store,
    source_id: &SourceId,
    ended_at: DateTime<Utc>,
) -> Result<()> {
    close_verified_source_assignments_at_boundary_inner(store, source_id, ended_at)?;
    Ok(())
}

pub fn close_active_verified_source_linkages(
    store: &Store,
    source_id: &SourceId,
    ended_at: DateTime<Utc>,
) -> Result<()> {
    let closed_account_ids =
        close_verified_source_assignments_at_boundary_inner(store, source_id, ended_at)?;
    for mut subscription in store.list_subscriptions()? {
        if !closed_account_ids.contains(&subscription.provider_account_id)
            || subscription.ended_at.is_some()
            || !is_verified_subscription_source(&subscription.record_source)
            || !timestamp_in_period(ended_at, subscription.started_at, subscription.ended_at)
        {
            continue;
        }
        validate_time_window(subscription.started_at, Some(ended_at), "subscription")?;
        subscription.ended_at = Some(ended_at);
        store.upsert_subscription(&subscription)?;
    }
    Ok(())
}

fn upsert_verified_subscription(
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
