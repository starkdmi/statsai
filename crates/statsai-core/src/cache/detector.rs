use super::{
    provider_has_cache_diagnostics, CACHE_LOSS_MIN_PERCENT, CACHE_LOSS_MIN_TOKENS,
    CACHE_WRITE_REPORTING_PROVIDERS,
};
use crate::ids::EventId;
use crate::types::{ModelInfo, UsageEvent};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How the detector classified one model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheCallClass {
    /// The first call observed in its stream. Nothing earlier to compare with.
    FirstObserved,
    /// The comparison restarts here; see [`CacheBoundaryReason`].
    Boundary,
    /// The previous call left too little reusable context to judge a loss.
    BelowThreshold,
    /// Compared with the previous call in its stream.
    Comparable,
    /// The call neither read nor wrote the cache, so it did not use it, as
    /// when a custom endpoint serves the request. Not judged, and it leaves
    /// nothing reusable for the next call.
    Uncached,
    /// Usage that cannot be placed in order or split into calls.
    Unclassifiable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheBoundaryReason {
    /// The model changed. Prompt caches are per model.
    ModelChange,
    /// The provider compacted the conversation, which rewrites the prompt.
    Compaction,
    /// The previous usage in this stream could not be classified.
    EvidenceGap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheUnclassifiableReason {
    /// The record had no timestamp, so its place in the stream is unknown.
    TimestampMissing,
    /// Several calls were recorded as one total with no per-call breakdown.
    Aggregated,
    /// The provider reported no input or cache counts.
    NoInputTelemetry,
    /// Another call in the stream has the same timestamp and no recorded order.
    AmbiguousOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheLoss {
    /// No cached reads remained.
    Full,
    /// Some cached context was still read.
    Partial,
}

/// What a request gap was measured between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheGapTiming {
    /// From the previous response to the record that sent this request.
    RequestStart,
    /// Between the two recorded responses. Includes this call's own run time,
    /// so it overstates idle time.
    RecordedEvents,
}

/// The cache lifetime a call's writes asked for, as Claude Code records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheWriteTtl {
    FiveMinutes,
    OneHour,
    Mixed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheCallVerdict {
    pub class: CacheCallClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<CacheBoundaryReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unclassifiable: Option<CacheUnclassifiableReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss: Option<CacheLoss>,
    /// Reusable context the previous call left, bounded by this call's input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_tokens: Option<u64>,
    /// For a suspected loss, the reusable context it did not read from the
    /// cache: the baseline minus its cached reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missed_tokens: Option<u64>,
    /// What the missed reuse cost at API prices: the call's cost minus its
    /// cost had the missed tokens been cache reads. Set by the caller that
    /// prices calls; `None` when the model's pricing is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missed_cost_micro_usd: Option<u64>,
    /// Seconds since the previous call in the stream completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_timing: Option<CacheGapTiming>,
    /// The model before a model change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_model: Option<String>,
    /// Cache lifetime the previous call's writes asked for, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_write_ttl: Option<CacheWriteTtl>,
}

impl CacheCallVerdict {
    fn new(class: CacheCallClass) -> Self {
        Self {
            class,
            boundary: None,
            unclassifiable: None,
            loss: None,
            baseline_tokens: None,
            missed_tokens: None,
            missed_cost_micro_usd: None,
            gap_seconds: None,
            gap_timing: None,
            previous_model: None,
            previous_write_ttl: None,
        }
    }

    fn unclassifiable(reason: CacheUnclassifiableReason) -> Self {
        Self {
            unclassifiable: Some(reason),
            ..Self::new(CacheCallClass::Unclassifiable)
        }
    }
}

/// One model call and its verdict, or one unclassifiable block of usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AnalyzedCacheCall {
    pub event_id: EventId,
    /// Position inside an aggregated event; zero for single-call events.
    pub call_index: u32,
    /// Model calls this entry stands for: one, or an aggregated event's count.
    pub requests: u64,
    pub provider: String,
    pub source_id: String,
    pub provider_account_id: Option<String>,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_hash: Option<String>,
    pub completed_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_at: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_ttl: Option<CacheWriteTtl>,
    pub verdict: CacheCallVerdict,
}

impl AnalyzedCacheCall {
    /// Ordinary input plus cache writes and reads.
    #[must_use]
    pub fn logical_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_tokens.unwrap_or(0))
            .saturating_add(self.cache_read_tokens)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StreamKey {
    provider: String,
    source_id: String,
    provider_account_id: Option<String>,
    session_id: String,
    file_hash: Option<String>,
    agent_hash: Option<String>,
}

/// Classifies every model call of the given events.
///
/// Pass whole sessions: a call is judged against its predecessor, and a
/// predecessor that is not passed in makes the call look like the first one.
/// Events of providers without call order are skipped. The result is sorted by
/// completion time, then event and call position, so equal input always gives
/// equal output.
#[must_use]
pub fn analyze_cache_calls<'a>(
    events: impl IntoIterator<Item = &'a UsageEvent>,
) -> Vec<AnalyzedCacheCall> {
    let mut streams: BTreeMap<StreamKey, Vec<(OrderKey, AnalyzedCacheCall)>> = BTreeMap::new();
    let mut analyzed = Vec::new();
    for event in events {
        if !provider_has_cache_diagnostics(&event.provider) {
            continue;
        }
        let calls = event_calls(event);
        let timestamp_missing = event
            .parse_evidence
            .as_ref()
            .is_some_and(|evidence| evidence.timestamp_inferred);
        if timestamp_missing {
            // Its place in the stream is unknown, so it neither gets compared
            // nor interrupts the calls around it.
            analyzed.push(unclassifiable_entry(
                event,
                CacheUnclassifiableReason::TimestampMissing,
            ));
            continue;
        }
        let line = event
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_line_number);
        let stream = streams.entry(stream_key(event)).or_default();
        for call in calls {
            let order = OrderKey {
                completed_at: call.completed_at,
                line,
                event_id: call.event_id.0.clone(),
                call_index: call.call_index,
            };
            stream.push((order, call));
        }
    }
    for mut stream in streams.into_values() {
        stream.sort_by(|left, right| left.0.cmp(&right.0));
        mark_ambiguous_order(&mut stream);
        classify_stream(&mut stream);
        analyzed.extend(stream.into_iter().map(|(_, call)| call));
    }
    analyzed.sort_by(|left, right| {
        left.completed_at
            .cmp(&right.completed_at)
            .then_with(|| left.event_id.0.cmp(&right.event_id.0))
            .then_with(|| left.call_index.cmp(&right.call_index))
    });
    analyzed
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct OrderKey {
    completed_at: DateTime<Utc>,
    line: Option<u64>,
    event_id: String,
    call_index: u32,
}

