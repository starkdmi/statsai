use anyhow::{bail, Context, Result};
use chrono::DateTime;
use serde::Serialize;
use serde_json::{json, Value};
use statsai_core::SyncBatch;
use statsai_store::{Store, SyncPreferences, SyncState};
use statsai_sync::{FileSink, StdoutSink, SyncSink};
use std::path::PathBuf;

use super::args::SyncCommand;
use super::format::format_cursor;

mod batch;
mod chunking;
mod http;

pub(crate) use batch::*;
#[cfg(test)]
pub(crate) use chunking::*;
pub(crate) use http::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncPayloadMode {
    Raw,
    Rollups,
}

pub(crate) fn effective_sync_preferences(
    store: &Store,
    command: &SyncCommand,
) -> Result<SyncPreferences> {
    let mut preferences = store.sync_preferences()?;
    if command.include_projects {
        preferences.include_projects = true;
    }
    if command.exclude_projects {
        preferences.include_projects = false;
        preferences.include_tasks = false;
    }
    if command.include_tasks {
        preferences.include_projects = true;
        preferences.include_tasks = true;
    }
    if command.exclude_tasks {
        preferences.include_tasks = false;
    }

    Ok(preferences.normalized())
}

fn apply_sync_preference_overrides(
    store: &Store,
    command: &SyncCommand,
) -> Result<SyncPreferences> {
    let original = store.sync_preferences()?;
    let preferences = effective_sync_preferences(store, command)?;
    if preferences != original {
        store.set_sync_preferences(preferences)?;
        eprintln!(
            "sync preferences updated: projects={} tasks={}",
            if preferences.include_projects {
                "enabled"
            } else {
                "disabled"
            },
            if preferences.include_tasks {
                "enabled"
            } else {
                "disabled"
            }
        );
        if (!original.include_projects && preferences.include_projects)
            || (!original.include_tasks && preferences.include_tasks)
        {
            eprintln!(
                "sync preferences changed privacy/backfill scope; the next sync may resend historical summaries to update the hosted mirror"
            );
        }
    }
    Ok(preferences)
}

