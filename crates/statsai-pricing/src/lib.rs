//! Model pricing helpers for `statsai`.
//!
//! Provides static model pricing lookup and cost estimation
//! decoupled from any specific adapter.

use chrono::{DateTime, Datelike, Utc};
use statsai_core::{micro_usd_to_cents_rounded, Confidence, CostInfo, ModelInfo, UsageCounts};

const PRICING_CATALOG_VERSION: &str = "official:2026-08-19";
const MICRO_USD_PER_USD: i128 = 1_000_000;
const TOKENS_PER_MILLION: i128 = 1_000_000;
const MULTIPLIER_SCALE: i128 = 10_000;

fn normalize_proxy_wrapped_model_name(lower: &str) -> Option<&'static str> {
    if lower.contains("claude-fable-5") {
        return Some("claude-fable-5");
    }
    if lower.contains("claude-mythos-5") {
        return Some("claude-mythos-5");
    }
    if lower.contains("claude-opus-5") {
        return Some("claude-opus-5");
    }
    if lower.contains("claude-sonnet-5") {
        return Some("claude-sonnet-5");
    }
    if lower.contains("claude-opus-4-8") || lower.contains("claude-opus-4.8") {
        return Some("claude-opus-4-8");
    }
    if lower.contains("claude-opus-4-7") || lower.contains("claude-opus-4.7") {
        return Some("claude-opus-4-7");
    }
    if lower.contains("claude-opus-4-6") || lower.contains("claude-opus-4.6") {
        return Some("claude-opus-4-6");
    }
    if lower.contains("claude-opus-4-5") || lower.contains("claude-opus-4.5") {
        return Some("claude-opus-4-5");
    }
    if lower.contains("claude-opus-4-1") || lower.contains("claude-opus-4.1") {
        return Some("claude-opus-4-1");
    }
    if lower.contains("claude-sonnet-4-6") || lower.contains("claude-sonnet-4.6") {
        return Some("claude-sonnet-4-6");
    }
    if lower.contains("claude-sonnet-4-5") || lower.contains("claude-sonnet-4.5") {
        return Some("claude-sonnet-4-5");
    }
    if lower.contains("claude-haiku-4-5") || lower.contains("claude-haiku-4.5") {
        return Some("claude-haiku-4-5");
    }
    if lower.contains("claude-sonnet-4") {
        return Some("claude-sonnet-4");
    }
    if lower.contains("claude-opus-4") {
        return Some("claude-opus-4");
    }
    if lower.contains("claude-sonnet-3-7") || lower.contains("claude-3-7-sonnet") {
        return Some("claude-sonnet-3-7");
    }
    if lower.contains("claude-sonnet-3-5") || lower.contains("claude-3-5-sonnet-20241022") {
        return Some("claude-sonnet-3-5");
    }
    if lower.contains("claude-haiku-3-5") || lower.contains("claude-haiku-3.5") {
        return Some("claude-haiku-3-5");
    }
    if lower.contains("gpt-5.6-sol") {
        return Some("gpt-5.6-sol");
    }
    if lower.contains("gpt-5.6-terra") {
        return Some("gpt-5.6-terra");
    }
    if lower.contains("gpt-5.6-luna") {
        return Some("gpt-5.6-luna");
    }
    if lower.contains("gpt-5.5") {
        return Some("gpt-5.5");
    }
    if lower.contains("gpt-5.4-mini") {
        return Some("gpt-5.4-mini");
    }
    if lower.contains("gpt-5.4") {
        return Some("gpt-5.4");
    }
    if lower.contains("codex-auto-review") {
        return Some("codex-auto-review");
    }
    if lower.contains("gpt-5.1-codex-mini") {
        return Some("gpt-5-mini");
    }
    if lower.contains("gpt-5.1-codex-max") {
        return Some("gpt-5.1-codex-max");
    }
    if lower.contains("gpt-5.3-codex") {
        return Some("gpt-5.3-codex");
    }
    if lower.contains("gpt-5.2-codex")
        || lower.contains("gpt-5.2-chat-latest")
        || lower.contains("gpt-5.2")
    {
        return Some("gpt-5.2");
    }
    if lower.contains("gpt-5.1-codex") {
        return Some("gpt-5-codex");
    }
    if lower.contains("gpt-5.1-chat-latest") || lower.contains("gpt-5.1") {
        return Some("gpt-5.1");
    }
    if lower.contains("gpt-5-mini") {
        return Some("gpt-5-mini");
    }
    if lower.contains("gpt-5-nano") {
        return Some("gpt-5-nano");
    }
    if lower.contains("gpt-5-chat-latest") || lower.contains("gpt-5") {
        return Some("gpt-5");
    }
    if lower.contains("grok-composer-2.5-fast") || lower.contains("composer-2.5-fast") {
        return Some("composer-2.5-fast");
    }
    if lower.contains("grok-composer-2.5") || lower.contains("composer-2.5") {
        return Some("composer-2.5");
    }
    if lower.contains("grok-4.6") {
        return Some("grok-4.6");
    }
    if lower.contains("grok-4.5-latest")
        || lower.contains("grok-4.5")
        || lower.contains("grok-build-latest")
    {
        return Some("grok-4.5");
    }
    if lower.contains("grok-build-0.1") || lower.contains("grok-build") {
        return Some("grok-build-0.1");
    }
    if lower.contains("grok-4.3-latest") || lower.contains("grok-4.3") {
        return Some("grok-4.3");
    }
    if lower.contains("grok-4.20-multi-agent-0309") {
        return Some("grok-4.20-multi-agent-0309");
    }
    if lower.contains("grok-4.20-0309-reasoning") {
        return Some("grok-4.20-0309-reasoning");
    }
    if lower.contains("grok-4.20-0309-non-reasoning") {
        return Some("grok-4.20-0309-non-reasoning");
    }
    None
}