fn stream_key(event: &UsageEvent) -> StreamKey {
    StreamKey {
        provider: event.provider.clone(),
        source_id: event.source_id.0.clone(),
        provider_account_id: event.provider_account_id.as_ref().map(|id| id.0.clone()),
        session_id: event.session.session_id.clone(),
        file_hash: event
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_file_path_hash.clone()),
        agent_hash: event
            .context
            .as_ref()
            .and_then(|context| context.agent_hash.clone()),
    }
}

/// The name the detector compares to tell whether consecutive calls used the
/// same model, and so the same prompt cache.
#[must_use]
pub fn cache_model_name(model: &ModelInfo) -> Option<String> {
    model
        .normalized_name
        .clone()
        .or_else(|| model.name.clone())
        .filter(|name| !name.trim().is_empty())
}

/// The event's model, unless the parser filled in a fallback because the
/// transcript did not name one: a guess cannot show that the model changed.
fn event_model(event: &UsageEvent) -> Option<String> {
    let inferred = event
        .parse_evidence
        .as_ref()
        .is_some_and(|evidence| evidence.model_inferred);
    event
        .model
        .as_ref()
        .filter(|_| !inferred)
        .and_then(cache_model_name)
}

fn base_entry(event: &UsageEvent, verdict: CacheCallVerdict) -> AnalyzedCacheCall {
    AnalyzedCacheCall {
        event_id: event.event_id.clone(),
        call_index: 0,
        requests: 1,
        provider: event.provider.clone(),
        source_id: event.source_id.0.clone(),
        provider_account_id: event.provider_account_id.as_ref().map(|id| id.0.clone()),
        session_id: event.session.session_id.clone(),
        agent_hash: event
            .context
            .as_ref()
            .and_then(|context| context.agent_hash.clone()),
        completed_at: event.created_at,
        requested_at: None,
        model: event_model(event),
        input_tokens: event.usage.input_tokens.unwrap_or(0),
        cache_read_tokens: event.usage.cache_read_tokens.unwrap_or(0),
        cache_creation_tokens: event.usage.cache_creation_tokens,
        write_ttl: None,
        verdict,
    }
}

