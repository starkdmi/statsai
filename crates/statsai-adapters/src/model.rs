use crate::value_as_u64;
use serde_json::Value;
use statsai_core::{ModelInfo, ReasoningLevel};
use statsai_pricing::normalize_model_name;

pub(crate) fn model_from_nested_value(value: &Value, fallback: Option<&str>) -> Option<ModelInfo> {
    let model = [
        value.get("model"),
        value.get("model_name"),
        value.pointer("/metadata/model"),
        value.pointer("/message/model"),
        value.pointer("/usage/model"),
        value.pointer("/request/model"),
        value.pointer("/data/model"),
        value.pointer("/data/model_name"),
        value.pointer("/data/metadata/model"),
        value.pointer("/result/model"),
        value.pointer("/result/model_name"),
        value.pointer("/result/metadata/model"),
        value.pointer("/response/model"),
        value.pointer("/response/model_name"),
        value.pointer("/response/metadata/model"),
        value.pointer("/payload/model"),
        value.pointer("/payload/model_name"),
        value.pointer("/payload/metadata/model"),
        value.pointer("/payload/info/model"),
        value.pointer("/payload/info/model_name"),
        value.pointer("/payload/info/metadata/model"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .or(fallback)?;
    Some(model_info(model))
}

pub(crate) fn claude_reasoning_state_from_value(value: &Value) -> ModelReasoningState {
    let effort = value
        .get("effort")
        .or_else(|| value.pointer("/message/effort"))
        .and_then(Value::as_str);
    if effort.is_some() {
        return ModelReasoningState::from_raw(effort);
    }

    let max_thinking_tokens = [
        value.pointer("/thinkingMetadata/maxThinkingTokens"),
        value.pointer("/thinking_metadata/maxThinkingTokens"),
        value.pointer("/thinking_metadata/max_thinking_tokens"),
        value.pointer("/message/thinkingMetadata/maxThinkingTokens"),
        value.pointer("/message/thinking_metadata/maxThinkingTokens"),
        value.pointer("/message/thinking_metadata/max_thinking_tokens"),
    ]
    .into_iter()
    .flatten()
    .find_map(value_as_u64);

    ModelReasoningState {
        level: None,
        raw: max_thinking_tokens.map(|value| value.to_string()),
    }
}

pub(crate) fn claude_speed_from_usage(usage: &Value) -> Option<&str> {
    usage
        .get("speed")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|speed| !speed.is_empty())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelReasoningState {
    pub(crate) level: Option<ReasoningLevel>,
    pub(crate) raw: Option<String>,
}

impl ModelReasoningState {
    pub(crate) fn from_raw(value: Option<&str>) -> Self {
        let raw = value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        Self {
            level: raw.as_deref().and_then(ReasoningLevel::parse),
            raw,
        }
    }
}

pub(crate) fn apply_reasoning_state(model: &mut ModelInfo, reasoning: &ModelReasoningState) {
    if model.reasoning_level.is_none() {
        model.reasoning_level = reasoning.level;
    }
    if model.reasoning_level_raw.is_none() {
        model.reasoning_level_raw = reasoning.raw.clone();
    }
}

pub(crate) fn model_info_with_reasoning(model: &str, reasoning: &ModelReasoningState) -> ModelInfo {
    let mut info = model_info(model);
    apply_reasoning_state(&mut info, reasoning);
    info
}

pub(crate) fn with_reasoning_state(
    model: Option<ModelInfo>,
    reasoning: &ModelReasoningState,
) -> Option<ModelInfo> {
    model.map(|mut model| {
        apply_reasoning_state(&mut model, reasoning);
        model
    })
}

pub(crate) fn with_model_metadata(
    model: Option<ModelInfo>,
    reasoning: &ModelReasoningState,
    speed: Option<&str>,
) -> Option<ModelInfo> {
    with_reasoning_state(model, reasoning).map(|mut model| {
        model.speed = speed.map(ToOwned::to_owned);
        model
    })
}

pub(crate) fn reasoning_state_from_model(model: &ModelInfo) -> ModelReasoningState {
    ModelReasoningState {
        level: model.reasoning_level,
        raw: model.reasoning_level_raw.clone(),
    }
}

pub(crate) fn same_model_identity(left: Option<&ModelInfo>, right: &ModelInfo) -> bool {
    left.and_then(|model| model.provider_model_id.as_deref()) == right.provider_model_id.as_deref()
}

pub(crate) fn model_info(model: &str) -> ModelInfo {
    let normalized = normalize_model_name(model);
    ModelInfo {
        name: Some(model.to_string()),
        normalized_name: Some(normalized),
        provider_model_id: Some(model.to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    }
}

pub(crate) fn opencode_model_info(value: &str) -> Option<ModelInfo> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(json) = serde_json::from_str::<Value>(trimmed) {
        return opencode_model_info_from_value(&json).or_else(|| {
            Some(opencode_named_model_info(
                trimmed,
                &ModelReasoningState::default(),
            ))
        });
    }
    Some(opencode_named_model_info(
        trimmed,
        &ModelReasoningState::default(),
    ))
}

pub(crate) fn normalize_provider_qualified_model_name(label: &str) -> String {
    label
        .rsplit_once('/')
        .map(|(_, model)| normalize_model_name(model))
        .unwrap_or_else(|| normalize_model_name(label))
}

pub(crate) fn opencode_model_info_from_value(value: &Value) -> Option<ModelInfo> {
    let label = opencode_model_label_from_value(value)?;
    let reasoning = opencode_reasoning_state_from_value(value);
    Some(opencode_named_model_info(&label, &reasoning))
}

pub(crate) fn opencode_named_model_info(label: &str, reasoning: &ModelReasoningState) -> ModelInfo {
    ModelInfo {
        name: Some(label.to_string()),
        normalized_name: Some(normalize_provider_qualified_model_name(label)),
        provider_model_id: Some(label.to_string()),
        speed: None,
        reasoning_level: reasoning.level,
        reasoning_level_raw: reasoning.raw.clone(),
    }
}

pub(crate) fn opencode_model_label_from_value(value: &Value) -> Option<String> {
    let provider = opencode_provider_id_from_value(value);
    let model = opencode_model_id_from_value(value)?;
    Some(
        provider
            .map(|provider| format!("{provider}/{model}"))
            .unwrap_or(model),
    )
}

pub(crate) fn opencode_message_model_info(value: &Value) -> Option<ModelInfo> {
    opencode_model_info_from_value(value)
}

pub(crate) fn opencode_provider_id_from_value(value: &Value) -> Option<&str> {
    value
        .get("providerID")
        .or_else(|| value.get("provider_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            value.get("model").and_then(|model| {
                model
                    .get("providerID")
                    .or_else(|| model.get("provider_id"))
                    .and_then(Value::as_str)
            })
        })
}

