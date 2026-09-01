use super::*;
use statsai_core::{Confidence, CostInfo, UsageCounts};
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

    let introductory = estimate_cost_at("claude_code", Some(&model), &usage, &introductory_date);
    let standard = estimate_cost_at("claude_code", Some(&model), &usage, &standard_date);

    assert_eq!(introductory.estimated_api_equivalent_usd, Some(1_200));
    assert_eq!(standard.estimated_api_equivalent_usd, Some(1_800));
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

    let before_review = estimate_cost_at("codex", Some(&review), &long_context, &before_boundary);
    let before_mapped = estimate_cost_at("codex", Some(&gpt_5_4), &long_context, &before_boundary);
    let before_aggregate = estimate_cost_at("codex", Some(&review), &aggregated, &before_boundary);
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
fn overlay_preserves_provider_reported_provenance() {
    let existing = CostInfo {
        currency: "USD".to_string(),
        estimated_api_equivalent_usd: Some(10),
        provider_reported_usd: Some(42),
        estimated_api_equivalent_micro_usd: Some(100_000),
        provider_reported_micro_usd: Some(420_000),
        pricing_source: Some("claude_stats_cache:costUSD".to_string()),
        pricing_version: Some("legacy".to_string()),
        confidence: Confidence::High,
    };
    let estimated = estimate_cost(
        "codex",
        Some(&test_model("gpt-5")),
        &UsageCounts {
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
            ..UsageCounts::default()
        },
    );

    let overlaid = overlay_estimated_cost(&existing, estimated.clone());

    assert_eq!(overlaid.provider_reported_usd, Some(42));
    assert_eq!(overlaid.provider_reported_micro_usd, Some(420_000));
    assert_eq!(
        overlaid.estimated_api_equivalent_usd,
        estimated.estimated_api_equivalent_usd
    );
    assert_eq!(
        overlaid.estimated_api_equivalent_micro_usd,
        estimated.estimated_api_equivalent_micro_usd
    );
    assert_eq!(
        overlaid.pricing_source.as_deref(),
        Some("claude_stats_cache:costUSD")
    );
    assert_eq!(
        overlaid.pricing_version.as_deref(),
        Some(PRICING_CATALOG_VERSION)
    );
    assert_eq!(overlaid.confidence, Confidence::High);
}

#[test]
fn overlay_replaces_estimated_only_cost() {
    let existing = unknown_cost();
    let estimated = estimate_cost(
        "codex",
        Some(&test_model("gpt-5")),
        &UsageCounts {
            input_tokens: Some(1_000_000),
            ..UsageCounts::default()
        },
    );

    let overlaid = overlay_estimated_cost(&existing, estimated.clone());

    assert_eq!(overlaid, estimated);
    assert!(overlaid.provider_reported_usd.is_none());
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
