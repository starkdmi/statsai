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