fn unclassifiable_entry(
    event: &UsageEvent,
    reason: CacheUnclassifiableReason,
) -> AnalyzedCacheCall {
    AnalyzedCacheCall {
        requests: event.usage.requests.unwrap_or(1).max(1),
        ..base_entry(event, CacheCallVerdict::unclassifiable(reason))
    }
}

/// The calls an event stands for, before classification. An event that cannot
/// be split into calls comes back as one unclassifiable entry.
fn event_calls(event: &UsageEvent) -> Vec<AnalyzedCacheCall> {
    let requests = event.usage.requests.unwrap_or(1).max(1);
    let recorded_calls = event
        .context
        .as_ref()
        .map(|context| context.calls.as_slice())
        .unwrap_or_default();
    if !recorded_calls.is_empty() {
        if recorded_calls.len() as u64 != requests {
            return vec![unclassifiable_entry(
                event,
                CacheUnclassifiableReason::Aggregated,
            )];
        }
        return recorded_calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                let verdict = if call.input_tokens.is_none() && call.cache_read_tokens.is_none() {
                    CacheCallVerdict::unclassifiable(CacheUnclassifiableReason::NoInputTelemetry)
                } else {
                    CacheCallVerdict {
                        boundary: call
                            .after_compaction
                            .then_some(CacheBoundaryReason::Compaction),
                        ..CacheCallVerdict::new(CacheCallClass::Comparable)
                    }
                };
                AnalyzedCacheCall {
                    call_index: index as u32,
                    completed_at: call.completed_at,
                    requested_at: call.requested_at,
                    input_tokens: call.input_tokens.unwrap_or(0),
                    cache_read_tokens: call.cache_read_tokens.unwrap_or(0),
                    cache_creation_tokens: call.cache_creation_tokens,
                    // A turn's calls can switch models, so the event's model,
                    // which is the turn's last, does not stand in for each.
                    model: call.model.clone(),
                    verdict,
                    ..base_entry(event, CacheCallVerdict::new(CacheCallClass::Comparable))
                }
            })
            .collect();
    }
    if requests > 1 {
        return vec![unclassifiable_entry(
            event,
            CacheUnclassifiableReason::Aggregated,
        )];
    }
    if event.usage.input_tokens.is_none() && event.usage.cache_read_tokens.is_none() {
        return vec![unclassifiable_entry(
            event,
            CacheUnclassifiableReason::NoInputTelemetry,
        )];
    }
    let context = event.context.as_ref();
    let compaction = context.is_some_and(|context| context.after_compaction);
    vec![AnalyzedCacheCall {
        requested_at: context.and_then(|context| context.requested_at),
        write_ttl: write_ttl(
            event.usage.cache_creation_5m_tokens,
            event.usage.cache_creation_1h_tokens,
        ),
        verdict: CacheCallVerdict {
            boundary: compaction.then_some(CacheBoundaryReason::Compaction),
            ..CacheCallVerdict::new(CacheCallClass::Comparable)
        },
        ..base_entry(event, CacheCallVerdict::new(CacheCallClass::Comparable))
    }]
}

fn write_ttl(five_minutes: Option<u64>, one_hour: Option<u64>) -> Option<CacheWriteTtl> {
    match (five_minutes.unwrap_or(0) > 0, one_hour.unwrap_or(0) > 0) {
        (true, true) => Some(CacheWriteTtl::Mixed),
        (true, false) => Some(CacheWriteTtl::FiveMinutes),
        (false, true) => Some(CacheWriteTtl::OneHour),
        (false, false) => None,
    }
}

/// Calls with one timestamp keep their order only when every two of them carry
/// source lines or belong to the same event. Otherwise none of them is
/// compared: any could have come first, so no member's predecessor is known.
fn mark_ambiguous_order(stream: &mut [(OrderKey, AnalyzedCacheCall)]) {
    let mut start = 0;
    while start < stream.len() {
        let completed_at = stream[start].0.completed_at;
        let end = start
            + stream[start..]
                .iter()
                .take_while(|(order, _)| order.completed_at == completed_at)
                .count();
        let group = &mut stream[start..end];
        let ordered = group.iter().enumerate().all(|(index, (left, _))| {
            group[index + 1..].iter().all(|(right, _)| {
                left.event_id == right.event_id || (left.line.is_some() && right.line.is_some())
            })
        });
        if !ordered {
            for (_, call) in group.iter_mut() {
                if call.verdict.class != CacheCallClass::Unclassifiable {
                    call.verdict =
                        CacheCallVerdict::unclassifiable(CacheUnclassifiableReason::AmbiguousOrder);
                }
            }
        }
        start = end;
    }
}

