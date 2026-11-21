//! Third-party models Cursor serves at their published per-token prices:
//! DeepSeek V4.1 Flash 1017, Kimi K3, GLM 5.3 and Muse.

use super::*;
use chrono::{DateTime, NaiveDate, Utc};
use statsai_core::{ModelInfo, UsageCounts};

/// 2026-11-21, the catalog date of pricing version 8, which added these
/// models. Every rate checked here is in effect that day.
fn catalog_day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 21).expect("catalog day")
}

fn catalog_time() -> DateTime<Utc> {
    catalog_day().and_hms_opt(12, 0, 0).expect("noon").and_utc()
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

#[test]
fn spellings_and_effort_suffixes_reach_their_models() {
    for (name, expected) in [
        ("deepseek-v4.1-flash-1017", "deepseek-v4.1-flash-1017"),
        ("deepseek-v41-flash-1017", "deepseek-v4.1-flash-1017"),
        ("deepseek-v41-flash-1017-high", "deepseek-v4.1-flash-1017"),
        ("deepseek-v41-flash-1017-max", "deepseek-v4.1-flash-1017"),
        ("deepseek-v4.1-flash-1017-none", "deepseek-v4.1-flash-1017"),
        ("DeepSeek-V4.1-Flash-1017", "deepseek-v4.1-flash-1017"),
        ("kimi-k3", "kimi-k3"),
        ("kimi-k3-high", "kimi-k3"),
        ("moonshotai/kimi-k3", "kimi-k3"),
        ("kimi-k3.1-pro", "kimi-k3.1-pro"),
        ("kimi-k3.1-pro-max", "kimi-k3.1-pro"),
        ("glm-5.3", "glm-5.3"),
        ("GLM-5.3", "glm-5.3"),
        ("glm-5.3-high", "glm-5.3"),
        ("z-ai/glm-5.3", "glm-5.3"),
        ("glm-5.3-flash", "glm-5.3-flash"),
        ("glm-5.3-flash-max", "glm-5.3-flash"),
        ("glm-5.3-flashx", "glm-5.3-flashx"),
        ("glm-5.3-flashx-legal", "glm-5.3-flashx"),
        ("muse-spark-1.3", "muse-spark-1.3"),
        ("muse-spark-1.3-xhigh", "muse-spark-1.3"),
        ("muse-wonder-1.0", "muse-wonder-1.0"),
        ("muse-wonder-1.0-minimal", "muse-wonder-1.0"),
    ] {
        assert_eq!(normalize_model_name(name), expected, "{name}");
    }
}

#[test]
fn neighbouring_models_are_not_priced_as_the_new_ones() {
    let table = embedded_pricing();
    for name in [
        // DeepSeek-V4.1-Flash without 1017 has a different cache-hit rate,
        // and deepseek-flash is an alias DeepSeek moves between versions.
        "deepseek-v4.1-flash",
        "deepseek-flash",
        "kimi-k2.7-code",
        "kimi-k30",
        "glm-5.2",
        "glm-5.30",
        // Other Muse versions may have other prices.
        "muse-spark",
        "muse-spark-1.2",
        "muse-spark-1.30",
        "muse-wonder",
        "muse-wonder-full",
    ] {
        assert_eq!(normalize_model_name(name), name, "{name}");
        assert_eq!(
            table.priced_model_name(&observed(name), catalog_day()),
            None,
            "{name}"
        );
    }
}

#[test]
fn cursor_models_use_their_published_rates() {
    let table = embedded_pricing();
    let usage = UsageCounts {
        input_tokens: Some(1_000_000),
        cache_creation_tokens: Some(1_000_000),
        cache_read_tokens: Some(1_000_000),
        output_tokens: Some(1_000_000),
        ..UsageCounts::default()
    };
    // input + cache write (billed as input) + cache read + output.
    for (name, micro_usd, model) in [
        (
            "deepseek-v41-flash-1017-high",
            1_806_000,
            "deepseek-v4.1-flash-1017",
        ),
        ("kimi-k3", 21_300_000, "kimi-k3"),
        ("kimi-k3.1-pro", 14_200_000, "kimi-k3.1-pro"),
        ("glm-5.3", 7_460_000, "glm-5.3"),
        ("glm-5.3-flash", 830_000, "glm-5.3-flash"),
        ("glm-5.3-flashx", 2_065_000, "glm-5.3-flashx"),
        ("muse-spark-1.3", 6_900_000, "muse-spark-1.3"),
        ("muse-wonder-1.0", 28_200_000, "muse-wonder-1.0"),
    ] {
        let cost = table.estimate_cost_at("cursor", Some(&observed(name)), &usage, &catalog_time());
        assert_eq!(
            cost.estimated_api_equivalent_micro_usd,
            Some(micro_usd),
            "{name}"
        );
        assert_eq!(
            cost.pricing_source,
            Some(format!("cursor_api_pricing:{model}")),
            "{name}"
        );
    }
}

#[test]
fn cursor_models_have_no_long_context_tier() {
    let table = embedded_pricing();
    // One known request with a 400k prompt keeps the base input rate.
    let long_prompt = UsageCounts {
        input_tokens: Some(400_000),
        requests: Some(1),
        ..UsageCounts::default()
    };
    for (name, micro_usd) in [
        ("deepseek-v41-flash-1017", 120_000),
        ("kimi-k3", 1_200_000),
        ("glm-5.3", 560_000),
        ("glm-5.3-flash", 60_000),
        ("muse-spark-1.3", 500_000),
        ("muse-wonder-1.0", 1_600_000),
    ] {
        assert_eq!(
            table
                .estimate_cost_at(
                    "cursor",
                    Some(&observed(name)),
                    &long_prompt,
                    &catalog_time()
                )
                .estimated_api_equivalent_micro_usd,
            Some(micro_usd),
            "{name}"
        );
    }
}
