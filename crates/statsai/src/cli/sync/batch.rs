use anyhow::Result;
#[cfg(test)]
use chrono::DateTime;
use chrono::{Duration, Utc};
#[cfg(test)]
use statsai_core::{
    micro_usd_to_cents_rounded, summary_id, Confidence, CostInfo, EventSource, PrivacyInfo,
    PrivacyMode, SummaryMetadata, UsageCounts, USAGE_SUMMARY_SCHEMA_VERSION,
};
use statsai_core::{
    project_contains_file_paths, sanitize_code_change_metric_for_sync,
    sanitize_task_bucket_for_sync, IdentitySource, ProjectInfo, ProviderAccount,
    SourceAccountAssignment, SourceKind, SourceLocation, Subscription, SyncAuthoritativeSnapshot,
    SyncBatch, TaskVerificationCursor, UsageEvent, UsageSummary, SYNC_BATCH_SCHEMA_VERSION,
};
use statsai_store::{QuotaQuery, Store};
#[cfg(test)]
use std::collections::BTreeMap;

use statsai::snapshot;

use super::super::args::SyncCommand;
#[cfg(test)]
use super::http::logical_http_rollup_batch_id;
use super::{effective_sync_preferences, rollup_mode_label, sync_payload_mode, SyncPayloadMode};

#[cfg(test)]
pub(crate) fn build_sync_batch(
    command: &SyncCommand,
    store: &Store,
    device_id: &str,
    target: &str,
) -> Result<(SyncBatch, SyncPayloadMode)> {
    build_sync_batch_with_identity_key(command, store, device_id, target, None)
}

