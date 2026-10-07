use super::{
    cache_gap_bin, AnalyzedCacheCall, CacheBoundaryReason, CacheCallClass, CacheGapTiming,
    CacheLoss, CACHE_GAP_BIN_COUNT, CACHE_HEALTH_VERSION,
};
use crate::types::UsageEvent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Bounded prompt-cache diagnostics for one daily summary.
///
/// Only counts leave the device: no timestamps, ids, or per-call values. Token
/// totals stay on the summary's `usage`, so this adds only what they cannot
/// say. `requests` equals `unclassifiable + first_observed + boundaries +
/// below_threshold + uncached + comparable`, and the gap bins add up to
/// `comparable`, `losses`, and `missed_tokens`. Monthly rollups embed every
/// day's object, so the fields most days leave at zero are left out then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheHealthV1 {
    pub version: u32,
    /// Model calls of providers the detector covers.
    pub requests: u64,
    pub unclassifiable: u64,
    /// First calls of their stream.
    pub first_observed: u64,
    /// First calls that read nothing from the cache.
    pub first_cold: u64,
    /// Calls after a model change, compaction, or unclassifiable usage.
    pub boundaries: u64,
    /// Calls whose predecessor left too little reusable context to judge.
    pub below_threshold: u64,
    /// Calls that neither read nor wrote the cache, so did not use it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub uncached: u64,
    pub comparable: u64,
    /// Suspected cache losses among comparable calls.
    pub losses: u64,
    /// Suspected losses that read nothing from the cache.
    pub full_losses: u64,
    /// Reusable context suspected losses did not read from the cache.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub missed_tokens: u64,
    /// What that missed reuse cost at API prices, in micro-USD.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub missed_cost_micro_usd: u64,
    /// Suspected losses on models without known pricing, left out of
    /// `missed_cost_micro_usd`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unpriced_losses: u64,
    /// Comparable calls whose gap was measured between recorded responses
    /// rather than from a recorded request start.
    pub timing_estimated: u64,
    /// Ordinary input from calls whose provider did not report cache writes,
    /// so it may include writes.
    pub write_split_unavailable_input_tokens: u64,
    /// Comparable calls by gap since the previous call, one count per bin of
    /// [`super::CACHE_GAP_BIN_UPPER_MINUTES`]: `<1m` first, `60m+` last.
    pub gap_comparable: Vec<u64>,
    /// Suspected losses in the same bins.
    pub gap_losses: Vec<u64>,
    /// Missed tokens in the same bins.
    #[serde(default = "empty_gap_bins", skip_serializing_if = "all_zero")]
    pub gap_missed_tokens: Vec<u64>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn all_zero(values: &[u64]) -> bool {
    values.iter().all(|value| *value == 0)
}

fn empty_gap_bins() -> Vec<u64> {
    vec![0; CACHE_GAP_BIN_COUNT]
}

impl Default for CacheHealthV1 {
    fn default() -> Self {
        Self {
            version: CACHE_HEALTH_VERSION,
            requests: 0,
            unclassifiable: 0,
            first_observed: 0,
            first_cold: 0,
            boundaries: 0,
            below_threshold: 0,
            uncached: 0,
            comparable: 0,
            losses: 0,
            full_losses: 0,
            missed_tokens: 0,
            missed_cost_micro_usd: 0,
            unpriced_losses: 0,
            timing_estimated: 0,
            write_split_unavailable_input_tokens: 0,
            gap_comparable: empty_gap_bins(),
            gap_losses: empty_gap_bins(),
            gap_missed_tokens: empty_gap_bins(),
        }
    }
}

impl CacheHealthV1 {
    /// Calls the detector could place and classify.
    #[must_use]
    pub fn analyzed(&self) -> u64 {
        self.requests.saturating_sub(self.unclassifiable)
    }

