use super::*;
use crate::*;

pub(crate) const CODEX_TASK_PREVIEW_RAW_BYTES: usize = 24 * 1024;

pub(crate) fn choose_best_task_preview(previews: &[CodexPromptPreview]) -> Option<String> {
    let mut best = None::<(i32, i32, &str)>;
    for preview in previews {
        let text = preview.text.as_str();
        let is_generic = task_title_is_generic(Some(text));
        let is_weak = task_title_is_weak_signal(Some(text));
        let mut score = task_title_signal_score(Some(text));
        if !is_generic {
            score += 6;
        }
        if !is_weak {
            score += 2;
        }
        score += preview.source.priority() * 4;
        let source_priority = preview.source.priority();
        if best.as_ref().is_none_or(|(best_score, best_source, _)| {
            score > *best_score || (score == *best_score && source_priority > *best_source)
        }) {
            best = Some((score, source_priority, text));
        }
    }

    best.and_then(|(score, _, text)| {
        let is_generic = task_title_is_generic(Some(text));
        let is_weak = task_title_is_weak_signal(Some(text));
        if (score > 0 || !is_weak) && !is_generic {
            Some(text.to_string())
        } else {
            None
        }
    })
}

pub(crate) fn codex_task_title(
    session_title: Option<&str>,
    prompt_preview: Option<&str>,
) -> (String, &'static str, bool) {
    let prompt_title = task_title_from_prompt(prompt_preview);
    if let Some(title) = summarize_task_text(session_title, 90) {
        let is_meta = task_title_is_generic(Some(title.as_str()));
        if !is_meta {
            if let Some(prompt_title) = prompt_title.as_ref() {
                if should_prefer_codex_prompt_title(title.as_str(), prompt_title.as_str()) {
                    let prompt_is_meta = task_title_is_generic(Some(prompt_title.as_str()));
                    return (prompt_title.clone(), "user_prompt", prompt_is_meta);
                }
            }
            return (title, "thread_name", false);
        }
        if prompt_title.is_none() {
            return (title, "thread_name", true);
        }
    }
    if let Some(prompt_title) = prompt_title {
        let is_meta = task_title_is_generic(Some(prompt_title.as_str()));
        return (prompt_title, "user_prompt", is_meta);
    }
    (
        "Codex task".to_string(),
        "default",
        task_title_is_generic(Some("Codex task")),
    )
}

pub(crate) fn should_prefer_codex_prompt_title(session_title: &str, prompt_title: &str) -> bool {
    let session_score = task_title_signal_score(Some(session_title));
    let prompt_score = task_title_signal_score(Some(prompt_title));
    let session_weak = task_title_is_weak_signal(Some(session_title));
    let shared_topic_count = title_topic_tokens(session_title)
        .intersection(&title_topic_tokens(prompt_title))
        .count();

    (session_weak && !task_title_is_weak_signal(Some(prompt_title)))
        || (shared_topic_count == 0 && prompt_score >= session_score + 2)
        || (shared_topic_count <= 1 && session_score < 6 && prompt_score > session_score)
}

pub(crate) fn materialize_codex_task_previews(
    candidates: &[CodexPromptPreviewCandidate],
) -> Vec<CodexPromptPreview> {
    let has_provider_native_event = candidates
        .iter()
        .any(|candidate| candidate.source == CodexPromptPreviewSource::UserMessageEvent);

    candidates
        .iter()
        .filter(|candidate| {
            !has_provider_native_event
                || candidate.source == CodexPromptPreviewSource::UserMessageEvent
        })
        .filter_map(|candidate| {
            task_preview_from_prompt(Some(candidate.raw_text.as_str()), 220).map(|text| {
                CodexPromptPreview {
                    text,
                    source: candidate.source,
                }
            })
        })
        .collect()
}

pub(crate) fn codex_user_message_preview(value: &Value) -> Option<CodexPromptPreviewCandidate> {
    if value.get("type").and_then(Value::as_str) == Some("response_item")
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("message")
        && value.pointer("/payload/role").and_then(Value::as_str) == Some("user")
    {
        return codex_message_content_preview_text(
            value.pointer("/payload/content"),
            CODEX_TASK_PREVIEW_RAW_BYTES,
        )
        .and_then(|text| codex_prompt_preview_input(Some(text.as_str())))
        .map(|raw_text| CodexPromptPreviewCandidate {
            raw_text,
            source: CodexPromptPreviewSource::ResponseItemUser,
        });
    }

    if value.get("type").and_then(Value::as_str) == Some("event_msg")
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("user_message")
    {
        return codex_prompt_preview_input(
            value
                .pointer("/payload/message")
                .and_then(Value::as_str)
                .or_else(|| value.pointer("/payload/text").and_then(Value::as_str)),
        )
        .map(|raw_text| CodexPromptPreviewCandidate {
            raw_text,
            source: CodexPromptPreviewSource::UserMessageEvent,
        });
    }

    None
}