pub(crate) fn sync(command: SyncCommand, store: &Store, device_id: &str) -> Result<()> {
    if command.since_last && (command.full || command.rebuild_rollups) {
        bail!("--since-last cannot be combined with --full or --rebuild-rollups");
    }

    let sync_preferences = effective_sync_preferences(store, &command)?;

    if command.reset_remote {
        if command.status || command.verify {
            bail!("--reset-remote cannot be combined with --status or --verify");
        }
        return sync_remote_reset(command, store);
    }

    if command.verify {
        return sync_verify(command, store, device_id);
    }

    if command.status {
        return sync_status(store, device_id);
    }

    let target = sync_target(&command)?;
    let mut cleared_tracking = None;
    let http_preflight = if command.sink == "http" && !command.dry_run {
        let preflight = load_http_sync_preflight(&command, &target)?;
        cleared_tracking = maybe_reset_http_sync_tracking_if_remote_changed(
            &command,
            store,
            &target,
            preflight.remote.as_ref(),
        )?;
        Some(preflight)
    } else {
        None
    };
    let outcome = (|| -> Result<()> {
        let code_change_identity_key = http_preflight
            .as_ref()
            .and_then(|preflight| preflight.remote.as_ref())
            .map(remote_code_change_identity_key)
            .transpose()?
            .flatten();
        let (mut batch, payload_mode) = build_sync_batch_with_identity_key(
            &command,
            store,
            device_id,
            &target,
            code_change_identity_key.as_ref(),
        )?;
        let hosted_task_sync_enabled = maybe_disable_http_hosted_task_sync_payload(
            &command,
            sync_preferences,
            http_preflight
                .as_ref()
                .and_then(|preflight| preflight.remote.as_ref()),
            &mut batch,
        )?;
        if let Some(warning) = code_change_dedup_warning(
            &command.sink,
            code_change_identity_key.is_some(),
            &batch.code_change_metrics,
        ) {
            eprintln!("{warning}");
        }

        if command.dry_run {
            eprintln!(
            "dry run: sink={} mode={} include_projects={} include_tasks={} sources={} events={} summaries={} task_buckets={} task_verifications={}",
            command.sink,
            sync_payload_mode_name(payload_mode),
            sync_preferences.include_projects,
            sync_preferences.include_tasks,
            batch.sources.len(),
            batch.events.len(),
            batch.summaries.len()
            ,
            batch.task_buckets.len(),
            batch.task_verifications.len(),
        );
            return Ok(());
        }

        let persisted_sync_preferences = apply_sync_preference_overrides(store, &command)?;
        debug_assert_eq!(persisted_sync_preferences, sync_preferences);

        let result = (|| -> Result<()> {
            match command.sink.as_str() {
                "stdout" => {
                    StdoutSink.send(&batch)?;
                    record_sync_batch_success(store, &command.sink, &target, &batch)?;
                    Ok(())
                }
                "file" => {
                    let output = command
                        .output
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("statsai-sync-batch.json"));
                    FileSink::new(output).send(&batch)?;
                    record_sync_batch_success(store, &command.sink, &target, &batch)?;
                    Ok(())
                }
                "http" => {
                    let endpoint = http_sync_endpoint(&command)?;
                    let auth_token = http_preflight
                        .as_ref()
                        .and_then(|preflight| preflight.auth_token.clone());
                    send_http_sync_batch(
                        store,
                        HttpSyncBatchRequest {
                            sink: &command.sink,
                            target: &target,
                            endpoint: &endpoint,
                            auth_token,
                            payload_mode,
                            hosted_task_sync_enabled,
                        },
                        &batch,
                    )?;
                    Ok(())
                }
                other => bail!("unsupported sync sink {other}"),
            }
        })();

        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                // The cursor goes back before the failure is recorded, not after:
                // `restore_sync_states` will not move a cursor backwards and
                // `record_sync_failure` stamps a newer `last_success_at`, so restoring
                // second would silently do nothing.
                restore_cleared_sync_tracking(store, &cleared_tracking);
                let _ = store.record_sync_failure(&command.sink, &target);
                Err(error)
            }
        }
    })();

    match outcome {
        Ok(()) => Ok(()),
        Err(error) => {
            // Everything between clearing the cursor and sending can fail too --
            // building the batch, resolving the identity key, persisting preference
            // overrides -- and those return without recording a sync failure. The
            // cursor still has to come back, or the next ordinary sync sees nothing
            // to protect and re-uploads the whole history unasked. A send failure has
            // already restored it above; repeating that here is a no-op, because the
            // recorded failure left a newer row.
            restore_cleared_sync_tracking(store, &cleared_tracking);
            Err(error)
        }
    }
}

/// Puts back a cursor that `--full` discarded, when the upload it was discarded for
/// did not get far enough to establish new progress.
fn restore_cleared_sync_tracking(store: &Store, cleared: &Option<SyncState>) {
    if let Some(state) = cleared {
        let _ = store.restore_sync_states(std::slice::from_ref(state));
    }
}

