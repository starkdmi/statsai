use chrono::{DateTime, Utc};
use statsai_core::{micro_usd_to_cents_rounded, Confidence, CostInfo, ModelInfo, UsageCounts};

use crate::catalog::{
    api_equivalent_pricing_model, priced_model_name, pricing_for_effective_speed,
    pricing_for_model_on, ModelPricing,
};
use crate::PRICING_CATALOG_VERSION;

const MICRO_USD_PER_USD: i128 = 1_000_000;
const TOKENS_PER_MILLION: i128 = 1_000_000;
const MULTIPLIER_SCALE: i128 = 10_000;

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
        "gpt-5.4" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" | "gpt-6-astra"
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

/// Overlays a freshly estimated cost onto a persisted [`CostInfo`].
///
/// Provider-reported amounts are never replaced. When a provider-reported
/// value is present, its provenance and confidence are preserved and only the
/// estimated fields and catalog version are updated.
#[must_use]
pub fn overlay_estimated_cost(existing: &CostInfo, estimated: CostInfo) -> CostInfo {
    let has_provider_reported =
        existing.provider_reported_usd.is_some() || existing.provider_reported_micro_usd.is_some();
    if has_provider_reported {
        CostInfo {
            currency: existing.currency.clone(),
            estimated_api_equivalent_usd: estimated.estimated_api_equivalent_usd,
            provider_reported_usd: existing.provider_reported_usd,
            estimated_api_equivalent_micro_usd: estimated.estimated_api_equivalent_micro_usd,
            provider_reported_micro_usd: existing.provider_reported_micro_usd,
            pricing_source: existing.pricing_source.clone(),
            pricing_version: estimated.pricing_version,
            confidence: existing.confidence.clone(),
        }
    } else {
        CostInfo {
            currency: estimated.currency,
            estimated_api_equivalent_usd: estimated.estimated_api_equivalent_usd,
            provider_reported_usd: existing.provider_reported_usd,
            estimated_api_equivalent_micro_usd: estimated.estimated_api_equivalent_micro_usd,
            provider_reported_micro_usd: existing.provider_reported_micro_usd,
            pricing_source: estimated.pricing_source,
            pricing_version: estimated.pricing_version,
            confidence: estimated.confidence,
        }
    }
}
