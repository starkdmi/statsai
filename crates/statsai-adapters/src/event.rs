use crate::{AdapterScan, ProviderAdapter, ScanOptions};
use chrono::{DateTime, Utc};
use statsai_core::{
    canonical_display, hash_text, project_bucket_key, semantic_event_id, summary_id, Confidence,
    EventSource, IdentitySource, MetricStats, ModelInfo, ParseEvidence, PrivacyInfo, PrivacyMode,
    ProjectInfo, RuntimeInfo, SessionInfo, SourceLocation, SummaryMetadata, SummaryMetrics,
    UsageCounts, UsageEvent, UsageSummary, USAGE_EVENT_SCHEMA_VERSION,
    USAGE_SUMMARY_SCHEMA_VERSION,
};
use statsai_pricing::estimate_cost_at;
use std::collections::HashSet;
use std::path::Path;

pub(crate) const SESSION_SCOPED_EVENT_KEY_VERSION: &str = "semantic_usage_event.v1";
pub(crate) const PATH_INDEPENDENT_EVENT_KEY_VERSION: &str = "semantic_usage_event.v4";
pub(crate) const PROVIDER_RECORD_EVENT_KEY_VERSION: &str = "provider_record_usage_event.v1";

pub(crate) fn usd_to_micro_usd(usd: f64) -> Option<i64> {
    let micro_usd = usd * 1_000_000.0;
    usd.is_finite()
        .then_some(micro_usd)
        .filter(|value| *value >= 0.0 && *value <= i64::MAX as f64)
        .map(|value| value.round() as i64)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EventDeduplication {
    SessionScoped,
    PathIndependent,
    ProviderRecord(String),
}

pub(crate) fn push_deduped(scan: &mut AdapterScan, seen: &mut HashSet<String>, event: UsageEvent) {
    let key = event
        .parse_evidence
        .as_ref()
        .and_then(|evidence| evidence.source_record_id.clone())
        .unwrap_or_else(|| event.event_id.0.clone());
    if seen.insert(key) {
        scan.events.push(event);
    } else {
        scan.diagnostics.duplicate_events += 1;
    }
}

pub(crate) fn merge_adapter_scan(
    target: &mut AdapterScan,
    seen: &mut HashSet<String>,
    mut source: AdapterScan,
) {
    for event in source.events.drain(..) {
        push_deduped(target, seen, event);
    }
    target.summaries.append(&mut source.summaries);
    target.task_spans.append(&mut source.task_spans);
    target
        .quota_observations
        .append(&mut source.quota_observations);
    target.diagnostics.files_scanned = target
        .diagnostics
        .files_scanned
        .saturating_add(source.diagnostics.files_scanned);
    target.diagnostics.files_skipped_unchanged = target
        .diagnostics
        .files_skipped_unchanged
        .saturating_add(source.diagnostics.files_skipped_unchanged);
    target.diagnostics.raw_rows = target
        .diagnostics
        .raw_rows
        .saturating_add(source.diagnostics.raw_rows);
    target.diagnostics.candidate_usage_rows = target
        .diagnostics
        .candidate_usage_rows
        .saturating_add(source.diagnostics.candidate_usage_rows);
    target.diagnostics.duplicate_events = target
        .diagnostics
        .duplicate_events
        .saturating_add(source.diagnostics.duplicate_events);
    target.diagnostics.skipped_zero_events = target
        .diagnostics
        .skipped_zero_events
        .saturating_add(source.diagnostics.skipped_zero_events);
    target.diagnostics.invalid_rows = target
        .diagnostics
        .invalid_rows
        .saturating_add(source.diagnostics.invalid_rows);
    target.diagnostics.timestamp_fallbacks = target
        .diagnostics
        .timestamp_fallbacks
        .saturating_add(source.diagnostics.timestamp_fallbacks);
    target.diagnostics.model_fallbacks = target
        .diagnostics
        .model_fallbacks
        .saturating_add(source.diagnostics.model_fallbacks);
}

pub(crate) struct ProviderEventParts<'a> {
    pub(crate) timestamp: DateTime<Utc>,
    pub(crate) session_started_at: Option<DateTime<Utc>>,
    pub(crate) session_ended_at: Option<DateTime<Utc>>,
    pub(crate) duration_seconds: Option<u64>,
    pub(crate) model: Option<ModelInfo>,
    pub(crate) usage: UsageCounts,
    pub(crate) runtime: Option<RuntimeInfo>,
    pub(crate) session_raw: String,
    pub(crate) project: Option<ProjectInfo>,
    pub(crate) event_kind: &'static str,
    pub(crate) source_file: &'a Path,
    pub(crate) source_line_number: Option<usize>,
    pub(crate) source_type: &'static str,
    pub(crate) model_inferred: bool,
    pub(crate) timestamp_inferred: bool,
    pub(crate) deduplication: EventDeduplication,
    pub(crate) dedupe_salt: Option<String>,
}