pub(crate) fn build_sync_batch_with_identity_key(
    command: &SyncCommand,
    store: &Store,
    device_id: &str,
    target: &str,
    code_change_identity_key: Option<&[u8; 32]>,
) -> Result<(SyncBatch, SyncPayloadMode)> {
    let created_at = Utc::now();
    let batch_id = format!("batch_{}", created_at.timestamp_millis());
    let sync_preferences = effective_sync_preferences(store, command)?;
    let include_projects = sync_preferences.include_projects;
    let payload_mode = sync_payload_mode(command)?;
    let state = if command.sink == "http" || command.since_last {
        store.sync_state(&command.sink, target)?
    } else {
        None
    };
    let event_cursor = if payload_mode == SyncPayloadMode::Rollups {
        None
    } else {
        state.as_ref().and_then(|state| {
            state
                .last_event_started_at
                .as_ref()
                .zip(state.last_event_id.as_deref())
        })
    };
    let summary_cursor = state.as_ref().and_then(|state| {
        state
            .last_summary_observed_at
            .as_ref()
            .zip(state.last_summary_id.as_deref())
    });
    let events: Vec<_> = if payload_mode == SyncPayloadMode::Rollups {
        Vec::new()
    } else {
        store
            .events_after(event_cursor)?
            .into_iter()
            .map(|event| sanitize_event_for_sync_with_projects(event, include_projects))
            .collect()
    };
    let all_passthrough_summaries: Vec<_> = if payload_mode == SyncPayloadMode::Rollups {
        store
            .summaries()?
            .into_iter()
            .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects))
            .filter(is_http_rollup_passthrough_summary)
            .collect()
    } else {
        Vec::new()
    };
    let mut summaries: Vec<_> = if payload_mode == SyncPayloadMode::Rollups {
        Vec::new()
    } else {
        store
            .summaries_after(summary_cursor)?
            .into_iter()
            .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects))
            .collect()
    };
    let all_sources: Vec<_> = store
        .list_sources()?
        .into_iter()
        .map(sanitize_source_for_sync)
        .collect();
    let all_accounts: Vec<_> = store
        .list_accounts()?
        .into_iter()
        .map(sanitize_account_for_sync)
        .collect();
    let all_source_account_assignments: Vec<_> = store
        .list_source_account_assignments()?
        .into_iter()
        .map(sanitize_source_account_assignment_for_sync)
        .collect();
    let all_subscriptions: Vec<_> = store
        .list_subscriptions()?
        .into_iter()
        .filter(|subscription| subscription.record_source != IdentitySource::LocalAuth)
        .map(sanitize_subscription_for_sync)
        .collect();
    let all_account_plan_observations = store.account_plan_projections(device_id)?;
    let all_account_evidence_summaries = store.account_evidence_summaries(device_id)?;
    let snapshot_source_ids = all_sources
        .iter()
        .map(|source| source.source_id.clone())
        .collect::<Vec<_>>();
    let snapshot_provider_account_ids = all_accounts
        .iter()
        .map(|account| account.provider_account_id.clone())
        .collect::<Vec<_>>();
    let snapshot_assignment_ids = all_source_account_assignments
        .iter()
        .map(|assignment| assignment.assignment_id.clone())
        .collect::<Vec<_>>();
    let snapshot_subscription_ids = all_subscriptions
        .iter()
        .map(|subscription| subscription.subscription_id.clone())
        .collect::<Vec<_>>();
    let snapshot_account_plan_observation_ids = all_account_plan_observations
        .iter()
        .map(|observation| observation.projection_id.clone())
        .collect::<Vec<_>>();
    let snapshot_account_evidence_summary_ids = all_account_evidence_summaries
        .iter()
        .map(|summary| summary.summary_id.clone())
        .collect::<Vec<_>>();
    let mut authoritative_snapshot = None;
    let sources = if payload_mode == SyncPayloadMode::Rollups {
        store.pending_sources_for_sync(&command.sink, target, &all_sources)?
    } else {
        all_sources
    };
    let accounts = if payload_mode == SyncPayloadMode::Rollups {
        store.pending_accounts_for_sync(&command.sink, target, &all_accounts)?
    } else {
        all_accounts
    };
    let source_account_assignments = if payload_mode == SyncPayloadMode::Rollups {
        store.pending_source_account_assignments_for_sync(
            &command.sink,
            target,
            &all_source_account_assignments,
        )?
    } else {
        all_source_account_assignments
    };
    let subscriptions = if payload_mode == SyncPayloadMode::Rollups {
        store.pending_subscriptions_for_sync(&command.sink, target, &all_subscriptions)?
    } else {
        all_subscriptions
    };
    let account_plan_observations = if payload_mode == SyncPayloadMode::Rollups {
        store.pending_account_plan_projections_for_sync(
            &command.sink,
            target,
            &all_account_plan_observations,
        )?
    } else {
        all_account_plan_observations
    };
    let account_evidence_summaries = if payload_mode == SyncPayloadMode::Rollups {
        store.pending_account_evidence_summaries_for_sync(
            &command.sink,
            target,
            &all_account_evidence_summaries,
        )?
    } else {
        all_account_evidence_summaries
    };
    let (task_buckets, task_verifications) = if sync_preferences.include_tasks {
        let task_verification_cursor = if command.sink == "http" || command.since_last {
            store.sync_task_verification_cursor(&command.sink, target)?
        } else {
            None
        };
        let full_task_sync = command.full || state.is_none();
        let task_buckets = store
            .pending_task_bucket_snapshots_for_sync(
                &command.sink,
                target,
                device_id,
                full_task_sync,
                task_verification_cursor.clone(),
            )?
            .into_iter()
            .map(sanitize_task_bucket_for_sync)
            .collect();
        let task_verifications = if full_task_sync {
            store.task_verifications()?
        } else {
            store.pending_task_verifications_for_sync(&command.sink, target)?
        };
        (task_buckets, task_verifications)
    } else {
        (Vec::new(), Vec::new())
    };
    if !command.dry_run {
        if let Some(identity_key) = code_change_identity_key {
            store.refresh_code_changes_with_identity_key(device_id, identity_key)?;
        } else {
            store.refresh_code_changes(device_id)?;
        }
    }
    let all_code_change_metrics = store
        .list_code_change_metrics(false)?
        .into_iter()
        .filter(|metric| metric.device_id == device_id)
        .map(|metric| sanitize_code_change_metric_for_sync(metric, include_projects))
        .collect::<Vec<_>>();
    let code_change_metrics = if command.full || state.is_none() {
        all_code_change_metrics.clone()
    } else {
        store.pending_code_change_metrics_for_sync(
            &command.sink,
            target,
            &all_code_change_metrics,
        )?
    };
    let all_code_change_metric_ids = all_code_change_metrics
        .iter()
        .map(|metric| metric.metric_id.clone())
        .collect::<Vec<_>>();
    let all_quota_cycle_contributions =
        store.quota_cycle_contributions(&QuotaQuery::default(), device_id)?;
    let quota_cycle_contributions = if command.full || state.is_none() {
        all_quota_cycle_contributions.clone()
    } else {
        store.pending_quota_cycle_contributions_for_sync(
            &command.sink,
            target,
            &all_quota_cycle_contributions,
        )?
    };
    let all_quota_cycle_contribution_ids = all_quota_cycle_contributions
        .iter()
        .map(|contribution| contribution.contribution_id.clone())
        .collect::<Vec<_>>();

    if payload_mode == SyncPayloadMode::Rollups {
        let label = rollup_mode_label(command);
        let should_bootstrap =
            !command.dry_run && store.sync_rollup_count()? == 0 && store.event_count()? > 0;
        if !command.dry_run && command.rebuild_rollups {
            let rebuilt = store.rebuild_sync_rollups()?;
            let marked_dirty = store.mark_all_sync_rollups_dirty()?;
            eprintln!(
                "{label}: rebuilt {} local daily summaries and marked {} dirty for full sync",
                rebuilt, marked_dirty
            );
        } else if should_bootstrap {
            let rebuilt = store.rebuild_sync_rollups()?;
            eprintln!(
                "{label}: bootstrapped {} local daily summaries from existing events",
                rebuilt
            );
        }

        let all_rollups: Vec<_> = store
            .all_sync_rollup_summaries()?
            .into_iter()
            .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects))
            .collect();
        let current_authoritative_snapshot = SyncAuthoritativeSnapshot {
            snapshot_id: format!("{batch_id}_authoritative"),
            part_index: 0,
            part_count: 1,
            source_ids: snapshot_source_ids,
            provider_account_ids: snapshot_provider_account_ids,
            source_account_assignment_ids: snapshot_assignment_ids,
            subscription_ids: snapshot_subscription_ids,
            summary_ids: all_passthrough_summaries
                .iter()
                .chain(all_rollups.iter())
                .map(|summary| summary.summary_id.clone())
                .collect(),
            code_change_metric_ids: all_code_change_metric_ids,
            quota_cycle_contribution_ids: all_quota_cycle_contribution_ids,
            account_plan_observation_ids: snapshot_account_plan_observation_ids,
            account_evidence_summary_ids: snapshot_account_evidence_summary_ids,
        };
        let failed_without_resume = state.as_ref().is_some_and(|state| {
            state.failure_count > 0 && state.pending_resume_batch_id.is_none()
        });
        let has_pending_resume = state
            .as_ref()
            .and_then(|state| state.pending_resume_batch_id.as_deref())
            .is_some();
        let has_retired_entities = store.sync_target_has_retired_entities(
            &command.sink,
            target,
            &current_authoritative_snapshot,
        )?;
        let full_history_sync =
            command.full || command.rebuild_rollups || state.is_none() || has_pending_resume;
        // A failure that recorded no chunk left the cursor exactly where it was --
        // every chunk that lands marks a resume point first -- so the summaries the
        // unacknowledged batch carried are still pending, and an incremental sync
        // re-sends precisely them. If that batch did reach the server after all, the
        // resend is deduplicated there. What an incremental sync cannot do by itself
        // is prove the mirror agrees about which entities exist, so the authoritative
        // snapshot goes with it: that is what lets the server retire anything stale.
        //
        // Re-uploading the whole account did the same job by brute force. On a real
        // account that was 422 summaries across 32 chunks for a failure that may have
        // delivered nothing at all.
        let unacknowledged_failure = !command.since_last && failed_without_resume;
        if full_history_sync || has_retired_entities || unacknowledged_failure {
            authoritative_snapshot = Some(current_authoritative_snapshot);
        }
        let rollups = if full_history_sync {
            all_rollups
        } else {
            store.pending_summaries_for_sync(&command.sink, target, &all_rollups)?
        };
        let passthrough_summaries = if full_history_sync {
            all_passthrough_summaries
        } else {
            store.pending_summaries_for_sync(&command.sink, target, &all_passthrough_summaries)?
        };
        eprintln!(
            "{label}: prepared {} local daily summaries for {} sync",
            rollups.len(),
            if full_history_sync {
                "full-history"
            } else {
                "incremental"
            }
        );
        summaries.extend(passthrough_summaries);
        summaries.extend(
            rollups
                .into_iter()
                .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects)),
        );
    }

    Ok((
        SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id,
            device_id: device_id.to_string(),
            sources,
            accounts,
            source_account_assignments,
            subscriptions,
            account_plan_observations,
            account_evidence_summaries,
            events,
            summaries,
            task_buckets,
            task_verifications,
            code_change_metrics,
            quota_cycle_contributions,
            authoritative_snapshot,
            created_at,
        },
        payload_mode,
    ))
}

