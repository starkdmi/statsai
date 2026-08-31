use super::*;

pub(crate) fn resolve_or_create_provider_account(
    store: &Store,
    provider: &str,
    provider_account_id_value: Option<&str>,
    provider_user_id: Option<&str>,
    email: Option<&str>,
    label: Option<String>,
) -> Result<ProviderAccount> {
    if let Some(provider_account_id_value) = provider_account_id_value {
        let provider_account_id = ProviderAccountId(provider_account_id_value.to_string());
        if let Some(account) = store.account(&provider_account_id)? {
            ensure_account_matches_provider(&account, provider)?;
            return Ok(account);
        }
        if provider_user_id.is_none() && email.is_none() {
            bail!("unknown provider account {provider_account_id_value}");
        }
    }
    upsert_provider_account(
        store,
        UpsertProviderAccountInput {
            provider,
            provider_user_id,
            email,
            label,
            plan_name: None,
            identity_source: Some(IdentitySource::UserConfigured),
            verified_at: None,
        },
    )
}

pub(crate) fn resolve_existing_provider_account(
    store: &Store,
    provider: &str,
    provider_account_id_value: Option<&str>,
    provider_user_id: Option<&str>,
    email: Option<&str>,
    label: Option<String>,
) -> Result<ProviderAccount> {
    if let Some(provider_account_id_value) = provider_account_id_value {
        let provider_account_id = ProviderAccountId(provider_account_id_value.to_string());
        let account = store
            .account(&provider_account_id)?
            .with_context(|| format!("unknown provider account {provider_account_id_value}"))?;
        ensure_account_matches_provider(&account, provider)?;
        return Ok(account);
    }

    if let Some(account) = find_existing_provider_account(store, provider, provider_user_id, email)?
    {
        return Ok(account);
    }

    let normalized_label = label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty());
    if let Some(label) = normalized_label {
        let mut matches = store.list_accounts()?.into_iter().filter(|account| {
            account.provider == provider && account.account_label.as_deref() == Some(label)
        });
        let Some(account) = matches.next() else {
            bail!("unknown provider account label {label} for {provider}");
        };
        if matches.next().is_some() {
            bail!("provider account label {label} is ambiguous for {provider}");
        }
        return Ok(account);
    }

    bail!("unknown provider account selector for {provider}")
}

pub(crate) fn ensure_account_matches_provider(
    account: &ProviderAccount,
    provider: &str,
) -> Result<()> {
    if account.provider != provider {
        bail!(
            "provider account {} belongs to {}, not {}",
            account.provider_account_id.0,
            account.provider,
            provider
        );
    }
    Ok(())
}

pub(crate) struct ConnectSourceToAccountInput<'a> {
    pub(crate) source_id: &'a SourceId,
    pub(crate) provider_account_id_value: Option<&'a str>,
    pub(crate) provider_user_id: Option<&'a str>,
    pub(crate) email: Option<&'a str>,
    pub(crate) label: Option<String>,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) ended_at: Option<DateTime<Utc>>,
}

