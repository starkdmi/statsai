use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use statsai::auth;
use statsai::snapshot;
use statsai_core::{SyncBatch, TaskVerification, TaskVerificationCursor};
use statsai_store::Store;
use statsai_sync::{validate_authenticated_http_endpoint, HttpSink};
use std::collections::BTreeSet;
use std::time::Duration as StdDuration;

use super::super::args::SyncCommand;
use super::batch::{record_rollup_sync_chunk_success, record_sync_batch_success};
use super::chunking::{
    has_non_code_change_payload, has_non_quota_cycle_payload, http_rollup_metadata_count,
    split_http_rollup_sync_batch_after_budget_error, split_http_rollup_sync_batches,
    HTTP_ROLLUP_SUMMARIES_PER_BATCH,
};
use super::SyncPayloadMode;

/// Times a chunk is resent after a transient endpoint failure before the run
/// gives up. A restarted worker recovers within seconds, so a handful of
/// attempts covers it without leaving a stalled sync running indefinitely.
const HTTP_ROLLUP_TRANSIENT_RESEND_ATTEMPTS: u32 = 3;

/// Delay before the first resend. Each further attempt doubles it.
const HTTP_ROLLUP_TRANSIENT_RESEND_DELAY: StdDuration = StdDuration::from_secs(1);

const HTTP_REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(30);

pub(crate) struct HttpSyncBatchRequest<'a> {
    pub(crate) sink: &'a str,
    pub(crate) target: &'a str,
    pub(crate) endpoint: &'a str,
    pub(crate) auth_token: Option<String>,
    pub(crate) payload_mode: SyncPayloadMode,
    pub(crate) hosted_task_sync_enabled: bool,
}

pub(crate) fn send_http_sync_batch(
    store: &Store,
    request: HttpSyncBatchRequest<'_>,
    batch: &SyncBatch,
) -> Result<()> {
    let task_sync_auth_token = request.auth_token.clone();
    let http_sink = HttpSink::new(request.endpoint, request.auth_token)?;
    let batches = if request.payload_mode == SyncPayloadMode::Rollups {
        split_http_rollup_sync_batches(batch)
    } else {
        vec![batch.clone()]
    };
    let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();

    if batches.len() > 1 {
        eprintln!(
            "http rollup mode: split sync into {} batches of at most {} summaries",
            batches.len(),
            HTTP_ROLLUP_SUMMARIES_PER_BATCH
        );
    }

    for (index, chunk) in batches.iter().enumerate() {
        if batches.len() > 1 {
            eprintln!(
                "http rollup mode: sending batch {}/{} ({})",
                index + 1,
                batches.len(),
                chunk.batch_id
            );
        }
        if request.payload_mode == SyncPayloadMode::Rollups {
            send_http_rollup_chunk_with_retry(&http_sink, chunk, &|synced_chunk| {
                record_rollup_sync_chunk_success(
                    store,
                    request.sink,
                    request.target,
                    &logical_batch_id,
                    synced_chunk,
                )
            })?;
        } else {
            let ack = http_sink.send_with_ack(chunk)?;
            println!("{}", serde_json::to_string_pretty(&ack)?);
            record_sync_batch_success(store, request.sink, request.target, batch)?;
        }
    }
    if request.payload_mode == SyncPayloadMode::Rollups {
        if let Some(snapshot) = batch.authoritative_snapshot.as_ref() {
            store.reconcile_sync_tracking_to_authoritative_snapshot(
                request.sink,
                request.target,
                snapshot,
            )?;
        }
        store.clear_pending_sync_resume(request.sink, request.target)?;
    }
    if request.hosted_task_sync_enabled {
        match pull_remote_task_verifications(
            store,
            request.sink,
            request.target,
            request.endpoint,
            task_sync_auth_token.as_deref(),
        ) {
            Ok(Some(cursor)) => {
                store.record_sync_success(
                    request.sink,
                    request.target,
                    &batch.batch_id,
                    &[],
                    &[],
                    Some(&cursor),
                )?;
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!(
                    "warning: sync upload succeeded, but pulling hosted task verifications failed: {error}"
                );
            }
        }
    }
    Ok(())
}

