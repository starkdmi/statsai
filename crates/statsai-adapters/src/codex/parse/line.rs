use super::*;

pub(crate) fn is_codex_session_meta(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("session_meta")
}

pub(crate) fn codex_model_from_value(value: &Value, fallback: Option<&str>) -> Option<ModelInfo> {
    model_from_nested_value(value, fallback)
}

pub(crate) fn is_codex_turn_context(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("turn_context")
}

pub(crate) fn is_codex_token_count(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("event_msg")
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("token_count")
}

pub(crate) fn is_codex_task_started(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("event_msg")
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("task_started")
}

pub(crate) fn is_codex_task_complete(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("event_msg")
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("task_complete")
}

pub(crate) fn codex_visible_message_role(value: &Value) -> Option<&str> {
    (value.get("type").and_then(Value::as_str) == Some("response_item")
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("message"))
    .then(|| value.pointer("/payload/role").and_then(Value::as_str))
    .flatten()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexLineKind {
    Irrelevant,
    SessionMeta,
    TurnContext,
    ResponseItemMessage,
    EventUserMessage,
    TokenCount,
    TaskStarted,
    TaskComplete,
    HeadlessUsage,
}

#[derive(Deserialize)]
pub(crate) struct CodexQuotaLineProbe {
    #[serde(rename = "type")]
    pub(crate) line_type: Option<String>,
    pub(crate) payload: Option<CodexQuotaPayloadProbe>,
}

#[derive(Deserialize)]
pub(crate) struct CodexQuotaPayloadProbe {
    #[serde(rename = "type")]
    pub(crate) payload_type: Option<String>,
    pub(crate) rate_limits: Option<serde::de::IgnoredAny>,
}

pub(crate) fn is_codex_quota_line_structurally(line: &str) -> bool {
    if !line.contains("\"event_msg\"")
        || !line.contains("\"token_count\"")
        || !line.contains("\"rate_limits\"")
    {
        return false;
    }
    serde_json::from_str::<CodexQuotaLineProbe>(line)
        .ok()
        .is_some_and(|probe| {
            probe.line_type.as_deref() == Some("event_msg")
                && probe.payload.is_some_and(|payload| {
                    payload.payload_type.as_deref() == Some("token_count")
                        && payload.rate_limits.is_some()
                })
        })
}

pub(crate) fn codex_line_header(line: &str) -> &str {
    codex_prefix_at_char_boundary(line, 256)
}

pub(crate) fn codex_line_kind(line: &str) -> CodexLineKind {
    let header = codex_line_header(line);
    if header.contains("\"type\":\"session_meta\"") {
        return CodexLineKind::SessionMeta;
    }
    if header.contains("\"type\":\"turn_context\"") {
        return CodexLineKind::TurnContext;
    }
    if header.contains("\"type\":\"response_item\"") {
        return if header.contains("\"payload\":{\"type\":\"message\"") {
            CodexLineKind::ResponseItemMessage
        } else {
            CodexLineKind::Irrelevant
        };
    }
    if header.contains("\"type\":\"event_msg\"") {
        if header.contains("\"payload\":{\"type\":\"user_message\"") {
            return CodexLineKind::EventUserMessage;
        }
        if header.contains("\"payload\":{\"type\":\"token_count\"") {
            return CodexLineKind::TokenCount;
        }
        if header.contains("\"payload\":{\"type\":\"task_started\"") {
            return CodexLineKind::TaskStarted;
        }
        if header.contains("\"payload\":{\"type\":\"task_complete\"") {
            return CodexLineKind::TaskComplete;
        }
        return CodexLineKind::Irrelevant;
    }
    if header.contains("\"usage\":")
        || header.contains("\"token_count\":")
        || header.contains("\"message\":{\"usage\":")
        || header.contains("\"data\":{\"usage\":")
        || header.contains("\"result\":{\"usage\":")
        || header.contains("\"response\":{\"usage\":")
    {
        return CodexLineKind::HeadlessUsage;
    }
    CodexLineKind::Irrelevant
}