#[must_use]
pub fn normalize_model_name(name: &str) -> String {
    let name = name.trim();
    let name = name
        .strip_prefix("anthropic/")
        .or_else(|| name.strip_prefix("openai/"))
        .unwrap_or(name);

    let lower = name.to_ascii_lowercase();

    match lower.as_str() {
        "claude-fable-5" => "claude-fable-5".to_string(),
        "claude-mythos-5" => "claude-mythos-5".to_string(),
        "claude-opus-5" | "claude-opus-5-thinking" => "claude-opus-5".to_string(),
        "claude-sonnet-5" | "claude-sonnet-5-thinking" => "claude-sonnet-5".to_string(),
        "claude-3-5-sonnet-20241022" | "claude-sonnet-3-5" => "claude-sonnet-3-5".to_string(),
        "claude-3-7-sonnet" | "claude-sonnet-3-7" => "claude-sonnet-3-7".to_string(),
        "claude-opus-4" => "claude-opus-4".to_string(),
        "claude-opus-4-1" | "claude-opus-4.1" => "claude-opus-4-1".to_string(),
        "claude-opus-4-5" | "claude-opus-4-5-thinking" | "claude-opus-4.5" => {
            "claude-opus-4-5".to_string()
        }
        "claude-opus-4-6" | "claude-opus-4-6-thinking" | "claude-opus-4.6" => {
            "claude-opus-4-6".to_string()
        }
        "claude-opus-4-7" | "claude-opus-4-7-thinking" | "claude-opus-4.7" => {
            "claude-opus-4-7".to_string()
        }
        "claude-opus-4-8" | "claude-opus-4-8-thinking" | "claude-opus-4.8" => {
            "claude-opus-4-8".to_string()
        }
        "claude-sonnet-4" => "claude-sonnet-4".to_string(),
        "claude-sonnet-4-5" | "claude-sonnet-4.5" => "claude-sonnet-4-5".to_string(),
        "claude-sonnet-4-6" | "claude-sonnet-4-6-thinking" | "claude-sonnet-4.6" => {
            "claude-sonnet-4-6".to_string()
        }
        "claude-haiku-4-5" | "claude-haiku-4.5" => "claude-haiku-4-5".to_string(),
        "claude-haiku-3-5" | "claude-haiku-3.5" => "claude-haiku-3-5".to_string(),
        "gpt-5" | "gpt-5-chat-latest" => "gpt-5".to_string(),
        "gpt-5.1" | "gpt-5.1-chat-latest" => "gpt-5.1".to_string(),
        "gpt-5-codex" | "gpt-5.1-codex" => "gpt-5-codex".to_string(),
        "gpt-5.1-codex-max" => "gpt-5.1-codex-max".to_string(),
        "gpt-5.1-codex-mini" => "gpt-5-mini".to_string(),
        "gpt-5.2" | "gpt-5.2-chat-latest" | "gpt-5.2-codex" => "gpt-5.2".to_string(),
        "gpt-5.3-codex" => "gpt-5.3-codex".to_string(),
        "codex-auto-review" => "codex-auto-review".to_string(),
        "gpt-5.4" => "gpt-5.4".to_string(),
        "gpt-5.4-mini" => "gpt-5.4-mini".to_string(),
        "gpt-5.6-sol" => "gpt-5.6-sol".to_string(),
        "gpt-5.6-terra" => "gpt-5.6-terra".to_string(),
        "gpt-5.6-luna" => "gpt-5.6-luna".to_string(),
        "gpt-5.5" => "gpt-5.5".to_string(),
        "gpt-5-mini" => "gpt-5-mini".to_string(),
        "gpt-5-nano" => "gpt-5-nano".to_string(),
        "composer-2.5" | "grok-composer-2.5" => "composer-2.5".to_string(),
        "composer-2.5-fast" | "grok-composer-2.5-fast" => "composer-2.5-fast".to_string(),
        "grok-build" | "grok-build-0.1" => "grok-build-0.1".to_string(),
        "grok-4.6" | "grok-4.6-build" | "grok-4.6-latest" => "grok-4.6".to_string(),
        "grok-4.5" | "grok-4.5-latest" | "grok-build-latest" => "grok-4.5".to_string(),
        "grok-4.3" | "grok-4.3-latest" => "grok-4.3".to_string(),
        "grok-4.20-multi-agent-0309" => "grok-4.20-multi-agent-0309".to_string(),
        "grok-4.20-0309-reasoning" => "grok-4.20-0309-reasoning".to_string(),
        "grok-4.20-0309-non-reasoning" => "grok-4.20-0309-non-reasoning".to_string(),
        _ => normalize_proxy_wrapped_model_name(&lower)
            .map(ToString::to_string)
            .unwrap_or_else(|| name.to_ascii_lowercase()),
    }
}

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

