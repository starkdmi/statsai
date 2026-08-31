use super::super::titles::{
    basic_normalize_phrase, looks_like_command_or_output_title, looks_like_path_stub_title,
    looks_like_structured_key_value_title, normalized_contains_fragments,
    normalized_matches_prefixes, LOW_SIGNAL_CONTAINS, LOW_SIGNAL_PREFIXES, META_WRAPPER_TOKENS,
    TITLE_TOPIC_STOP_WORDS, WRAPPER_FILLER_TOKENS,
};
use super::{expand_inline_markdown_headings, looks_like_prompt_scaffolding_line};

const REQUEST_MARKERS: &[&str] = &[
    "My request for Codex:",
    "My request for Claude Code:",
    "My request for Claude:",
    "My request for OpenCode:",
];

pub(crate) fn clean_task_text(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let mut candidate = strip_request_wrapper(value);
    if let Some(extracted) = extract_structured_task_signal(&candidate) {
        candidate = extracted;
    }
    candidate = expand_inline_markdown_headings(&candidate);
    candidate = candidate.replace("```", " ");

    let mut cleaned_lines = Vec::<String>::new();
    let mut in_code_fence = false;
    for raw_line in candidate.lines() {
        let Some(line) = clean_task_line(raw_line) else {
            continue;
        };
        if line.starts_with("```") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence || line.is_empty() {
            continue;
        }
        if task_scaffolding_line(&line) {
            continue;
        }
        cleaned_lines.push(line);
        if cleaned_lines.len() >= 6 {
            break;
        }
    }

    let compact = cleaned_lines.join(" ");
    let compact = compact.split_whitespace().collect::<Vec<_>>().join(" ");
    (!compact.is_empty()).then_some(compact)
}

pub(crate) fn truncate_task_text(compact: String, width: usize) -> Option<String> {
    let compact_len = compact.chars().count();
    if compact_len <= width {
        return Some(compact);
    }
    if width <= 3 {
        return Some(".".repeat(width));
    }
    let shortened = compact
        .chars()
        .take(width.saturating_sub(3))
        .collect::<String>();
    Some(format!("{}...", shortened.trim_end()))
}

pub(crate) fn polish_task_title_candidate(value: &str) -> String {
    let mut title = value.trim().to_string();
    if title.is_empty() {
        return String::new();
    }

    if let Some(stripped) = strip_meta_wrapper_prefix(&title) {
        title = stripped;
    }

    while let Some(stripped) = strip_conversational_prefix(&title) {
        if stripped == title || stripped.is_empty() {
            break;
        }
        title = stripped;
    }

    title = strip_inline_image_references(&title);
    title = strip_metric_dump_suffix(&title);
    title = strip_artifact_tokens(&title);
    title = strip_trailing_heading_wrapper_suffix(&title);
    title = title
        .replace('?', " ")
        .trim_start_matches([',', ':', ';', '-', '.'])
        .trim()
        .trim_end_matches([',', ':', ';', '?', '.'])
        .trim()
        .to_string();
    title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        value.trim().to_string()
    } else {
        title
    }
}

fn strip_trailing_heading_wrapper_suffix(value: &str) -> String {
    for suffix in [" implementation plan", " plan"] {
        let Some(cutoff) = value.len().checked_sub(suffix.len()) else {
            continue;
        };
        if value.len() > suffix.len()
            && value
                .get(cutoff..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
        {
            return value[..cutoff]
                .trim()
                .trim_end_matches([':', ';', '-', ','])
                .trim()
                .to_string();
        }
    }
    value.trim().to_string()
}

fn strip_metric_dump_suffix(value: &str) -> String {
    let lowercase = value.to_ascii_lowercase();
    let markers = [
        " coverage=",
        " f1@",
        " avg_tiou",
        " avg_tiou=",
        " mae=",
        " titlef1=",
        " cider=",
        " pred=",
        " gold=",
        " gen=",
    ];
    let cutoff = markers
        .iter()
        .filter_map(|marker| lowercase.find(marker))
        .min();
    cutoff
        .map(|index| {
            value[..index]
                .trim()
                .trim_end_matches(':')
                .trim()
                .to_string()
        })
        .unwrap_or_else(|| value.trim().to_string())
}

fn strip_inline_image_references(value: &str) -> String {
    let mut remaining = value;
    let mut cleaned = String::new();
    while let Some(start) = remaining.find("[Image") {
        cleaned.push_str(&remaining[..start]);
        let tail = &remaining[start..];
        let Some(end) = tail.find(']') else {
            remaining = tail;
            break;
        };
        remaining = &tail[end + 1..];
    }
    cleaned.push_str(remaining);
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_artifact_tokens(value: &str) -> String {
    value
        .split_whitespace()
        .filter(|token| !should_drop_artifact_token(token))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn strip_plain_role_prefix(value: &str) -> Option<String> {
    let lowercase = value.to_ascii_lowercase();
    for prefix in ["assistant:", "user:", "developer:", "system:", "d user:"] {
        if lowercase.starts_with(prefix) {
            let stripped = value[prefix.len()..].trim();
            return (!stripped.is_empty()).then_some(stripped.to_string());
        }
    }
    None
}

pub(crate) fn strip_meta_wrapper_prefix(value: &str) -> Option<String> {
    let (prefix, suffix) = value.split_once(':')?;
    let prefix_tokens = basic_normalize_phrase(prefix)
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if prefix_tokens.is_empty() || prefix_tokens.len() > 8 {
        return None;
    }
    let content_tokens = prefix_tokens
        .iter()
        .filter(|token| !TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()))
        .collect::<Vec<_>>();
    if content_tokens.is_empty() {
        return None;
    }
    let meta_token_count = content_tokens
        .iter()
        .filter(|token| {
            META_WRAPPER_TOKENS.contains(&token.as_str())
                || WRAPPER_FILLER_TOKENS.contains(&token.as_str())
        })
        .count();
    if meta_token_count < content_tokens.len() {
        return None;
    }
    let suffix = suffix.trim();
    (!suffix.is_empty()).then_some(suffix.to_string())
}

fn should_drop_artifact_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|character: char| {
        matches!(
            character,
            ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\''
        )
    });
    if trimmed.is_empty() {
        return false;
    }
    let lowercase = trimmed.to_ascii_lowercase();
    lowercase.starts_with("http://")
        || lowercase.starts_with("https://")
        || lowercase.starts_with("/users/")
        || lowercase.starts_with("/kaggle/")
        || lowercase.contains("jupyter-proxy.kaggle.net")
        || lowercase.starts_with("token=eyj")
        || lowercase.starts_with("<image")
        || lowercase.starts_with("name=")
        || lowercase.starts_with("name=[image")
        || lowercase.starts_with("</image")
        || lowercase.contains("[image")
        || lowercase.ends_with("]>")
        || lowercase == "[image"
        || looks_like_opaque_token(trimmed)
}

