//! Prompt-cache diagnostics.
//!
//! The report answers three questions for a period: how much logical input was
//! served from the provider's prompt cache, when that reuse dropped, and how
//! much context the model calls processed regardless. The first and last come
//! straight from token totals. The second needs request order, so it runs a
//! conservative detector over each stream of model calls.
//!
//! A stream is one agent's calls within one session: the same provider, source,
//! account, session, transcript file, and sub-agent. Each call is compared only
//! with the call before it in its stream. The previous call leaves its cache
//! reads plus its known cache writes reusable; the current call is expected to
//! read about that much again, bounded by its own input size. When reads fall at
//! least half and at least [`CACHE_LOSS_MIN_TOKENS`] below that baseline, the
//! call is a suspected cache loss. That is a heuristic over recorded token
//! counts, not a provider-confirmed miss.
//!
//! Comparisons restart, rather than count as losses, at the first call of a
//! stream, after a model change, after compaction, and after a call whose
//! usage cannot be placed or split into calls. A call that neither read nor
//! wrote the cache did not use it, as when a custom endpoint serves the
//! request, so it is counted apart and not judged.

mod detector;
mod health;
mod report;

pub use detector::*;
pub use health::*;
pub use report::*;

/// Version of the detector rules and of the synced `cache_health` object.
pub const CACHE_HEALTH_VERSION: u32 = 1;

/// Smallest drop in cached reads that can count as a loss.
pub const CACHE_LOSS_MIN_TOKENS: u64 = 4_096;

/// A loss also needs reads to fall by at least this share of the baseline.
pub const CACHE_LOSS_MIN_PERCENT: u64 = 50;

/// Upper bounds, in minutes, of the request-gap bins. The last bin is open.
pub const CACHE_GAP_BIN_UPPER_MINUTES: [u64; 7] = [1, 5, 10, 15, 30, 45, 60];

/// Number of request-gap bins, including the open-ended last one.
pub const CACHE_GAP_BIN_COUNT: usize = CACHE_GAP_BIN_UPPER_MINUTES.len() + 1;

/// Providers whose usage events record calls in order, which the detector
/// needs. Token totals for every other provider still count toward hit rates.
pub const CACHE_DIAGNOSTIC_PROVIDERS: [&str; 2] = ["claude_code", "codex"];

#[must_use]
pub fn provider_has_cache_diagnostics(provider: &str) -> bool {
    CACHE_DIAGNOSTIC_PROVIDERS.contains(&provider)
}

/// Providers that report cache writes on every call that uses the cache, so a
/// call reporting no reads and no writes did not use it. Codex reports zero
/// writes on calls that do, so it is not one of them.
pub const CACHE_WRITE_REPORTING_PROVIDERS: [&str; 1] = ["claude_code"];

/// Index of the gap bin for a gap between calls. Bins are closed below and
/// open above, so a gap of exactly five minutes lands in `5–10`.
#[must_use]
pub fn cache_gap_bin(gap_seconds: u64) -> usize {
    CACHE_GAP_BIN_UPPER_MINUTES
        .iter()
        .position(|upper| gap_seconds < upper * 60)
        .unwrap_or(CACHE_GAP_BIN_COUNT - 1)
}

/// Label of a gap bin, such as `<1m`, `5–10m`, or `60m+`.
#[must_use]
pub fn cache_gap_bin_label(index: usize) -> String {
    let lower = index
        .checked_sub(1)
        .and_then(|previous| CACHE_GAP_BIN_UPPER_MINUTES.get(previous));
    match (lower, CACHE_GAP_BIN_UPPER_MINUTES.get(index)) {
        (None, Some(upper)) => format!("<{upper}m"),
        (Some(lower), Some(upper)) => format!("{lower}–{upper}m"),
        (Some(lower), None) => format!("{lower}m+"),
        (None, None) => String::new(),
    }
}

/// Cached reads as a share of all logical input: ordinary input, cache writes,
/// and cache reads. Token-weighted, so a long call counts for its size.
#[must_use]
pub fn cache_hit_ratio(
    input_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
) -> Option<f64> {
    let logical = input_tokens
        .saturating_add(cache_creation_tokens)
        .saturating_add(cache_read_tokens);
    (logical > 0).then(|| cache_read_tokens as f64 / logical as f64)
}
