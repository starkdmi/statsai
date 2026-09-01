use chrono::{Datelike, Utc};
use statsai_core::ModelInfo;

use crate::normalize::normalize_model_name;

#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub cache_creation_per_million: f64,
    pub cached_input_per_million: f64,
    pub output_per_million: f64,
}

const CLAUDE_SONNET_5_STANDARD_PRICING_START: (i32, u32, u32) = (2026, 9, 1);
const GPT_5_6_LUNA_TERRA_PRICE_CUT_START: (i32, u32, u32) = (2026, 7, 30);

fn pricing(
    input_per_million: f64,
    cached_input_per_million: f64,
    output_per_million: f64,
) -> ModelPricing {
    ModelPricing {
        input_per_million,
        cache_creation_per_million: input_per_million,
        cached_input_per_million,
        output_per_million,
    }
}

fn pricing_with_cache_creation(
    input_per_million: f64,
    cache_creation_per_million: f64,
    cached_input_per_million: f64,
    output_per_million: f64,
) -> ModelPricing {
    ModelPricing {
        input_per_million,
        cache_creation_per_million,
        cached_input_per_million,
        output_per_million,
    }
}

pub(crate) fn pricing_for_effective_speed(
    model_name: &str,
    speed: Option<&str>,
    standard: ModelPricing,
) -> (ModelPricing, bool) {
    let is_fast = speed.is_some_and(|speed| speed.trim().eq_ignore_ascii_case("fast"));
    if !is_fast {
        return (standard, false);
    }

    let fast = match model_name {
        "claude-opus-5" | "claude-opus-4-8" => pricing_with_cache_creation(10.0, 12.5, 1.0, 50.0),
        // Historical fast-mode rates. Effective `usage.speed` is authoritative:
        // unsupported requests either failed or reported `standard` after fallback.
        "claude-opus-4-6" | "claude-opus-4-7" => {
            pricing_with_cache_creation(30.0, 37.5, 3.0, 150.0)
        }
        _ => return (standard, false),
    };
    (fast, true)
}

#[must_use]
pub fn pricing_for_model(model_name: &str) -> Option<ModelPricing> {
    pricing_for_model_on(model_name, Utc::now().date_naive())
}

fn date_tuple(date: chrono::NaiveDate) -> (i32, u32, u32) {
    (date.year(), date.month(), date.day())
}

/// Maps an observed model to the catalog model whose rates should be reused.
///
/// Observed identity stays unchanged; this mapping is pricing-only.
pub(crate) fn api_equivalent_pricing_model(
    model_name: &str,
    usage_date: chrono::NaiveDate,
) -> Option<&'static str> {
    match model_name {
        "codex-auto-review" => {
            if date_tuple(usage_date) >= GPT_5_6_LUNA_TERRA_PRICE_CUT_START {
                Some("gpt-5.6-luna")
            } else {
                Some("gpt-5.4")
            }
        }
        _ => None,
    }
}

fn gpt_5_6_luna_pricing(usage_date: chrono::NaiveDate) -> ModelPricing {
    if date_tuple(usage_date) >= GPT_5_6_LUNA_TERRA_PRICE_CUT_START {
        pricing_with_cache_creation(0.2, 0.25, 0.02, 1.2)
    } else {
        pricing_with_cache_creation(1.0, 1.25, 0.1, 6.0)
    }
}

fn gpt_5_6_terra_pricing(usage_date: chrono::NaiveDate) -> ModelPricing {
    if date_tuple(usage_date) >= GPT_5_6_LUNA_TERRA_PRICE_CUT_START {
        pricing_with_cache_creation(2.0, 2.5, 0.2, 12.0)
    } else {
        pricing_with_cache_creation(2.5, 3.125, 0.25, 15.0)
    }
}

