use super::*;

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
