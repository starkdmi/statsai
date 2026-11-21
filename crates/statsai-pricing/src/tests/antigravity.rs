//! The model names Antigravity records (STA-175): the label the user selected
//! as `name`, and the name the backend served as `provider_model_id`.

use super::*;
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;
use statsai_core::{ModelInfo, UsageCounts};

/// 2026-11-21, the catalog date of pricing version 8, which added these
/// models. Every rate checked here is in effect that day.
fn catalog_day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 21).expect("catalog day")
}

fn catalog_time() -> DateTime<Utc> {
    catalog_day().and_hms_opt(12, 0, 0).expect("noon").and_utc()
}

fn observed(name: Option<&str>, served: Option<&str>) -> ModelInfo {
    ModelInfo {
        name: name.map(str::to_string),
        normalized_name: name.or(served).map(normalize_qualified_model_name),
        provider_model_id: served.map(str::to_string),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    }
}

fn priced_as(table: &PricingTable, name: Option<&str>, served: Option<&str>) -> Option<String> {
    table.priced_model_name(&observed(name, served), catalog_day())
}

fn every_token_kind() -> UsageCounts {
    UsageCounts {
        input_tokens: Some(1_000_000),
        cache_creation_tokens: Some(1_000_000),
        cache_read_tokens: Some(1_000_000),
        output_tokens: Some(1_000_000),
        ..UsageCounts::default()
    }
}

/// Selected labels, on their own, and the model each is priced as.
const LABELS: &[(&str, Option<&str>)] = &[
    ("gemini-3-flash-agent", Some("gemini-3-flash")),
    ("gemini-3.5-flash-low", Some("gemini-3.5-flash")),
    ("gemini-3.5-flash-extra-low", Some("gemini-3.5-flash")),
    ("gemini-3.8-flash-low", Some("gemini-3.8-flash")),
    ("gemini-3.8-flash-high", Some("gemini-3.8-flash")),
    ("gemini-4-neon-medium", Some("gemini-4-neon")),
    ("gpt-oss-120b-medium", Some("gpt-oss-120b")),
    ("claude-sonnet-5-5-low", Some("claude-sonnet-5-5")),
    ("claude-opus-4-6-thinking", Some("claude-opus-4-6")),
    ("claude-sonnet-4-6", Some("claude-sonnet-4-6")),
];

/// Served names, on their own, and the model each is priced as.
const SERVED_NAMES: &[(&str, Option<&str>)] = &[
    // Shared by Gemini 3 Flash and Gemini 3.5 Flash requests.
    ("gemini-3-flash-a", None),
    ("gemini-3.8-flash-n", Some("gemini-3.8-flash")),
    ("gemini-3.8-flash", Some("gemini-3.8-flash")),
    ("gemini-4-neon", Some("gemini-4-neon")),
    ("gpt-oss-120b-medium", Some("gpt-oss-120b")),
    ("claude-sonnet-5-5-low", Some("claude-sonnet-5-5")),
    ("claude-opus-4-6-thinking", Some("claude-opus-4-6")),
    ("claude-sonnet-4-6", Some("claude-sonnet-4-6")),
];

/// The label and served-name pairs Antigravity records together.
const PAIRS: &[(&str, &str, &str)] = &[
    ("gemini-3-flash-agent", "gemini-3-flash-a", "gemini-3-flash"),
    (
        "gemini-3.5-flash-low",
        "gemini-3-flash-a",
        "gemini-3.5-flash",
    ),
    (
        "gemini-3.5-flash-extra-low",
        "gemini-3-flash-a",
        "gemini-3.5-flash",
    ),
    (
        "gemini-3.8-flash-low",
        "gemini-3.8-flash-n",
        "gemini-3.8-flash",
    ),
    (
        "gemini-3.8-flash-low",
        "gemini-3.8-flash",
        "gemini-3.8-flash",
    ),
    (
        "gemini-3.8-flash-high",
        "gemini-3.8-flash-n",
        "gemini-3.8-flash",
    ),
    (
        "gemini-3.8-flash-high",
        "gemini-3.8-flash",
        "gemini-3.8-flash",
    ),
    ("gemini-4-neon-medium", "gemini-4-neon", "gemini-4-neon"),
    ("gpt-oss-120b-medium", "gpt-oss-120b-medium", "gpt-oss-120b"),
    (
        "claude-sonnet-5-5-low",
        "claude-sonnet-5-5-low",
        "claude-sonnet-5-5",
    ),
    (
        "claude-opus-4-6-thinking",
        "claude-opus-4-6-thinking",
        "claude-opus-4-6",
    ),
    (
        "claude-sonnet-4-6",
        "claude-sonnet-4-6",
        "claude-sonnet-4-6",
    ),
];

