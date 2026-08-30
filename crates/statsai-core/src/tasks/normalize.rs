use super::titles::{
    basic_normalize_phrase, has_explicit_task_intent, looks_like_command_or_output_title,
    looks_like_metric_result_stub, looks_like_path_stub_title, looks_like_sensitive_locator_dump,
    looks_like_structured_key_value_title, normalized_contains_fragments,
    normalized_matches_prefixes, normalized_title_tokens, task_title_is_generic,
    task_title_is_session_meta, task_title_is_weak_signal, task_title_signal_score,
    LOW_SIGNAL_CONTAINS, LOW_SIGNAL_PREFIXES, META_WRAPPER_TOKENS, TITLE_TOPIC_STOP_WORDS,
    WRAPPER_FILLER_TOKENS,
};
use std::borrow::Cow;
use std::collections::BTreeSet;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

const PROMPT_SCAFFOLD_PREFIXES: &[&str] = &[
    "continue working toward the active thread goal",
    "the objective below is user-provided data",
    "continuation behavior:",
    "work from evidence:",
    "completion audit:",
    "blocked audit:",
    "budget:",
];

const PROMPT_SCAFFOLD_CONTAINS: &[&str] = &[
    "your training data",
    "before writing code",
    "read the relevant guide",
    "follow the instructions",
    "running unified exec processes",
    "tools/commands were aborted",
    "partially executed",
    "active thread goal",
    "objective below",
    "treat it as the task to pursue",
    "continuation behavior",
    "completion audit",
    "blocked audit",
    "work from evidence",
];

const REQUEST_MARKERS: &[&str] = &[
    "My request for Codex:",
    "My request for Claude Code:",
    "My request for Claude:",
    "My request for OpenCode:",
];

const BRANCH_PREFIXES: &[&str] = &[
    "feature", "feat", "fix", "bugfix", "hotfix", "chore", "task", "story", "ticket",
];

#[must_use]
pub fn normalize_task_title(value: &str) -> String {
    let canonical = value.nfc().collect::<String>();
    let cleaned = clean_task_text(&canonical).unwrap_or_else(|| canonical.trim().to_string());
    let cleaned = polish_task_title_candidate(&cleaned);
    let mut normalized = String::with_capacity(cleaned.len());
    let mut previous_was_space = false;
    let mut can_append_mark = false;
    for character in cleaned.chars() {
        if is_combining_mark(character) {
            if can_append_mark {
                normalized.push(character);
            }
            continue;
        }
        if character.is_alphanumeric() {
            normalized.extend(character.to_lowercase());
            previous_was_space = false;
            can_append_mark = true;
            continue;
        }
        can_append_mark = false;
        if character.is_whitespace() || matches!(character, '-' | '_' | '/' | ':' | '.') {
            if !previous_was_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            previous_was_space = true;
        }
    }
    normalized.trim().to_string()
}

#[must_use]
pub fn summarize_task_text(value: Option<&str>, width: usize) -> Option<String> {
    truncate_task_text(clean_task_text(value?)?, width)
}

#[must_use]
pub fn task_title_from_prompt(value: Option<&str>) -> Option<String> {
    task_preview_from_prompt(value, 90)
}

pub(crate) const TASK_PREVIEW_MAX_INPUT_BYTES: usize = 24 * 1024;

const TASK_PREVIEW_MAX_INPUT_LINES: usize = 128;

const TASK_PREVIEW_FAST_SCAN_BYTES: usize = 16 * 1024;

const TASK_PREVIEW_FAST_SCAN_LINES: usize = 128;

#[must_use]
pub fn task_preview_from_prompt(value: Option<&str>, width: usize) -> Option<String> {
    let raw = value?;
    if let Some(candidate) = fast_structured_task_preview_candidate(raw) {
        return truncate_task_text(candidate, width);
    }
    let bounded = bounded_task_preview_input(raw);
    select_task_prompt_candidate(bounded.as_ref())
        .and_then(|candidate| truncate_task_text(candidate, width))
}

pub(crate) fn bounded_task_preview_input(raw: &str) -> Cow<'_, str> {
    if raw.len() <= TASK_PREVIEW_MAX_INPUT_BYTES {
        return Cow::Borrowed(raw);
    }

    let mut excerpt = String::new();
    let mut used_bytes = 0usize;
    let mut used_lines = 0usize;

    for line in raw.lines() {
        if used_lines >= TASK_PREVIEW_MAX_INPUT_LINES || used_bytes >= TASK_PREVIEW_MAX_INPUT_BYTES
        {
            break;
        }

        let line_bytes = line.len();
        let remaining_bytes = TASK_PREVIEW_MAX_INPUT_BYTES.saturating_sub(used_bytes);
        let fits_with_newline = line_bytes.saturating_add(1) <= remaining_bytes;
        if !fits_with_newline {
            if remaining_bytes == 0 {
                break;
            }
            excerpt.push_str(prefix_at_char_boundary(line, remaining_bytes));
            break;
        }

        excerpt.push_str(line);
        excerpt.push('\n');
        used_bytes = used_bytes.saturating_add(line_bytes).saturating_add(1);
        used_lines = used_lines.saturating_add(1);
    }

    if excerpt.is_empty() {
        return Cow::Borrowed(prefix_at_char_boundary(raw, TASK_PREVIEW_MAX_INPUT_BYTES));
    }

    Cow::Owned(excerpt)
}