#[cfg(test)]
pub(crate) fn record_rollup_sync_success(
    store: &Store,
    sink: &str,
    target: &str,
    batch: &SyncBatch,
) -> Result<()> {
    let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();
    record_rollup_sync_chunk_success(store, sink, target, &logical_batch_id, batch)?;
    if let Some(snapshot) = batch.authoritative_snapshot.as_ref() {
        store.reconcile_sync_tracking_to_authoritative_snapshot(sink, target, snapshot)?;
    }
    store.clear_pending_sync_resume(sink, target)?;
    Ok(())
}

pub(crate) fn record_rollup_sync_chunk_success(
    store: &Store,
    sink: &str,
    target: &str,
    logical_batch_id: &str,
    batch: &SyncBatch,
) -> Result<()> {
    store.record_rollup_chunk_sync_success(sink, target, logical_batch_id, batch)?;
    store.mark_code_change_metrics_synced(
        &batch
            .code_change_metrics
            .iter()
            .map(|metric| metric.metric_id.clone())
            .collect::<Vec<_>>(),
    )?;
    store.record_quota_cycle_contributions_synced(
        sink,
        target,
        &batch.quota_cycle_contributions,
    )?;
    store.record_account_plan_projections_synced(sink, target, &batch.account_plan_observations)?;
    store.record_account_evidence_summaries_synced(
        sink,
        target,
        &batch.account_evidence_summaries,
    )?;
    snapshot::invalidate_dashboard_cache();
    Ok(())
}

