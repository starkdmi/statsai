use super::state::{watch_sources_for_adapter, VerificationDependencySnapshot};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use statsai_adapters::{
    default_adapters, remap_account_evidence_account_ids,
    retain_accounts_referenced_by_account_evidence, AccountEvidenceScan, ProviderAdapter,
    ScanCandidateFile, ScanOptions, VerifiedSourceObservation,
};
use statsai_core::{
    hash_text, provider_account_id_from_identity, timestamp_in_period, IdentitySource,
    ProviderAccountId, SourceAccountAssignment, SourceLocation, SourceVerificationMode, UsageEvent,
    UsageSummary,
};
use statsai_store::{
    find_existing_provider_account, reconcile_verified_source_state, upsert_provider_account,
    verified_source_observation_hash, ScanFileReplacement, ScanFileStateEntry, Store,
    UpsertProviderAccountInput,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(super) fn enqueue_background_scan(
    pending: &Arc<Mutex<HashSet<PathBuf>>>,
    signal: &mpsc::SyncSender<()>,
    changed: Vec<PathBuf>,
) {
    pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .extend(changed);
    let _ = signal.try_send(());
}

pub(super) fn process_background_scan(
    pending: &Arc<Mutex<HashSet<PathBuf>>>,
    signal: &mpsc::SyncSender<()>,
    changed: Vec<PathBuf>,
    retry_delay: Duration,
    scan: impl FnOnce(&[PathBuf]) -> Result<()>,
) -> bool {
    if let Err(error) = scan(&changed) {
        eprintln!("daemon: background scan failed and will be retried: {error:#}");
        std::thread::sleep(retry_delay);
        enqueue_background_scan(pending, signal, changed);
        return false;
    }
    true
}

pub(super) fn rescan_changed_sources(
    scan_store: &Store,
    commit_store: &Arc<Mutex<Store>>,
    device_id: &str,
    changed: &[PathBuf],
    verification_dependencies: &VerificationDependencySnapshot,
) -> Result<()> {
    let adapters: Vec<Box<dyn ProviderAdapter>> = default_adapters();
    rescan_changed_sources_with_adapters_and_commit_store_and_dependencies(
        scan_store,
        Some(commit_store),
        device_id,
        changed,
        &adapters,
        verification_dependencies,
    )
}

#[cfg(test)]
pub(super) fn rescan_changed_sources_with_adapters(
    store: &Store,
    device_id: &str,
    changed: &[PathBuf],
    adapters: &[Box<dyn ProviderAdapter>],
) -> Result<()> {
    rescan_changed_sources_with_adapters_and_dependencies(
        store,
        device_id,
        changed,
        adapters,
        &VerificationDependencySnapshot::default(),
    )
}

pub(super) fn rescan_changed_sources_with_adapters_and_dependencies(
    store: &Store,
    device_id: &str,
    changed: &[PathBuf],
    adapters: &[Box<dyn ProviderAdapter>],
    verification_dependencies: &VerificationDependencySnapshot,
) -> Result<()> {
    rescan_changed_sources_with_adapters_and_commit_store_and_dependencies(
        store,
        None,
        device_id,
        changed,
        adapters,
        verification_dependencies,
    )
}

#[cfg(test)]
pub(super) fn rescan_changed_sources_with_adapters_and_commit_store(
    scan_store: &Store,
    commit_store: Option<&Arc<Mutex<Store>>>,
    device_id: &str,
    changed: &[PathBuf],
    adapters: &[Box<dyn ProviderAdapter>],
) -> Result<()> {
    rescan_changed_sources_with_adapters_and_commit_store_and_dependencies(
        scan_store,
        commit_store,
        device_id,
        changed,
        adapters,
        &VerificationDependencySnapshot::default(),
    )
}

fn rescan_changed_sources_with_adapters_and_commit_store_and_dependencies(
    scan_store: &Store,
    commit_store: Option<&Arc<Mutex<Store>>>,
    device_id: &str,
    changed: &[PathBuf],
    adapters: &[Box<dyn ProviderAdapter>],
    verification_dependencies: &VerificationDependencySnapshot,
) -> Result<()> {
    let configured = scan_store
        .list_sources()
        .context("list sources for changed-source rescan")?;
    let mut failed = false;

    for adapter in adapters {
        let sources = scan_sources_for_paths(
            adapter.as_ref(),
            &configured,
            changed,
            verification_dependencies,
        );
        for mut source in sources {
            let verification_mode = source.verification_mode.clone();
            let account_evidence_enabled =
                matches!(verification_mode, SourceVerificationMode::Auto);
            let mut account_evidence = statsai_adapters::AccountEvidenceScan::default();
            if account_evidence_enabled {
                let account_evidence_checkpoints = scan_store
                    .account_evidence_checkpoints(&source.source_id)
                    .context("load account evidence checkpoints")?;
                account_evidence = match adapter
                    .collect_account_evidence(&source, &account_evidence_checkpoints)
                {
                    Ok(evidence) => evidence,
                    Err(error) => {
                        eprintln!(
                            "daemon: account evidence scan failed for {}: {error}",
                            source.path_label.as_deref().unwrap_or("unknown")
                        );
                        failed = true;
                        continue;
                    }
                };
                let known_account_aliases = canonicalize_known_account_evidence(
                    scan_store,
                    adapter.provider(),
                    &mut account_evidence,
                )
                .context("canonicalize known account evidence")?;
                scan_store
                    .retain_unseen_account_evidence(
                        &source.source_id,
                        &mut account_evidence.identity_observations,
                        &mut account_evidence.plan_observations,
                        &mut account_evidence.conversation_bindings,
                    )
                    .context("filter previously collected account evidence")?;
                retain_accounts_referenced_by_account_evidence(
                    adapter.provider(),
                    &known_account_aliases,
                    &mut account_evidence,
                );
            }
            let has_account_evidence = !account_evidence.identity_observations.is_empty()
                || !account_evidence.plan_observations.is_empty()
                || !account_evidence.conversation_bindings.is_empty()
                || !account_evidence.checkpoints.is_empty();
            let expected_data_version = commit_store
                .map(|_| scan_store.data_version())
                .transpose()
                .context("capture database generation for changed-source rescan")?;
            let expected_source = configured
                .iter()
                .find(|configured_source| configured_source.source_id == source.source_id)
                .cloned();
            let cache_candidates = match adapter.scan_candidates(&source) {
                Ok(candidates) => candidates,
                Err(e) => {
                    eprintln!(
                        "daemon: scan candidate discovery failed for {}: {e}",
                        source.path_label.as_deref().unwrap_or("unknown")
                    );
                    failed = true;
                    continue;
                }
            };
            let compatible_scan_signatures =
                scan_candidate_compatible_signatures(&cache_candidates);
            let file_cache_entries = scan_file_state_entries(&cache_candidates);
            let selection = match scan_store
                .select_scan_file_state_entries_with_task_requirement_and_compatibility(
                    &source.source_id,
                    &file_cache_entries,
                    false,
                    &compatible_scan_signatures,
                ) {
                Ok(selection) => selection,
                Err(e) => {
                    eprintln!(
                        "daemon: scan cache lookup failed for {}: {e}",
                        source.path_label.as_deref().unwrap_or("unknown")
                    );
                    failed = true;
                    continue;
                }
            };
            let pending_file_entries = selection.pending_entries;
            let compatible_entries_to_upgrade = selection.compatible_entries_to_upgrade;
            let tracked_file_entries = match scan_store.scan_file_entries(&source.source_id) {
                Ok(entries) => entries,
                Err(e) => {
                    eprintln!("daemon: scan cache listing failed: {e}");
                    failed = true;
                    continue;
                }
            };
            let current_cache_keys = file_cache_entries
                .iter()
                .map(|entry| entry.cache_key.as_str())
                .collect::<HashSet<_>>();
            let removed_file_entries = tracked_file_entries
                .into_iter()
                .filter(|entry| !current_cache_keys.contains(entry.cache_key.as_str()))
                .collect::<Vec<_>>();
            let has_cache_entry_upgrades = !compatible_entries_to_upgrade.is_empty();
            let probed_verified_source_state =
                if matches!(verification_mode, SourceVerificationMode::Disabled) {
                    VerifiedSourceObservation::Unavailable
                } else {
                    match adapter.probe_verified_source_state(&source) {
                        Ok(state) => state,
                        Err(e) => {
                            eprintln!(
                                "daemon: verified auth probe failed for {}: {e}",
                                source.path_label.as_deref().unwrap_or("unknown")
                            );
                            failed = true;
                            continue;
                        }
                    }
                };
            let next_verified_state_hash =
                if matches!(verification_mode, SourceVerificationMode::Auto) {
                    match &probed_verified_source_state {
                        VerifiedSourceObservation::Unavailable => {
                            source.verified_state_hash.clone()
                        }
                        observation => match verified_source_observation_hash(observation) {
                            Ok(hash) => hash,
                            Err(e) => {
                                eprintln!(
                                    "daemon: verified auth hash failed for {}: {e}",
                                    source.path_label.as_deref().unwrap_or("unknown")
                                );
                                failed = true;
                                continue;
                            }
                        },
                    }
                } else {
                    None
                };
            let verified_state_changed = matches!(verification_mode, SourceVerificationMode::Auto)
                && source.verified_state_hash != next_verified_state_hash;
            let rescan_file_entries = if removed_file_entries.is_empty() {
                &pending_file_entries
            } else {
                &file_cache_entries
            };
            if pending_file_entries.is_empty()
                && removed_file_entries.is_empty()
                && !has_cache_entry_upgrades
                && !verified_state_changed
                && !has_account_evidence
            {
                continue;
            }
            let options = ScanOptions {
                device_id: device_id.to_string(),
                collect_tasks: false,
                selected_cache_keys: Some(
                    rescan_file_entries
                        .iter()
                        .map(|entry| entry.cache_key.clone())
                        .collect::<HashSet<_>>(),
                ),
            };
            let scan_result = if rescan_file_entries.is_empty() {
                Ok(statsai_adapters::AdapterScan::default())
            } else {
                adapter.scan(&source, &options)
            };
            match scan_result {
                Ok(mut scan) => {
                    let parsed_events = scan.events.len();
                    let parsed_summaries = scan.summaries.len();
                    let effective_verified_source_state =
                        if matches!(verification_mode, SourceVerificationMode::Disabled) {
                            VerifiedSourceObservation::Unavailable
                        } else if rescan_file_entries.is_empty() {
                            probed_verified_source_state
                        } else {
                            scan.verified_source_state
                                .take()
                                .map(Box::new)
                                .map(VerifiedSourceObservation::Verified)
                                .unwrap_or(probed_verified_source_state)
                        };
                    let effective_verified_state_hash =
                        if matches!(verification_mode, SourceVerificationMode::Auto) {
                            match &effective_verified_source_state {
                                VerifiedSourceObservation::Unavailable => {
                                    source.verified_state_hash.clone()
                                }
                                observation => {
                                    match verified_source_observation_hash(observation) {
                                        Ok(hash) => hash,
                                        Err(e) => {
                                            eprintln!(
                                                "daemon: verified auth hash failed for {}: {e}",
                                                source.path_label.as_deref().unwrap_or("unknown")
                                            );
                                            failed = true;
                                            continue;
                                        }
                                    }
                                }
                            }
                        } else {
                            None
                        };
                    let reconciled_file_hashes = rescan_file_entries
                        .iter()
                        .chain(removed_file_entries.iter())
                        .map(|entry| hash_text(&entry.cache_key))
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>();
                    let removed_cache_keys = removed_file_entries
                        .iter()
                        .map(|entry| entry.cache_key.clone())
                        .collect::<Vec<_>>();
                    let commit_result = commit_source_scan_if_current(
                        scan_store,
                        commit_store,
                        expected_data_version,
                        expected_source.as_ref(),
                        &mut source,
                        |store, source| {
                            reconcile_verified_source_state(
                                store,
                                source,
                                &effective_verified_source_state,
                                effective_verified_state_hash,
                            )
                            .context("reconcile verified auth state")?;
                            store
                                .upsert_source(source)
                                .context("update source verified auth state")?;
                            if account_evidence_enabled {
                                canonicalize_account_evidence(
                                    store,
                                    adapter.provider(),
                                    &mut account_evidence,
                                )?;
                                store.upsert_account_identity_observations(
                                    &account_evidence.identity_observations,
                                )?;
                                store.upsert_account_plan_observations(
                                    &account_evidence.plan_observations,
                                )?;
                                store.reconcile_source_account_evidence_assignments(
                                    &source.source_id,
                                )?;
                                store.upsert_conversation_account_bindings(
                                    &account_evidence.conversation_bindings,
                                )?;
                                store.reattribute_conversation_bound_events(&source.source_id)?;
                                store.upsert_account_evidence_checkpoints(
                                    &account_evidence.checkpoints,
                                )?;
                            }
                            if pending_file_entries.is_empty() && removed_file_entries.is_empty() {
                                store
                                    .upgrade_scan_file_entries(
                                        &source.source_id,
                                        &compatible_entries_to_upgrade,
                                    )
                                    .context("upgrade scan cache")?;
                                return Ok(None);
                            }
                            apply_source_account_resolution(
                                store,
                                source,
                                &mut scan.events,
                                &mut scan.summaries,
                            )
                            .context("resolve source accounts")?;
                            if account_evidence_enabled {
                                store.apply_conversation_account_bindings(
                                    &source.source_id,
                                    &mut scan.events,
                                )?;
                            }
                            let replacement = store
                                .replace_scan_file_records(ScanFileReplacement {
                                    source_id: &source.source_id,
                                    reconciled_file_hashes: &reconciled_file_hashes,
                                    events: &scan.events,
                                    summaries: &scan.summaries,
                                    pending_entries: &pending_file_entries,
                                    compatible_entries_to_upgrade: &compatible_entries_to_upgrade,
                                    removed_cache_keys: &removed_cache_keys,
                                })
                                .context("atomically reconcile scan files")?;
                            Ok(Some(replacement))
                        },
                    );
                    match commit_result {
                        Ok(Some(replacement)) => {
                            eprintln!(
                                "daemon: rescanned {} ({}) — files={}, cached={}, parsed_events={}, inserted_events={}, parsed_summaries={}, summaries_written={}",
                                source.provider,
                                source.path_label.as_deref().unwrap_or("unknown"),
                                scan.diagnostics.files_scanned,
                                scan.diagnostics.files_skipped_unchanged,
                                parsed_events,
                                replacement.inserted_events,
                                parsed_summaries,
                                replacement.written_summaries
                            );
                        }
                        Ok(None) => {
                            eprintln!(
                                "daemon: reconciled auth/cache state for {} ({})",
                                source.provider,
                                source.path_label.as_deref().unwrap_or("unknown")
                            );
                        }
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!(
                                    "commit changed-source scan for {}",
                                    source.path_label.as_deref().unwrap_or("unknown")
                                )
                            });
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "daemon: scan failed for {}: {e}",
                        source.path_label.as_deref().unwrap_or("unknown")
                    );
                    failed = true;
                }
            }
        }
    }

    if failed {
        anyhow::bail!("one or more changed sources could not be rescanned");
    }
    Ok(())
}

