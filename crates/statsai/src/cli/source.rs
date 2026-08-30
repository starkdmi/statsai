use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use statsai_adapters::{
    adapter_for_provider, remap_account_evidence_account_ids, AccountEvidenceScan,
    VerifiedSourceObservation,
};
use statsai_core::{
    expand_home_path, home_dir, path_hash, periods_overlap, provider_account_id_from_identity,
    source_account_assignment_id, timestamp_in_period, IdentitySource, LocationOrigin,
    ProviderAccount, ProviderAccountId, SourceAccountAssignment, SourceAccountAssignmentId,
    SourceId, SourceLocation, SourceVerificationMode, UsageEvent, UsageSummary,
    SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION,
};
use statsai_store::{
    close_active_verified_source_linkages, find_existing_provider_account, upsert_provider_account,
    Store, UpsertProviderAccountInput,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::args::{SourceCommand, SourceSubcommand};
use super::format::{abbreviate_home, parse_date};

use crate::{active_subscription, validate_time_window};

pub(crate) fn source(command: SourceCommand, store: &Store, device_id: &str) -> Result<()> {
    match command.command {
        SourceSubcommand::Add { provider, path } => {
            let adapter = adapter_for_provider(&provider)
                .with_context(|| format!("unsupported provider {provider}"))?;
            let path = normalize_configured_source_path(adapter.provider(), &path)?;
            let mut source = SourceLocation::local_adapter(
                adapter.provider(),
                adapter.id(),
                adapter.version(),
                &path,
                LocationOrigin::Configured,
            );
            source.path_label = Some(path.to_string_lossy().to_string());
            store.upsert_source(&source)?;
            println!("{}", serde_json::to_string_pretty(&source)?);
        }
        SourceSubcommand::Enable { source_id } => {
            let source_id = statsai_core::SourceId(source_id);
            let source = store
                .set_source_enabled(&source_id, true)?
                .with_context(|| format!("unknown source {}", source_id.0))?;
            println!("{}", serde_json::to_string_pretty(&source)?);
        }
        SourceSubcommand::Disable { source_id } => {
            let source_id = statsai_core::SourceId(source_id);
            let source = store
                .set_source_enabled(&source_id, false)?
                .with_context(|| format!("unknown source {}", source_id.0))?;
            println!("{}", serde_json::to_string_pretty(&source)?);
        }
        SourceSubcommand::Remove {
            source_id,
            delete_data,
        } => {
            let source_id = statsai_core::SourceId(source_id);
            let source = store
                .source(&source_id)?
                .with_context(|| format!("unknown source {}", source_id.0))?;
            let deleted_events = if delete_data {
                store.delete_events_for_sources(std::slice::from_ref(&source_id))?
            } else {
                0
            };
            let deleted_summaries = if delete_data {
                store.delete_summaries_for_sources(std::slice::from_ref(&source_id))?
            } else {
                0
            };
            let deleted_scan_entries = if delete_data {
                store.delete_scan_file_entries_for_sources(std::slice::from_ref(&source_id))?
            } else {
                0
            };
            let deleted_task_spans: statsai_store::TaskDeletionImpact = if delete_data {
                store.delete_task_spans_for_sources(std::slice::from_ref(&source_id))?
            } else {
                Default::default()
            };
            let deleted_quota_observations = if delete_data {
                store.delete_quota_observations_for_sources(std::slice::from_ref(&source_id))?
            } else {
                0
            };
            let deleted_account_evidence = if delete_data {
                store.delete_account_evidence_for_sources(std::slice::from_ref(&source_id))?
            } else {
                0
            };
            if delete_data {
                store.clear_orphaned_quota_usage_links()?;
            }
            let deleted_trace_edits = if delete_data {
                store.delete_archive_import_for_sources(std::slice::from_ref(&source_id))?
            } else {
                0
            };
            let rebuilt_work_items =
                if delete_data && !deleted_task_spans.affected_project_buckets.is_empty() {
                    store.rebuild_task_work_items_for_project_buckets(
                        &deleted_task_spans.affected_project_buckets,
                    )?
                } else {
                    0
                };
            let deleted = store.delete_source(&source_id)?;
            // Metrics built from this source's data are already materialized and
            // the authoritative snapshot keeps republishing them, so they are
            // rebuilt now rather than left live until the next collect or sync.
            // Traces are not the only input: committed metrics are discovered
            // from the project paths carried by usage summaries, so a source that
            // produced Git-derived churn but no reconstructed edits orphans them
            // just as surely. Any data deletion therefore rebuilds.
            let rebuilt_code_change_metrics = if delete_data {
                store.refresh_code_changes(device_id)?.metrics
            } else {
                0
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "source_id": source_id.0,
                    "deleted": deleted,
                    "delete_data": delete_data,
                    "deleted_events": deleted_events,
                    "deleted_summaries": deleted_summaries,
                    "deleted_scan_cache_entries": deleted_scan_entries,
                    "deleted_task_spans": deleted_task_spans.deleted,
                    "deleted_quota_observations": deleted_quota_observations,
                    "deleted_account_evidence": deleted_account_evidence,
                    "deleted_code_change_traces": deleted_trace_edits,
                    "work_items_rebuilt": rebuilt_work_items,
                    "code_change_metrics_rebuilt": rebuilt_code_change_metrics,
                    "source": source
                }))?
            );
        }
        SourceSubcommand::List => {
            println!("{}", serde_json::to_string_pretty(&store.list_sources()?)?);
        }
        SourceSubcommand::Connect {
            source_id,
            path,
            provider_account_id,
            provider_user_id,
            email,
            label,
            started_at,
            ended_at,
        } => {
            let source = resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
            let assignment = connect_source_to_account(
                store,
                ConnectSourceToAccountInput {
                    source_id: &source.source_id,
                    provider_account_id_value: provider_account_id.as_deref(),
                    provider_user_id: provider_user_id.as_deref(),
                    email: email.as_deref(),
                    label,
                    started_at: parse_date(&started_at)?,
                    ended_at: ended_at.as_deref().map(parse_date).transpose()?,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&assignment)?);
        }
        SourceSubcommand::History { source_id, path } => {
            let assignments = if source_id.is_some() || path.is_some() {
                let source =
                    resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
                store.list_source_account_assignments_for_source(&source.source_id)?
            } else {
                store.list_source_account_assignments()?
            };
            println!("{}", serde_json::to_string_pretty(&assignments)?);
        }
        SourceSubcommand::Mode {
            source_id,
            path,
            mode,
        } => {
            let mut source =
                resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
            source.verification_mode = parse_source_verification_mode(&mode)?;
            if !matches!(source.verification_mode, SourceVerificationMode::Auto) {
                source.verified_state_hash = None;
            }
            if matches!(source.verification_mode, SourceVerificationMode::Disabled) {
                close_active_verified_source_linkages(store, &source.source_id, Utc::now())?;
            }
            if !matches!(source.verification_mode, SourceVerificationMode::Auto) {
                store
                    .delete_account_evidence_for_sources(std::slice::from_ref(&source.source_id))?;
            }
            source.updated_at = Utc::now();
            store.upsert_source(&source)?;
            println!("{}", serde_json::to_string_pretty(&source)?);
        }
        SourceSubcommand::Unassign {
            source_id,
            path,
            at,
        } => {
            let source = resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
            let ended_at = at
                .as_deref()
                .map(parse_date)
                .transpose()?
                .unwrap_or_else(Utc::now);
            let assignment = disconnect_source_from_account(
                store,
                &source.source_id,
                None,
                None,
                None,
                ended_at,
            )?;
            println!("{}", serde_json::to_string_pretty(&assignment)?);
        }
        SourceSubcommand::Explain { source_id, path } => {
            let source = resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
            let explanation = explain_source(store, &source)?;
            println!("{}", serde_json::to_string_pretty(&explanation)?);
        }
        SourceSubcommand::Disconnect {
            source_id,
            path,
            provider_account_id,
            provider_user_id,
            email,
            ended_at,
        } => {
            let source = resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
            let assignment = disconnect_source_from_account(
                store,
                &source.source_id,
                provider_account_id.as_deref(),
                provider_user_id.as_deref(),
                email.as_deref(),
                parse_date(&ended_at)?,
            )?;
            println!("{}", serde_json::to_string_pretty(&assignment)?);
        }
    }
    Ok(())
}

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