struct Previous {
    completed_at: DateTime<Utc>,
    reusable_tokens: u64,
    model: Option<String>,
    write_ttl: Option<CacheWriteTtl>,
}

fn classify_stream(stream: &mut [(OrderKey, AnalyzedCacheCall)]) {
    let mut started = false;
    let mut after_gap = false;
    let mut previous: Option<Previous> = None;
    for (_, call) in stream.iter_mut() {
        if call.verdict.class == CacheCallClass::Unclassifiable {
            started = true;
            after_gap = true;
            previous = None;
            continue;
        }
        let compaction = call.verdict.boundary == Some(CacheBoundaryReason::Compaction);
        let mut verdict = CacheCallVerdict::new(CacheCallClass::Comparable);
        if let Some(previous) = previous.as_ref() {
            let (gap_seconds, gap_timing) = gap(previous.completed_at, call);
            verdict.gap_seconds = Some(gap_seconds);
            verdict.gap_timing = Some(gap_timing);
            verdict.previous_write_ttl = previous.write_ttl;
        }
        let model_changed = previous.as_ref().is_some_and(|previous| {
            previous.model.is_some() && call.model.is_some() && previous.model != call.model
        });
        if skipped_the_cache(call) {
            verdict.class = CacheCallClass::Uncached;
        } else if !started {
            verdict.class = CacheCallClass::FirstObserved;
        } else if after_gap {
            verdict.class = CacheCallClass::Boundary;
            verdict.boundary = Some(CacheBoundaryReason::EvidenceGap);
        } else if compaction {
            verdict.class = CacheCallClass::Boundary;
            verdict.boundary = Some(CacheBoundaryReason::Compaction);
        } else if model_changed {
            verdict.class = CacheCallClass::Boundary;
            verdict.boundary = Some(CacheBoundaryReason::ModelChange);
            verdict.previous_model = previous
                .as_ref()
                .and_then(|previous| previous.model.clone());
        } else if let Some(previous) = previous.as_ref() {
            let baseline = previous.reusable_tokens.min(call.logical_input_tokens());
            verdict.baseline_tokens = Some(baseline);
            if baseline < CACHE_LOSS_MIN_TOKENS {
                verdict.class = CacheCallClass::BelowThreshold;
            } else {
                verdict.loss = loss(baseline, call.cache_read_tokens);
                verdict.missed_tokens = verdict
                    .loss
                    .map(|_| baseline.saturating_sub(call.cache_read_tokens));
            }
        }
        call.verdict = verdict;
        started = true;
        after_gap = false;
        previous = Some(Previous {
            completed_at: call.completed_at,
            reusable_tokens: call
                .cache_read_tokens
                .saturating_add(call.cache_creation_tokens.unwrap_or(0)),
            model: call.model.clone(),
            write_ttl: call.write_ttl,
        });
    }
}

/// Whether a call reported no cache reads and no cache writes, for a provider
/// whose calls report writes whenever they use the cache.
fn skipped_the_cache(call: &AnalyzedCacheCall) -> bool {
    CACHE_WRITE_REPORTING_PROVIDERS.contains(&call.provider.as_str())
        && call.input_tokens > 0
        && call.cache_read_tokens == 0
        && call.cache_creation_tokens == Some(0)
}

/// Seconds from the previous response to this request. A recorded request
/// start that precedes the previous response belongs to an earlier exchange,
/// so the gap falls back to the two responses' own timestamps.
fn gap(previous_completed_at: DateTime<Utc>, call: &AnalyzedCacheCall) -> (u64, CacheGapTiming) {
    let seconds = |from: DateTime<Utc>, to: DateTime<Utc>| (to - from).num_seconds().max(0) as u64;
    match call.requested_at {
        Some(requested_at) if requested_at >= previous_completed_at => (
            seconds(previous_completed_at, requested_at),
            CacheGapTiming::RequestStart,
        ),
        _ => (
            seconds(previous_completed_at, call.completed_at),
            CacheGapTiming::RecordedEvents,
        ),
    }
}

fn loss(baseline: u64, cache_read_tokens: u64) -> Option<CacheLoss> {
    let drop = baseline.saturating_sub(cache_read_tokens);
    let suspected = drop >= CACHE_LOSS_MIN_TOKENS
        && u128::from(drop) * 100 >= u128::from(baseline) * u128::from(CACHE_LOSS_MIN_PERCENT);
    suspected.then_some(if cache_read_tokens == 0 {
        CacheLoss::Full
    } else {
        CacheLoss::Partial
    })
}