pub(crate) fn usage_event<A: ProviderAdapter + ?Sized>(
    adapter: &A,
    source: &SourceLocation,
    options: &ScanOptions,
    parts: ProviderEventParts<'_>,
) -> UsageEvent {
    let session_hash = hash_text(&parts.session_raw);
    let session_started_at = parts.session_started_at.unwrap_or(parts.timestamp);
    let session_ended_at = parts.session_ended_at.unwrap_or(parts.timestamp);
    let project_key = project_bucket_key(parts.project.as_ref());
    let model_key = parts
        .model
        .as_ref()
        .and_then(|model| model.normalized_name.as_deref().or(model.name.as_deref()))
        .unwrap_or("unknown");
    let event_kind_key = parts
        .dedupe_salt
        .as_deref()
        .map(|salt| format!("{}:{salt}", parts.event_kind))
        .unwrap_or_else(|| parts.event_kind.to_string());
    let (event_key_version, semantic_key) = match parts.deduplication {
        EventDeduplication::ProviderRecord(provider_record_id) => (
            PROVIDER_RECORD_EVENT_KEY_VERSION,
            format!(
                "{PROVIDER_RECORD_EVENT_KEY_VERSION}:{event_kind_key}:{}",
                hash_text(&provider_record_id)
            ),
        ),
        EventDeduplication::SessionScoped => (
            SESSION_SCOPED_EVENT_KEY_VERSION,
            if parts.session_started_at.is_some() || parts.session_ended_at.is_some() {
                format!(
                    "{SESSION_SCOPED_EVENT_KEY_VERSION}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
                    event_kind_key,
                    session_hash,
                    session_started_at.timestamp_millis(),
                    session_ended_at.timestamp_millis(),
                    model_key,
                    parts.usage.input_tokens.unwrap_or(0),
                    parts.usage.cache_read_tokens.unwrap_or(0),
                    parts.usage.output_tokens.unwrap_or(0),
                    parts.usage.reasoning_tokens.unwrap_or(0),
                    parts.usage.computed_total()
                )
            } else {
                format!(
                    "{SESSION_SCOPED_EVENT_KEY_VERSION}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
                    event_kind_key,
                    session_hash,
                    parts.timestamp.timestamp_millis(),
                    model_key,
                    parts.usage.input_tokens.unwrap_or(0),
                    parts.usage.cache_read_tokens.unwrap_or(0),
                    parts.usage.output_tokens.unwrap_or(0),
                    parts.usage.reasoning_tokens.unwrap_or(0),
                    parts.usage.computed_total()
                )
            },
        ),
        EventDeduplication::PathIndependent => (
            PATH_INDEPENDENT_EVENT_KEY_VERSION,
            if parts.session_started_at.is_some() || parts.session_ended_at.is_some() {
                format!(
                    "{PATH_INDEPENDENT_EVENT_KEY_VERSION}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
                    event_kind_key,
                    project_key,
                    session_started_at.timestamp_millis(),
                    session_ended_at.timestamp_millis(),
                    model_key,
                    parts.usage.input_tokens.unwrap_or(0),
                    parts.usage.cache_read_tokens.unwrap_or(0),
                    parts.usage.output_tokens.unwrap_or(0),
                    parts.usage.reasoning_tokens.unwrap_or(0),
                    parts.usage.computed_total()
                )
            } else {
                format!(
                    "{PATH_INDEPENDENT_EVENT_KEY_VERSION}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
                    event_kind_key,
                    project_key,
                    parts.timestamp.timestamp_millis(),
                    model_key,
                    parts.usage.input_tokens.unwrap_or(0),
                    parts.usage.cache_read_tokens.unwrap_or(0),
                    parts.usage.output_tokens.unwrap_or(0),
                    parts.usage.reasoning_tokens.unwrap_or(0),
                    parts.usage.computed_total()
                )
            },
        ),
    };
    let event_id = semantic_event_id(adapter.provider(), &source.source_id, &semantic_key);
    let file_path_hash = hash_text(&canonical_display(parts.source_file));
    let source_record_id = format!("usage_key_{}", &hash_text(&semantic_key)[..32]);
    let cost = estimate_cost_at(
        adapter.provider(),
        parts.model.as_ref(),
        &parts.usage,
        &parts.timestamp,
    );

    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id,
        device_id: options.device_id.clone(),
        provider: adapter.provider().to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        subscription_id: None,
        source: EventSource {
            adapter_id: adapter.id().to_string(),
            adapter_version: adapter.version().to_string(),
            source_kind: source.source_kind.clone(),
            location_origin: Some(source.location_origin.clone()),
            source_type: parts.source_type.to_string(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some(source_record_id.clone()),
            parse_confidence: if parts.model_inferred || parts.timestamp_inferred {
                Confidence::Medium
            } else {
                Confidence::High
            },
        },
        session: SessionInfo {
            session_id: format!("session_{}", &session_hash[..24]),
            local_session_id_hash: Some(session_hash),
            title: None,
            started_at: session_started_at,
            ended_at: parts.session_ended_at,
            duration_seconds: parts.duration_seconds,
        },
        model: parts.model,
        runtime: parts.runtime,
        cost,
        parse_evidence: Some(ParseEvidence {
            event_key_version: event_key_version.to_string(),
            source_file_path_hash: Some(file_path_hash),
            source_line_number: parts.source_line_number.map(|value| value as u64),
            source_record_id: Some(semantic_key),
            model_inferred: parts.model_inferred,
            timestamp_inferred: parts.timestamp_inferred,
            account_identity_source: IdentitySource::Unresolved,
        }),
        usage: parts.usage,
        project: parts.project,
        git: None,
        privacy: metadata_only_privacy(),
        created_at: parts.timestamp,
        imported_at: Utc::now(),
    }
}

