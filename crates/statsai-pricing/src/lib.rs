//! Model pricing helpers for `statsai`.
//!
//! Provides static model pricing lookup and cost estimation
//! decoupled from any specific adapter.

use statsai_core::{Confidence, CostInfo, ModelInfo, UsageCounts};

fn normalize_proxy_wrapped_model_name(lower: &str) -> Option<&'static str> {
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
        "grok-4.5" | "grok-4.5-latest" | "grok-build-latest" => "grok-4.5".to_string(),
        "grok-4.3" | "grok-4.3-latest" => "grok-4.3".to_string(),
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

#[must_use]
pub fn pricing_for_model(model_name: &str) -> Option<ModelPricing> {
    let normalized = model_name.to_ascii_lowercase();
    match normalized.as_str() {
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
        "gpt-5.6-terra" => Some(pricing_with_cache_creation(2.5, 3.125, 0.25, 15.0)),
        "gpt-5.6-luna" => Some(pricing_with_cache_creation(1.0, 1.25, 0.1, 6.0)),
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
        "grok-build-0.1" => Some(pricing(1.0, 1.0, 2.0)),
        "grok-4.3" => Some(pricing(1.25, 1.25, 2.5)),
        // xAI lists $2/M input, $0.50/M cached input, and $6/M output.
        "grok-4.5" => Some(pricing(2.0, 0.5, 6.0)),
        _ => None,
    }
}

fn priced_model_name(model: &ModelInfo) -> Option<String> {
    let candidates = [
        model.normalized_name.as_deref(),
        model.name.as_deref(),
        model.provider_model_id.as_deref(),
    ];

    for candidate in candidates.into_iter().flatten() {
        let normalized = normalize_model_name(candidate);
        if pricing_for_model(&normalized).is_some() {
            return Some(normalized);
        }
        if let Some((_, suffix)) = normalized.rsplit_once('/') {
            let suffix = normalize_model_name(suffix);
            if pricing_for_model(&suffix).is_some() {
                return Some(suffix);
            }
        }
    }

    None
}

#[must_use]
pub fn estimate_cost(provider: &str, model: Option<&ModelInfo>, usage: &UsageCounts) -> CostInfo {
    let Some(model_name) = model.and_then(priced_model_name) else {
        return unknown_cost();
    };
    let Some(pricing) = pricing_for_model(&model_name) else {
        return unknown_cost();
    };

    let input = usage.input_tokens.unwrap_or(0);
    let cache_creation = usage.cache_creation_tokens.unwrap_or(0);
    let cached = usage.cache_read_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    let reasoning = usage.reasoning_tokens.unwrap_or(0);
    let cost = (input as f64 * pricing.input_per_million
        + cache_creation as f64 * pricing.cache_creation_per_million
        + cached as f64 * pricing.cached_input_per_million
        + (output as f64 + reasoning as f64) * pricing.output_per_million)
        / 1_000_000.0;
    let cost_cents = (cost * 100.0).round() as i64;

    let pricing_source = match model_name.as_str() {
        "composer-2.5" | "composer-2.5-fast" => format!("cursor_model_pricing:{model_name}"),
        "grok-build-0.1" | "grok-4.3" | "grok-4.5" => {
            format!("xai_api_pricing:{model_name}")
        }
        _ => format!("{provider}_api_pricing:{model_name}"),
    };

    CostInfo {
        currency: "USD".to_string(),
        estimated_api_equivalent_usd: Some(cost_cents),
        provider_reported_usd: None,
        pricing_source: Some(pricing_source),
        pricing_version: Some("static:2026-07".to_string()),
        confidence: Confidence::Medium,
    }
}

#[must_use]
pub fn unknown_cost() -> CostInfo {
    CostInfo {
        currency: "USD".to_string(),
        estimated_api_equivalent_usd: None,
        provider_reported_usd: None,
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

        assert_eq!(cost.estimated_api_equivalent_usd, Some(850));
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some("xai_api_pricing:grok-4.5")
        );
    }

    #[test]
    fn estimates_cost_for_proxy_wrapped_claude_model() {
        let model = statsai_core::ModelInfo {
            name: Some("google/antigravity-claude-opus-4-5-thinking".to_string()),
            normalized_name: Some("google/antigravity-claude-opus-4-5-thinking".to_string()),
            provider_model_id: Some("google/antigravity-claude-opus-4-5-thinking".to_string()),
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
    fn estimates_cost_for_legacy_claude_opus_4() {
        let model = statsai_core::ModelInfo {
            name: Some("claude-opus-4".to_string()),
            normalized_name: Some("claude-opus-4".to_string()),
            provider_model_id: Some("claude-opus-4".to_string()),
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
    fn unknown_model_returns_unknown_cost() {
        let model = statsai_core::ModelInfo {
            name: Some("unknown-model".to_string()),
            normalized_name: Some("unknown-model".to_string()),
            provider_model_id: Some("unknown-model".to_string()),
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
    fn estimates_gpt_5_6_variant_costs_with_their_distinct_rates() {
        let usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_creation_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..UsageCounts::default()
        };

        for (model_name, expected_cents) in [
            ("gpt-5.6-sol", 4_175),
            ("gpt-5.6-terra", 2_088),
            ("gpt-5.6-luna", 835),
        ] {
            let model = statsai_core::ModelInfo {
                name: Some(model_name.to_string()),
                normalized_name: Some(model_name.to_string()),
                provider_model_id: Some(model_name.to_string()),
                reasoning_level: None,
                reasoning_level_raw: None,
            };

            let cost = estimate_cost("codex", Some(&model), &usage);
            let expected_source = format!("codex_api_pricing:{model_name}");
            assert_eq!(cost.estimated_api_equivalent_usd, Some(expected_cents));
            assert_eq!(
                cost.pricing_source.as_deref(),
                Some(expected_source.as_str())
            );
        }
    }
}