pub(crate) fn connect_source_to_account(
    store: &Store,
    input: ConnectSourceToAccountInput<'_>,
) -> Result<SourceAccountAssignment> {
    let ConnectSourceToAccountInput {
        source_id,
        provider_account_id_value,
        provider_user_id,
        email,
        label,
        started_at,
        ended_at,
    } = input;
    let source = store
        .source(source_id)?
        .with_context(|| format!("unknown source {}", source_id.0))?;
    let account = resolve_or_create_provider_account(
        store,
        &source.provider,
        provider_account_id_value,
        provider_user_id,
        email,
        label,
    )?;
    validate_time_window(started_at, ended_at, "source connection")?;

    let overlaps: Vec<_> = store
        .list_source_account_assignments_for_source(&source.source_id)?
        .into_iter()
        .filter(|assignment| {
            periods_overlap(
                started_at,
                ended_at,
                assignment.started_at,
                assignment.ended_at,
            )
        })
        .collect();

    if overlaps.len() > 1 {
        bail!(
            "source {} has multiple overlapping account connections around {}",
            source.source_id.0,
            started_at.to_rfc3339()
        );
    }

    if let Some(existing) = overlaps.first() {
        if existing.provider_account_id == account.provider_account_id {
            let merged_started_at = existing.started_at.min(started_at);
            let merged_ended_at = match (existing.ended_at, ended_at) {
                (None, _) | (_, None) => None,
                (Some(left), Some(right)) => Some(left.max(right)),
            };

            if existing.started_at == merged_started_at && existing.ended_at == merged_ended_at {
                return Ok(existing.clone());
            }

            let previous_assignment_id = existing.assignment_id.clone();
            let now = Utc::now();
            let merged = SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: source_account_assignment_id(
                    &source.source_id,
                    &account.provider_account_id,
                    merged_started_at,
                ),
                source_id: source.source_id.clone(),
                provider: source.provider.clone(),
                provider_account_id: account.provider_account_id.clone(),
                started_at: merged_started_at,
                ended_at: merged_ended_at,
                record_source: IdentitySource::UserConfigured,
                verified_at: existing.verified_at,
                created_at: existing.created_at,
                updated_at: now,
            };
            if previous_assignment_id != merged.assignment_id {
                store.delete_source_account_assignment(&previous_assignment_id)?;
            }
            store.upsert_source_account_assignment(&merged)?;
            reattribute_source_records(store, &source.source_id)?;
            return Ok(merged);
        }

        preserve_non_overlapping_source_assignment_segments(
            store, &source, existing, started_at, ended_at,
        )?;
    }

    validate_source_assignment_overlap(
        store,
        &source.source_id,
        &account.provider_account_id,
        started_at,
        ended_at,
        None,
    )?;
    let now = Utc::now();
    let assignment = SourceAccountAssignment {
        schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
        assignment_id: source_account_assignment_id(
            &source.source_id,
            &account.provider_account_id,
            started_at,
        ),
        source_id: source.source_id.clone(),
        provider: source.provider.clone(),
        provider_account_id: account.provider_account_id,
        started_at,
        ended_at,
        record_source: IdentitySource::UserConfigured,
        verified_at: None,
        created_at: now,
        updated_at: now,
    };
    store.upsert_source_account_assignment(&assignment)?;
    reattribute_source_records(store, &source.source_id)?;
    Ok(assignment)
}

pub(crate) fn preserve_non_overlapping_source_assignment_segments(
    store: &Store,
    source: &SourceLocation,
    existing: &SourceAccountAssignment,
    replacement_started_at: DateTime<Utc>,
    replacement_ended_at: Option<DateTime<Utc>>,
) -> Result<()> {
    let now = Utc::now();
    let preserve_before = existing.started_at < replacement_started_at;
    let preserve_after = replacement_ended_at
        .map(|replacement_ended_at| {
            existing
                .ended_at
                .map(|existing_ended_at| existing_ended_at > replacement_ended_at)
                .unwrap_or(true)
        })
        .unwrap_or(false);

    if preserve_before {
        let mut before = existing.clone();
        before.ended_at = Some(replacement_started_at);
        before.updated_at = now;
        validate_time_window(before.started_at, before.ended_at, "source connection")?;
        store.upsert_source_account_assignment(&before)?;
    } else {
        store.delete_source_account_assignment(&existing.assignment_id)?;
    }

    if preserve_after {
        let tail_started_at =
            replacement_ended_at.expect("preserve_after requires finite replacement end");
        let tail = SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: source_account_assignment_id(
                &source.source_id,
                &existing.provider_account_id,
                tail_started_at,
            ),
            source_id: source.source_id.clone(),
            provider: source.provider.clone(),
            provider_account_id: existing.provider_account_id.clone(),
            started_at: tail_started_at,
            ended_at: existing.ended_at,
            record_source: existing.record_source.clone(),
            verified_at: existing.verified_at,
            created_at: now,
            updated_at: now,
        };
        validate_time_window(tail.started_at, tail.ended_at, "source connection")?;
        store.upsert_source_account_assignment(&tail)?;
    }

    Ok(())
}