#[test]
fn every_label_and_served_name_reaches_its_model_or_stays_unpriced() {
    let table = embedded_pricing();
    for (label, expected) in LABELS {
        assert_eq!(
            priced_as(&table, Some(label), None).as_deref(),
            *expected,
            "label {label}"
        );
    }
    for (served, expected) in SERVED_NAMES {
        assert_eq!(
            priced_as(&table, None, Some(served)).as_deref(),
            *expected,
            "served name {served}"
        );
    }
    for (label, served, expected) in PAIRS {
        assert_eq!(
            priced_as(&table, Some(label), Some(served)).as_deref(),
            Some(*expected),
            "label {label} served as {served}"
        );
    }
}

#[test]
fn effort_and_mode_suffixes_do_not_change_the_model() {
    for (name, expected) in [
        ("gemini-3-flash", "gemini-3-flash"),
        ("gemini-3-flash-preview", "gemini-3-flash"),
        ("gemini-3-flash-high", "gemini-3-flash"),
        ("gemini-3.5-flash", "gemini-3.5-flash"),
        ("gemini-3.5-flash-high", "gemini-3.5-flash"),
        ("gemini-3.5-flash-thinking", "gemini-3.5-flash"),
        ("gemini-3.8-flash-medium", "gemini-3.8-flash"),
        ("gemini-3.8-flash-extra-low", "gemini-3.8-flash"),
        ("gemini-4-neon-high", "gemini-4-neon"),
        ("gemini-4-neon-low", "gemini-4-neon"),
        ("gpt-oss-120b", "gpt-oss-120b"),
        ("gpt-oss-120b-high", "gpt-oss-120b"),
        ("openai/gpt-oss-120b-low", "gpt-oss-120b"),
    ] {
        assert_eq!(normalize_model_name(name), expected, "{name}");
    }
    assert_eq!(
        normalize_qualified_model_name("google/gemini-3.5-flash-low"),
        "gemini-3.5-flash"
    );
}

#[test]
fn gemini_3_3_5_and_3_8_flash_and_lite_variants_do_not_fold_into_each_other() {
    // "gemini-3-flash" does not occur in "gemini-3.5-flash" or
    // "gemini-3.8-flash", and Flash-Lite has its own rule ahead of Flash.
    for (name, expected) in [
        ("gemini-3-flash-agent", "gemini-3-flash"),
        ("gemini-3.5-flash-low", "gemini-3.5-flash"),
        ("gemini-3.8-flash-low", "gemini-3.8-flash"),
        ("gemini-3.5-flash-lite", "gemini-3.5-flash-lite"),
        ("gemini-3.5-flash-lite-low", "gemini-3.5-flash-lite"),
        ("gemini-3.5-flash-lite-preview", "gemini-3.5-flash-lite"),
        // Gemini 3.1 Flash-Lite has no catalog entry and is not Gemini 3 Flash.
        ("gemini-3.1-flash-lite", "gemini-3.1-flash-lite"),
        ("gemini-3.1-pro", "gemini-3.1-pro"),
        // Google's 3.8 speech models contain "gemini-3.8-flash" but bill
        // audio rates: rules ahead of 3.8 Flash give each its own name, so
        // neither takes a text price, even a future 3.8 Flash-Lite one.
        ("gemini-3.8-flash-lite-tts", "gemini-3.8-flash-lite-tts"),
        ("gemini-3.8-flash-tts", "gemini-3.8-flash-tts"),
        ("gemini-3.8-flash-lite", "gemini-3.8-flash-lite"),
    ] {
        assert_eq!(normalize_model_name(name), expected, "{name}");
    }

    let table = embedded_pricing();
    // Some of these have published prices of their own, so the test only
    // rules out the Flash model each would otherwise fall into.
    for (name, not_model) in [
        ("gemini-3.1-flash-lite", "gemini-3-flash"),
        ("gemini-3.5-flash-lite", "gemini-3.5-flash"),
        ("gemini-3.8-flash-lite", "gemini-3.8-flash"),
        ("gemini-3.8-flash-lite-tts", "gemini-3.8-flash"),
        ("gemini-3.8-flash-tts", "gemini-3.8-flash"),
    ] {
        assert_ne!(
            priced_as(&table, Some(name), None).as_deref(),
            Some(not_model),
            "{name}"
        );
    }

    let output_rate = |name: &str| {
        table
            .pricing_for_model_on(name, catalog_day())
            .map(|pricing| pricing.output_per_million)
    };
    assert_eq!(output_rate("gemini-3-flash"), Some(3.0));
    assert_eq!(output_rate("gemini-3.5-flash"), Some(9.0));
    assert_eq!(output_rate("gemini-3.5-flash-lite"), Some(2.5));
    assert_eq!(output_rate("gemini-3.8-flash"), Some(3.75));
}