pub(crate) fn opencode_model_id_from_value(value: &Value) -> Option<String> {
    value
        .get("modelID")
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            value
                .get("model")
                .and_then(opencode_model_id_from_model_value)
        })
        .or_else(|| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
}

pub(crate) fn opencode_model_id_from_model_value(value: &Value) -> Option<String> {
    value
        .get("modelID")
        .or_else(|| value.get("id"))
        .or_else(|| value.get("model"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

pub(crate) fn opencode_reasoning_state_from_value(value: &Value) -> ModelReasoningState {
    ModelReasoningState::from_raw(value.get("variant").and_then(Value::as_str).or_else(|| {
        value
            .get("model")
            .and_then(|model| model.get("variant"))
            .and_then(Value::as_str)
    }))
}

pub(crate) fn opencode_message_has_variant(value: &Value) -> bool {
    opencode_reasoning_state_from_value(value).raw.is_some()
}

pub(crate) fn codex_reasoning_state_from_value(value: &Value) -> ModelReasoningState {
    ModelReasoningState::from_raw(
        value
            .pointer("/payload/collaboration_mode/settings/reasoning_effort")
            .and_then(Value::as_str)
            .or_else(|| value.pointer("/payload/effort").and_then(Value::as_str)),
    )
}

#[test]
fn opencode_model_info_uses_model_name_for_stats_and_preserves_provider_identity() {
    let foo = opencode_model_info(r#"{"id":"model-x","providerID":"foo"}"#).expect("foo");
    let bar = opencode_model_info(r#"{"id":"model-x","providerID":"bar"}"#).expect("bar");

    assert_eq!(foo.name.as_deref(), Some("foo/model-x"));
    assert_eq!(bar.name.as_deref(), Some("bar/model-x"));
    assert_eq!(foo.provider_model_id.as_deref(), Some("foo/model-x"));
    assert_eq!(bar.provider_model_id.as_deref(), Some("bar/model-x"));
    assert_eq!(foo.normalized_name.as_deref(), Some("model-x"));
    assert_eq!(bar.normalized_name.as_deref(), Some("model-x"));
    assert_eq!(foo.reasoning_level, None);
    assert_eq!(bar.reasoning_level_raw, None);
}

#[test]
fn opencode_model_info_maps_variant_to_reasoning_fields() {
    let model =
        opencode_model_info(r#"{"providerID":"openai","modelID":"gpt-5.5","variant":"xhigh"}"#)
            .expect("model");

    assert_eq!(model.provider_model_id.as_deref(), Some("openai/gpt-5.5"));
    assert_eq!(model.reasoning_level, Some(ReasoningLevel::Xhigh));
    assert_eq!(model.reasoning_level_raw.as_deref(), Some("xhigh"));
}

#[test]
fn opencode_model_info_normalizes_provider_qualified_known_aliases() {
    let deepseek = opencode_model_info("opencode-go/deepseek-v4-pro").expect("deepseek");
    let grok = opencode_model_info(r#"{"id":"grok-build","providerID":"xai"}"#).expect("grok");

    assert_eq!(
        deepseek.name.as_deref(),
        Some("opencode-go/deepseek-v4-pro")
    );
    assert_eq!(
        deepseek.provider_model_id.as_deref(),
        Some("opencode-go/deepseek-v4-pro")
    );
    assert_eq!(deepseek.normalized_name.as_deref(), Some("deepseek-v4-pro"));
    assert_eq!(grok.name.as_deref(), Some("xai/grok-build"));
    assert_eq!(grok.provider_model_id.as_deref(), Some("xai/grok-build"));
    assert_eq!(grok.normalized_name.as_deref(), Some("grok-build-0.1"));
}