pub(crate) fn disconnect_source_from_account(
    store: &Store,
    source_id: &SourceId,
    provider_account_id_value: Option<&str>,
    provider_user_id: Option<&str>,
    email: Option<&str>,
    ended_at: DateTime<Utc>,
) -> Result<SourceAccountAssignment> {
    let source = store
        .source(source_id)?
        .with_context(|| format!("unknown source {}", source_id.0))?;
    let account_filter =
        if provider_account_id_value.is_some() || provider_user_id.is_some() || email.is_some() {
            Some(
                resolve_existing_provider_account(
                    store,
                    &source.provider,
                    provider_account_id_value,
                    provider_user_id,
                    email,
                    None,
                )?
                .provider_account_id,
            )
        } else {
            None
        };
    let mut active: Vec<_> = store
        .list_source_account_assignments_for_source(&source.source_id)?
        .into_iter()
        .filter(|assignment| {
            timestamp_in_period(ended_at, assignment.started_at, assignment.ended_at)
        })
        .filter(|assignment| {
            account_filter
                .as_ref()
                .map(|account_id| &assignment.provider_account_id == account_id)
                .unwrap_or(true)
        })
        .collect();
    let Some(mut assignment) = active.pop() else {
        bail!(
            "no active source connection found for {} at {}",
            source.source_id.0,
            ended_at.to_rfc3339()
        );
    };
    validate_time_window(assignment.started_at, Some(ended_at), "source connection")?;
    assignment.ended_at = Some(ended_at);
    assignment.updated_at = Utc::now();
    store.upsert_source_account_assignment(&assignment)?;
    reattribute_source_records(store, &source.source_id)?;
    Ok(assignment)
}

pub(crate) fn validate_source_assignment_overlap(
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

pub(crate) fn reattribute_source_records(store: &Store, source_id: &SourceId) -> Result<()> {
    if store.source(source_id)?.is_none() {
        return Ok(());
    }
    let assignments = store.list_source_account_assignments_for_source(source_id)?;
    let mut events = store.events_for_source(source_id)?;
    let mut summaries = store.summaries_for_source(source_id)?;
    for event in &mut events {
        apply_account_resolution_to_event(&assignments, event);
    }
    for summary in &mut summaries {
        apply_account_resolution_to_summary(&assignments, summary);
    }
    store.rewrite_events(&events)?;
    store.rewrite_summaries(&summaries)?;
    store.reattribute_quota_observations(source_id)?;
    store.rebuild_quota_plan_observations_for_source(source_id)?;
    Ok(())
}

pub(crate) fn canonicalize_account_evidence(
    store: &Store,
    provider: &str,
    evidence: &mut AccountEvidenceScan,
) -> Result<()> {
    let mut canonical_ids = HashMap::new();
    for observed in &evidence.accounts {
        let Some(detected_id) = provider_account_id_from_identity(
            provider,
            observed.provider_user_id.as_deref(),
            observed.email.as_deref(),
        ) else {
            continue;
        };
        let account = upsert_provider_account(
            store,
            UpsertProviderAccountInput {
                provider,
                provider_user_id: observed.provider_user_id.as_deref(),
                email: observed.email.as_deref(),
                label: None,
                // Plan evidence has its own history and must not mutate billing/account facts.
                plan_name: None,
                identity_source: Some(IdentitySource::LocalAuth),
                verified_at: Some(observed.observed_at),
            },
        )?;
        canonical_ids.insert(detected_id, account.provider_account_id);
    }

    remap_account_evidence_account_ids(evidence, &canonical_ids);
    Ok(())
}

pub(crate) fn canonicalize_known_account_evidence(
    store: &Store,
    provider: &str,
    evidence: &mut AccountEvidenceScan,
) -> Result<HashMap<ProviderAccountId, ProviderAccountId>> {
    let mut canonical_ids = HashMap::new();
    for observed in &evidence.accounts {
        let Some(detected_id) = provider_account_id_from_identity(
            provider,
            observed.provider_user_id.as_deref(),
            observed.email.as_deref(),
        ) else {
            continue;
        };
        if let Some(account) = find_existing_provider_account(
            store,
            provider,
            observed.provider_user_id.as_deref(),
            observed.email.as_deref(),
        )? {
            canonical_ids.insert(detected_id, account.provider_account_id);
        }
    }
    remap_account_evidence_account_ids(evidence, &canonical_ids);
    Ok(canonical_ids)
}

pub(crate) fn apply_source_account_resolution(
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

pub(crate) fn apply_account_resolution_to_event(
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

pub(crate) fn apply_account_resolution_to_summary(
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

pub(crate) fn keep_detected_account_identity(
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

pub(crate) fn should_clear_resolved_account(
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