/// Returns the cursor this cleared, so a failed opted-in run can put it back.
fn maybe_reset_http_sync_tracking_if_remote_changed(
    command: &SyncCommand,
    store: &Store,
    target: &str,
    remote: Option<&Value>,
) -> Result<Option<SyncState>> {
    let Some(local_state) = store.sync_state("http", target)? else {
        return Ok(None);
    };
    if local_state.last_batch_id.trim().is_empty() {
        return Ok(None);
    }

    let Some(remote) = remote else {
        return Ok(None);
    };
    let sync_preferences = effective_sync_preferences(store, command)?;
    let local_verify = sync_local_verify(
        store,
        "http",
        target,
        Some(&local_state),
        sync_preferences.include_projects,
    )?;
    let batch_mismatch = !remote_sync_batch_matches_local_state(remote, &local_state);
    let metadata_gap = remote_metadata_gap_reason(remote, &local_verify);
    if batch_mismatch || metadata_gap.is_some() {
        let remote_last_batch = remote_last_sync_batch_id(remote).unwrap_or("none");
        let mut reasons = Vec::new();
        if batch_mismatch {
            reasons.push(format!(
                "remote last batch ({remote_last_batch}) no longer matches local last batch ({})",
                local_state.last_batch_id
            ));
        }
        if let Some(gap) = metadata_gap {
            reasons.push(format!("remote mirror is missing synced metadata ({gap})"));
        }
        // Reaching here means a cursor exists and is about to be discarded, which
        // turns the next sync into a full re-upload of the account. That is an
        // expensive, mostly-redundant operation to start from a command that asked
        // for an incremental one, and `--dry-run` cannot warn about it because the
        // mismatch is only visible once the server has answered. Whatever caused the
        // divergence, the decision to re-upload everything is the caller's.
        if !command.full {
            bail!(
                "refusing to re-upload this device's full history: {}.\n\
                 The local sync cursor for {} would have to be discarded, and every \
                 summary re-sent. Rerun with --full to do that deliberately.",
                reasons.join("; "),
                target
            );
        }
        eprintln!(
            "http rollup mode: {}; clearing local sync tracking for target {}",
            reasons.join("; "),
            target
        );
        store.clear_sync_tracking_for_target("http", target)?;
        return Ok(Some(local_state));
    }

    Ok(None)
}

fn sync_remote_reset(command: SyncCommand, store: &Store) -> Result<()> {
    if command.sink != "http" {
        bail!("--reset-remote is currently supported only with --sink http");
    }

    let endpoint = http_sync_endpoint(&command)?;
    if command.dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "sink": command.sink,
                "target": endpoint,
                "endpoint": endpoint,
                "would_reset_remote_sync_data": true,
                "remote_reset_scope": "paired_device",
                "would_clear_local_sync_tracking": true,
                "dry_run": true,
            }))?
        );
        return Ok(());
    }

    if !command.yes {
        bail!(
            "--reset-remote deletes mirrored hosted sync data for this paired device; rerun with --yes"
        );
    }

    eprintln!(
        "warning: --reset-remote deletes mirrored hosted sync data for this paired device. Other paired devices are not affected."
    );

    let auth_token = resolve_http_auth_token(&command, true)?
        .context("device login required; run `statsai auth login` first")?;
    let remote = http_remote_reset(&endpoint, &auth_token)?;
    ensure_device_remote_reset_response(&remote)?;
    store.clear_sync_tracking()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "sink": command.sink,
            "target": endpoint,
            "endpoint": endpoint,
            "cleared_local_sync_tracking": true,
            "remote": remote,
        }))?
    );
    Ok(())
}

pub(crate) fn ensure_device_remote_reset_response(response: &Value) -> Result<()> {
    if response.get("ok").and_then(Value::as_bool) != Some(true)
        || response.get("scope").and_then(Value::as_str) != Some("device_mirror")
        || response.get("device_id").and_then(Value::as_str).is_none()
    {
        bail!("remote reset returned an unexpected scope; local sync tracking was not cleared");
    }
    Ok(())
}

fn maybe_disable_http_hosted_task_sync_payload(
    command: &SyncCommand,
    sync_preferences: SyncPreferences,
    remote: Option<&Value>,
    batch: &mut SyncBatch,
) -> Result<bool> {
    if command.sink != "http" {
        return Ok(sync_preferences.include_tasks);
    }
    if !sync_preferences.include_tasks {
        return Ok(false);
    }
    if remote.is_none_or(remote_hosted_tasks_enabled) {
        return Ok(true);
    }
    if batch.task_buckets.is_empty() && batch.task_verifications.is_empty() {
        return Ok(false);
    }
    eprintln!(
        "http sync: hosted task access is not enabled for this account; skipping {} task buckets and {} task verifications",
        batch.task_buckets.len(),
        batch.task_verifications.len()
    );
    batch.task_buckets.clear();
    batch.task_verifications.clear();
    Ok(false)
}