fn pricing_for_effective_speed(
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
fn api_equivalent_pricing_model(
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

fn pricing_for_model_on(model_name: &str, usage_date: chrono::NaiveDate) -> Option<ModelPricing> {
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

fn priced_model_name(model: &ModelInfo, usage_date: chrono::NaiveDate) -> Option<String> {
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

#[must_use]
pub fn estimate_cost(provider: &str, model: Option<&ModelInfo>, usage: &UsageCounts) -> CostInfo {
    let now = Utc::now();
    estimate_cost_at(provider, model, usage, &now)
}

#[must_use]
pub fn estimate_cost_at(
    provider: &str,
    model: Option<&ModelInfo>,
    usage: &UsageCounts,
    occurred_at: &DateTime<Utc>,
) -> CostInfo {
    let usage_date = occurred_at.date_naive();
    let Some(model) = model else {
        return unknown_cost();
    };
    let Some(observed_name) = priced_model_name(model, usage_date) else {
        return unknown_cost();
    };
    let model_name = api_equivalent_pricing_model(&observed_name, usage_date)
        .map(ToString::to_string)
        .unwrap_or_else(|| observed_name.clone());
    let Some(standard_pricing) = pricing_for_model_on(&model_name, usage_date) else {
        return unknown_cost();
    };
    let (pricing, uses_fast_mode_pricing) =
        pricing_for_effective_speed(&model_name, model.speed.as_deref(), standard_pricing);

    let (input_multiplier, output_multiplier) = pricing_multipliers(&model_name, usage);
    let mut numerator = component_cost_numerator(
        i128::from(usage.input_tokens.unwrap_or(0)),
        pricing.input_per_million,
        input_multiplier,
    );
    numerator = numerator.saturating_add(cache_creation_cost_numerator(
        &model_name,
        pricing,
        usage,
        input_multiplier,
    ));
    numerator = numerator.saturating_add(component_cost_numerator(
        i128::from(usage.cache_read_tokens.unwrap_or(0)),
        pricing.cached_input_per_million,
        input_multiplier,
    ));
    let generated_tokens = i128::from(usage.output_tokens.unwrap_or(0))
        .saturating_add(i128::from(usage.reasoning_tokens.unwrap_or(0)));
    numerator = numerator.saturating_add(component_cost_numerator(
        generated_tokens,
        pricing.output_per_million,
        output_multiplier,
    ));
    let denominator = TOKENS_PER_MILLION.saturating_mul(MULTIPLIER_SCALE);
    let cost_micro_usd = rounded_i128_to_i64(numerator, denominator);
    let cost_cents = micro_usd_to_cents_rounded(cost_micro_usd);

    let mut pricing_source = match model_name.as_str() {
        "composer-2.5" | "composer-2.5-fast" => format!("cursor_model_pricing:{observed_name}"),
        "grok-build-0.1"
        | "grok-4.3"
        | "grok-4.5"
        | "grok-4.6"
        | "grok-4.20-multi-agent-0309"
        | "grok-4.20-0309-reasoning"
        | "grok-4.20-0309-non-reasoning" => {
            format!("xai_api_pricing:{observed_name}")
        }
        _ => format!("{provider}_api_pricing:{observed_name}"),
    };
    if uses_fast_mode_pricing {
        pricing_source.push_str(":fast");
    }

    CostInfo {
        currency: "USD".to_string(),
        estimated_api_equivalent_usd: Some(cost_cents),
        provider_reported_usd: None,
        estimated_api_equivalent_micro_usd: Some(cost_micro_usd),
        provider_reported_micro_usd: None,
        pricing_source: Some(pricing_source),
        pricing_version: Some(PRICING_CATALOG_VERSION.to_string()),
        confidence: Confidence::Medium,
    }
}

fn dollars_per_million_to_micro_usd(rate: f64) -> i128 {
    (rate * MICRO_USD_PER_USD as f64).round() as i128
}

fn component_cost_numerator(tokens: i128, rate: f64, multiplier: i128) -> i128 {
    tokens
        .saturating_mul(dollars_per_million_to_micro_usd(rate))
        .saturating_mul(multiplier)
}

fn rounded_i128_to_i64(numerator: i128, denominator: i128) -> i64 {
    let rounded = numerator.saturating_add(denominator / 2) / denominator;
    i64::try_from(rounded).unwrap_or(i64::MAX)
}

fn cache_creation_cost_numerator(
    model_name: &str,
    pricing: ModelPricing,
    usage: &UsageCounts,
    multiplier: i128,
) -> i128 {
    let total = usage.cache_creation_tokens.unwrap_or(0);
    if !model_name.starts_with("claude-") {
        return component_cost_numerator(
            i128::from(total),
            pricing.cache_creation_per_million,
            multiplier,
        );
    }

    let one_hour = usage.cache_creation_1h_tokens.unwrap_or(0).min(total);
    let five_minute = usage
        .cache_creation_5m_tokens
        .unwrap_or(0)
        .min(total.saturating_sub(one_hour));
    let unclassified = total.saturating_sub(one_hour.saturating_add(five_minute));
    let default_lifetime = five_minute.saturating_add(unclassified);
    component_cost_numerator(
        i128::from(default_lifetime),
        pricing.cache_creation_per_million,
        multiplier,
    )
    .saturating_add(component_cost_numerator(
        i128::from(one_hour),
        pricing.input_per_million * 2.0,
        multiplier,
    ))
}

fn pricing_multipliers(model_name: &str, usage: &UsageCounts) -> (i128, i128) {
    const OPENAI_LONG_CONTEXT_THRESHOLD: u64 = 272_000;
    const XAI_LONG_CONTEXT_THRESHOLD: u64 = 200_000;

    let prompt_tokens = usage
        .input_tokens
        .unwrap_or(0)
        .saturating_add(usage.cache_creation_tokens.unwrap_or(0))
        .saturating_add(usage.cache_read_tokens.unwrap_or(0));
    let is_openai_long_context_model = matches!(
        model_name,
        "gpt-5.4" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna"
    );
    let is_xai_long_context_model = matches!(
        model_name,
        "grok-build-0.1"
            | "grok-4.3"
            | "grok-4.5"
            | "grok-4.6"
            | "grok-4.20-multi-agent-0309"
            | "grok-4.20-0309-reasoning"
            | "grok-4.20-0309-non-reasoning"
    );
    if is_openai_long_context_model
        && usage.requests == Some(1)
        && prompt_tokens > OPENAI_LONG_CONTEXT_THRESHOLD
    {
        (20_000, 15_000)
    } else if is_xai_long_context_model
        && usage.requests == Some(1)
        && prompt_tokens >= XAI_LONG_CONTEXT_THRESHOLD
    {
        (20_000, 20_000)
    } else {
        (MULTIPLIER_SCALE, MULTIPLIER_SCALE)
    }
}

#[must_use]
pub fn unknown_cost() -> CostInfo {
    CostInfo {
        currency: "USD".to_string(),
        estimated_api_equivalent_usd: None,
        provider_reported_usd: None,
        estimated_api_equivalent_micro_usd: None,
        provider_reported_micro_usd: None,
        pricing_source: Some("unknown".to_string()),
        pricing_version: None,
        confidence: Confidence::Low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use statsai_core::UsageCounts;

    #[test]
    fn normalizes_claude_thinking_variant() {
        assert_eq!(
            normalize_model_name("claude-opus-4-5-thinking"),
            "claude-opus-4-5"
        );
        assert_eq!(
            normalize_model_name("claude-opus-4-6-thinking"),
            "claude-opus-4-6"
        );
        assert_eq!(
            normalize_model_name("claude-sonnet-4-6-thinking"),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn normalizes_codex_aliases() {
        assert_eq!(normalize_model_name("gpt-5.1-codex"), "gpt-5-codex");
        assert_eq!(normalize_model_name("gpt-5.1-codex-mini"), "gpt-5-mini");
    }

    #[test]
    fn normalizes_gpt_5_6_family_and_proxy_wrapped_ids() {
        assert_eq!(normalize_model_name("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(normalize_model_name("gpt-5.6-terra"), "gpt-5.6-terra");
        assert_eq!(normalize_model_name("gpt-5.6-luna"), "gpt-5.6-luna");
        assert_eq!(
            normalize_model_name("relay/openai-gpt-5.6-terra"),
            "gpt-5.6-terra"
        );
    }

    #[test]
    fn normalizes_provider_prefixes() {
        assert_eq!(
            normalize_model_name("anthropic/claude-sonnet-4-5"),
            "claude-sonnet-4-5"
        );
        assert_eq!(normalize_model_name("openai/gpt-5"), "gpt-5");
        assert_eq!(normalize_model_name("openai/gpt-5.2-codex"), "gpt-5.2");
        assert_eq!(normalize_model_name("openai/gpt-5.4"), "gpt-5.4");
    }

    #[test]
    fn normalizes_proxy_wrapped_model_names() {
        assert_eq!(
            normalize_model_name("google/antigravity-claude-opus-4-5-thinking"),
            "claude-opus-4-5"
        );
        assert_eq!(
            normalize_model_name("openrouter/claude-opus-4-6-thinking"),
            "claude-opus-4-6"
        );
        assert_eq!(
            normalize_model_name("openrouter/claude-opus-4-8-thinking"),
            "claude-opus-4-8"
        );
        assert_eq!(
            normalize_model_name("openrouter/claude-sonnet-4-6-thinking"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            normalize_model_name("google/antigravity-claude-sonnet-4-5-thinking"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model_name("relay/openai-gpt-5.2-codex"),
            "gpt-5.2"
        );
        assert_eq!(
            normalize_model_name("relay/openai-gpt-5-mini"),
            "gpt-5-mini"
        );
        assert_eq!(
            normalize_model_name("relay/openai-gpt-5-nano"),
            "gpt-5-nano"
        );
    }

    #[test]
    fn normalizes_unknown_model_to_lowercase() {
        assert_eq!(normalize_model_name("SomeNewModel"), "somenewmodel");
    }

    #[test]
    fn normalizes_grok_build_aliases() {
        assert_eq!(normalize_model_name("grok-build"), "grok-build-0.1");
        assert_eq!(normalize_model_name("grok-4.5-latest"), "grok-4.5");
        assert_eq!(normalize_model_name("grok-build-latest"), "grok-4.5");
        assert_eq!(normalize_model_name("openrouter/x-ai/grok-4.5"), "grok-4.5");
        assert_eq!(
            normalize_model_name("openrouter/x-ai/grok-build-latest"),
            "grok-4.5"
        );
        assert_eq!(normalize_model_name("grok-4.6"), "grok-4.6");
        assert_eq!(normalize_model_name("grok-4.6-build"), "grok-4.6");
        assert_eq!(normalize_model_name("grok-4.6-latest"), "grok-4.6");
        assert_eq!(normalize_model_name("openrouter/x-ai/grok-4.6"), "grok-4.6");
        assert_eq!(
            normalize_model_name("openrouter/x-ai/grok-4.6-build"),
            "grok-4.6"
        );
        assert_eq!(normalize_model_name("relay/xai-grok-4.6"), "grok-4.6");
    }

    #[test]
    fn normalizes_cursor_composer_aliases() {
        assert_eq!(
            normalize_model_name("grok-composer-2.5-fast"),
            "composer-2.5-fast"
        );
        assert_eq!(normalize_model_name("grok-composer-2.5"), "composer-2.5");
        assert_eq!(
            pricing_for_model("composer-2.5-fast").map(|pricing| (
                pricing.input_per_million,
                pricing.cached_input_per_million,
                pricing.output_per_million
            )),
            Some((3.0, 0.5, 15.0))
        );
    }

    #[test]
    fn normalizes_whitespace() {
        assert_eq!(normalize_model_name("  gpt-5  "), "gpt-5");
    }

    #[test]
    fn estimates_cost_for_known_model() {
        let model = statsai_core::ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
            ..UsageCounts::default()
        };
        let cost = estimate_cost("codex", Some(&model), &usage);
        assert!(cost.estimated_api_equivalent_usd.is_some());
        assert!(cost
            .pricing_source
            .as_deref()
            .unwrap()
            .starts_with("codex_api_pricing"));
    }

    #[test]
    fn estimates_cost_for_provider_prefixed_model() {
        let model = statsai_core::ModelInfo {
            name: Some("xai/grok-build-0.1".to_string()),
            normalized_name: Some("xai/grok-build-0.1".to_string()),
            provider_model_id: Some("xai/grok-build-0.1".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
            ..UsageCounts::default()
        };

        let cost = estimate_cost("opencode", Some(&model), &usage);

        assert_eq!(cost.estimated_api_equivalent_usd, Some(200));
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some("xai_api_pricing:grok-build-0.1")
        );
    }

    #[test]
    fn estimates_grok_4_5_cost_with_cached_input() {
        let model = statsai_core::ModelInfo {
            name: Some("grok-4.5-latest".to_string()),
            normalized_name: Some("grok-4.5".to_string()),
            provider_model_id: Some("grok-4.5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };

        let cost = estimate_cost("grok_build", Some(&model), &usage);

        assert_eq!(cost.estimated_api_equivalent_usd, Some(830));
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some("xai_api_pricing:grok-4.5")
        );
    }

    #[test]
    fn estimates_grok_4_3_cost_with_discounted_cached_input() {
        let model = statsai_core::ModelInfo {
            name: Some("grok-4.3-latest".to_string()),
            normalized_name: Some("grok-4.3".to_string()),
            provider_model_id: Some("grok-4.3".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };

        let cost = estimate_cost("grok_build", Some(&model), &usage);

        assert_eq!(cost.estimated_api_equivalent_usd, Some(395));
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some("xai_api_pricing:grok-4.3")
        );
    }

    #[test]
    fn estimates_cost_for_proxy_wrapped_claude_model() {
        let model = statsai_core::ModelInfo {
            name: Some("google/antigravity-claude-opus-4-5-thinking".to_string()),
            normalized_name: Some("google/antigravity-claude-opus-4-5-thinking".to_string()),
            provider_model_id: Some("google/antigravity-claude-opus-4-5-thinking".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };

        let cost = estimate_cost("opencode", Some(&model), &usage);

        assert_eq!(cost.estimated_api_equivalent_usd, Some(3050));
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some("opencode_api_pricing:claude-opus-4-5")
        );
    }

    #[test]
    fn estimates_cost_for_claude_family_alias() {
        let model = statsai_core::ModelInfo {
            name: Some("claude-opus-4-6-thinking".to_string()),
            normalized_name: Some("claude-opus-4-6".to_string()),
            provider_model_id: Some("claude-opus-4-6-thinking".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };

        let cost = estimate_cost("claude_code", Some(&model), &usage);

        assert_eq!(cost.estimated_api_equivalent_usd, Some(3050));
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some("claude_code_api_pricing:claude-opus-4-6")
        );
    }

    #[test]
    fn claude_fast_mode_prices_all_token_categories_at_premium_rates() {
        let standard_model = statsai_core::ModelInfo {
            name: Some("claude-opus-5".to_string()),
            normalized_name: Some("claude-opus-5".to_string()),
            provider_model_id: Some("claude-opus-5".to_string()),
            speed: Some("standard".to_string()),
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let fast_model = statsai_core::ModelInfo {
            speed: Some("fast".to_string()),
            ..standard_model.clone()
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_creation_tokens: Some(2_000_000),
            cache_creation_5m_tokens: Some(1_000_000),
            cache_creation_1h_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };

        let standard = estimate_cost("claude_code", Some(&standard_model), &usage);
        let fast = estimate_cost("claude_code", Some(&fast_model), &usage);

        assert_eq!(standard.estimated_api_equivalent_usd, Some(4_675));
        assert_eq!(
            standard.estimated_api_equivalent_micro_usd,
            Some(46_750_000)
        );
        assert_eq!(fast.estimated_api_equivalent_usd, Some(9_350));
        assert_eq!(fast.estimated_api_equivalent_micro_usd, Some(93_500_000));
        assert_eq!(
            fast.pricing_source.as_deref(),
            Some("claude_code_api_pricing:claude-opus-5:fast")
        );
    }

    #[test]
    fn historical_claude_fast_mode_uses_the_six_times_opus_rate() {
        let standard_model = statsai_core::ModelInfo {
            name: Some("claude-opus-4-7".to_string()),
            normalized_name: Some("claude-opus-4-7".to_string()),
            provider_model_id: Some("claude-opus-4-7".to_string()),
            speed: Some("standard".to_string()),
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let fast_model = statsai_core::ModelInfo {
            speed: Some("fast".to_string()),
            ..standard_model.clone()
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };

        let standard = estimate_cost("claude_code", Some(&standard_model), &usage);
        let fast = estimate_cost("claude_code", Some(&fast_model), &usage);

        assert_eq!(standard.estimated_api_equivalent_usd, Some(3_000));
        assert_eq!(fast.estimated_api_equivalent_usd, Some(18_000));
        assert_eq!(
            fast.pricing_source.as_deref(),
            Some("claude_code_api_pricing:claude-opus-4-7:fast")
        );
    }

    #[test]
    fn estimates_cost_for_legacy_claude_opus_4() {
        let model = statsai_core::ModelInfo {
            name: Some("claude-opus-4".to_string()),
            normalized_name: Some("claude-opus-4".to_string()),
            provider_model_id: Some("claude-opus-4".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };

        let cost = estimate_cost("claude_code", Some(&model), &usage);

        assert_eq!(cost.estimated_api_equivalent_usd, Some(9150));
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some("claude_code_api_pricing:claude-opus-4")
        );
    }

    #[test]
    fn estimates_cost_for_provider_prefixed_openai_models() {
        let model = statsai_core::ModelInfo {
            name: Some("openai/gpt-5.2-codex".to_string()),
            normalized_name: Some("openai/gpt-5.2-codex".to_string()),
            provider_model_id: Some("openai/gpt-5.2-codex".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
            ..UsageCounts::default()
        };

        let cost = estimate_cost("opencode", Some(&model), &usage);

        assert_eq!(cost.estimated_api_equivalent_usd, Some(875));
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some("opencode_api_pricing:gpt-5.2")
        );

        let model = statsai_core::ModelInfo {
            name: Some("openai/gpt-5.4".to_string()),
            normalized_name: Some("openai/gpt-5.4".to_string()),
            provider_model_id: Some("openai/gpt-5.4".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let cost = estimate_cost("opencode", Some(&model), &usage);

        assert_eq!(cost.estimated_api_equivalent_usd, Some(1000));
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some("opencode_api_pricing:gpt-5.4")
        );
    }

    #[test]
    fn gpt_5_4_long_context_pricing_requires_one_explicit_request() {
        let model = statsai_core::ModelInfo {
            name: Some("gpt-5.4".to_string()),
            normalized_name: Some("gpt-5.4".to_string()),
            provider_model_id: Some("gpt-5.4".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let boundary_usage = UsageCounts {
            input_tokens: Some(272_000),
            output_tokens: Some(1_000_000),
            requests: Some(1),
            ..UsageCounts::default()
        };
        let long_cached_usage = UsageCounts {
            input_tokens: Some(100_000),
            cache_read_tokens: Some(200_000),
            output_tokens: Some(1_000_000),
            requests: Some(1),
            ..UsageCounts::default()
        };
        let aggregate_usage = UsageCounts {
            input_tokens: Some(100_000),
            cache_read_tokens: Some(200_000),
            output_tokens: Some(1_000_000),
            requests: Some(2),
            ..UsageCounts::default()
        };

        let boundary_cost = estimate_cost("codex", Some(&model), &boundary_usage);
        let long_cached_cost = estimate_cost("codex", Some(&model), &long_cached_usage);
        let aggregate_cost = estimate_cost("codex", Some(&model), &aggregate_usage);

        assert_eq!(boundary_cost.estimated_api_equivalent_usd, Some(1568));
        assert_eq!(long_cached_cost.estimated_api_equivalent_usd, Some(2310));
        assert_eq!(aggregate_cost.estimated_api_equivalent_usd, Some(1530));
    }

    #[test]
    fn unknown_model_returns_unknown_cost() {
        let model = statsai_core::ModelInfo {
            name: Some("unknown-model".to_string()),
            normalized_name: Some("unknown-model".to_string()),
            provider_model_id: Some("unknown-model".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            total_tokens: Some(100),
            ..UsageCounts::default()
        };
        let cost = estimate_cost("codex", Some(&model), &usage);
        assert_eq!(cost.confidence, Confidence::Low);
        assert!(cost.estimated_api_equivalent_usd.is_none());
    }

    #[test]
    fn missing_model_returns_unknown_cost() {
        let usage = UsageCounts {
            total_tokens: Some(100),
            ..UsageCounts::default()
        };
        let cost = estimate_cost("codex", None, &usage);
        assert_eq!(cost.confidence, Confidence::Low);
    }

    #[test]
    fn cached_input_reduces_billable() {
        let model = statsai_core::ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(200_000),
            cache_read_tokens: Some(800_000),
            output_tokens: Some(0),
            ..UsageCounts::default()
        };
        let cost = estimate_cost("codex", Some(&model), &usage);
        // Uncached input = 200K at $1.25/M, cached input = 800K at $0.125/M -> 35 cents.
        assert_eq!(cost.estimated_api_equivalent_usd, Some(35));
    }

    #[test]
    fn reasoning_tokens_are_billed_as_output() {
        let model = statsai_core::ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            output_tokens: Some(100_000),
            reasoning_tokens: Some(50_000),
            ..UsageCounts::default()
        };
        let cost = estimate_cost("codex", Some(&model), &usage);
        assert_eq!(cost.estimated_api_equivalent_usd, Some(150));
    }

    #[test]
    fn output_and_reasoning_pricing_does_not_overflow() {
        let model = statsai_core::ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            output_tokens: Some(u64::MAX),
            reasoning_tokens: Some(u64::MAX),
            ..UsageCounts::default()
        };

        let cost = estimate_cost("codex", Some(&model), &usage);

        assert!(cost
            .estimated_api_equivalent_usd
            .is_some_and(|cost| cost > 0));
    }

    #[test]
    fn cache_creation_tokens_are_billed_separately() {
        let model = statsai_core::ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_creation_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };
        let cost = estimate_cost("codex", Some(&model), &usage);
        assert_eq!(cost.estimated_api_equivalent_usd, Some(1263));
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some("codex_api_pricing:gpt-5")
        );
    }

    #[test]
    fn claude_one_hour_cache_writes_use_the_extended_ttl_rate() {
        let model = statsai_core::ModelInfo {
            name: Some("claude-sonnet-4-6".to_string()),
            normalized_name: Some("claude-sonnet-4-6".to_string()),
            provider_model_id: Some("claude-sonnet-4-6".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            cache_creation_tokens: Some(1_000_000),
            cache_creation_5m_tokens: Some(400_000),
            cache_creation_1h_tokens: Some(600_000),
            ..UsageCounts::default()
        };

        let cost = estimate_cost("claude_code", Some(&model), &usage);

        assert_eq!(cost.estimated_api_equivalent_usd, Some(510));
    }

    #[test]
    fn unclassified_claude_cache_writes_keep_the_legacy_five_minute_rate() {
        let model = statsai_core::ModelInfo {
            name: Some("claude-sonnet-4-6".to_string()),
            normalized_name: Some("claude-sonnet-4-6".to_string()),
            provider_model_id: Some("claude-sonnet-4-6".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            cache_creation_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };

        let cost = estimate_cost("claude_code", Some(&model), &usage);

        assert_eq!(cost.estimated_api_equivalent_usd, Some(375));
    }

    #[test]
    fn estimates_gpt_5_6_variant_costs_with_their_distinct_rates() {
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_creation_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };
        let after_price_cut = parse_utc("2026-07-30T00:00:00Z");

        for (model_name, expected_cents) in [
            ("gpt-5.6-sol", 4_175),
            ("gpt-5.6-terra", 1_670),
            ("gpt-5.6-luna", 167),
        ] {
            let model = statsai_core::ModelInfo {
                name: Some(model_name.to_string()),
                normalized_name: Some(model_name.to_string()),
                provider_model_id: Some(model_name.to_string()),
                speed: None,
                reasoning_level: None,
                reasoning_level_raw: None,
            };

            let cost = estimate_cost_at("codex", Some(&model), &usage, &after_price_cut);
            let expected_source = format!("codex_api_pricing:{model_name}");
            assert_eq!(cost.estimated_api_equivalent_usd, Some(expected_cents));
            assert_eq!(
                cost.pricing_source.as_deref(),
                Some(expected_source.as_str())
            );
        }
    }

    #[test]
    fn preserves_sub_cent_cost_in_micro_usd() {
        let model = statsai_core::ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            ..UsageCounts::default()
        };

        let cost = estimate_cost("codex", Some(&model), &usage);

        assert_eq!(cost.estimated_api_equivalent_micro_usd, Some(2_250));
        assert_eq!(cost.estimated_api_equivalent_usd, Some(0));
    }

    #[test]
    fn applies_verified_xai_cached_and_long_context_rates() {
        let model = statsai_core::ModelInfo {
            name: Some("grok-4.5".to_string()),
            normalized_name: Some("grok-4.5".to_string()),
            provider_model_id: Some("grok-4.5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let cached_usage = UsageCounts {
            cache_read_tokens: Some(100_000),
            requests: Some(1),
            ..UsageCounts::default()
        };
        let long_context_usage = UsageCounts {
            input_tokens: Some(200_000),
            output_tokens: Some(10_000),
            requests: Some(1),
            ..UsageCounts::default()
        };

        let cached_cost = estimate_cost("grok_build", Some(&model), &cached_usage);
        let long_context_cost = estimate_cost("grok_build", Some(&model), &long_context_usage);

        assert_eq!(cached_cost.estimated_api_equivalent_micro_usd, Some(30_000));
        assert_eq!(
            long_context_cost.estimated_api_equivalent_micro_usd,
            Some(920_000)
        );
    }

    #[test]
    fn applies_gpt_5_6_long_context_rates() {
        let model = statsai_core::ModelInfo {
            name: Some("gpt-5.6-terra".to_string()),
            normalized_name: Some("gpt-5.6-terra".to_string()),
            provider_model_id: Some("gpt-5.6-terra".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(300_000),
            output_tokens: Some(10_000),
            requests: Some(1),
            ..UsageCounts::default()
        };

        let after_price_cut = parse_utc("2026-07-30T00:00:00Z");
        let cost = estimate_cost_at("codex", Some(&model), &usage, &after_price_cut);

        assert_eq!(cost.estimated_api_equivalent_micro_usd, Some(1_380_000));
        assert_eq!(cost.estimated_api_equivalent_usd, Some(138));
    }

    #[test]
    fn sonnet_5_pricing_uses_usage_date() {
        let model = statsai_core::ModelInfo {
            name: Some("claude-sonnet-5".to_string()),
            normalized_name: Some("claude-sonnet-5".to_string()),
            provider_model_id: Some("claude-sonnet-5".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };
        let introductory_date = chrono::DateTime::parse_from_rfc3339("2026-08-31T23:59:59Z")
            .expect("valid timestamp")
            .with_timezone(&chrono::Utc);
        let standard_date = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .expect("valid timestamp")
            .with_timezone(&chrono::Utc);

        let introductory =
            estimate_cost_at("claude_code", Some(&model), &usage, &introductory_date);
        let standard = estimate_cost_at("claude_code", Some(&model), &usage, &standard_date);

        assert_eq!(introductory.estimated_api_equivalent_usd, Some(1_200));
        assert_eq!(standard.estimated_api_equivalent_usd, Some(1_800));
    }

    #[test]
    fn sonnet_5_reports_aggregate_periods_that_cross_its_price_change() {
        let before = chrono::NaiveDate::from_ymd_opt(2026, 8, 31).expect("before boundary");
        let boundary = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).expect("boundary");
        let after = chrono::NaiveDate::from_ymd_opt(2026, 9, 2).expect("after boundary");

        assert!(pricing_changes_between("claude-sonnet-5", before, boundary));
        assert!(pricing_changes_between(
            "anthropic/claude-sonnet-5",
            after,
            before
        ));
        assert!(!pricing_changes_between("claude-sonnet-5", boundary, after));
        assert!(!pricing_changes_between("claude-opus-5", before, after));
    }

    fn test_model(name: &str) -> statsai_core::ModelInfo {
        statsai_core::ModelInfo {
            name: Some(name.to_string()),
            normalized_name: Some(name.to_string()),
            provider_model_id: Some(name.to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        }
    }

    fn parse_utc(value: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(value)
            .expect("valid timestamp")
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn preserves_codex_auto_review_observed_identity() {
        assert_eq!(
            normalize_model_name("codex-auto-review"),
            "codex-auto-review"
        );
        assert_eq!(
            normalize_model_name("openai/codex-auto-review"),
            "codex-auto-review"
        );
        assert_eq!(
            normalize_model_name("relay/codex-auto-review"),
            "codex-auto-review"
        );
    }

    #[test]
    fn codex_auto_review_reuses_gpt_5_4_rates_before_july_30() {
        let review = test_model("codex-auto-review");
        let gpt_5_4 = test_model("gpt-5.4");
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_creation_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };
        let before_boundary = parse_utc("2026-07-29T23:59:59Z");

        let review_cost = estimate_cost_at("codex", Some(&review), &usage, &before_boundary);
        let gpt_5_4_cost = estimate_cost_at("codex", Some(&gpt_5_4), &usage, &before_boundary);
        let opencode_cost = estimate_cost_at("opencode", Some(&review), &usage, &before_boundary);

        assert_eq!(
            review_cost.estimated_api_equivalent_usd,
            gpt_5_4_cost.estimated_api_equivalent_usd
        );
        assert_eq!(
            review_cost.estimated_api_equivalent_micro_usd,
            gpt_5_4_cost.estimated_api_equivalent_micro_usd
        );
        assert_eq!(
            review_cost.pricing_source.as_deref(),
            Some("codex_api_pricing:codex-auto-review")
        );
        assert_eq!(
            opencode_cost.pricing_source.as_deref(),
            Some("opencode_api_pricing:codex-auto-review")
        );
        assert_eq!(
            gpt_5_4_cost.pricing_source.as_deref(),
            Some("codex_api_pricing:gpt-5.4")
        );
        assert_eq!(review_cost.confidence, Confidence::Medium);
        assert_eq!(
            review_cost.pricing_version.as_deref(),
            Some(PRICING_CATALOG_VERSION)
        );
    }

    #[test]
    fn codex_auto_review_reuses_luna_rates_from_july_30() {
        let review = test_model("codex-auto-review");
        let luna = test_model("gpt-5.6-luna");
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_creation_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };
        let on_boundary = parse_utc("2026-07-30T00:00:00Z");

        let review_cost = estimate_cost_at("codex", Some(&review), &usage, &on_boundary);
        let luna_cost = estimate_cost_at("codex", Some(&luna), &usage, &on_boundary);

        assert_eq!(
            review_cost.estimated_api_equivalent_usd,
            luna_cost.estimated_api_equivalent_usd
        );
        assert_eq!(
            review_cost.estimated_api_equivalent_micro_usd,
            luna_cost.estimated_api_equivalent_micro_usd
        );
        assert_eq!(review_cost.estimated_api_equivalent_usd, Some(167));
        assert_eq!(luna_cost.estimated_api_equivalent_usd, Some(167));
        assert_eq!(
            review_cost.estimated_api_equivalent_micro_usd,
            Some(1_670_000)
        );
        assert_eq!(
            review_cost.pricing_source.as_deref(),
            Some("codex_api_pricing:codex-auto-review")
        );
        assert_eq!(
            luna_cost.pricing_source.as_deref(),
            Some("codex_api_pricing:gpt-5.6-luna")
        );
        assert_eq!(review_cost.confidence, Confidence::Medium);
    }

    #[test]
    fn codex_auto_review_reuses_mapped_model_long_context_multipliers() {
        let review = test_model("codex-auto-review");
        let gpt_5_4 = test_model("gpt-5.4");
        let luna = test_model("gpt-5.6-luna");
        let long_context = UsageCounts {
            input_tokens: Some(300_000),
            output_tokens: Some(10_000),
            requests: Some(1),
            ..UsageCounts::default()
        };
        let aggregated = UsageCounts {
            requests: Some(2),
            ..long_context.clone()
        };
        let before_boundary = parse_utc("2026-07-29T12:00:00Z");
        let on_boundary = parse_utc("2026-07-30T00:00:00Z");

        let before_review =
            estimate_cost_at("codex", Some(&review), &long_context, &before_boundary);
        let before_mapped =
            estimate_cost_at("codex", Some(&gpt_5_4), &long_context, &before_boundary);
        let before_aggregate =
            estimate_cost_at("codex", Some(&review), &aggregated, &before_boundary);
        let after_review = estimate_cost_at("codex", Some(&review), &long_context, &on_boundary);
        let after_mapped = estimate_cost_at("codex", Some(&luna), &long_context, &on_boundary);

        assert_eq!(
            before_review.estimated_api_equivalent_micro_usd,
            before_mapped.estimated_api_equivalent_micro_usd
        );
        assert_eq!(
            after_review.estimated_api_equivalent_micro_usd,
            after_mapped.estimated_api_equivalent_micro_usd
        );
        assert_eq!(
            after_review.estimated_api_equivalent_micro_usd,
            Some(138_000)
        );
        assert_ne!(
            before_review.estimated_api_equivalent_micro_usd,
            before_aggregate.estimated_api_equivalent_micro_usd
        );
        assert_eq!(
            before_review.pricing_source.as_deref(),
            Some("codex_api_pricing:codex-auto-review")
        );
        assert_eq!(
            after_review.pricing_source.as_deref(),
            Some("codex_api_pricing:codex-auto-review")
        );
    }

    #[test]
    fn codex_auto_review_reports_aggregate_periods_that_cross_its_equivalent_change() {
        let before = chrono::NaiveDate::from_ymd_opt(2026, 7, 29).expect("before boundary");
        let boundary = chrono::NaiveDate::from_ymd_opt(2026, 7, 30).expect("boundary");
        let after = chrono::NaiveDate::from_ymd_opt(2026, 7, 31).expect("after boundary");

        assert!(pricing_changes_between(
            "codex-auto-review",
            before,
            boundary
        ));
        assert!(pricing_changes_between(
            "openai/codex-auto-review",
            after,
            before
        ));
        assert!(!pricing_changes_between(
            "codex-auto-review",
            boundary,
            after
        ));
        assert!(!pricing_changes_between("gpt-5.4", before, after));
    }

    #[test]
    fn gpt_5_6_luna_and_terra_pricing_uses_usage_date() {
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_creation_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };
        let before_boundary = parse_utc("2026-07-29T23:59:59Z");
        let on_boundary = parse_utc("2026-07-30T00:00:00Z");

        let luna = estimate_cost_at(
            "codex",
            Some(&test_model("gpt-5.6-luna")),
            &usage,
            &before_boundary,
        );
        let luna_cut = estimate_cost_at(
            "codex",
            Some(&test_model("gpt-5.6-luna")),
            &usage,
            &on_boundary,
        );
        let terra = estimate_cost_at(
            "codex",
            Some(&test_model("gpt-5.6-terra")),
            &usage,
            &before_boundary,
        );
        let terra_cut = estimate_cost_at(
            "codex",
            Some(&test_model("gpt-5.6-terra")),
            &usage,
            &on_boundary,
        );

        assert_eq!(luna.estimated_api_equivalent_usd, Some(835));
        assert_eq!(luna_cut.estimated_api_equivalent_usd, Some(167));
        assert_eq!(terra.estimated_api_equivalent_usd, Some(2_088));
        assert_eq!(terra_cut.estimated_api_equivalent_usd, Some(1_670));
    }

    #[test]
    fn gpt_5_6_luna_and_terra_report_aggregate_periods_that_cross_the_july_30_cut() {
        let before = chrono::NaiveDate::from_ymd_opt(2026, 7, 29).expect("before boundary");
        let boundary = chrono::NaiveDate::from_ymd_opt(2026, 7, 30).expect("boundary");
        let after = chrono::NaiveDate::from_ymd_opt(2026, 7, 31).expect("after boundary");

        assert!(pricing_changes_between("gpt-5.6-luna", before, boundary));
        assert!(pricing_changes_between(
            "openai/gpt-5.6-luna",
            after,
            before
        ));
        assert!(pricing_changes_between("gpt-5.6-terra", before, boundary));
        assert!(pricing_changes_between(
            "openai/gpt-5.6-terra",
            after,
            before
        ));
        assert!(!pricing_changes_between("gpt-5.6-luna", boundary, after));
        assert!(!pricing_changes_between("gpt-5.6-terra", boundary, after));
        assert!(!pricing_changes_between("gpt-5.6-sol", before, after));
    }

    #[test]
    fn grok_4_6_uses_official_short_context_rates_without_rewriting_identity() {
        let grok_4_6 = test_model("grok-4.6");
        let grok_4_6_build = test_model("grok-4.6-build");
        let grok_4_5 = test_model("grok-4.5");
        let usage = UsageCounts {
            input_tokens: Some(60_000),
            cache_read_tokens: Some(40_000),
            output_tokens: Some(10_000),
            requests: Some(1),
            ..UsageCounts::default()
        };
        let million_token_usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };

        let grok_4_6_cost = estimate_cost("grok_build", Some(&grok_4_6), &usage);
        let grok_4_6_build_cost = estimate_cost("grok_build", Some(&grok_4_6_build), &usage);
        let grok_4_5_cost = estimate_cost("grok_build", Some(&grok_4_5), &usage);
        let grok_4_6_million = estimate_cost("grok_build", Some(&grok_4_6), &million_token_usage);
        let grok_4_5_million = estimate_cost("grok_build", Some(&grok_4_5), &million_token_usage);

        assert_eq!(grok_4_6_cost.estimated_api_equivalent_usd, Some(20));
        assert_eq!(
            grok_4_6_cost.estimated_api_equivalent_micro_usd,
            Some(200_000)
        );
        assert_eq!(
            grok_4_6_build_cost.estimated_api_equivalent_micro_usd,
            grok_4_6_cost.estimated_api_equivalent_micro_usd
        );
        assert_ne!(
            grok_4_6_cost.estimated_api_equivalent_micro_usd,
            grok_4_5_cost.estimated_api_equivalent_micro_usd
        );
        assert_eq!(grok_4_6_million.estimated_api_equivalent_usd, Some(850));
        assert_eq!(
            grok_4_6_million.estimated_api_equivalent_micro_usd,
            Some(8_500_000)
        );
        assert_eq!(grok_4_5_million.estimated_api_equivalent_usd, Some(830));
        assert_eq!(
            grok_4_6_cost.pricing_source.as_deref(),
            Some("xai_api_pricing:grok-4.6")
        );
        assert_eq!(
            grok_4_6_build_cost.pricing_source.as_deref(),
            Some("xai_api_pricing:grok-4.6")
        );
        assert_eq!(
            grok_4_5_cost.pricing_source.as_deref(),
            Some("xai_api_pricing:grok-4.5")
        );
        assert_eq!(
            grok_4_6_cost.pricing_version.as_deref(),
            Some(PRICING_CATALOG_VERSION)
        );
    }

    #[test]
    fn grok_4_6_wrapped_ids_keep_observed_identity() {
        let usage = UsageCounts {
            input_tokens: Some(100_000),
            output_tokens: Some(10_000),
            requests: Some(1),
            ..UsageCounts::default()
        };
        let wrapped = test_model("openrouter/x-ai/grok-4.6");

        let cost = estimate_cost("opencode", Some(&wrapped), &usage);

        assert_eq!(cost.estimated_api_equivalent_usd, Some(26));
        assert_eq!(cost.estimated_api_equivalent_micro_usd, Some(260_000));
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some("xai_api_pricing:grok-4.6")
        );
    }

    #[test]
    fn grok_4_6_xai_long_context_boundary_includes_cached_tokens() {
        let model = test_model("grok-4.6");
        let just_below = UsageCounts {
            input_tokens: Some(119_999),
            cache_read_tokens: Some(80_000),
            output_tokens: Some(10_000),
            requests: Some(1),
            ..UsageCounts::default()
        };
        let on_threshold = UsageCounts {
            input_tokens: Some(120_000),
            cache_read_tokens: Some(80_000),
            output_tokens: Some(10_000),
            requests: Some(1),
            ..UsageCounts::default()
        };

        let short = estimate_cost("grok_build", Some(&model), &just_below);
        let long = estimate_cost("grok_build", Some(&model), &on_threshold);

        assert_eq!(short.estimated_api_equivalent_micro_usd, Some(339_998));
        assert_eq!(long.estimated_api_equivalent_micro_usd, Some(680_000));
        assert_eq!(
            short.pricing_source.as_deref(),
            Some("xai_api_pricing:grok-4.6")
        );
        assert_eq!(
            long.pricing_source.as_deref(),
            Some("xai_api_pricing:grok-4.6")
        );
    }

    #[test]
    fn grok_4_6_reasoning_tokens_use_the_output_rate() {
        let model = test_model("grok-4.6");
        let with_output = UsageCounts {
            input_tokens: Some(10_000),
            output_tokens: Some(10_000),
            requests: Some(1),
            ..UsageCounts::default()
        };
        let with_reasoning = UsageCounts {
            input_tokens: Some(10_000),
            reasoning_tokens: Some(10_000),
            requests: Some(1),
            ..UsageCounts::default()
        };

        let output_cost = estimate_cost("grok_build", Some(&model), &with_output);
        let reasoning_cost = estimate_cost("grok_build", Some(&model), &with_reasoning);

        assert_eq!(output_cost.estimated_api_equivalent_micro_usd, Some(80_000));
        assert_eq!(
            reasoning_cost.estimated_api_equivalent_micro_usd,
            output_cost.estimated_api_equivalent_micro_usd
        );
    }

    #[test]
    fn grok_4_6_mixed_requests_sum_short_and_long_context_costs() {
        let model = test_model("grok-4.6");
        let short_request = UsageCounts {
            input_tokens: Some(60_000),
            cache_read_tokens: Some(40_000),
            output_tokens: Some(10_000),
            requests: Some(1),
            ..UsageCounts::default()
        };
        let long_request = UsageCounts {
            input_tokens: Some(120_000),
            cache_read_tokens: Some(80_000),
            output_tokens: Some(10_000),
            requests: Some(1),
            ..UsageCounts::default()
        };
        let aggregated = UsageCounts {
            input_tokens: Some(180_000),
            cache_read_tokens: Some(120_000),
            output_tokens: Some(20_000),
            requests: Some(2),
            ..UsageCounts::default()
        };

        let short = estimate_cost("grok_build", Some(&model), &short_request);
        let long = estimate_cost("grok_build", Some(&model), &long_request);
        let aggregate = estimate_cost("grok_build", Some(&model), &aggregated);
        let mut combined = statsai_core::CostAccumulator::default();
        combined.add_estimated(&short);
        combined.add_estimated(&long);

        assert_eq!(short.estimated_api_equivalent_micro_usd, Some(200_000));
        assert_eq!(long.estimated_api_equivalent_micro_usd, Some(680_000));
        assert_eq!(combined.micro_usd(), Some(880_000));
        assert_eq!(combined.cents_rounded(), Some(88));
        assert_eq!(aggregate.estimated_api_equivalent_micro_usd, Some(540_000));
        assert_ne!(
            combined.micro_usd(),
            aggregate.estimated_api_equivalent_micro_usd
        );
    }
}
