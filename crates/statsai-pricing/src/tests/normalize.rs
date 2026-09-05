use super::*;
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
fn normalizes_fable_5_1_ahead_of_fable_5() {
    assert_eq!(normalize_model_name("claude-fable-5-1"), "claude-fable-5-1");
    assert_eq!(normalize_model_name("claude-fable-5.1"), "claude-fable-5-1");
    assert_eq!(
        normalize_model_name("claude-fable-5-1-thinking-max"),
        "claude-fable-5-1"
    );
    assert_eq!(
        normalize_model_name("claude-fable-5-thinking-high"),
        "claude-fable-5"
    );
}

#[test]
fn normalizes_reversed_cursor_claude_names() {
    assert_eq!(
        normalize_model_name("claude-4.5-sonnet"),
        "claude-sonnet-4-5"
    );
    assert_eq!(normalize_model_name("claude-4.5-haiku"), "claude-haiku-4-5");
    assert_eq!(normalize_model_name("claude-5-opus"), "claude-opus-5");
    // Canonical ordering is untouched by the reversed-name fallback.
    assert_eq!(
        normalize_model_name("claude-sonnet-4-5"),
        "claude-sonnet-4-5"
    );
}

#[test]
fn normalizes_gpt_6_astra() {
    assert_eq!(normalize_model_name("gpt-6-astra"), "gpt-6-astra");
    assert_eq!(normalize_model_name("gpt-6-astra-codex"), "gpt-6-astra");
    assert_eq!(
        normalize_model_name("openai/gpt-6-astra-preview"),
        "gpt-6-astra"
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
fn refreshing_normalization_recovers_a_stale_cached_name() {
    // What a scan produced before Fable 5.1 had its own catalog entry.
    let stale = statsai_core::ModelInfo {
        name: Some("claude-fable-5-1-thinking-max".to_string()),
        normalized_name: Some("claude-fable-5".to_string()),
        provider_model_id: Some("claude-fable-5-1-thinking-max".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    };

    let refreshed = model_with_refreshed_normalization(&stale);

    assert_eq!(
        refreshed.normalized_name.as_deref(),
        Some("claude-fable-5-1")
    );
    // Only the derived field moves; the observation is untouched.
    assert_eq!(refreshed.name, stale.name);
    assert_eq!(refreshed.provider_model_id, stale.provider_model_id);
}

#[test]
fn refreshing_normalization_keeps_provider_qualified_and_nameless_models_intact() {
    let qualified = statsai_core::ModelInfo {
        name: Some("xai/grok-build-0.1".to_string()),
        normalized_name: Some("xai/grok-build-0.1".to_string()),
        provider_model_id: Some("xai/grok-build-0.1".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    };
    assert_eq!(
        model_with_refreshed_normalization(&qualified)
            .normalized_name
            .as_deref(),
        Some("grok-build-0.1")
    );

    // An unpriced model drops its provider segment too. Gating on pricing
    // would split one model's history across two group names.
    let unpriced = statsai_core::ModelInfo {
        name: Some("google/gemini-3.1-pro".to_string()),
        normalized_name: Some("gemini-3.1-pro".to_string()),
        provider_model_id: Some("google/gemini-3.1-pro".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    };
    assert_eq!(
        model_with_refreshed_normalization(&unpriced)
            .normalized_name
            .as_deref(),
        Some("gemini-3.1-pro")
    );

    let nameless = statsai_core::ModelInfo {
        name: None,
        normalized_name: Some("whatever-was-cached".to_string()),
        provider_model_id: None,
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    };
    assert_eq!(model_with_refreshed_normalization(&nameless), nameless);
}

#[test]
fn refreshing_normalization_agrees_with_what_a_fresh_scan_records() {
    // A stored event's refreshed name must equal what an adapter would write
    // today, priced or not, or repricing splits a model into two groups.
    for label in [
        "google/gemini-3.1-pro",
        "openrouter/x-ai/grok-4.6",
        "anthropic/claude-sonnet-4-5",
        "some-vendor/not-a-real-model",
        "gpt-5.6-luna",
    ] {
        let scanned = normalize_qualified_model_name(label);
        let model = statsai_core::ModelInfo {
            name: Some(label.to_string()),
            normalized_name: Some(scanned.clone()),
            provider_model_id: Some(label.to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        };

        let refreshed = model_with_refreshed_normalization(&model);

        assert_eq!(refreshed.normalized_name.as_deref(), Some(scanned.as_str()));
    }
}