fn pull_remote_task_verifications(
    store: &Store,
    sink: &str,
    target: &str,
    endpoint: &str,
    auth_token: Option<&str>,
) -> Result<Option<TaskVerificationCursor>> {
    let Some(auth_token) = auth_token.filter(|token| !token.is_empty()) else {
        return Ok(None);
    };
    validate_authenticated_http_endpoint(endpoint)?;
    let Some(feed_url) = http_task_verification_feed_url(endpoint) else {
        return Ok(None);
    };
    let mut request = ureq::get(&feed_url)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .set("Authorization", &format!("Bearer {auth_token}"));
    if let Some(cursor) = store.sync_task_verification_cursor(sink, target)? {
        request = request
            .query("updatedAt", &cursor.updated_at.to_rfc3339())
            .query("verificationId", &cursor.verification_id.0);
    }
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) if optional_task_verification_feed_status(code) => {
            return Ok(None);
        }
        Err(error) => return Err(http_request_error("pull task verifications", error)),
    };
    let feed: TaskVerificationFeedResponse = response
        .into_json()
        .context("parse task verification feed")?;
    let mut affected_buckets = BTreeSet::new();
    for verification in &feed.verifications {
        if store.merge_task_verification(verification)? {
            affected_buckets.extend(store.project_buckets_for_task_verification(verification)?);
        }
    }
    store.record_task_verifications_synced(sink, target, &feed.verifications)?;
    if !affected_buckets.is_empty() {
        store.rebuild_task_work_items_for_project_buckets(&affected_buckets)?;
        snapshot::invalidate_dashboard_cache();
    }
    Ok(feed.next_cursor)
}

pub(crate) fn optional_task_verification_feed_status(status: u16) -> bool {
    matches!(status, 404 | 405 | 501)
}

fn send_http_rollup_chunk_with_retry<F>(
    http_sink: &HttpSink,
    chunk: &SyncBatch,
    on_success: &F,
) -> Result<()>
where
    F: Fn(&SyncBatch) -> Result<()>,
{
    send_http_rollup_chunk_with_retry_using(chunk, &|chunk| {
        let ack = http_sink.send_with_ack(chunk)?;
        println!("{}", serde_json::to_string_pretty(&ack)?);
        on_success(chunk)?;
        Ok(())
    })
}

pub(crate) fn send_http_rollup_chunk_with_retry_using<F>(
    chunk: &SyncBatch,
    send_chunk: &F,
) -> Result<()>
where
    F: Fn(&SyncBatch) -> Result<()>,
{
    send_http_rollup_chunk_with_retry_using_sleep(chunk, send_chunk, &std::thread::sleep)
}