pub(crate) fn prefix_at_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn fast_structured_task_preview_candidate(raw: &str) -> Option<String> {
    let window = prefix_at_char_boundary(raw, TASK_PREVIEW_FAST_SCAN_BYTES);
    for line in window.lines().take(TASK_PREVIEW_FAST_SCAN_LINES) {
        let sentence_breaks = [". ", "? ", "! "]
            .into_iter()
            .map(|marker| line.matches(marker).count())
            .sum::<usize>();
        if sentence_breaks > 1 && !line.trim_start().starts_with('#') {
            continue;
        }
        let Some(candidate) = clean_task_line(line) else {
            continue;
        };
        let mut polished = polish_task_title_candidate(&candidate);
        if let Some(stripped) = strip_plain_role_prefix(&polished) {
            polished = polish_task_title_candidate(&stripped);
        }
        let normalized = basic_normalize_phrase(&polished);
        let token_count = normalized.split_whitespace().count();
        if polished.is_empty()
            || polished.len() > 160
            || token_count > 18
            || task_title_is_generic(Some(polished.as_str()))
            || task_title_is_weak_signal(Some(polished.as_str()))
            || task_scaffolding_line(&polished)
            || looks_like_metric_result_stub(line, &polished)
            || (looks_like_statemental_heading(&polished) && !has_explicit_task_intent(&polished))
            || normalized.starts_with("transcript start")
            || normalized.starts_with("transcript end")
        {
            continue;
        }
        return Some(polished);
    }

    None
}

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

fn truncate_task_text(compact: String, width: usize) -> Option<String> {
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

fn strip_plain_role_prefix(value: &str) -> Option<String> {
    let lowercase = value.to_ascii_lowercase();
    for prefix in ["assistant:", "user:", "developer:", "system:", "d user:"] {
        if lowercase.starts_with(prefix) {
            let stripped = value[prefix.len()..].trim();
            return (!stripped.is_empty()).then_some(stripped.to_string());
        }
    }
    None
}

fn strip_meta_wrapper_prefix(value: &str) -> Option<String> {
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

fn strip_conversational_prefix(value: &str) -> Option<String> {
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

fn select_task_prompt_candidate(raw: &str) -> Option<String> {
    if looks_like_sensitive_locator_dump(raw) {
        return None;
    }

    if let Some(candidate) = leading_markdown_heading_candidate(raw) {
        return Some(candidate);
    }

    let mut best = None::<(i32, String)>;
    for candidate in prompt_candidate_fragments(raw) {
        let polished = polish_task_title_candidate(&candidate);
        if polished.is_empty() || looks_like_metric_result_stub(&candidate, &polished) {
            continue;
        }
        let score = prompt_candidate_score(&polished);
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, polished));
        }
    }
    if let Some((score, candidate)) = best {
        if score > 0 {
            return Some(candidate);
        }
    }

    let compact = clean_task_text(raw)?;
    let polished = polish_task_title_candidate(&compact);
    if polished.is_empty()
        || looks_like_metric_result_stub(&compact, &polished)
        || task_scaffolding_line(&polished)
        || looks_like_prompt_scaffolding_line(&polished)
    {
        return None;
    }
    Some(polished)
}

fn leading_markdown_heading_candidate(raw: &str) -> Option<String> {
    let expanded = expand_inline_markdown_headings(raw);
    let supporting_topic_sets = prompt_supporting_topic_sets(raw);
    let mut best = None::<(i32, String)>;
    for line in expanded.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            continue;
        }
        let mut heading = trimmed.trim_start_matches('#').trim().to_string();
        if heading.is_empty() {
            continue;
        }
        if let Some(stripped) = strip_meta_wrapper_prefix(&heading) {
            heading = stripped;
        }
        let polished = polish_task_title_candidate(&heading);
        if polished.is_empty()
            || looks_like_document_section_heading(&polished)
            || starts_with_document_section_label(&polished)
            || looks_like_prompt_scaffolding_line(&polished)
        {
            continue;
        }
        let score = prompt_candidate_score(&polished)
            + markdown_heading_support_score(&polished, &supporting_topic_sets);
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, polished));
        }
    }
    best.filter(|(score, _)| *score > 0).map(|(_, title)| title)
}