pub(crate) struct MetadataSummaryParts<'a> {
    pub(crate) source_file: &'a Path,
    pub(crate) summary_format: &'a str,
    pub(crate) semantic_key: &'a str,
    pub(crate) observed_at: DateTime<Utc>,
    pub(crate) metadata: SummaryMetadata,
    pub(crate) model: Option<ModelInfo>,
    pub(crate) runtime: Option<RuntimeInfo>,
    pub(crate) project: Option<ProjectInfo>,
}

pub(crate) fn metadata_summary<A: ProviderAdapter + ?Sized>(
    adapter: &A,
    source: &SourceLocation,
    options: &ScanOptions,
    parts: MetadataSummaryParts<'_>,
) -> UsageSummary {
    let file_path_hash = hash_text(&canonical_display(parts.source_file));
    let usage = UsageCounts::default();
    UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: summary_id(adapter.provider(), &source.source_id, parts.semantic_key),
        device_id: options.device_id.clone(),
        provider: adapter.provider().to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        source: EventSource {
            adapter_id: adapter.id().to_string(),
            adapter_version: adapter.version().to_string(),
            source_kind: source.source_kind.clone(),
            location_origin: Some(source.location_origin.clone()),
            source_type: parts.summary_format.to_string(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some(format!(
                "summary_key_{}",
                &hash_text(parts.semantic_key)[..32]
            )),
            parse_confidence: Confidence::Medium,
        },
        model: parts.model.clone(),
        models: Vec::new(),
        usage: usage.clone(),
        cost: estimate_cost_at(
            adapter.provider(),
            parts.model.as_ref(),
            &usage,
            &parts.observed_at,
        ),
        parse_evidence: Some(ParseEvidence {
            event_key_version: "metadata_summary.v1".to_string(),
            source_file_path_hash: Some(file_path_hash),
            source_line_number: None,
            source_record_id: Some(parts.semantic_key.to_string()),
            model_inferred: parts.model.is_none(),
            timestamp_inferred: false,
            account_identity_source: IdentitySource::Unresolved,
        }),
        project: parts.project,
        privacy: metadata_only_privacy(),
        metrics: parts.runtime.map(runtime_to_summary_metrics),
        period_start: None,
        period_end: None,
        observed_at: parts.observed_at,
        metadata: parts.metadata,
        imported_at: Utc::now(),
    }
}

