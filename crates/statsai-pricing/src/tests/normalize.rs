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