fn prompt_candidate_fragments(raw: &str) -> Vec<String> {
    let mut seen = BTreeSet::<String>::new();
    let mut candidates = Vec::<String>::new();
    let fragments = split_prompt_fragments(raw);

    for fragment in std::iter::once(raw)
        .chain(raw.lines())
        .chain(fragments.iter().map(String::as_str))
    {
        if let Some(stripped) = strip_conversational_prefix(fragment) {
            push_prompt_candidate(stripped.as_str(), &mut candidates, &mut seen);
        }
        push_prompt_candidate(fragment, &mut candidates, &mut seen);
    }

    candidates
}

fn push_prompt_candidate(raw: &str, candidates: &mut Vec<String>, seen: &mut BTreeSet<String>) {
    let Some(candidate) = clean_task_text(raw) else {
        return;
    };
    if candidate.is_empty() || !seen.insert(candidate.clone()) {
        return;
    }
    candidates.push(candidate);
}

fn split_prompt_fragments(value: &str) -> Vec<String> {
    expand_inline_markdown_headings(value)
        .replace("\r\n", "\n")
        .replace(['\r', '|'], "\n")
        .replace("? ", "?\n")
        .replace("! ", "!\n")
        .replace(". ", ".\n")
        .lines()
        .map(str::trim)
        .filter(|fragment| !fragment.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn prompt_supporting_topic_sets(raw: &str) -> Vec<BTreeSet<String>> {
    let mut topic_sets = Vec::<BTreeSet<String>>::new();
    let mut seen = BTreeSet::<String>::new();
    for fragment in raw
        .lines()
        .chain(split_prompt_fragments(raw).iter().map(String::as_str))
    {
        let trimmed = fragment.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(candidate) = clean_task_text(fragment) else {
            continue;
        };
        if candidate.is_empty() || !seen.insert(candidate.clone()) {
            continue;
        }
        let topic_tokens = title_topic_tokens(&candidate);
        if !topic_tokens.is_empty() {
            topic_sets.push(topic_tokens);
        }
    }
    topic_sets
}

fn markdown_heading_support_score(
    heading: &str,
    supporting_topic_sets: &[BTreeSet<String>],
) -> i32 {
    let heading_tokens = title_topic_tokens(heading);
    if heading_tokens.is_empty() {
        return -6;
    }
    let overlap_score = supporting_topic_sets
        .iter()
        .map(|topic_set| heading_tokens.intersection(topic_set).count())
        .sum::<usize>();
    if overlap_score >= 4 {
        6
    } else if overlap_score >= 2 {
        3
    } else if overlap_score == 1 && has_explicit_task_intent(heading) {
        0
    } else if has_explicit_task_intent(heading) {
        -2
    } else if heading_tokens.len() <= 4 && !looks_like_statemental_heading(heading) {
        0
    } else if overlap_score == 1 {
        -8
    } else {
        -20
    }
}

fn looks_like_statemental_heading(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.len() < 4 || tokens.len() > 10 {
        return false;
    }
    let has_subject = tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "this" | "that" | "these" | "those" | "it" | "you" | "your"
        )
    });
    let has_copula = tokens
        .iter()
        .any(|token| matches!(token.as_str(), "is" | "are" | "was" | "were"));
    has_subject && has_copula
}

fn expand_inline_markdown_headings(value: &str) -> String {
    let mut expanded = String::with_capacity(value.len() + 16);
    let characters = value.chars().collect::<Vec<_>>();
    for (index, character) in characters.iter().enumerate() {
        if *character == '#' {
            let previous = index.checked_sub(1).and_then(|idx| characters.get(idx));
            let next = characters.get(index + 1);
            let starts_heading = previous
                .is_some_and(|previous| previous.is_whitespace() || matches!(previous, ':' | ';'))
                && next.is_some_and(|next| next.is_whitespace() || *next == '#');
            if starts_heading && !expanded.ends_with('\n') {
                expanded.push('\n');
            }
        }
        expanded.push(*character);
    }
    expanded
}

fn prompt_candidate_score(value: &str) -> i32 {
    if task_scaffolding_line(value) || looks_like_prompt_scaffolding_line(value) {
        return -100;
    }
    if task_title_is_session_meta(Some(value)) {
        return -80;
    }

    let normalized = normalize_task_title(value);
    if normalized.is_empty() {
        return -100;
    }

    let token_count = normalized.split_whitespace().count();
    let mut score = 0;
    if has_explicit_task_intent(value) {
        score += 6;
    }
    if task_title_is_generic(Some(value)) {
        score -= 6;
    } else {
        score += 4;
    }
    if task_title_is_weak_signal(Some(value)) {
        score -= 3;
    } else {
        score += 2;
    }
    if (2..=14).contains(&token_count) {
        score += 1;
    } else if token_count > 20 {
        score -= 2;
    }
    score + task_title_signal_score(Some(value)) / 3
}

