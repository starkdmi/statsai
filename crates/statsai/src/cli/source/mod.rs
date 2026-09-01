use super::args::{SourceCommand, SourceSubcommand};
use super::format::{abbreviate_home, parse_date};
use super::subscription::{active_subscription, validate_time_window};
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

mod account;
mod paths;

pub(crate) use account::*;
pub(crate) use paths::*;

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