fn sync_status(store: &Store, device_id: &str) -> Result<()> {
    let sync_preferences = store.sync_preferences()?;
    println!(
        "preferences projects={} tasks={}",
        if sync_preferences.include_projects {
            "enabled"
        } else {
            "disabled"
        },
        if sync_preferences.include_tasks {
            "enabled"
        } else {
            "disabled"
        }
    );
    let states = store.list_sync_states()?;
    if states.is_empty() {
        println!("no sync state recorded");
        return Ok(());
    }
    for state in states {
        let display_batch_id = logical_http_rollup_batch_id(&state.last_batch_id);
        let task_bucket_status =
            store.task_bucket_sync_status(&state.sink, &state.target, device_id)?;
        println!(
            "{} target={} last_success={} batch={} event_cursor={} summary_cursor={} task_verification_cursor={} task_bucket_backlog={}/{} failures={}",
            state.sink,
            state.target,
            state.last_success_at.to_rfc3339(),
            display_batch_id,
            format_cursor(
                state
                    .last_event_started_at
                    .as_ref()
                    .map(DateTime::to_rfc3339),
                state.last_event_id.as_deref()
            ),
            format_cursor(
                state
                    .last_summary_observed_at
                    .as_ref()
                    .map(DateTime::to_rfc3339),
                state.last_summary_id.as_deref()
            ),
            format_cursor(
                state
                    .last_task_verification_updated_at
                    .as_ref()
                    .map(DateTime::to_rfc3339),
                state.last_task_verification_id.as_deref()
            ),
            task_bucket_status.dirty,
            task_bucket_status.total,
            state.failure_count
        );
    }
    Ok(())
}

fn sync_verify(command: SyncCommand, store: &Store, device_id: &str) -> Result<()> {
    if command.sink != "http" {
        bail!("--verify is currently supported only with --sink http");
    }
    sync_http_verify(command, store, device_id)
}

fn sync_http_verify(command: SyncCommand, store: &Store, device_id: &str) -> Result<()> {
    let endpoint = http_sync_endpoint(&command)?;
    let local_state = store.sync_state("http", &endpoint)?;
    let sync_preferences = effective_sync_preferences(store, &command)?;
    let auth_token = resolve_http_auth_token(&command, true)?
        .context("device login required; run `statsai auth login` first")?;
    let report = HttpVerifyReport {
        sink: command.sink,
        target: endpoint.clone(),
        endpoint: endpoint.clone(),
        device_id: device_id.to_string(),
        local: sync_local_verify(
            store,
            "http",
            &endpoint,
            local_state.as_ref(),
            sync_preferences.include_projects,
        )?,
        remote: http_remote_verify(&endpoint, &auth_token)?,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

pub(crate) fn sync_local_verify(
    store: &Store,
    sink: &str,
    target: &str,
    local_state: Option<&SyncState>,
    include_projects: bool,
) -> Result<SyncLocalVerify> {
    let all_sources = store.list_sources()?;
    let all_accounts = store.list_accounts()?;
    let all_source_account_assignments = store.list_source_account_assignments()?;
    let all_subscriptions = store.list_subscriptions()?;
    let sync_sources: Vec<_> = all_sources
        .iter()
        .cloned()
        .map(sanitize_source_for_sync)
        .collect();
    let sync_accounts: Vec<_> = all_accounts
        .iter()
        .cloned()
        .map(sanitize_account_for_sync)
        .collect();
    let sync_source_account_assignments: Vec<_> = all_source_account_assignments
        .iter()
        .cloned()
        .map(sanitize_source_account_assignment_for_sync)
        .collect();
    let sync_subscriptions: Vec<_> = all_subscriptions
        .iter()
        .cloned()
        .map(sanitize_subscription_for_sync)
        .collect();
    let passthrough_summaries: Vec<_> = store
        .summaries()?
        .into_iter()
        .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects))
        .filter(is_http_rollup_passthrough_summary)
        .collect();
    let rollup_summaries: Vec<_> = store
        .all_sync_rollup_summaries()?
        .into_iter()
        .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects))
        .collect();

    Ok(SyncLocalVerify {
        sync_state: local_state.map(sync_state_report),
        total_sources: all_sources.len(),
        enabled_sources: all_sources.iter().filter(|source| source.enabled).count(),
        pending_sources: store
            .pending_sources_for_sync(sink, target, &sync_sources)?
            .len(),
        total_accounts: all_accounts.len(),
        pending_accounts: store
            .pending_accounts_for_sync(sink, target, &sync_accounts)?
            .len(),
        total_source_account_assignments: all_source_account_assignments.len(),
        pending_source_account_assignments: store
            .pending_source_account_assignments_for_sync(
                sink,
                target,
                &sync_source_account_assignments,
            )?
            .len(),
        total_subscriptions: all_subscriptions.len(),
        pending_subscriptions: store
            .pending_subscriptions_for_sync(sink, target, &sync_subscriptions)?
            .len(),
        total_passthrough_summaries: passthrough_summaries.len(),
        pending_passthrough_summaries: store
            .pending_summaries_for_sync(sink, target, &passthrough_summaries)?
            .len(),
        total_rollups: rollup_summaries.len(),
        pending_rollups: store
            .pending_summaries_for_sync(sink, target, &rollup_summaries)?
            .len(),
        dirty_rollups: store.dirty_sync_rollup_summaries()?.len(),
    })
}

