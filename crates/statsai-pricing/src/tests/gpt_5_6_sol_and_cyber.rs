//! Pricing version 9: GPT-5.6 Sol's promotion ends after 2026-11-30, every
//! `gpt-5.6` id that names no other variant routes to Sol, and the Daybreak
//! cyber models bill at their own rates instead of a family catch-all.

use super::*;
use crate::catalog::pricing_for_model_on;
use chrono::{DateTime, NaiveDate, Utc};
use statsai_core::{ModelInfo, UsageCounts};

fn day(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
}

/// 2026-11-23, the catalog date of pricing version 9.
fn catalog_day() -> NaiveDate {
    day(2026, 11, 23)
}

/// The last day of Sol's promotion.
fn last_promotional_day() -> NaiveDate {
    day(2026, 11, 30)
}

/// The first day Sol bills at list rates again.
fn first_list_day() -> NaiveDate {
    day(2026, 12, 1)
}

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .expect("valid timestamp")
        .with_timezone(&Utc)
}

fn observed(name: &str) -> ModelInfo {
    ModelInfo {
        name: Some(name.to_string()),
        normalized_name: Some(normalize_qualified_model_name(name)),
        provider_model_id: Some(name.to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    }
}

fn observed_fast(name: &str) -> ModelInfo {
    ModelInfo {
        speed: Some("fast".to_string()),
        ..observed(name)
    }
}

fn one_million_of_each() -> UsageCounts {
    UsageCounts {
        input_tokens: Some(1_000_000),
        cache_creation_tokens: Some(1_000_000),
        cache_read_tokens: Some(1_000_000),
        output_tokens: Some(1_000_000),
        ..UsageCounts::default()
    }
}

const SOL_PROMOTION: ModelPricing = ModelPricing {
    input_per_million: 4.0,
    cache_creation_per_million: 5.0,
    cached_input_per_million: 0.4,
    output_per_million: 20.0,
};

const SOL_LIST: ModelPricing = ModelPricing {
    input_per_million: 5.0,
    cache_creation_per_million: 6.25,
    cached_input_per_million: 0.5,
    output_per_million: 30.0,
};

const GPT_5_6_CYBER: ModelPricing = ModelPricing {
    input_per_million: 12.5,
    cache_creation_per_million: 15.625,
    cached_input_per_million: 1.25,
    output_per_million: 75.0,
};

const GPT_5_5_CYBER_PREVIEW: ModelPricing = ModelPricing {
    input_per_million: 12.5,
    cache_creation_per_million: 12.5,
    cached_input_per_million: 1.25,
    output_per_million: 75.0,
};

#[test]
fn spellings_and_suffixes_reach_their_models() {
    for (name, expected) in [
        // The unsuffixed alias, and every gpt-5.6 id that names no other
        // variant: effort, dated, fast and proxy-wrapped spellings.
        ("gpt-5.6", "gpt-5.6-sol"),
        ("GPT-5.6", "gpt-5.6-sol"),
        ("openai/gpt-5.6", "gpt-5.6-sol"),
        ("openrouter/gpt-5.6", "gpt-5.6-sol"),
        ("relay/openai-gpt-5.6", "gpt-5.6-sol"),
        ("gpt-5.6-high", "gpt-5.6-sol"),
        ("gpt-5.6-xhigh", "gpt-5.6-sol"),
        ("gpt-5.6-max", "gpt-5.6-sol"),
        ("gpt-5.6-fast", "gpt-5.6-sol"),
        ("gpt-5.6-2026-09-01", "gpt-5.6-sol"),
        ("gpt-5.6-codex", "gpt-5.6-sol"),
        // The named variants keep their own models.
        ("gpt-5.6-sol", "gpt-5.6-sol"),
        ("gpt-5.6-sol-high", "gpt-5.6-sol"),
        ("gpt-5.6-terra", "gpt-5.6-terra"),
        ("gpt-5.6-terra-high", "gpt-5.6-terra"),
        ("relay/openai-gpt-5.6-terra", "gpt-5.6-terra"),
        ("gpt-5.6-luna", "gpt-5.6-luna"),
        ("gpt-5.6-luna-2026-09-01", "gpt-5.6-luna"),
        // Daybreak cyber models.
        ("gpt-5.6-cyber", "gpt-5.6-cyber"),
        ("GPT-5.6-Cyber", "gpt-5.6-cyber"),
        ("gpt-5.6-cyber-high", "gpt-5.6-cyber"),
        ("openai/gpt-5.6-cyber", "gpt-5.6-cyber"),
        ("relay/openai-gpt-5.6-cyber", "gpt-5.6-cyber"),
        ("gpt-5.5-cyber-preview", "gpt-5.5-cyber-preview"),
        ("gpt-5.5-cyber", "gpt-5.5-cyber-preview"),
        ("gpt-5.5-cyber-high", "gpt-5.5-cyber-preview"),
        ("openai/gpt-5.5-cyber", "gpt-5.5-cyber-preview"),
        // Unchanged neighbours.
        ("gpt-5.5", "gpt-5.5"),
        ("gpt-5", "gpt-5"),
        ("gpt-6-cyber", "gpt-6-cyber"),
    ] {
        assert_eq!(normalize_model_name(name), expected, "{name}");
        assert_eq!(normalize_qualified_model_name(name), expected, "{name}");
    }
}

/// Each name and a model it must not be priced as.
#[test]
fn neighbouring_models_are_not_priced_as_the_new_ones() {
    let table = embedded_pricing();
    for (name, not_model) in [
        // The catch-alls these ids used to fall into.
        ("gpt-5.6", "gpt-5"),
        ("gpt-5.6-high", "gpt-5"),
        ("relay/openai-gpt-5.6", "gpt-5"),
        ("gpt-5.6-cyber", "gpt-5"),
        ("gpt-5.6-cyber-high", "gpt-5"),
        ("gpt-5.5-cyber", "gpt-5.5"),
        ("gpt-5.5-cyber-preview", "gpt-5.5"),
        // The gpt-5.6 rule must not take the named variants or another version.
        ("gpt-5.6-cyber", "gpt-5.6-sol"),
        ("gpt-5.6-cyber-high", "gpt-5.6-sol"),
        ("gpt-5.6-terra", "gpt-5.6-sol"),
        ("gpt-5.6-luna", "gpt-5.6-sol"),
        ("gpt-5.60", "gpt-5.6-sol"),
        ("gpt-5.5", "gpt-5.6-sol"),
        ("gpt-5", "gpt-5.6-sol"),
        // Other cyber models are not priced as these two.
        ("gpt-6-cyber", "gpt-5.6-cyber"),
        ("gpt-6-cyber", "gpt-5.5-cyber-preview"),
        ("gpt-5.50-cyber", "gpt-5.5-cyber-preview"),
    ] {
        assert_ne!(normalize_model_name(name), not_model, "{name}");
        assert_ne!(
            table
                .priced_model_name(&observed(name), catalog_day())
                .as_deref(),
            Some(not_model),
            "{name}"
        );
    }
}

#[test]
fn gpt_6_cyber_stays_unpriced() {
    let table = embedded_pricing();
    for date in [catalog_day(), last_promotional_day(), first_list_day()] {
        assert_eq!(pricing_for_model_on("gpt-6-cyber", date), None, "{date}");
        assert_eq!(
            table.priced_model_name(&observed("gpt-6-cyber"), date),
            None,
            "{date}"
        );
    }
    let cost = table.estimate_cost_at(
        "codex",
        Some(&observed("gpt-6-cyber")),
        &one_million_of_each(),
        &at("2026-11-23T12:00:00Z"),
    );
    assert_eq!(cost, unknown_cost());
}

#[test]
fn gpt_5_6_sol_returns_to_list_rates_on_december_1() {
    for name in ["gpt-5.6-sol", "gpt-5.6", "gpt-5.6-high", "openai/gpt-5.6"] {
        let model = normalize_qualified_model_name(name);
        // Both earlier periods are unchanged.
        assert_eq!(
            pricing_for_model_on(&model, day(2026, 8, 20)),
            Some(SOL_LIST),
            "{name}"
        );
        assert_eq!(
            pricing_for_model_on(&model, day(2026, 8, 21)),
            Some(SOL_PROMOTION),
            "{name}"
        );
        assert_eq!(
            pricing_for_model_on(&model, last_promotional_day()),
            Some(SOL_PROMOTION),
            "{name}"
        );
        assert_eq!(
            pricing_for_model_on(&model, first_list_day()),
            Some(SOL_LIST),
            "{name}"
        );
    }
}

#[test]
fn gpt_5_6_sol_reports_aggregate_periods_that_cross_its_promotion_end() {
    let after = day(2026, 12, 2);

    assert!(pricing_changes_between(
        "gpt-5.6-sol",
        last_promotional_day(),
        first_list_day()
    ));
    assert!(pricing_changes_between(
        "gpt-5.6",
        first_list_day(),
        last_promotional_day()
    ));
    assert!(pricing_changes_between(
        "openai/gpt-5.6-high",
        last_promotional_day(),
        first_list_day()
    ));
    assert!(!pricing_changes_between(
        "gpt-5.6-sol",
        first_list_day(),
        after
    ));
    assert!(!pricing_changes_between(
        "gpt-5.6-sol",
        day(2026, 11, 1),
        last_promotional_day()
    ));
    // The end of Sol's promotion is not a boundary for the other variants.
    for name in [
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.6-cyber",
        "gpt-5.5-cyber-preview",
    ] {
        assert!(
            !pricing_changes_between(name, last_promotional_day(), first_list_day()),
            "{name}"
        );
    }
}

#[test]
fn gpt_5_6_ids_bill_at_sol_rates_across_the_promotion_end() {
    let table = embedded_pricing();
    let usage = one_million_of_each();
    // input + cache write + cached input + output.
    // Promotion: 4 + 5 + 0.40 + 20. List: 5 + 6.25 + 0.50 + 30.
    // Fast mode is twice the standard rate in effect that day.
    for (name, fast, timestamp, micro_usd, source) in [
        (
            "gpt-5.6",
            false,
            "2026-11-30T23:59:59Z",
            29_400_000,
            "codex_api_pricing:gpt-5.6-sol",
        ),
        (
            "gpt-5.6",
            false,
            "2026-12-01T00:00:00Z",
            41_750_000,
            "codex_api_pricing:gpt-5.6-sol",
        ),
        (
            "gpt-5.6-sol",
            false,
            "2026-11-30T23:59:59Z",
            29_400_000,
            "codex_api_pricing:gpt-5.6-sol",
        ),
        (
            "gpt-5.6-sol",
            false,
            "2026-12-01T00:00:00Z",
            41_750_000,
            "codex_api_pricing:gpt-5.6-sol",
        ),
        (
            "gpt-5.6-high",
            false,
            "2026-12-01T00:00:00Z",
            41_750_000,
            "codex_api_pricing:gpt-5.6-sol",
        ),
        (
            "relay/openai-gpt-5.6",
            false,
            "2026-12-01T00:00:00Z",
            41_750_000,
            "codex_api_pricing:gpt-5.6-sol",
        ),
        (
            "gpt-5.6",
            true,
            "2026-11-30T23:59:59Z",
            58_800_000,
            "codex_api_pricing:gpt-5.6-sol:fast",
        ),
        (
            "gpt-5.6",
            true,
            "2026-12-01T00:00:00Z",
            83_500_000,
            "codex_api_pricing:gpt-5.6-sol:fast",
        ),
        (
            "gpt-5.6-fast",
            false,
            "2026-11-30T23:59:59Z",
            58_800_000,
            "codex_api_pricing:gpt-5.6-sol:fast",
        ),
        (
            "gpt-5.6-fast",
            false,
            "2026-12-01T00:00:00Z",
            83_500_000,
            "codex_api_pricing:gpt-5.6-sol:fast",
        ),
    ] {
        let model = if fast {
            observed_fast(name)
        } else {
            observed(name)
        };
        let cost = table.estimate_cost_at("codex", Some(&model), &usage, &at(timestamp));
        assert_eq!(
            cost.estimated_api_equivalent_micro_usd,
            Some(micro_usd),
            "{name} fast={fast} at {timestamp}"
        );
        assert_eq!(
            cost.pricing_source.as_deref(),
            Some(source),
            "{name} fast={fast} at {timestamp}"
        );
    }
}

#[test]
fn daybreak_cyber_models_use_their_published_rates() {
    for date in [catalog_day(), last_promotional_day(), first_list_day()] {
        assert_eq!(
            pricing_for_model_on("gpt-5.6-cyber", date),
            Some(GPT_5_6_CYBER),
            "{date}"
        );
        assert_eq!(
            pricing_for_model_on("gpt-5.5-cyber-preview", date),
            Some(GPT_5_5_CYBER_PREVIEW),
            "{date}"
        );
    }

    let table = embedded_pricing();
    let usage = one_million_of_each();
    // gpt-5.6-cyber: 12.50 + 15.625 (30-minute write) + 1.25 + 75.
    // gpt-5.5-cyber-preview: 12.50 + 12.50 (write billed as input) + 1.25 + 75.
    for (name, micro_usd, model) in [
        ("gpt-5.6-cyber", 104_375_000, "gpt-5.6-cyber"),
        ("gpt-5.6-cyber-high", 104_375_000, "gpt-5.6-cyber"),
        (
            "gpt-5.5-cyber-preview",
            101_250_000,
            "gpt-5.5-cyber-preview",
        ),
        ("gpt-5.5-cyber", 101_250_000, "gpt-5.5-cyber-preview"),
    ] {
        let cost = table.estimate_cost_at(
            "codex",
            Some(&observed(name)),
            &usage,
            &at("2026-11-23T12:00:00Z"),
        );
        assert_eq!(
            cost.estimated_api_equivalent_micro_usd,
            Some(micro_usd),
            "{name}"
        );
        assert_eq!(
            cost.pricing_source,
            Some(format!("codex_api_pricing:{model}")),
            "{name}"
        );
    }
}

#[test]
fn daybreak_cyber_models_have_no_fast_rate_or_long_context_tier() {
    let table = embedded_pricing();
    let when = at("2026-11-23T12:00:00Z");
    // One known request with a 300k prompt keeps the base rates:
    // 300k * $12.50 + 10k * $75.
    let long_prompt = UsageCounts {
        input_tokens: Some(300_000),
        output_tokens: Some(10_000),
        requests: Some(1),
        ..UsageCounts::default()
    };
    for (name, model) in [
        ("gpt-5.6-cyber", "gpt-5.6-cyber"),
        ("gpt-5.5-cyber-preview", "gpt-5.5-cyber-preview"),
    ] {
        let long = table.estimate_cost_at("codex", Some(&observed(name)), &long_prompt, &when);
        assert_eq!(
            long.estimated_api_equivalent_micro_usd,
            Some(4_500_000),
            "{name}"
        );

        // A fast request bills at the standard rate.
        let usage = one_million_of_each();
        let standard = table.estimate_cost_at("codex", Some(&observed(name)), &usage, &when);
        let fast = table.estimate_cost_at("codex", Some(&observed_fast(name)), &usage, &when);
        assert_eq!(
            fast.estimated_api_equivalent_micro_usd, standard.estimated_api_equivalent_micro_usd,
            "{name}"
        );
        assert_eq!(
            fast.pricing_source,
            Some(format!("codex_api_pricing:{model}")),
            "{name}"
        );
    }
}