fn ensure_account_matches_provider(account: &ProviderAccount, provider: &str) -> Result<()> {
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

pub(crate) fn parse_source_verification_mode(value: &str) -> Result<SourceVerificationMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(SourceVerificationMode::Auto),
        "manual_only" | "manual-only" => Ok(SourceVerificationMode::ManualOnly),
        "disabled" => Ok(SourceVerificationMode::Disabled),
        _ => bail!("unsupported verification mode {value}; use auto, manual_only, or disabled"),
    }
}

pub(crate) fn resolve_source_reference(
    store: &Store,
    source_id: Option<&str>,
    path: Option<&Path>,
) -> Result<SourceLocation> {
    match (source_id, path) {
        (Some(_), Some(_)) => bail!("pass either --source-id or --path, not both"),
        (Some(source_id), None) => store
            .source(&SourceId(source_id.to_string()))?
            .with_context(|| format!("source {source_id} not found")),
        (None, Some(path)) => {
            let normalized = expand_home_path(&path.to_string_lossy());
            let target_hash = path_hash(&normalized);
            let mut matches = store
                .list_sources()?
                .into_iter()
                .filter(|source| source.path_hash.as_deref() == Some(target_hash.as_str()));
            let Some(source) = matches.next() else {
                bail!("no source found for path {}", normalized.display());
            };
            if matches.next().is_some() {
                bail!(
                    "multiple sources match path {}; use --source-id instead",
                    normalized.display()
                );
            }
            Ok(source)
        }
        (None, None) => bail!("pass either --source-id or --path"),
    }
}