pub(crate) fn sync_target(command: &SyncCommand) -> Result<String> {
    match command.sink.as_str() {
        "http" => http_sync_endpoint(command),
        "file" => Ok(command
            .output
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "statsai-sync-batch.json".to_string())),
        other => Ok(other.to_string()),
    }
}

pub(crate) fn sync_payload_mode(command: &SyncCommand) -> Result<SyncPayloadMode> {
    match command.sink.as_str() {
        "http" => Ok(SyncPayloadMode::Rollups),
        _ => Ok(SyncPayloadMode::Raw),
    }
}

fn sync_payload_mode_name(mode: SyncPayloadMode) -> &'static str {
    match mode {
        SyncPayloadMode::Raw => "raw",
        SyncPayloadMode::Rollups => "rollups",
    }
}

pub(crate) fn rollup_mode_label(command: &SyncCommand) -> &'static str {
    let _ = command;
    "http rollup mode"
}

#[derive(Debug, Serialize)]
struct HttpVerifyReport {
    sink: String,
    target: String,
    endpoint: String,
    device_id: String,
    local: SyncLocalVerify,
    remote: Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct SyncLocalVerify {
    pub(crate) sync_state: Option<SyncStateReport>,
    pub(crate) total_sources: usize,
    pub(crate) enabled_sources: usize,
    pub(crate) pending_sources: usize,
    pub(crate) total_accounts: usize,
    pub(crate) pending_accounts: usize,
    pub(crate) total_source_account_assignments: usize,
    pub(crate) pending_source_account_assignments: usize,
    pub(crate) total_subscriptions: usize,
    pub(crate) pending_subscriptions: usize,
    pub(crate) total_passthrough_summaries: usize,
    pub(crate) pending_passthrough_summaries: usize,
    pub(crate) total_rollups: usize,
    pub(crate) pending_rollups: usize,
    pub(crate) dirty_rollups: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct SyncStateReport {
    last_success_at: String,
    last_batch_id: String,
    event_cursor: String,
    summary_cursor: String,
    failure_count: u64,
}

/// Warns when committed code-change metrics cannot deduplicate across devices.
///
/// Without the user-scoped identity key from the HTTP preflight, each device
/// mints its own opaque ID for the same commit, so a shared backend would count
/// that commit once per device.
pub(crate) fn code_change_dedup_warning(
    sink: &str,
    has_identity_key: bool,
    metrics: &[statsai_core::CodeChangeMetric],
) -> Option<&'static str> {
    if sink != "http" || has_identity_key {
        return None;
    }
    metrics
        .iter()
        .any(|metric| metric.kind == statsai_core::CodeChangeMetricKind::Committed)
        .then_some(
            "warning: this sync endpoint did not provide a code-change identity key; \
             committed code-change metrics cannot be deduplicated across your devices",
        )
}

pub(crate) fn remote_sync_batch_matches_local_state(
    remote: &Value,
    local_state: &statsai_store::SyncState,
) -> bool {
    remote_last_sync_batch_id(remote)
        .map(|batch_id| {
            logical_http_rollup_batch_id(batch_id)
                == logical_http_rollup_batch_id(&local_state.last_batch_id)
        })
        .unwrap_or(false)
}

pub(crate) fn remote_metadata_gap_reason(
    remote: &Value,
    local: &SyncLocalVerify,
) -> Option<String> {
    let mut reasons = Vec::new();
    push_remote_metadata_gap(
        &mut reasons,
        "sources",
        remote
            .pointer("/mirrorCounts/sources")
            .and_then(Value::as_u64),
        local.total_sources,
        local.pending_sources,
    );
    push_remote_metadata_gap(
        &mut reasons,
        "accounts",
        remote
            .pointer("/mirrorCounts/accounts")
            .and_then(Value::as_u64),
        local.total_accounts,
        local.pending_accounts,
    );
    push_remote_metadata_gap(
        &mut reasons,
        "source_account_assignments",
        remote
            .pointer("/mirrorCounts/source_account_assignments")
            .and_then(Value::as_u64),
        local.total_source_account_assignments,
        local.pending_source_account_assignments,
    );
    push_remote_metadata_gap(
        &mut reasons,
        "subscriptions",
        remote
            .pointer("/mirrorCounts/subscriptions")
            .and_then(Value::as_u64),
        local.total_subscriptions,
        local.pending_subscriptions,
    );

    if reasons.is_empty() {
        None
    } else {
        Some(reasons.join(", "))
    }
}

fn push_remote_metadata_gap(
    reasons: &mut Vec<String>,
    label: &str,
    remote_count: Option<u64>,
    local_total: usize,
    local_pending: usize,
) {
    if local_pending > 0 {
        return;
    }
    if let Some(remote_count) = remote_count {
        if remote_count != local_total as u64 {
            reasons.push(format!("{label} {remote_count}!={local_total}"));
        }
    }
}

fn sync_state_report(state: &statsai_store::SyncState) -> SyncStateReport {
    SyncStateReport {
        last_success_at: state.last_success_at.to_rfc3339(),
        last_batch_id: logical_http_rollup_batch_id(&state.last_batch_id),
        event_cursor: format_cursor(
            state
                .last_event_started_at
                .as_ref()
                .map(DateTime::to_rfc3339),
            state.last_event_id.as_deref(),
        ),
        summary_cursor: format_cursor(
            state
                .last_summary_observed_at
                .as_ref()
                .map(DateTime::to_rfc3339),
            state.last_summary_id.as_deref(),
        ),
        failure_count: state.failure_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use statsai_store::SyncState;

    fn sync_command(full: bool) -> SyncCommand {
        SyncCommand {
            sink: "http".to_string(),
            output: None,
            endpoint: None,
            auth_token: None,
            rebuild_rollups: false,
            full,
            since_last: false,
            status: false,
            verify: false,
            reset_remote: false,
            yes: false,
            dry_run: false,
            include_projects: false,
            exclude_projects: false,
            include_tasks: false,
            exclude_tasks: false,
        }
    }

    fn store_with_cursor(path: &std::path::Path, target: &str) -> Store {
        let store = Store::open(path).expect("open store");
        store
            .restore_sync_states(&[SyncState {
                sink: "http".to_string(),
                target: target.to_string(),
                last_success_at: Utc::now(),
                last_batch_id: "batch-local".to_string(),
                last_event_started_at: None,
                last_event_id: None,
                last_summary_observed_at: None,
                last_summary_id: None,
                last_task_verification_updated_at: None,
                last_task_verification_id: None,
                failure_count: 0,
                pending_resume_batch_id: None,
            }])
            .expect("seed cursor");
        store
    }

    #[test]
    fn a_diverged_remote_does_not_silently_re_upload_everything() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = "https://api.example.test/api/sync/batches";
        let store = store_with_cursor(&directory.path().join("store.sqlite"), target);
        let remote = json!({ "device": { "last_sync_batch_id": "batch-remote-ahead" } });

        let error = maybe_reset_http_sync_tracking_if_remote_changed(
            &sync_command(false),
            &store,
            target,
            Some(&remote),
        )
        .expect_err("a diverged remote must not clear tracking on its own");

        assert!(error.to_string().contains("--full"));
        assert!(
            store
                .sync_state("http", target)
                .expect("read cursor")
                .is_some(),
            "the cursor must survive so the sync can be retried after the cause is understood"
        );
    }

    #[test]
    fn a_failed_opted_in_upload_leaves_the_guard_armed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = "https://api.example.test/api/sync/batches";
        let store = store_with_cursor(&directory.path().join("store.sqlite"), target);
        let remote = json!({ "device": { "last_sync_batch_id": "batch-remote-ahead" } });

        let cleared = maybe_reset_http_sync_tracking_if_remote_changed(
            &sync_command(true),
            &store,
            target,
            Some(&remote),
        )
        .expect("--full clears tracking")
        .expect("the cleared cursor is handed back");

        // What `sync` does when the opted-in upload fails, in the order it does it.
        store
            .restore_sync_states(std::slice::from_ref(&cleared))
            .expect("restore the cursor");
        store
            .record_sync_failure("http", target)
            .expect("record the failure");

        let restored = store
            .sync_state("http", target)
            .expect("read cursor")
            .expect("cursor exists again");
        assert_eq!(
            restored.last_batch_id, "batch-local",
            "recording the failure must not blank the restored cursor"
        );

        // A later ordinary sync therefore still meets the guard rather than sliding
        // past it into an unrequested full upload.
        let error = maybe_reset_http_sync_tracking_if_remote_changed(
            &sync_command(false),
            &store,
            target,
            Some(&remote),
        )
        .expect_err("the guard must still fire after a failed opted-in run");
        assert!(error.to_string().contains("--full"));
    }