fn looks_like_prompt_scaffolding_line(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    normalized_matches_prefixes(&normalized, PROMPT_SCAFFOLD_PREFIXES)
        || normalized_contains_fragments(&normalized, PROMPT_SCAFFOLD_CONTAINS)
}

fn looks_like_document_section_heading(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() || tokens.len() > 4 {
        return false;
    }
    let section_token_count = tokens
        .iter()
        .filter(|token| {
            matches!(
                **token,
                "approach"
                    | "background"
                    | "context"
                    | "current"
                    | "details"
                    | "implementation"
                    | "issue"
                    | "issues"
                    | "overview"
                    | "problem"
                    | "state"
                    | "steps"
                    | "summary"
            )
        })
        .count();
    section_token_count >= 1 && section_token_count == tokens.len()
}

fn starts_with_document_section_label(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    if tokens.len() < 2 {
        return false;
    }
    matches!(
        tokens.first().copied(),
        Some(
            "approach"
                | "background"
                | "context"
                | "current"
                | "details"
                | "implementation"
                | "issue"
                | "issues"
                | "overview"
                | "problem"
                | "state"
                | "steps"
                | "summary"
        )
    )
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

fn clean_task_line(raw_line: &str) -> Option<String> {
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

fn task_scaffolding_line(value: &str) -> bool {
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

#[must_use]
pub fn choose_best_task_title<'a>(
    primary: Option<&'a str>,
    fallback: Option<&'a str>,
    default_title: &'a str,
) -> (String, &'static str) {
    let primary = primary.and_then(|value| summarize_task_text(Some(value), 90));
    if primary
        .as_deref()
        .is_some_and(|value| !task_title_is_generic(Some(value)))
    {
        return (primary.expect("primary title exists"), "primary");
    }
    let fallback = fallback.and_then(|value| task_title_from_prompt(Some(value)));
    if let Some(title) = primary.or(fallback) {
        return (title, "fallback");
    }
    (default_title.to_string(), "default")
}

#[must_use]
pub fn extract_issue_keys(values: &[&str]) -> Vec<String> {
    let mut keys = BTreeSet::new();
    for value in values {
        for raw_token in value.split(|character: char| {
            !(character.is_ascii_alphanumeric() || character == '-' || character == '#')
        }) {
            let token = raw_token.trim_matches(|character: char| {
                !(character.is_ascii_alphanumeric() || character == '-' || character == '#')
            });
            if token.is_empty() {
                continue;
            }
            if token.starts_with('#')
                && token.len() > 1
                && token[1..]
                    .chars()
                    .all(|character| character.is_ascii_digit())
            {
                keys.insert(token.to_string());
                continue;
            }
            let mut parts = token.split('-');
            let Some(left) = parts.next() else {
                continue;
            };
            let Some(right) = parts.next() else {
                continue;
            };
            if left.is_empty()
                || right.is_empty()
                || !left
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_uppercase())
                || !left
                    .chars()
                    .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
                || !right.chars().all(|character| character.is_ascii_digit())
            {
                continue;
            }
            keys.insert(format!("{left}-{right}"));
        }
    }
    keys.into_iter().collect()
}

#[must_use]
pub fn branch_family(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let issue_keys = extract_issue_keys(&[value]);
    if let Some(issue_key) = issue_keys.first() {
        return Some(issue_key.to_ascii_lowercase());
    }

    let tail = value
        .rsplit('/')
        .next()
        .unwrap_or(value)
        .trim_matches(|character: char| character == '-' || character == '_' || character == '.');
    let normalized = normalize_task_title(tail);
    if normalized.is_empty() {
        return None;
    }
    let words = normalized.split_whitespace().collect::<Vec<_>>();
    let stripped = if words
        .first()
        .is_some_and(|word| BRANCH_PREFIXES.contains(word))
    {
        words.into_iter().skip(1).collect::<Vec<_>>().join(" ")
    } else {
        normalized
    };
    (!stripped.is_empty()).then_some(stripped)
}

#[must_use]
pub fn title_topic_tokens(value: &str) -> BTreeSet<String> {
    normalize_task_title(value)
        .split_whitespace()
        .filter(|token| token.len() >= 3 && !TITLE_TOPIC_STOP_WORDS.contains(token))
        .map(ToOwned::to_owned)
        .collect()
}