pub(crate) fn runtime_to_summary_metrics(runtime: RuntimeInfo) -> SummaryMetrics {
    SummaryMetrics {
        active_seconds: runtime.latency_ms.map(|value| value as f64 / 1000.0),
        tracked_requests: runtime.total_messages,
        tracked_output_tokens: None,
        tracked_reasoning_tokens: None,
        latency_ms: runtime.latency_ms.map(metric_single_sample),
        time_to_first_token_ms: runtime.time_to_first_token_ms.map(metric_single_sample),
        generated_tps: None,
        visible_tps: None,
        overall_generated_tps: None,
        overall_visible_tps: None,
        cache_hit_ratio: None,
        reasoning_share: None,
        total_messages: runtime.total_messages,
        user_messages: runtime.user_messages,
        assistant_messages: runtime.assistant_messages,
        developer_messages: runtime.developer_messages,
    }
}

pub(crate) fn metric_single_sample(value: u64) -> MetricStats {
    let value = value as f64;
    MetricStats {
        samples: 1,
        avg: Some(value),
        min: Some(value),
        max: Some(value),
        p50: Some(value),
        p95: Some(value),
        sum: Some(value),
    }
}

pub(crate) fn metric_from_samples(samples: &[u64]) -> Option<MetricStats> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples
        .iter()
        .map(|value| *value as f64)
        .collect::<Vec<_>>();
    sorted.sort_by(f64::total_cmp);
    let sum = sorted.iter().sum::<f64>();
    let percentile = |percent: f64| -> f64 {
        let index = ((sorted.len() - 1) as f64 * percent).round() as usize;
        sorted[index]
    };
    Some(MetricStats {
        samples: sorted.len() as u64,
        avg: Some(sum / sorted.len() as f64),
        min: sorted.first().copied(),
        max: sorted.last().copied(),
        p50: Some(percentile(0.50)),
        p95: Some(percentile(0.95)),
        sum: Some(sum),
    })
}

pub(crate) fn infer_missing_output(
    total: Option<u64>,
    input: Option<u64>,
    cache_creation: Option<u64>,
    cache_read: Option<u64>,
    reasoning: Option<u64>,
) -> Option<u64> {
    total.and_then(|total| {
        let known = input.unwrap_or(0)
            + cache_creation.unwrap_or(0)
            + cache_read.unwrap_or(0)
            + reasoning.unwrap_or(0);
        (total > known).then_some(total - known)
    })
}