    #[test]
    fn a_failure_before_sending_also_puts_the_cursor_back() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = "https://api.example.test/api/sync/batches";
        let store = store_with_cursor(&directory.path().join("store.sqlite"), target);
        let remote = json!({ "device": { "last_sync_batch_id": "batch-remote-ahead" } });

        let cleared = maybe_reset_http_sync_tracking_if_remote_changed(
            &sync_command(true),
            &store,
            target,
            Some(&remote),
        )
        .expect("--full clears tracking")
        .expect("the cleared cursor is handed back");
        assert!(
            store
                .sync_state("http", target)
                .expect("read cursor")
                .is_none(),
            "the clear really removed it"
        );

        // Building the batch, resolving the identity key and persisting preference
        // overrides all fail without recording a sync failure, so this is the only
        // thing that puts the cursor back on those paths.
        restore_cleared_sync_tracking(&store, &Some(cleared));

        assert_eq!(
            store
                .sync_state("http", target)
                .expect("read cursor")
                .expect("cursor exists again")
                .last_batch_id,
            "batch-local"
        );
        let error = maybe_reset_http_sync_tracking_if_remote_changed(
            &sync_command(false),
            &store,
            target,
            Some(&remote),
        )
        .expect_err("the guard must still fire");
        assert!(error.to_string().contains("--full"));
    }

    #[test]
    fn full_opts_in_to_re_uploading_everything() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = "https://api.example.test/api/sync/batches";
        let store = store_with_cursor(&directory.path().join("store.sqlite"), target);
        let remote = json!({ "device": { "last_sync_batch_id": "batch-remote-ahead" } });

        maybe_reset_http_sync_tracking_if_remote_changed(
            &sync_command(true),
            &store,
            target,
            Some(&remote),
        )
        .expect("--full clears tracking deliberately");

        assert!(
            store
                .sync_state("http", target)
                .expect("read cursor")
                .is_none(),
            "--full is the caller asking for the full re-upload"
        );
    }
}