pub(crate) fn record_sync_batch_success(
    store: &Store,
    sink: &str,
    target: &str,
    batch: &SyncBatch,
) -> Result<()> {
    let task_verification_cursor = sync_batch_task_verification_cursor(batch);
    store.record_sync_success(
        sink,
        target,
        &batch.batch_id,
        &batch.events,
        &batch.summaries,
        task_verification_cursor.as_ref(),
    )?;
    store.record_task_bucket_snapshots_synced(
        sink,
        target,
        &batch.device_id,
        &batch.task_buckets,
    )?;
    store.record_task_verifications_synced(sink, target, &batch.task_verifications)?;
    store.record_code_change_metrics_synced(sink, target, &batch.code_change_metrics)?;
    store.record_account_plan_projections_synced(sink, target, &batch.account_plan_observations)?;
    store.record_account_evidence_summaries_synced(
        sink,
        target,
        &batch.account_evidence_summaries,
    )?;
    store.record_quota_cycle_contributions_synced(
        sink,
        target,
        &batch.quota_cycle_contributions,
    )?;
    store.mark_code_change_metrics_synced(
        &batch
            .code_change_metrics
            .iter()
            .map(|metric| metric.metric_id.clone())
            .collect::<Vec<_>>(),
    )?;
    snapshot::invalidate_dashboard_cache();
    Ok(())
}

fn sync_batch_task_verification_cursor(batch: &SyncBatch) -> Option<TaskVerificationCursor> {
    batch
        .task_buckets
        .iter()
        .filter_map(|bucket| bucket.applied_verification_cursor.clone())
        .max_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left.verification_id.0.cmp(&right.verification_id.0))
        })
}

pub(crate) fn sanitize_source_for_sync(mut source: SourceLocation) -> SourceLocation {
    source.path_label = None;
    source
}

pub(crate) fn sanitize_account_for_sync(mut account: ProviderAccount) -> ProviderAccount {
    if !matches!(account.identity_source, IdentitySource::UserConfigured) {
        account.account_label = None;
    }
    // The email and provider user id stay. They are how a person tells one of
    // their own accounts from another in the dashboard, which is the whole
    // point of syncing accounts at all; stripping them left every account
    // showing as a bare `acct_` hash. The new evidence types are the ones that
    // must never carry them -- those travel as hashes and are covered by their
    // own contracts. `plan_name` still goes, because a plan is now evidence
    // rather than an account attribute.
    account.plan_name = None;
    account
}

pub(crate) fn sanitize_source_account_assignment_for_sync(
    assignment: SourceAccountAssignment,
) -> SourceAccountAssignment {
    assignment
}

pub(crate) fn sanitize_event_for_sync(mut event: UsageEvent) -> UsageEvent {
    event.source.source_record_id = None;
    if let Some(evidence) = event.parse_evidence.as_mut() {
        evidence.source_line_number = None;
        evidence.source_record_id = None;
    }
    event.project = event.project.and_then(sanitize_project_for_sync);
    if project_contains_file_paths(event.project.as_ref()) {
        event.privacy.contains_file_paths = true;
    }
    event
}

