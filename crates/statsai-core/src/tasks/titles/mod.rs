use super::normalize::{
    clean_task_text, normalize_task_title, polish_task_title_candidate, title_topic_tokens,
};

mod generic;
mod looks;

pub(crate) use generic::*;
pub(crate) use looks::*;

#[must_use]
pub fn task_title_signal_score(value: Option<&str>) -> i32 {
    let Some(raw) = value else {
        return -100;
    };
    if looks_like_sensitive_locator_dump(raw) {
        return -100;
    }
    let Some(value) = clean_task_text(raw) else {
        return -100;
    };
    let value = polish_task_title_candidate(&value);
    if value.is_empty() {
        return -100;
    }
    if looks_like_metric_result_stub(raw, &value) {
        return -24;
    }

    let normalized = basic_normalize_phrase(&value);
    if normalized.is_empty() {
        return -100;
    }

    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    let content_token_count = tokens
        .iter()
        .filter(|token| !TITLE_TOPIC_STOP_WORDS.contains(token))
        .count();
    let topic_token_count = title_topic_tokens(&value).len();
    let pathlike_token_count = value
        .split_whitespace()
        .filter(|token| token.contains('/') || token.contains('\\'))
        .count();
    let flag_count = value
        .split_whitespace()
        .filter(|token| token.starts_with("--"))
        .count();
    let mut score = 0;

    if task_title_is_generic(Some(value.as_str())) {
        score -= 12;
    } else {
        score += 5;
    }
    if has_explicit_task_intent(&value) {
        score += 6;
    }
    if starts_with_task_verb(&normalized) {
        score += 4;
    }
    if looks_like_command_or_output_title(&value) {
        score -= 12;
    }
    if looks_like_instructional_preamble_title(&value) {
        score -= 10;
    }
    score += match tokens.len() {
        3..=10 => 6,
        2..=14 => 3,
        15..=22 => -2,
        0..=1 => -8,
        _ => -5,
    };
    score += match content_token_count {
        2..=8 => 4,
        9..=14 => 2,
        0..=1 => -5,
        _ => 0,
    };
    score += match topic_token_count {
        2..=6 => 4,
        7..=10 => 2,
        0..=1 => -4,
        _ => 0,
    };
    if pathlike_token_count >= 2 {
        score -= 8;
    } else if pathlike_token_count == 1 && flag_count >= 1 {
        score -= 5;
    }
    if value.ends_with('?') {
        score -= 2;
    }

    score
}

#[must_use]
pub fn task_title_is_generic(value: Option<&str>) -> bool {
    let Some(raw) = value else {
        return true;
    };
    if looks_like_sensitive_locator_dump(raw) {
        return true;
    }
    let Some(value) = clean_task_text(raw) else {
        return true;
    };
    let value = polish_task_title_candidate(&value);
    if value.is_empty() {
        return true;
    }
    if looks_like_metric_result_stub(raw, &value) {
        return true;
    }
    let lowercase = value.to_ascii_lowercase();
    if looks_like_sensitive_locator_dump(&lowercase) {
        return true;
    }
    let normalized = basic_normalize_phrase(&value);
    if GENERIC_PLACEHOLDER_EXACT.contains(&normalized.as_str()) {
        return true;
    }
    if looks_like_short_dialogue_management_title(&value) {
        return true;
    }
    if looks_like_provider_placeholder_title(&value) {
        return true;
    }
    if looks_like_meta_wrapper_title(&value) {
        return true;
    }
    if looks_like_presentational_wrapper_title(&value) {
        return true;
    }
    if looks_like_review_guidance_title(&value) {
        return true;
    }
    if looks_like_generic_workflow_title(&value) {
        return true;
    }
    if looks_like_meta_conversation_title(&value) {
        return true;
    }
    if looks_like_abstract_followup_title(&value) {
        return true;
    }
    if looks_like_abstract_objective_title(&value) {
        return true;
    }
    if looks_like_structured_key_value_title(&value) {
        return true;
    }
    if looks_like_path_stub_title(&value) {
        return true;
    }
    if normalized_matches_prefixes(&normalized, LOW_SIGNAL_PREFIXES) {
        return true;
    }
    if normalized_contains_fragments(&normalized, LOW_SIGNAL_CONTAINS) {
        return true;
    }
    if looks_like_command_or_output_title(&value) {
        return true;
    }
    looks_like_instructional_preamble_title(&value)
}

#[must_use]
pub fn task_title_is_weak_signal(value: Option<&str>) -> bool {
    let Some(value) = value.and_then(clean_task_text) else {
        return true;
    };
    let value = polish_task_title_candidate(&value);
    if value.is_empty() {
        return true;
    }
    if task_title_is_generic(Some(value.as_str())) {
        return true;
    }
    let normalized = normalize_task_title(&value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    (tokens.len() == 1
        && tokens[0].len() <= 12
        && tokens[0]
            .chars()
            .all(|character| character.is_ascii_lowercase()))
        || looks_like_locator_stub(&normalized)
        || looks_like_short_meta_instruction(&value)
        || task_title_signal_score(Some(value.as_str())) < 5
}

#[must_use]
pub fn task_title_is_session_meta(value: Option<&str>) -> bool {
    let Some(value) = value.and_then(clean_task_text) else {
        return false;
    };
    let value = polish_task_title_candidate(&value);
    if value.is_empty() {
        return false;
    }
    looks_like_session_control_meta_title(&value)
}

pub(crate) fn normalized_title_tokens(value: &str) -> Vec<String> {
    normalize_task_title(value)
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect()
}

pub(crate) fn basic_normalize_phrase(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut previous_was_space = false;
    for character in value.chars() {
        if character.is_alphanumeric() {
            for lowercase in character.to_lowercase() {
                normalized.push(lowercase);
            }
            previous_was_space = false;
            continue;
        }
        if !previous_was_space && !normalized.is_empty() {
            normalized.push(' ');
        }
        previous_was_space = true;
    }
    normalized.trim().to_string()
}

pub(crate) fn normalized_matches_prefixes(value: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| {
        let normalized_phrase = basic_normalize_phrase(phrase);
        !normalized_phrase.is_empty() && value.starts_with(&normalized_phrase)
    })
}

pub(crate) fn normalized_contains_fragments(value: &str, fragments: &[&str]) -> bool {
    fragments.iter().any(|fragment| {
        let normalized_fragment = basic_normalize_phrase(fragment);
        !normalized_fragment.is_empty() && value.contains(&normalized_fragment)
    })
}

fn starts_with_task_verb(normalized: &str) -> bool {
    normalized
        .split_whitespace()
        .next()
        .is_some_and(has_explicit_task_intent)
}

pub(crate) fn looks_like_structured_key_value_title(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let first = trimmed.chars().next();
    if !matches!(first, Some('"') | Some('{') | Some('[')) {
        return false;
    }
    let normalized = basic_normalize_phrase(trimmed);
    let token_count = normalized.split_whitespace().count();
    token_count <= 12 && trimmed.matches("\":").count() >= 1 && trimmed.matches('"').count() >= 4
}

pub(crate) fn has_explicit_task_intent(value: &str) -> bool {
    let normalized = normalize_task_title(value);
    if normalized.is_empty() {
        return false;
    }
    if [
        "i want ",
        "i want to ",
        "i need ",
        "i need to ",
        "need to ",
        "please ",
        "lets ",
        "let s ",
    ]
    .iter()
    .any(|prefix| normalized.starts_with(prefix))
    {
        return true;
    }
    normalized.split_whitespace().any(explicit_task_verb_token)
}
