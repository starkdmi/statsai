use super::titles::{task_title_is_generic, TITLE_TOPIC_STOP_WORDS};
use std::collections::BTreeSet;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

mod clean;
mod prompt;

pub(crate) use clean::*;
pub use prompt::*;

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