fn sanitize_event_for_sync_with_projects(event: UsageEvent, include_projects: bool) -> UsageEvent {
    let mut event = sanitize_event_for_sync(event);
    if !include_projects {
        event.project = None;
    }
    event
}

fn sanitize_project_for_sync(project: ProjectInfo) -> Option<ProjectInfo> {
    statsai_core::sanitize_project_for_sync(project)
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct SyncRollupStatsAccumulator {
    provider: String,
    source_id: statsai_core::SourceId,
    provider_account_id: Option<statsai_core::ProviderAccountId>,
    source: EventSource,
    period_start: DateTime<Utc>,
    period_end: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    account_key: String,
    day_key: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_creation_5m_tokens: u64,
    cache_creation_1h_tokens: u64,
    cache_read_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
    events: u64,
    estimated_cost_micro_usd: i64,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SyncRollupStatsBucketKey {
    provider: String,
    source_id: String,
    account_key: String,
    day_key: String,
}

#[cfg(test)]
fn sync_rollup_stats_bucket_key(event: &UsageEvent) -> SyncRollupStatsBucketKey {
    SyncRollupStatsBucketKey {
        provider: event.provider.clone(),
        source_id: event.source_id.0.clone(),
        account_key: event
            .provider_account_id
            .as_ref()
            .map(|id| id.0.clone())
            .unwrap_or_else(|| "unlinked".to_string()),
        day_key: event.session.started_at.date_naive().to_string(),
    }
}

#[cfg(test)]
pub(crate) fn build_sync_rollup_stats_summaries(
    events: &[UsageEvent],
    device_id: &str,
) -> Vec<UsageSummary> {
    let mut buckets: BTreeMap<String, SyncRollupStatsAccumulator> = BTreeMap::new();
    for event in events {
        let key = sync_rollup_stats_bucket_key(event);
        let day = event.session.started_at.date_naive();
        let start = day
            .and_hms_opt(0, 0, 0)
            .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
            .unwrap_or(event.session.started_at);
        let end = start + chrono::Duration::days(1);
        let entry = buckets
            .entry(format!(
                "{}|{}|{}|{}",
                key.provider, key.source_id, key.account_key, key.day_key
            ))
            .or_insert_with(|| SyncRollupStatsAccumulator {
                provider: event.provider.clone(),
                source_id: event.source_id.clone(),
                provider_account_id: event.provider_account_id.clone(),
                source: event.source.clone(),
                period_start: start,
                period_end: end,
                observed_at: event.session.started_at,
                account_key: key.account_key.clone(),
                day_key: key.day_key.clone(),
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_creation_5m_tokens: 0,
                cache_creation_1h_tokens: 0,
                cache_read_tokens: 0,
                reasoning_tokens: 0,
                total_tokens: 0,
                events: 0,
                estimated_cost_micro_usd: 0,
            });
        entry.input_tokens = entry
            .input_tokens
            .saturating_add(event.usage.input_tokens.unwrap_or(0));
        entry.output_tokens = entry
            .output_tokens
            .saturating_add(event.usage.output_tokens.unwrap_or(0));
        entry.cache_creation_tokens = entry
            .cache_creation_tokens
            .saturating_add(event.usage.cache_creation_tokens.unwrap_or(0));
        entry.cache_creation_5m_tokens = entry
            .cache_creation_5m_tokens
            .saturating_add(event.usage.cache_creation_5m_tokens.unwrap_or(0));
        entry.cache_creation_1h_tokens = entry
            .cache_creation_1h_tokens
            .saturating_add(event.usage.cache_creation_1h_tokens.unwrap_or(0));
        entry.cache_read_tokens = entry
            .cache_read_tokens
            .saturating_add(event.usage.cache_read_tokens.unwrap_or(0));
        entry.reasoning_tokens = entry
            .reasoning_tokens
            .saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
        entry.total_tokens = entry
            .total_tokens
            .saturating_add(event.usage.computed_total());
        entry.events = entry.events.saturating_add(1);
        entry.estimated_cost_micro_usd = entry
            .estimated_cost_micro_usd
            .saturating_add(event.cost.estimated_micro_usd().unwrap_or(0));
        if event.session.started_at > entry.observed_at {
            entry.observed_at = event.session.started_at;
        }
    }

    buckets
        .into_values()
        .map(|bucket| UsageSummary {
            schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: summary_id(
                &bucket.provider,
                &bucket.source_id,
                &format!("daily_stats:{}:{}", bucket.day_key, bucket.account_key),
            ),
            device_id: device_id.to_string(),
            provider: bucket.provider,
            source_id: bucket.source_id,
            provider_account_id: bucket.provider_account_id,
            source: EventSource {
                source_record_id: None,
                ..bucket.source
            },
            model: None,
            models: Vec::new(),
            usage: UsageCounts {
                input_tokens: Some(bucket.input_tokens),
                output_tokens: Some(bucket.output_tokens),
                cache_creation_tokens: Some(bucket.cache_creation_tokens),
                cache_creation_5m_tokens: Some(bucket.cache_creation_5m_tokens),
                cache_creation_1h_tokens: Some(bucket.cache_creation_1h_tokens),
                cache_read_tokens: Some(bucket.cache_read_tokens),
                reasoning_tokens: Some(bucket.reasoning_tokens),
                total_tokens: Some(bucket.total_tokens),
                requests: Some(bucket.events),
                local_prompt_eval_tokens: None,
                local_eval_tokens: None,
            },
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: Some(micro_usd_to_cents_rounded(
                    bucket.estimated_cost_micro_usd,
                )),
                provider_reported_usd: None,
                estimated_api_equivalent_micro_usd: Some(bucket.estimated_cost_micro_usd),
                provider_reported_micro_usd: None,
                pricing_source: Some("local_rollup".to_string()),
                pricing_version: None,
                confidence: Confidence::Medium,
            },
            parse_evidence: None,
            project: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            metrics: None,
            period_start: Some(bucket.period_start),
            period_end: Some(bucket.period_end),
            observed_at: bucket.observed_at,
            metadata: SummaryMetadata {
                summary_format: "daily_rollup.v1".to_string(),
                summary_version: Some("6".to_string()),
                total_sessions: None,
                total_messages: None,
                last_computed_at: Some(Utc::now()),
            },
            imported_at: Utc::now(),
        })
        .collect()
}

pub(crate) fn sanitize_summary_for_sync(summary: UsageSummary) -> UsageSummary {
    statsai_core::sanitize_summary_for_sync(summary)
}

pub(crate) fn sanitize_summary_for_sync_with_projects(
    summary: UsageSummary,
    include_projects: bool,
) -> UsageSummary {
    let mut summary = sanitize_summary_for_sync(summary);
    if !include_projects {
        summary.project = None;
    }
    summary
}

pub(crate) fn is_daily_rollup_summary(summary: &UsageSummary) -> bool {
    summary.metadata.summary_format == "daily_rollup.v1"
}

fn summary_spans_single_day(summary: &UsageSummary) -> bool {
    let start = summary
        .period_start
        .as_ref()
        .or(summary.period_end.as_ref())
        .unwrap_or(&summary.observed_at);
    let end = summary
        .period_end
        .as_ref()
        .or(summary.period_start.as_ref())
        .unwrap_or(&summary.observed_at);
    start.date_naive() == end.date_naive()
}

fn summary_fits_single_daily_report_day(summary: &UsageSummary) -> bool {
    let start = summary
        .period_start
        .as_ref()
        .or(summary.period_end.as_ref())
        .unwrap_or(&summary.observed_at);
    let end = summary
        .period_end
        .as_ref()
        .or(summary.period_start.as_ref())
        .unwrap_or(&summary.observed_at);
    if start.date_naive() == end.date_naive() {
        return true;
    }
    let duration = *end - *start;
    duration >= Duration::zero() && duration <= Duration::hours(25)
}

fn is_exact_daily_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "external_daily" | "manual_daily" | "custom_daily" | "ccusage_daily"
    )
}

fn is_exact_period_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "manual_period_summary" | "custom_period_summary"
    )
}

pub(crate) fn is_http_rollup_passthrough_summary(summary: &UsageSummary) -> bool {
    if is_daily_rollup_summary(summary) {
        return false;
    }
    if summary.metadata.summary_format == "claude_stats_cache" {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalSummary {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalAdapter {
        return true;
    }
    (is_exact_daily_passthrough_summary(summary) && summary_fits_single_daily_report_day(summary))
        || (is_exact_period_passthrough_summary(summary) && !summary_spans_single_day(summary))
}

pub(crate) fn sanitize_subscription_for_sync(mut subscription: Subscription) -> Subscription {
    subscription.notes = None;
    subscription
}