pub(crate) fn sum_usage_counts(left: &UsageCounts, right: &UsageCounts) -> UsageCounts {
    fn sum_field(left: Option<u64>, right: Option<u64>) -> Option<u64> {
        if left.is_some() || right.is_some() {
            Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0)))
        } else {
            None
        }
    }

    UsageCounts {
        input_tokens: sum_field(left.input_tokens, right.input_tokens),
        output_tokens: sum_field(left.output_tokens, right.output_tokens),
        cache_creation_tokens: sum_field(left.cache_creation_tokens, right.cache_creation_tokens),
        cache_creation_5m_tokens: sum_field(
            left.cache_creation_5m_tokens,
            right.cache_creation_5m_tokens,
        ),
        cache_creation_1h_tokens: sum_field(
            left.cache_creation_1h_tokens,
            right.cache_creation_1h_tokens,
        ),
        cache_read_tokens: sum_field(left.cache_read_tokens, right.cache_read_tokens),
        reasoning_tokens: sum_field(left.reasoning_tokens, right.reasoning_tokens),
        total_tokens: sum_field(left.total_tokens, right.total_tokens),
        requests: sum_field(left.requests, right.requests),
        local_prompt_eval_tokens: sum_field(
            left.local_prompt_eval_tokens,
            right.local_prompt_eval_tokens,
        ),
        local_eval_tokens: sum_field(left.local_eval_tokens, right.local_eval_tokens),
    }
}

pub(crate) fn subtract_usage_counts(
    current: &UsageCounts,
    previous: Option<&UsageCounts>,
) -> UsageCounts {
    let subtract = |left: Option<u64>, right: Option<u64>| {
        let value = left.unwrap_or(0).saturating_sub(right.unwrap_or(0));
        (value > 0).then_some(value)
    };
    UsageCounts {
        input_tokens: subtract(
            current.input_tokens,
            previous.and_then(|usage| usage.input_tokens),
        ),
        output_tokens: subtract(
            current.output_tokens,
            previous.and_then(|usage| usage.output_tokens),
        ),
        cache_creation_tokens: subtract(
            current.cache_creation_tokens,
            previous.and_then(|usage| usage.cache_creation_tokens),
        ),
        cache_creation_5m_tokens: subtract(
            current.cache_creation_5m_tokens,
            previous.and_then(|usage| usage.cache_creation_5m_tokens),
        ),
        cache_creation_1h_tokens: subtract(
            current.cache_creation_1h_tokens,
            previous.and_then(|usage| usage.cache_creation_1h_tokens),
        ),
        cache_read_tokens: subtract(
            current.cache_read_tokens,
            previous.and_then(|usage| usage.cache_read_tokens),
        ),
        reasoning_tokens: subtract(
            current.reasoning_tokens,
            previous.and_then(|usage| usage.reasoning_tokens),
        ),
        total_tokens: subtract(
            current.total_tokens,
            previous.and_then(|usage| usage.total_tokens),
        ),
        requests: Some(1),
        local_prompt_eval_tokens: None,
        local_eval_tokens: None,
    }
}

pub(crate) fn metadata_only_privacy() -> PrivacyInfo {
    PrivacyInfo {
        mode: PrivacyMode::MetadataOnly,
        contains_prompt_text: false,
        contains_response_text: false,
        contains_file_paths: false,
    }
}

#[test]
fn summed_usage_counts_preserve_cache_creation_lifetimes() {
    let left = UsageCounts {
        cache_creation_tokens: Some(10),
        cache_creation_5m_tokens: Some(7),
        cache_creation_1h_tokens: Some(3),
        ..UsageCounts::default()
    };
    let right = UsageCounts {
        cache_creation_tokens: Some(20),
        cache_creation_5m_tokens: Some(11),
        cache_creation_1h_tokens: Some(9),
        ..UsageCounts::default()
    };

    let usage = sum_usage_counts(&left, &right);

    assert_eq!(usage.cache_creation_tokens, Some(30));
    assert_eq!(usage.cache_creation_5m_tokens, Some(18));
    assert_eq!(usage.cache_creation_1h_tokens, Some(12));
}