pub(crate) fn pricing_for_model_on(
    model_name: &str,
    usage_date: chrono::NaiveDate,
) -> Option<ModelPricing> {
    let normalized = model_name.to_ascii_lowercase();
    if let Some(mapped) = api_equivalent_pricing_model(&normalized, usage_date) {
        return pricing_for_model_on(mapped, usage_date);
    }
    match normalized.as_str() {
        "claude-fable-5" | "claude-mythos-5" => {
            Some(pricing_with_cache_creation(10.0, 12.5, 1.0, 50.0))
        }
        "claude-opus-5" => Some(pricing_with_cache_creation(5.0, 6.25, 0.5, 25.0)),
        "claude-sonnet-5" => {
            let date = (usage_date.year(), usage_date.month(), usage_date.day());
            if date >= CLAUDE_SONNET_5_STANDARD_PRICING_START {
                Some(pricing_with_cache_creation(3.0, 3.75, 0.3, 15.0))
            } else {
                Some(pricing_with_cache_creation(2.0, 2.5, 0.2, 10.0))
            }
        }
        "claude-opus-4" | "claude-opus-4-1" => {
            Some(pricing_with_cache_creation(15.0, 18.75, 1.5, 75.0))
        }
        "claude-opus-4-5" | "claude-opus-4-6" | "claude-opus-4-7" | "claude-opus-4-8" => {
            Some(pricing_with_cache_creation(5.0, 6.25, 0.5, 25.0))
        }
        "claude-sonnet-4" | "claude-sonnet-4-5" | "claude-sonnet-4-6" => {
            Some(pricing_with_cache_creation(3.0, 3.75, 0.3, 15.0))
        }
        "claude-haiku-4-5" => Some(pricing_with_cache_creation(1.0, 1.25, 0.1, 5.0)),
        // GPT-5.6 uses a 1.25x cache-write multiplier and a 90% cache-read discount.
        "gpt-5.6-sol" => Some(pricing_with_cache_creation(5.0, 6.25, 0.5, 30.0)),
        "gpt-5.6-terra" => Some(gpt_5_6_terra_pricing(usage_date)),
        "gpt-5.6-luna" => Some(gpt_5_6_luna_pricing(usage_date)),
        "gpt-5.5" => Some(pricing(5.0, 0.5, 30.0)),
        "gpt-5.4" => Some(pricing(2.5, 0.25, 15.0)),
        "gpt-5.4-mini" => Some(pricing(0.75, 0.075, 4.5)),
        "gpt-5.3-codex" | "gpt-5.2" | "gpt-5.2-chat-latest" | "gpt-5.2-codex" => {
            Some(pricing(1.75, 0.175, 14.0))
        }
        "gpt-5-codex"
        | "gpt-5.1-codex"
        | "gpt-5.1-codex-max"
        | "gpt-5"
        | "gpt-5.1"
        | "gpt-5-chat-latest"
        | "gpt-5.1-chat-latest" => Some(pricing(1.25, 0.125, 10.0)),
        "gpt-5-mini" | "gpt-5.1-codex-mini" => Some(pricing(0.25, 0.025, 2.0)),
        "gpt-5-nano" => Some(pricing(0.05, 0.005, 0.4)),
        "composer-2.5" => Some(pricing(0.5, 0.2, 2.5)),
        "composer-2.5-fast" => Some(pricing(3.0, 0.5, 15.0)),
        "grok-build-0.1" => Some(pricing(1.0, 0.2, 2.0)),
        "grok-4.3"
        | "grok-4.20-multi-agent-0309"
        | "grok-4.20-0309-reasoning"
        | "grok-4.20-0309-non-reasoning" => Some(pricing(1.25, 0.2, 2.5)),
        "grok-4.5" => Some(pricing(2.0, 0.3, 6.0)),
        // Official Grok 4.6 cached-input rate is $0.50/M below 200k prompt tokens
        // ($1.00/M at or above 200k): https://docs.x.ai/developers/models/grok-4.6
        "grok-4.6" => Some(pricing(2.0, 0.5, 6.0)),
        _ => None,
    }
}

/// Reports whether an aggregate usage period spans a known model-price change.
///
/// Aggregates without daily token allocation cannot be priced accurately across
/// these boundaries because the token share on each side is unknown.
#[must_use]
pub fn pricing_changes_between(
    model_name: &str,
    period_start: chrono::NaiveDate,
    period_end: chrono::NaiveDate,
) -> bool {
    let (period_start, period_end) = if period_start <= period_end {
        (period_start, period_end)
    } else {
        (period_end, period_start)
    };
    let start = date_tuple(period_start);
    let end = date_tuple(period_end);

    match normalize_model_name(model_name).as_str() {
        "claude-sonnet-5" => {
            start < CLAUDE_SONNET_5_STANDARD_PRICING_START
                && end >= CLAUDE_SONNET_5_STANDARD_PRICING_START
        }
        "gpt-5.6-luna" | "gpt-5.6-terra" | "codex-auto-review" => {
            start < GPT_5_6_LUNA_TERRA_PRICE_CUT_START && end >= GPT_5_6_LUNA_TERRA_PRICE_CUT_START
        }
        _ => false,
    }
}

pub(crate) fn priced_model_name(
    model: &ModelInfo,
    usage_date: chrono::NaiveDate,
) -> Option<String> {
    let candidates = [
        model.normalized_name.as_deref(),
        model.name.as_deref(),
        model.provider_model_id.as_deref(),
    ];

    for candidate in candidates.into_iter().flatten() {
        let normalized = normalize_model_name(candidate);
        if pricing_for_model_on(&normalized, usage_date).is_some() {
            return Some(normalized);
        }
        if let Some((_, suffix)) = normalized.rsplit_once('/') {
            let suffix = normalize_model_name(suffix);
            if pricing_for_model_on(&suffix, usage_date).is_some() {
                return Some(suffix);
            }
        }
    }

    None
}