#[test]
fn gemini_3_flash_a_is_never_priced_as_a_single_model() {
    let table = embedded_pricing();

    // A `gemini-3-flash` pattern exists and would match the served name.
    let rule = table
        .patterns
        .iter()
        .find(|pattern| {
            pattern
                .contains
                .iter()
                .any(|needle| "gemini-3-flash-a".contains(needle.as_str()))
        })
        .expect("a pattern that matches gemini-3-flash-a");
    assert_eq!(rule.model, "gemini-3-flash");

    // The exact `names` key is checked first and keeps it as observed.
    for spelling in ["gemini-3-flash-a", " GEMINI-3-FLASH-A "] {
        assert_eq!(normalize_model_name(spelling), "gemini-3-flash-a");
    }
    assert_eq!(
        table.pricing_for_model_on("gemini-3-flash-a", catalog_day()),
        None
    );
    let cost = table.estimate_cost_at(
        "antigravity",
        Some(&observed(
            Some("gemini-3-flash-a"),
            Some("gemini-3-flash-a"),
        )),
        &every_token_kind(),
        &catalog_time(),
    );
    assert_eq!(cost.estimated_api_equivalent_micro_usd, None);
    assert_eq!(cost.pricing_source.as_deref(), Some("unknown"));

    // A 3.5 Flash label with no price falls back to the served name, which
    // must not bill the request as Gemini 3 Flash.
    let mut file: Value =
        serde_json::from_str(embedded_pricing_text()).expect("embedded pricing.json is JSON");
    file["models"]
        .as_object_mut()
        .expect("models")
        .remove("gemini-3.5-flash")
        .expect("gemini-3.5-flash is priced");
    let without_3_5 = PricingTable::parse(&file.to_string()).expect("valid pricing file");
    for label in ["gemini-3.5-flash-low", "gemini-3.5-flash-extra-low"] {
        let model = observed(Some(label), Some("gemini-3-flash-a"));
        assert_eq!(without_3_5.priced_model_name(&model, catalog_day()), None);
        let cost = without_3_5.estimate_cost_at(
            "antigravity",
            Some(&model),
            &every_token_kind(),
            &catalog_time(),
        );
        assert_eq!(cost.estimated_api_equivalent_micro_usd, None, "{label}");
    }
}

#[test]
fn gemini_and_gpt_oss_use_vertex_ai_rates() {
    let table = embedded_pricing();
    for (label, served, micro_usd, source) in [
        // input + cache write (billed as input) + cache read + output.
        (
            "gemini-3-flash-agent",
            "gemini-3-flash-a",
            4_050_000,
            "vertex_ai_pricing:gemini-3-flash",
        ),
        (
            "gemini-3.5-flash-low",
            "gemini-3-flash-a",
            12_150_000,
            "vertex_ai_pricing:gemini-3.5-flash",
        ),
        (
            "gemini-3.5-flash-lite",
            "gemini-3.5-flash-lite",
            3_130_000,
            "vertex_ai_pricing:gemini-3.5-flash-lite",
        ),
        // The introductory price, in effect through 2026-12-31.
        (
            "gemini-3.8-flash-high",
            "gemini-3.8-flash",
            5_325_000,
            "vertex_ai_pricing:gemini-3.8-flash",
        ),
        (
            "gemini-4-neon-medium",
            "gemini-4-neon",
            21_150_000,
            "vertex_ai_pricing:gemini-4-neon",
        ),
        // No cached-input rate is published, so cache reads bill as input.
        (
            "gpt-oss-120b-medium",
            "gpt-oss-120b-medium",
            630_000,
            "vertex_ai_pricing:gpt-oss-120b",
        ),
    ] {
        let cost = table.estimate_cost_at(
            "antigravity",
            Some(&observed(Some(label), Some(served))),
            &every_token_kind(),
            &catalog_time(),
        );
        assert_eq!(
            cost.estimated_api_equivalent_micro_usd,
            Some(micro_usd),
            "{label}"
        );
        assert_eq!(cost.pricing_source.as_deref(), Some(source), "{label}");
    }
}

#[test]
fn gemini_thinking_bills_as_output_without_a_prompt_size_tier() {
    let table = embedded_pricing();
    let model = observed(Some("gemini-3.5-flash-low"), Some("gemini-3-flash-a"));
    let thinking = UsageCounts {
        output_tokens: Some(1_000_000),
        reasoning_tokens: Some(1_000_000),
        ..UsageCounts::default()
    };
    // 2M generated tokens at $9/M.
    assert_eq!(
        table
            .estimate_cost_at("antigravity", Some(&model), &thinking, &catalog_time())
            .estimated_api_equivalent_micro_usd,
        Some(18_000_000)
    );

    // One known request with a 300k prompt keeps the base rates: none of
    // these models has a long-context tier.
    let long_prompt = UsageCounts {
        input_tokens: Some(300_000),
        requests: Some(1),
        ..UsageCounts::default()
    };
    for (label, micro_usd) in [
        ("gemini-3-flash-agent", 150_000),
        ("gemini-3.5-flash-low", 450_000),
        ("gemini-3.8-flash-low", 225_000),
        ("gemini-4-neon-medium", 900_000),
        ("gpt-oss-120b-medium", 27_000),
    ] {
        assert_eq!(
            table
                .estimate_cost_at(
                    "antigravity",
                    Some(&observed(Some(label), None)),
                    &long_prompt,
                    &catalog_time()
                )
                .estimated_api_equivalent_micro_usd,
            Some(micro_usd),
            "{label}"
        );
    }
}