pub(crate) fn source_verification_mode(source: &SourceLocation) -> SourceVerificationMode {
    source.verification_mode.clone()
}

fn probe_source_verified_observation(source: &SourceLocation) -> Result<VerifiedSourceObservation> {
    if matches!(
        source_verification_mode(source),
        SourceVerificationMode::Disabled
    ) {
        return Ok(VerifiedSourceObservation::Unavailable);
    }
    let Some(adapter) = adapter_for_provider(&source.provider) else {
        return Ok(VerifiedSourceObservation::Unavailable);
    };
    adapter.probe_verified_source_state(source)
}

pub(crate) fn explain_source(store: &Store, source: &SourceLocation) -> Result<Value> {
    let detected_auth_observation = if matches!(
        source_verification_mode(source),
        SourceVerificationMode::Disabled
    ) {
        None
    } else {
        Some(probe_source_verified_observation(source)?)
    };
    explain_source_with_observation(store, source, detected_auth_observation.as_ref())
}

pub(crate) fn explain_source_with_observation(
    store: &Store,
    source: &SourceLocation,
    detected_auth_observation: Option<&VerifiedSourceObservation>,
) -> Result<Value> {
    let assignments = store.list_source_account_assignments_for_source(&source.source_id)?;
    let detected_auth_state = detected_auth_observation
        .map(serde_json::to_value)
        .transpose()?;
    let now = Utc::now();
    let current_assignment = assignment_for_timestamp(&assignments, now).cloned();
    let current_subscription = current_assignment
        .as_ref()
        .and_then(|assignment| {
            active_subscription(
                store,
                &source.provider,
                &assignment.provider_account_id,
                None,
                now,
            )
            .ok()
        })
        .map(serde_json::to_value)
        .transpose()?;
    Ok(json!({
        "source": source,
        "verification_mode": source.verification_mode,
        "verified_state_hash": source.verified_state_hash,
        "detected_auth_state": detected_auth_state,
        "current_assignment": current_assignment,
        "current_subscription": current_subscription,
        "history": assignments,
        "explanation": {
            "usage_is_primary": true,
            "subscriptions_are_secondary": true,
            "unassigned_means": "usage without an active source-to-account connection"
        }
    }))
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

pub(crate) fn normalize_configured_source_path(provider: &str, path: &Path) -> Result<PathBuf> {
    let mut path = expand_cli_path(path)?;
    if provider_matches(provider, "claude_code")
        && path.file_name().is_some_and(|name| name == "projects")
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    if provider_matches(provider, "codex")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "sessions" | "archived_sessions"))
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    if provider_matches(provider, "opencode")
        && path.file_name().is_some_and(|name| name == "opencode.db")
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    if provider_matches(provider, "grok_build")
        && path.file_name().is_some_and(|name| name == "sessions")
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
}

pub(crate) fn expand_cli_path(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir().context("HOME is not set");
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return Ok(home_dir().context("HOME is not set")?.join(rest));
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("read current directory")?
        .join(path))
}

