//! Model pricing helpers for `ai-stats`.
//!
//! Provides static model pricing lookup and cost estimation
//! decoupled from any specific adapter.

use ai_stats_core::{Confidence, CostInfo, ModelInfo, UsageCounts};

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
        "claude-opus-4-5" | "claude-opus-4-5-thinking" | "claude-opus-4.5" => {
            "claude-opus-4-5".to_string()
        }
        "claude-sonnet-4" => "claude-sonnet-4".to_string(),
        "claude-sonnet-4-5" | "claude-sonnet-4.5" => "claude-sonnet-4-5".to_string(),
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
        "gpt-5.5" => "gpt-5.5".to_string(),
        "gpt-5-mini" => "gpt-5-mini".to_string(),
        "gpt-5-nano" => "gpt-5-nano".to_string(),
        _ => name.to_ascii_lowercase(),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub cached_input_per_million: f64,
    pub output_per_million: f64,
}

#[must_use]
pub fn pricing_for_model(model_name: &str) -> Option<ModelPricing> {
    let normalized = model_name.to_ascii_lowercase();
    match normalized.as_str() {
        "gpt-5.5" => Some(ModelPricing {
            input_per_million: 5.0,
            cached_input_per_million: 0.5,
            output_per_million: 30.0,
        }),
        "gpt-5.4" => Some(ModelPricing {
            input_per_million: 2.5,
            cached_input_per_million: 0.25,
            output_per_million: 15.0,
        }),
        "gpt-5.4-mini" => Some(ModelPricing {
            input_per_million: 0.75,
            cached_input_per_million: 0.075,
            output_per_million: 4.5,
        }),
        "gpt-5.3-codex" | "gpt-5.2" | "gpt-5.2-chat-latest" | "gpt-5.2-codex" => {
            Some(ModelPricing {
                input_per_million: 1.75,
                cached_input_per_million: 0.175,
                output_per_million: 14.0,
            })
        }
        "gpt-5-codex"
        | "gpt-5.1-codex"
        | "gpt-5.1-codex-max"
        | "gpt-5"
        | "gpt-5.1"
        | "gpt-5-chat-latest"
        | "gpt-5.1-chat-latest" => Some(ModelPricing {
            input_per_million: 1.25,
            cached_input_per_million: 0.125,
            output_per_million: 10.0,
        }),
        "gpt-5-mini" | "gpt-5.1-codex-mini" => Some(ModelPricing {
            input_per_million: 0.25,
            cached_input_per_million: 0.025,
            output_per_million: 2.0,
        }),
        "gpt-5-nano" => Some(ModelPricing {
            input_per_million: 0.05,
            cached_input_per_million: 0.005,
            output_per_million: 0.4,
        }),
        _ => None,
    }
}

#[must_use]
pub fn estimate_cost(provider: &str, model: Option<&ModelInfo>, usage: &UsageCounts) -> CostInfo {
    let Some(model_name) =
        model.and_then(|model| model.normalized_name.as_deref().or(model.name.as_deref()))
    else {
        return unknown_cost();
    };
    let Some(pricing) = pricing_for_model(model_name) else {
        return unknown_cost();
    };

    let input = usage.input_tokens.unwrap_or(0);
    let cached = usage.cache_read_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    let reasoning = usage.reasoning_tokens.unwrap_or(0);
    let cost = (input as f64 * pricing.input_per_million
        + cached as f64 * pricing.cached_input_per_million
        + (output + reasoning) as f64 * pricing.output_per_million)
        / 1_000_000.0;

    CostInfo {
        currency: "USD".to_string(),
        estimated_api_equivalent_usd: Some(cost),
        provider_reported_usd: None,
        pricing_source: Some(format!("{provider}_api_pricing:{model_name}")),
        pricing_version: Some("static:2026-05".to_string()),
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
    use ai_stats_core::UsageCounts;

    #[test]
    fn normalizes_claude_thinking_variant() {
        assert_eq!(
            normalize_model_name("claude-opus-4-5-thinking"),
            "claude-opus-4-5"
        );
    }

    #[test]
    fn normalizes_codex_aliases() {
        assert_eq!(normalize_model_name("gpt-5.1-codex"), "gpt-5-codex");
        assert_eq!(normalize_model_name("gpt-5.1-codex-mini"), "gpt-5-mini");
    }

    #[test]
    fn normalizes_provider_prefixes() {
        assert_eq!(
            normalize_model_name("anthropic/claude-sonnet-4-5"),
            "claude-sonnet-4-5"
        );
        assert_eq!(normalize_model_name("openai/gpt-5"), "gpt-5");
    }

    #[test]
    fn normalizes_unknown_model_to_lowercase() {
        assert_eq!(normalize_model_name("SomeNewModel"), "somenewmodel");
    }

    #[test]
    fn normalizes_whitespace() {
        assert_eq!(normalize_model_name("  gpt-5  "), "gpt-5");
    }

    #[test]
    fn estimates_cost_for_known_model() {
        let model = ai_stats_core::ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
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
    fn unknown_model_returns_unknown_cost() {
        let model = ai_stats_core::ModelInfo {
            name: Some("unknown-model".to_string()),
            normalized_name: Some("unknown-model".to_string()),
            provider_model_id: Some("unknown-model".to_string()),
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
        let model = ai_stats_core::ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
        };
        let usage = UsageCounts {
            input_tokens: Some(200_000),
            cache_read_tokens: Some(800_000),
            output_tokens: Some(0),
            ..UsageCounts::default()
        };
        let cost = estimate_cost("codex", Some(&model), &usage);
        // Uncached input = 200K at $1.25/M, cached input = 800K at $0.125/M.
        assert!((cost.estimated_api_equivalent_usd.unwrap() - 0.35).abs() < 1e-9);
    }

    #[test]
    fn reasoning_tokens_are_billed_as_output() {
        let model = ai_stats_core::ModelInfo {
            name: Some("gpt-5".to_string()),
            normalized_name: Some("gpt-5".to_string()),
            provider_model_id: Some("gpt-5".to_string()),
        };
        let usage = UsageCounts {
            output_tokens: Some(100_000),
            reasoning_tokens: Some(50_000),
            ..UsageCounts::default()
        };
        let cost = estimate_cost("codex", Some(&model), &usage);
        assert!((cost.estimated_api_equivalent_usd.unwrap() - 1.5).abs() < 1e-9);
    }
}