pub(crate) fn codex_event_user_message_preview_from_line(
    line: &str,
    max_bytes: usize,
) -> Option<String> {
    codex_json_string_prefix_after_marker(line, "\"message\":\"", max_bytes)
        .or_else(|| codex_json_string_prefix_after_marker(line, "\"text\":\"", max_bytes))
}

pub(crate) fn codex_response_item_user_preview_from_line(
    line: &str,
    max_bytes: usize,
) -> Option<String> {
    let mut preview = String::new();
    let mut search_from = 0usize;
    let markers = [
        "\"text\":\"",
        "\"content\":{\"text\":\"",
        "\"input\":{\"text\":\"",
    ];

    while preview.len() < max_bytes {
        let mut next_marker = None;
        for marker in markers {
            if let Some(relative) = line[search_from..].find(marker) {
                let absolute = search_from.saturating_add(relative);
                match next_marker {
                    Some((best, _)) if absolute >= best => {}
                    _ => next_marker = Some((absolute, marker)),
                }
            }
        }
        let Some((marker_index, marker)) = next_marker else {
            break;
        };
        if !preview.is_empty() {
            if preview.len().saturating_add(1) > max_bytes {
                break;
            }
            preview.push('\n');
        }
        let remaining = max_bytes.saturating_sub(preview.len());
        let value_start = marker_index.saturating_add(marker.len());
        let Some(part) = codex_json_string_prefix_at(line, value_start, remaining) else {
            break;
        };
        preview.push_str(&part);
        search_from = value_start;
        if part.len() < remaining {
            break;
        }
    }

    (!preview.is_empty()).then_some(preview)
}

pub(crate) fn codex_json_string_prefix_after_marker(
    haystack: &str,
    marker: &str,
    max_output_bytes: usize,
) -> Option<String> {
    let start = haystack.find(marker)?.saturating_add(marker.len());
    codex_json_string_prefix_at(haystack, start, max_output_bytes)
}

pub(crate) fn codex_json_string_prefix_at(
    haystack: &str,
    start: usize,
    max_output_bytes: usize,
) -> Option<String> {
    let bytes = haystack.as_bytes();
    if start >= bytes.len() {
        return None;
    }
    let mut output = String::new();
    let mut index = start;

    while index < bytes.len() && output.len() < max_output_bytes {
        match bytes[index] {
            b'"' => break,
            b'\\' => {
                index = index.saturating_add(1);
                let escaped = bytes.get(index).copied()?;
                match escaped {
                    b'"' => output.push('"'),
                    b'\\' => output.push('\\'),
                    b'/' => output.push('/'),
                    b'b' => output.push('\u{0008}'),
                    b'f' => output.push('\u{000C}'),
                    b'n' => output.push('\n'),
                    b'r' => output.push('\r'),
                    b't' => output.push('\t'),
                    b'u' => {
                        let (decoded, consumed) = codex_decode_json_unicode_escape(bytes, index)?;
                        if output.len().saturating_add(decoded.len_utf8()) > max_output_bytes {
                            break;
                        }
                        output.push(decoded);
                        index = consumed;
                    }
                    _ => return None,
                }
                index = index.saturating_add(1);
            }
            _ => {
                let character = haystack[index..].chars().next()?;
                if output.len().saturating_add(character.len_utf8()) > max_output_bytes {
                    break;
                }
                output.push(character);
                index = index.saturating_add(character.len_utf8());
            }
        }
    }

    (!output.is_empty()).then_some(output)
}

pub(crate) fn codex_decode_json_unicode_escape(
    bytes: &[u8],
    escape_index: usize,
) -> Option<(char, usize)> {
    let scalar = codex_unicode_escape_scalar(bytes, escape_index.saturating_add(1))?;
    let mut consumed = escape_index.saturating_add(4);
    if !(0xD800..=0xDBFF).contains(&scalar) {
        return char::from_u32(scalar).map(|character| (character, consumed));
    }

    if bytes.get(consumed.saturating_add(1)) != Some(&b'\\')
        || bytes.get(consumed.saturating_add(2)) != Some(&b'u')
    {
        return char::from_u32(0xFFFD).map(|character| (character, consumed));
    }
    let low = codex_unicode_escape_scalar(bytes, consumed.saturating_add(3))?;
    if !(0xDC00..=0xDFFF).contains(&low) {
        return char::from_u32(0xFFFD).map(|character| (character, consumed));
    }
    consumed = consumed.saturating_add(6);
    let combined = 0x10000 + (((scalar - 0xD800) << 10) | (low - 0xDC00));
    char::from_u32(combined).map(|character| (character, consumed))
}