fn looks_like_opaque_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|character: char| {
        matches!(
            character,
            ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']'
        )
    });
    if trimmed.len() < 16 {
        return false;
    }
    let lowercase = trimmed.to_ascii_lowercase();
    if lowercase.starts_with("eyj") {
        return true;
    }
    let has_alpha = trimmed
        .chars()
        .any(|character| character.is_ascii_alphabetic());
    let has_digit = trimmed.chars().any(|character| character.is_ascii_digit());
    let safe_chars = trimmed.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '=')
    });
    has_alpha && has_digit && safe_chars
}

pub(crate) fn strip_conversational_prefix(value: &str) -> Option<String> {
    let lowercase = value.to_ascii_lowercase();
    for prefix in [
        "could you please ",
        "could you ",
        "can you please ",
        "can you ",
        "please ",
        "show me ",
        "tell me ",
        "how to ",
        "what are ",
        "what is ",
        "what about ",
        "do we ",
        "does this ",
        "did we ",
        "is there ",
        "are these ",
        "are they ",
        "would this ",
        "would it ",
        "there they say ",
        "they say ",
        "let's ",
        "lets ",
        "i mean ",
        "i meant ",
        "okay, ",
        "okay. ",
        "ok, ",
        "ok. ",
        "again. ",
        "hm, ",
        "hmm, ",
        "aha, ",
        "interesting, ",
        "now, ",
        "now. ",
        "so ",
        "so, ",
        "but ",
    ] {
        if lowercase.starts_with(prefix) {
            let stripped = value[prefix.len()..].trim();
            return (!stripped.is_empty()).then_some(stripped.to_string());
        }
    }
    None
}

fn strip_request_wrapper(value: &str) -> String {
    let mut best_index: Option<usize> = None;
    let lowercase = value.to_ascii_lowercase();
    for marker in REQUEST_MARKERS {
        let marker_lower = marker.to_ascii_lowercase();
        if let Some(index) = lowercase.rfind(&marker_lower) {
            best_index = Some(best_index.map_or(index, |current| current.max(index)));
        }
    }
    best_index
        .map(|index| value[index..].trim())
        .and_then(|suffix| {
            suffix
                .split_once(':')
                .map(|(_, value)| value.trim().to_string())
        })
        .unwrap_or_else(|| value.trim().to_string())
}

pub(crate) fn clean_task_line(raw_line: &str) -> Option<String> {
    let stripped = strip_request_wrapper(raw_line);
    let stripped = stripped.trim();
    if stripped.is_empty() {
        return None;
    }

    let mut line = stripped.trim_start_matches('#').trim().to_string();
    if line.is_empty() {
        return None;
    }

    if let Some(stripped) = strip_meta_wrapper_prefix(&line) {
        line = stripped;
    }
    line = line.trim_start_matches('#').trim().to_string();

    if let Some(extracted) = extract_structured_task_signal(&line) {
        line = extracted;
    }
    if let Some(stripped) = strip_plain_role_prefix(&line) {
        line = stripped;
    }
    line = strip_trailing_subagent_marker(&line);
    line = line.trim_start_matches('>').trim().to_string();
    line = line.trim_start_matches('-').trim().to_string();

    while let Some(rest) = strip_bracketed_counter(&line) {
        line = rest.trim().to_string();
    }

    let lowercase = line.to_ascii_lowercase();
    if lowercase.starts_with("transcript delta start")
        || lowercase.starts_with("transcript delta end")
    {
        let mut remainder = line
            .split_once(':')
            .map(|(_, value)| value.trim().to_string())
            .unwrap_or_default();
        remainder = remainder.trim_start_matches('>').trim().to_string();
        if remainder.is_empty() {
            return None;
        }
        line = remainder;
    }

    let compact = line.split_whitespace().collect::<Vec<_>>().join(" ");
    (!compact.is_empty()).then_some(compact)
}