/// Sends one chunk, resending it through transient failures and splitting it
/// when the endpoint rejects its size.
///
/// The two remedies are deliberately separate. A rejected size is a decision
/// about this batch and is answered by sending less; a transient failure is the
/// absence of a decision and is answered by sending the same thing again.
/// Applying either remedy to the other's failure would be wrong: splitting on a
/// restarted worker multiplies the requests that just failed, and resending an
/// oversized batch unchanged can only be rejected again.
///
/// `sleep` is injected so tests exercise the backoff without waiting through it.
pub(crate) fn send_http_rollup_chunk_with_retry_using_sleep<F, S>(
    chunk: &SyncBatch,
    send_chunk: &F,
    sleep: &S,
) -> Result<()>
where
    F: Fn(&SyncBatch) -> Result<()>,
    S: Fn(StdDuration),
{
    let mut resends = 0_u32;
    loop {
        match send_chunk(chunk) {
            Ok(()) => return Ok(()),
            // The endpoint never reached a decision about this batch, so the
            // batch was neither accepted nor rejected; only the answer was
            // lost. Ingest records the batch ID and applies the payload in one
            // transaction, so resending the identical chunk either applies it
            // exactly once or is acknowledged as a duplicate. Failing here
            // instead would strand every batch still queued behind this one,
            // which is how a single restarted worker used to abort a whole
            // multi-chunk sync.
            Err(error)
                if resends < HTTP_ROLLUP_TRANSIENT_RESEND_ATTEMPTS
                    && is_transient_http_sync_error(&error) =>
            {
                let delay = HTTP_ROLLUP_TRANSIENT_RESEND_DELAY * 2_u32.pow(resends);
                resends += 1;
                eprintln!(
                    "http rollup mode: {} did not complete ({error}); resending in {}s ({resends}/{HTTP_ROLLUP_TRANSIENT_RESEND_ATTEMPTS})",
                    chunk.batch_id,
                    delay.as_secs(),
                );
                sleep(delay);
            }
            Err(error) if should_retry_http_rollup_chunk_after_error(chunk, &error) => {
                let smaller_chunks = split_http_rollup_sync_batch_after_budget_error(chunk);
                if smaller_chunks.len() <= 1 {
                    return Err(error);
                }
                eprintln!(
                    "http rollup mode: {} rejected {}; retrying as {} smaller batches",
                    http_rollup_retry_error_label(&error),
                    chunk.batch_id,
                    smaller_chunks.len()
                );
                for smaller_chunk in &smaller_chunks {
                    send_http_rollup_chunk_with_retry_using_sleep(
                        smaller_chunk,
                        send_chunk,
                        sleep,
                    )?;
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
}

/// Failures that leave a batch's fate unknown rather than deciding it.
///
/// Only server-side infrastructure statuses qualify. 429 is deliberately
/// excluded: the endpoint advertises its own `Retry-After`, which this backoff
/// cannot see, so resending on our own schedule would work against the limit it
/// asked for. 501 is excluded because "not implemented" is a decision that
/// repeating cannot change.
pub(crate) fn is_transient_http_sync_error(error: &anyhow::Error) -> bool {
    http_sync_error_status(error).is_some_and(|status| matches!(status, 500 | 502 | 503 | 504))
}

/// Reads the status of a sync endpoint failure whatever its body looks like.
///
/// `parse_http_sync_error` needs a JSON body to find an error code in. The
/// failures worth resending come from the infrastructure in front of the
/// worker and answer in plain text, so the status is read on its own here.
pub(crate) fn http_sync_error_status(error: &anyhow::Error) -> Option<u16> {
    let message = error.to_string();
    let rest = message.strip_prefix("sync endpoint returned HTTP ")?;
    rest.split(':').next()?.trim().parse().ok()
}

pub(crate) fn should_retry_http_rollup_chunk_after_error(
    chunk: &SyncBatch,
    error: &anyhow::Error,
) -> bool {
    if !(is_http_sync_error(error, 413, "sync_batch_d1_query_budget_exceeded")
        || is_http_sync_error(error, 413, "sync_batch_too_large"))
    {
        return false;
    }
    chunk.summaries.len() > 1
        || chunk.sources.len() > 1
        || chunk.accounts.len() > 1
        || chunk.source_account_assignments.len() > 1
        || chunk.subscriptions.len() > 1
        || chunk.account_plan_observations.len() > 1
        || chunk.account_evidence_summaries.len() > 1
        || chunk.task_buckets.len() > 1
        || chunk.task_verifications.len() > 1
        || chunk.code_change_metrics.len() > 1
        || chunk.quota_cycle_contributions.len() > 1
        || (!chunk.task_buckets.is_empty() && !chunk.task_verifications.is_empty())
        || (http_rollup_metadata_count(chunk) > 0 && !chunk.summaries.is_empty())
        || (!chunk.code_change_metrics.is_empty() && has_non_code_change_payload(chunk))
        || (!chunk.quota_cycle_contributions.is_empty() && has_non_quota_cycle_payload(chunk))
}

fn http_rollup_retry_error_label(error: &anyhow::Error) -> &'static str {
    if is_http_sync_error(error, 413, "sync_batch_too_large") {
        "batch size"
    } else {
        "D1 budget"
    }
}

fn is_http_sync_error(error: &anyhow::Error, status: u16, error_code: &str) -> bool {
    parse_http_sync_error(error).is_some_and(|parsed| {
        parsed.status == status
            && parsed
                .body
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|value| value == error_code)
    })
}

#[derive(Debug)]
struct ParsedHttpSyncError {
    status: u16,
    body: Value,
}

fn parse_http_sync_error(error: &anyhow::Error) -> Option<ParsedHttpSyncError> {
    let message = error.to_string();
    let rest = message.strip_prefix("sync endpoint returned HTTP ")?;
    let (status_text, body_text) = rest.split_once(':')?;
    let status = status_text.trim().parse().ok()?;
    let body = serde_json::from_str(body_text.trim()).ok()?;
    Some(ParsedHttpSyncError { status, body })
}

pub(crate) fn http_sync_endpoint(command: &SyncCommand) -> Result<String> {
    if let Some(endpoint) = command
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(endpoint.to_string());
    }
    Ok(hosted_http_sync_endpoint())
}

/// Batch endpoint of the hosted deployment this build is configured against.
fn hosted_http_sync_endpoint() -> String {
    format!(
        "{}/api/sync/batches",
        auth::cloudflare_api_url().trim_end_matches('/')
    )
}

pub(crate) fn resolve_http_auth_token(
    command: &SyncCommand,
    required: bool,
) -> Result<Option<String>> {
    if let Some(token) = command
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(token.to_string()));
    }

    if let Some(token) = std::env::var("STATSAI_SYNC_TOKEN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(token));
    }

    let token = auth::get_or_refresh_token()?;
    if required {
        Ok(Some(token.context(
            "device login required; run `statsai auth login` first",
        )?))
    } else {
        Ok(token)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TaskVerificationFeedResponse {
    #[serde(default)]
    verifications: Vec<TaskVerification>,
    next_cursor: Option<TaskVerificationCursor>,
}

#[derive(Debug)]
pub(crate) struct HttpSyncPreflight {
    pub(crate) auth_token: Option<String>,
    pub(crate) remote: Option<Value>,
}

pub(crate) fn load_http_sync_preflight(
    command: &SyncCommand,
    endpoint: &str,
) -> Result<HttpSyncPreflight> {
    // The hosted endpoint authenticates every route, and its preflight status
    // is what drives remote-drift detection and committed-metric blinding.
    // Without a token both would be skipped silently and the run would fail
    // later on a bare 401, so the missing login is reported here instead. A
    // self-hosted endpoint may accept unauthenticated batches.
    let auth_token = resolve_http_auth_token(
        command,
        http_endpoint_requires_authentication(endpoint, &hosted_http_sync_endpoint()),
    )?;
    let remote = auth_token
        .as_deref()
        .map(|token| http_remote_preflight_status(endpoint, token))
        .transpose()?
        .flatten();
    Ok(HttpSyncPreflight { auth_token, remote })
}

pub(crate) fn http_remote_verify(endpoint: &str, auth_token: &str) -> Result<Value> {
    validate_authenticated_http_endpoint(endpoint)?;
    let url = http_verify_status_url(endpoint)?;
    let request = ureq::get(&url)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .set("Authorization", &format!("Bearer {auth_token}"));
    match request.call() {
        Ok(response) => http_response_json(response, "verify sync status"),
        Err(error) => Err(http_request_error("verify sync status", error)),
    }
}

fn http_remote_preflight_status(endpoint: &str, auth_token: &str) -> Result<Option<Value>> {
    validate_authenticated_http_endpoint(endpoint)?;
    let Some(url) = http_preflight_status_url(endpoint) else {
        return Ok(None);
    };
    let request = ureq::get(&url)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .set("Authorization", &format!("Bearer {auth_token}"));
    match request.call() {
        Ok(response) => http_response_json(response, "load sync preflight status").map(Some),
        Err(ureq::Error::Status(code, _)) if optional_http_sync_preflight_status(code) => Ok(None),
        Err(error) => Err(http_request_error("load sync preflight status", error)),
    }
}

pub(crate) fn http_remote_reset(endpoint: &str, auth_token: &str) -> Result<Value> {
    validate_authenticated_http_endpoint(endpoint)?;
    let url = http_reset_url(endpoint)?;
    let body = serde_json::to_string(&json!({
        "confirm": "reset_synced_data",
        "scope": "device_mirror",
    }))?;
    let request = ureq::post(&url)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .set("Authorization", &format!("Bearer {auth_token}"))
        .set("Content-Type", "application/json");
    match request.send_string(&body) {
        Ok(response) => http_response_json(response, "reset remote sync data"),
        Err(error) => Err(http_request_error("reset remote sync data", error)),
    }
}

pub(crate) fn http_verify_status_url(endpoint: &str) -> Result<String> {
    let endpoint = endpoint.trim_end_matches('/');
    if let Some(prefix) = endpoint.strip_suffix("/api/sync/batches") {
        return Ok(format!("{prefix}/api/sync/status"));
    }
    bail!(
        "http verify expects a Cloudflare sync endpoint ending in /api/sync/batches; got {}",
        endpoint
    )
}

/// Whether an endpoint is this build's hosted deployment, which authenticates
/// every route it exposes.
///
/// Identity is compared against the configured hosted endpoint rather than
/// inferred from the route shape: a self-hosted deployment serves the same
/// `/api/sync/batches` path and may legitimately accept unauthenticated
/// batches, so requiring a device login from it would break a supported setup.
pub(crate) fn http_endpoint_requires_authentication(endpoint: &str, hosted_endpoint: &str) -> bool {
    endpoint.trim().trim_end_matches('/') == hosted_endpoint.trim().trim_end_matches('/')
}

pub(crate) fn http_preflight_status_url(endpoint: &str) -> Option<String> {
    let endpoint = endpoint.trim_end_matches('/');
    endpoint
        .strip_suffix("/api/sync/batches")
        .map(|prefix| format!("{prefix}/api/sync/status?view=preflight"))
}

pub(crate) fn http_reset_url(endpoint: &str) -> Result<String> {
    let endpoint = endpoint.trim_end_matches('/');
    if let Some(prefix) = endpoint.strip_suffix("/api/sync/batches") {
        return Ok(format!("{prefix}/api/sync/reset"));
    }
    bail!(
        "http reset expects a Cloudflare sync endpoint ending in /api/sync/batches; got {}",
        endpoint
    )
}

pub(crate) fn http_task_verification_feed_url(endpoint: &str) -> Option<String> {
    let endpoint = endpoint.trim_end_matches('/');
    if let Some(prefix) = endpoint.strip_suffix("/api/sync/batches") {
        return Some(format!("{prefix}/api/task-sync/verifications"));
    }
    None
}

fn http_request_error(action: &str, error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            anyhow::anyhow!("HTTP {action} failed (HTTP {code}): {body}")
        }
        other => anyhow::anyhow!("HTTP {action} failed: {other}"),
    }
}

