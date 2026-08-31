use super::super::titles::{
    basic_normalize_phrase, has_explicit_task_intent, looks_like_metric_result_stub,
    looks_like_sensitive_locator_dump, normalized_contains_fragments, normalized_matches_prefixes,
    normalized_title_tokens, task_title_is_generic, task_title_is_session_meta,
    task_title_is_weak_signal, task_title_signal_score,
};
use super::{
    clean_task_line, clean_task_text, normalize_task_title, polish_task_title_candidate,
    strip_conversational_prefix, strip_meta_wrapper_prefix, strip_plain_role_prefix,
    task_scaffolding_line, title_topic_tokens, truncate_task_text,
};
use std::borrow::Cow;
use std::collections::BTreeSet;

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

pub(crate) fn expand_inline_markdown_headings(value: &str) -> String {
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

pub(crate) fn looks_like_prompt_scaffolding_line(value: &str) -> bool {
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