pub(crate) fn path_label_from_hashless_source(source: &SourceLocation) -> Option<String> {
    let home = home_dir()?;
    match (source.provider.as_str(), source.location_origin.clone()) {
        ("claude_code", LocationOrigin::Default) if source.path_hash.is_some() => {
            let a = home.join(".config/claude/projects");
            let b = home.join(".claude/projects");
            let hash = source.path_hash.as_ref()?;
            for path in [a, b] {
                if statsai_core::path_hash(&path) == *hash {
                    return Some(path.to_string_lossy().to_string());
                }
            }
            None
        }
        ("codex", LocationOrigin::Default) if source.path_hash.is_some() => {
            let root = home.join(".codex");
            let hash = source.path_hash.as_ref()?;
            if statsai_core::path_hash(&root) == *hash {
                return Some(root.to_string_lossy().to_string());
            }
            None
        }
        _ => None,
    }
}

pub(crate) fn sources_refer_to_same_location(
    left: &SourceLocation,
    right: &SourceLocation,
) -> bool {
    if left.source_kind != right.source_kind || !provider_matches(&left.provider, &right.provider) {
        return false;
    }
    if left.source_id == right.source_id
        || left
            .path_hash
            .as_deref()
            .zip(right.path_hash.as_deref())
            .is_some_and(|(left, right)| left == right)
    {
        return true;
    }
    match (comparable_source_path(left), comparable_source_path(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

pub(crate) fn dedupe_overlapping_sources(sources: Vec<SourceLocation>) -> Vec<SourceLocation> {
    sources
        .iter()
        .enumerate()
        .filter_map(|(index, source)| {
            let Some(source_path) = comparable_source_path(source) else {
                return Some(source.clone());
            };
            let shadowed = sources.iter().enumerate().any(|(other_index, other)| {
                if index == other_index || !provider_matches(&source.provider, &other.provider) {
                    return false;
                }
                let Some(other_path) = comparable_source_path(other) else {
                    return false;
                };
                if !provider_shadowing_covers_nested_source(source, &source_path, &other_path) {
                    return false;
                }
                other_path != source_path
                    && source_path.starts_with(&other_path)
                    && source_preference_rank(other) >= source_preference_rank(source)
            });
            (!shadowed).then(|| source.clone())
        })
        .collect()
}

pub(crate) fn comparable_source_path(source: &SourceLocation) -> Option<PathBuf> {
    let path = PathBuf::from(source.path_label.as_deref()?);
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

pub(crate) fn provider_shadowing_covers_nested_source(
    source: &SourceLocation,
    source_path: &Path,
    other_path: &Path,
) -> bool {
    match canonical_provider_name(&source.provider) {
        Some("claude_code") => true,
        Some("codex") => codex_source_path_is_covered_by_parent(other_path, source_path),
        _ => false,
    }
}

fn codex_source_path_is_covered_by_parent(parent_path: &Path, child_path: &Path) -> bool {
    if parent_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "sessions" | "archived_sessions"))
    {
        return child_path.starts_with(parent_path);
    }

    child_path.starts_with(parent_path.join("sessions"))
        || child_path.starts_with(parent_path.join("archived_sessions"))
}

pub(crate) fn source_preference_rank(source: &SourceLocation) -> u8 {
    match source.location_origin {
        LocationOrigin::Configured | LocationOrigin::Env => 3,
        LocationOrigin::Discovered => 2,
        LocationOrigin::Default => 1,
    }
}

pub(crate) fn provider_matches(left: &str, right: &str) -> bool {
    match (
        canonical_provider_name(left),
        canonical_provider_name(right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => left == right || left.replace('-', "_") == right || left.replace('_', "-") == right,
    }
}

pub(crate) fn canonical_provider(provider: &str) -> Result<String> {
    canonical_provider_name(provider)
        .map(str::to_string)
        .with_context(|| format!("unsupported provider {provider}"))
}

pub(crate) fn canonical_provider_name(provider: &str) -> Option<&'static str> {
    adapter_for_provider(provider).map(|adapter| adapter.provider())
}

pub(crate) fn persist_source_after_preview(store: &Store, source: &SourceLocation) -> Result<()> {
    store.upsert_source(source)
}

pub(crate) fn location_origin_label(origin: &LocationOrigin) -> &'static str {
    match origin {
        LocationOrigin::Default => "default",
        LocationOrigin::Configured => "configured",
        LocationOrigin::Env => "env",
        LocationOrigin::Discovered => "discovered",
    }
}

pub(crate) fn preview_path_label(source: &SourceLocation) -> String {
    source
        .path_label
        .as_deref()
        .map(abbreviate_home)
        .unwrap_or_else(|| "unknown".to_string())
}