pub(crate) fn task_scaffolding_line(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    normalized.is_empty()
        || normalized_matches_prefixes(&normalized, LOW_SIGNAL_PREFIXES)
        || normalized_contains_fragments(&normalized, LOW_SIGNAL_CONTAINS)
        || looks_like_prompt_scaffolding_line(value)
        || looks_like_structured_key_value_title(value)
        || looks_like_path_stub_title(value)
        || looks_like_command_or_output_title(value)
        || looks_like_context_markup(value)
        || looks_like_file_reference(value)
}

fn extract_structured_task_signal(value: &str) -> Option<String> {
    let compact = value.replace("```", " ");
    let lowercase = compact.to_ascii_lowercase();
    let likely_wrapped = lowercase.contains("transcript delta")
        || lowercase.contains("::code-comment{")
        || lowercase.contains("tool exec_command result")
        || lowercase.contains("tool write_stdin result")
        || lowercase.contains("found one actionable issue");
    if !likely_wrapped {
        return None;
    }

    if let Some(user_content) = extract_role_segment(&compact, "user:") {
        return Some(user_content);
    }

    if let Some(comment_title) = extract_code_comment_title(&compact) {
        let comment_title = strip_code_comment_severity(&comment_title);
        if lowercase.contains("code review") || lowercase.contains("actionable issue") {
            return Some(format!("Code review: {comment_title}"));
        }
        return Some(comment_title);
    }

    if lowercase.contains("code review") {
        return Some("Code review".to_string());
    }

    None
}

fn extract_role_segment(value: &str, role_marker: &str) -> Option<String> {
    let lowercase = value.to_ascii_lowercase();
    let start = lowercase.find(&role_marker.to_ascii_lowercase())?;
    let after_marker = value[start + role_marker.len()..].trim();
    if after_marker.is_empty() {
        return None;
    }

    let after_lower = after_marker.to_ascii_lowercase();
    let boundaries = [
        " assistant:",
        " developer:",
        " system:",
        " tool ",
        " chunk id:",
        " wall time:",
        " process exited with code",
        " process running with session id",
        " original token count:",
        " output:",
        " found one actionable issue:",
        " ::code-comment{",
    ];
    let end = boundaries
        .iter()
        .filter_map(|boundary| after_lower.find(boundary))
        .min()
        .unwrap_or(after_marker.len());
    let content = after_marker[..end].trim();
    (!content.is_empty()).then_some(content.to_string())
}

fn extract_code_comment_title(value: &str) -> Option<String> {
    let marker = "::code-comment{title=\"";
    let start = value.find(marker)?;
    let rest = &value[start + marker.len()..];
    let end = rest.find('"')?;
    let title = rest[..end].trim();
    (!title.is_empty()).then_some(title.to_string())
}

fn strip_code_comment_severity(value: &str) -> String {
    let trimmed = value.trim();
    if let Some(stripped) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
    {
        return stripped.1.trim().to_string();
    }
    trimmed.to_string()
}

fn strip_bracketed_counter(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if !trimmed.starts_with('[') {
        return None;
    }
    let end = trimmed.find(']')?;
    trimmed[1..end]
        .chars()
        .all(|character| character.is_ascii_digit())
        .then_some(trimmed[end + 1..].trim())
}

fn strip_trailing_subagent_marker(value: &str) -> String {
    let trimmed = value.trim();
    if let Some(prefix) = trimmed
        .strip_suffix(')')
        .and_then(|prefix| prefix.rsplit_once("(@"))
        .and_then(|(head, tail)| tail.trim().ends_with("subagent").then_some(head))
    {
        return prefix.trim_end().to_string();
    }
    trimmed.to_string()
}

fn looks_like_context_markup(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with('<')
        && (trimmed.ends_with('>')
            || trimmed.contains("<cwd>")
            || trimmed.contains("<shell>")
            || trimmed.contains("<current_date>")
            || trimmed.contains("<timezone>"))
}

fn looks_like_file_reference(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with("/Users/")
        || trimmed.starts_with("~/")
        || trimmed.starts_with("/var/")
        || trimmed.contains(": /Users/")
        || trimmed.contains(": /var/")
        || trimmed.contains(".png:")
        || trimmed.contains(".jpg:")
        || trimmed.contains(".jpeg:")
        || trimmed.contains(".md:")
        || trimmed.contains(".json:")
}