    pub fn add_call(&mut self, call: &AnalyzedCacheCall) {
        self.requests = self.requests.saturating_add(call.requests);
        let verdict = &call.verdict;
        match verdict.class {
            CacheCallClass::Unclassifiable => {
                self.unclassifiable = self.unclassifiable.saturating_add(call.requests);
            }
            CacheCallClass::FirstObserved => {
                self.first_observed = self.first_observed.saturating_add(1);
                if call.cache_read_tokens == 0 {
                    self.first_cold = self.first_cold.saturating_add(1);
                }
            }
            CacheCallClass::Boundary => self.boundaries = self.boundaries.saturating_add(1),
            CacheCallClass::BelowThreshold => {
                self.below_threshold = self.below_threshold.saturating_add(1);
            }
            CacheCallClass::Uncached => self.uncached = self.uncached.saturating_add(1),
            CacheCallClass::Comparable => {
                self.comparable = self.comparable.saturating_add(1);
                if verdict.gap_timing == Some(CacheGapTiming::RecordedEvents) {
                    self.timing_estimated = self.timing_estimated.saturating_add(1);
                }
                let lost = verdict.loss.is_some();
                if lost {
                    self.losses = self.losses.saturating_add(1);
                }
                if verdict.loss == Some(CacheLoss::Full) {
                    self.full_losses = self.full_losses.saturating_add(1);
                }
                match (lost, verdict.missed_cost_micro_usd) {
                    (true, Some(cost)) => {
                        self.missed_cost_micro_usd =
                            self.missed_cost_micro_usd.saturating_add(cost);
                    }
                    (true, None) => self.unpriced_losses = self.unpriced_losses.saturating_add(1),
                    (false, _) => {}
                }
                let missed = verdict.missed_tokens.unwrap_or(0);
                self.missed_tokens = self.missed_tokens.saturating_add(missed);
                if let Some(bin) = verdict.gap_seconds.map(cache_gap_bin) {
                    if let Some(count) = self.gap_comparable.get_mut(bin) {
                        *count = count.saturating_add(1);
                    }
                    if let Some(count) = self.gap_losses.get_mut(bin).filter(|_| lost) {
                        *count = count.saturating_add(1);
                    }
                    if let Some(tokens) = self.gap_missed_tokens.get_mut(bin) {
                        *tokens = tokens.saturating_add(missed);
                    }
                }
            }
        }
    }

    /// Counts ordinary input as unsplit when its provider reported no cache
    /// writes. An event that lists its calls is counted call by call: summing
    /// one reported zero with unreported writes would hide the rest.
    pub fn add_event_input(&mut self, event: &UsageEvent) {
        let calls = event
            .context
            .as_ref()
            .map(|context| context.calls.as_slice())
            .unwrap_or_default();
        let unsplit = if !calls.is_empty() {
            calls
                .iter()
                .filter(|call| call.cache_creation_tokens.is_none())
                .map(|call| call.input_tokens.unwrap_or(0))
                .fold(0, u64::saturating_add)
        } else if event.usage.cache_creation_tokens.is_none() {
            event.usage.input_tokens.unwrap_or(0)
        } else {
            0
        };
        self.write_split_unavailable_input_tokens = self
            .write_split_unavailable_input_tokens
            .saturating_add(unsplit);
    }

    pub fn merge(&mut self, other: &Self) {
        self.requests = self.requests.saturating_add(other.requests);
        self.unclassifiable = self.unclassifiable.saturating_add(other.unclassifiable);
        self.first_observed = self.first_observed.saturating_add(other.first_observed);
        self.first_cold = self.first_cold.saturating_add(other.first_cold);
        self.boundaries = self.boundaries.saturating_add(other.boundaries);
        self.below_threshold = self.below_threshold.saturating_add(other.below_threshold);
        self.uncached = self.uncached.saturating_add(other.uncached);
        self.comparable = self.comparable.saturating_add(other.comparable);
        self.losses = self.losses.saturating_add(other.losses);
        self.full_losses = self.full_losses.saturating_add(other.full_losses);
        self.missed_tokens = self.missed_tokens.saturating_add(other.missed_tokens);
        self.missed_cost_micro_usd = self
            .missed_cost_micro_usd
            .saturating_add(other.missed_cost_micro_usd);
        self.unpriced_losses = self.unpriced_losses.saturating_add(other.unpriced_losses);
        self.timing_estimated = self.timing_estimated.saturating_add(other.timing_estimated);
        self.write_split_unavailable_input_tokens = self
            .write_split_unavailable_input_tokens
            .saturating_add(other.write_split_unavailable_input_tokens);
        for (count, other) in self.gap_comparable.iter_mut().zip(&other.gap_comparable) {
            *count = count.saturating_add(*other);
        }
        for (count, other) in self.gap_losses.iter_mut().zip(&other.gap_losses) {
            *count = count.saturating_add(*other);
        }
        for (tokens, other) in self
            .gap_missed_tokens
            .iter_mut()
            .zip(&other.gap_missed_tokens)
        {
            *tokens = tokens.saturating_add(*other);
        }
    }
}

/// Boundary counts by reason. Local only: the synced object carries the total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheBoundaryCounts {
    pub model_change: u64,
    pub compaction: u64,
    pub evidence_gap: u64,
}

impl CacheBoundaryCounts {
    pub fn add_call(&mut self, call: &AnalyzedCacheCall) {
        if call.verdict.class != CacheCallClass::Boundary {
            return;
        }
        let count = match call.verdict.boundary {
            Some(CacheBoundaryReason::ModelChange) => &mut self.model_change,
            Some(CacheBoundaryReason::Compaction) => &mut self.compaction,
            Some(CacheBoundaryReason::EvidenceGap) | None => &mut self.evidence_gap,
        };
        *count = count.saturating_add(1);
    }
}