pub(crate) fn codex_unicode_escape_scalar(bytes: &[u8], start: usize) -> Option<u32> {
    let end = start.saturating_add(4);
    let hex = std::str::from_utf8(bytes.get(start..end)?).ok()?;
    u32::from_str_radix(hex, 16).ok()
}

pub(crate) fn codex_prompt_preview_input(value: Option<&str>) -> Option<String> {
    let raw = value?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(
        codex_prefix_at_char_boundary(raw, CODEX_TASK_PREVIEW_RAW_BYTES)
            .trim()
            .to_string(),
    )
}

pub(crate) fn codex_message_content_preview_text(
    value: Option<&Value>,
    max_bytes: usize,
) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(codex_prefix_at_char_boundary(text, max_bytes).to_string());
    }
    let array = value.as_array()?;
    let mut excerpt = String::new();
    let mut used_bytes = 0usize;

    for part in array.iter().filter_map(|item| {
        item.get("text")
            .and_then(Value::as_str)
            .or_else(|| item.pointer("/content/text").and_then(Value::as_str))
            .or_else(|| item.pointer("/input/text").and_then(Value::as_str))
    }) {
        if used_bytes >= max_bytes {
            break;
        }
        if !excerpt.is_empty() {
            if used_bytes.saturating_add(1) > max_bytes {
                break;
            }
            excerpt.push('\n');
            used_bytes = used_bytes.saturating_add(1);
        }

        let remaining_bytes = max_bytes.saturating_sub(used_bytes);
        if part.len() > remaining_bytes {
            excerpt.push_str(codex_prefix_at_char_boundary(part, remaining_bytes));
            break;
        }

        excerpt.push_str(part);
        used_bytes = used_bytes.saturating_add(part.len());
    }

    (!excerpt.is_empty()).then_some(excerpt)
}

pub(crate) fn codex_prefix_at_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub(crate) fn codex_timestamp_from_text(
    text: Option<&str>,
    fallback: DateTime<Utc>,
) -> (DateTime<Utc>, bool) {
    text.and_then(|value| {
        DateTime::parse_from_rfc3339(value)
            .map(|parsed| parsed.with_timezone(&Utc))
            .ok()
            .or_else(|| value.parse::<i64>().ok().and_then(timestamp_from_number))
    })
    .map(|timestamp| (timestamp, false))
    .unwrap_or((fallback, true))
}

pub(crate) fn codex_preview_from_response_parts(
    parts: &[CodexFastContentPart<'_>],
    max_bytes: usize,
) -> Option<String> {
    let mut excerpt = String::new();
    let mut used_bytes = 0usize;

    for part in parts.iter().filter_map(|part| {
        part.text
            .as_deref()
            .or_else(|| {
                part.content
                    .as_ref()
                    .and_then(|content| content.text.as_deref())
            })
            .or_else(|| part.input.as_ref().and_then(|input| input.text.as_deref()))
    }) {
        if used_bytes >= max_bytes {
            break;
        }
        if !excerpt.is_empty() {
            if used_bytes.saturating_add(1) > max_bytes {
                break;
            }
            excerpt.push('\n');
            used_bytes = used_bytes.saturating_add(1);
        }
        let remaining_bytes = max_bytes.saturating_sub(used_bytes);
        if part.len() > remaining_bytes {
            excerpt.push_str(codex_prefix_at_char_boundary(part, remaining_bytes));
            break;
        }
        excerpt.push_str(part);
        used_bytes = used_bytes.saturating_add(part.len());
    }

    (!excerpt.is_empty()).then_some(excerpt)
}

pub(crate) fn codex_task_timestamp(value: &Value, pointers: &[&str]) -> Option<DateTime<Utc>> {
    pointers
        .iter()
        .filter_map(|pointer| value.pointer(pointer))
        .find_map(timestamp_from_scalar)
}

pub(crate) fn codex_task_u64(value: &Value, pointers: &[&str]) -> Option<u64> {
    pointers
        .iter()
        .filter_map(|pointer| value.pointer(pointer))
        .find_map(value_as_u64)
}

pub(crate) fn codex_duration_from_turn_timestamps(
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Option<u64> {
    let millis = completed_at
        .signed_duration_since(started_at)
        .num_milliseconds();
    (millis >= 0).then_some(millis as u64)
}