pub(super) fn commit_source_scan_if_current<T>(
    scan_store: &Store,
    commit_store: Option<&Arc<Mutex<Store>>>,
    expected_data_version: Option<i64>,
    expected_source: Option<&SourceLocation>,
    source: &mut SourceLocation,
    commit: impl FnOnce(&Store, &mut SourceLocation) -> Result<T>,
) -> Result<T> {
    let Some(commit_store) = commit_store else {
        return commit(scan_store, source);
    };

    let store = super::lock_store(commit_store);
    let expected_data_version = expected_data_version
        .context("missing database generation for independent scan connection")?;
    store.apply_scan_update(|store| {
        let current_data_version = scan_store
            .data_version()
            .context("verify database generation before scan commit")?;
        if current_data_version != expected_data_version {
            anyhow::bail!(
                "database changed while scanning (expected generation {}, found {})",
                expected_data_version,
                current_data_version
            );
        }

        let current_source = store
            .source(&source.source_id)
            .context("re-read source before scan commit")?;
        if current_source.as_ref() != expected_source {
            anyhow::bail!("source changed while scanning");
        }
        if let Some(current_source) = current_source {
            *source = current_source;
        }

        commit(store, source)
    })
}

pub(super) fn scan_sources_for_paths(
    adapter: &dyn ProviderAdapter,
    configured: &[SourceLocation],
    changed: &[PathBuf],
    verification_dependencies: &VerificationDependencySnapshot,
) -> Vec<SourceLocation> {
    watch_sources_for_adapter(adapter, configured)
        .into_iter()
        .filter(|source| {
            source_in_changed_paths(source, changed, verification_dependencies.paths_for(source))
        })
        .collect()
}