pub(crate) fn remote_hosted_tasks_enabled(remote: &Value) -> bool {
    remote
        .pointer("/capabilities/hostedTasks")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

pub(crate) fn remote_code_change_identity_key(remote: &Value) -> Result<Option<[u8; 32]>> {
    let Some(value) = remote.pointer("/capabilities/codeChangeIdentityKey") else {
        return Ok(None);
    };
    let encoded = value
        .as_str()
        .context("sync preflight returned a non-string code-change identity key")?;
    let decoded = hex::decode(encoded)
        .context("sync preflight returned an invalid code-change identity key")?;
    let key = decoded.try_into().map_err(|_| {
        anyhow::anyhow!("sync preflight returned a code-change identity key with invalid length")
    })?;
    Ok(Some(key))
}

pub(crate) fn optional_http_sync_preflight_status(status: u16) -> bool {
    matches!(status, 404 | 405 | 501)
}

fn http_response_json(response: ureq::Response, action: &str) -> Result<Value> {
    let body = response
        .into_string()
        .with_context(|| format!("read HTTP {action} response body"))?;
    serde_json::from_str(&body).with_context(|| format!("parse HTTP {action} response JSON"))
}

pub(crate) fn logical_http_rollup_batch_id(batch_id: &str) -> String {
    let mut current = batch_id.to_string();
    loop {
        let next = strip_one_http_rollup_batch_suffix(&current);
        if next == current {
            return current;
        }
        current = next;
    }
}

fn strip_one_http_rollup_batch_suffix(batch_id: &str) -> String {
    if let Some(index) = batch_id.rfind("_part_") {
        let suffix = &batch_id[(index + "_part_".len())..];
        if let Some((part, total)) = suffix.split_once("_of_") {
            if part.parse::<usize>().is_ok() && total.parse::<usize>().is_ok() {
                return batch_id[..index].to_string();
            }
        }
    }

    for marker in [
        "_sources_",
        "_accounts_",
        "_assignments_",
        "_subscriptions_",
        "_task_buckets_",
        "_task_verifications_",
        "_code_changes_",
        "_snapshot_",
    ] {
        if let Some(index) = batch_id.rfind(marker) {
            let suffix = &batch_id[(index + marker.len())..];
            if suffix.parse::<usize>().is_ok() {
                return batch_id[..index].to_string();
            }
        }
    }

    batch_id.to_string()
}

pub(crate) fn remote_last_sync_batch_id(remote: &Value) -> Option<&str> {
    remote
        .pointer("/device/last_sync_batch_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