fn source_in_changed_paths(
    source: &SourceLocation,
    changed: &[PathBuf],
    verification_dependencies: &[PathBuf],
) -> bool {
    let Some(label) = source.path_label.as_deref() else {
        return false;
    };
    std::iter::once(PathBuf::from(label))
        .chain(verification_dependencies.iter().cloned())
        .any(|dependency| {
            changed.iter().any(|changed_path| {
                changed_path.starts_with(&dependency) || dependency.starts_with(changed_path)
            })
        })
}

fn scan_file_state_entries(candidates: &[ScanCandidateFile]) -> Vec<ScanFileStateEntry> {
    candidates
        .iter()
        .map(|candidate| ScanFileStateEntry {
            cache_key: candidate.cache_key.clone(),
            cache_signature: candidate.cache_signature.clone(),
        })
        .collect()
}

fn scan_candidate_compatible_signatures(
    candidates: &[ScanCandidateFile],
) -> HashMap<String, Vec<String>> {
    candidates
        .iter()
        .filter(|candidate| !candidate.compatible_cache_signatures.is_empty())
        .map(|candidate| {
            (
                candidate.cache_key.clone(),
                candidate.compatible_cache_signatures.clone(),
            )
        })
        .collect()
}

fn apply_source_account_resolution(
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

fn canonicalize_account_evidence(
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

fn canonicalize_known_account_evidence(
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

fn assignment_for_timestamp(
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
