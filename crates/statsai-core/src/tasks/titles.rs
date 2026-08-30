use super::normalize::{
    clean_task_text, normalize_task_title, polish_task_title_candidate, title_topic_tokens,
};

const GENERIC_PLACEHOLDER_EXACT: &[&str] = &[
    "no prompt",
    "single cell",
    "work item",
    "unresolved work item",
];

const PHATIC_TOKENS: &[&str] = &[
    "hi",
    "hello",
    "hey",
    "yes",
    "yeah",
    "yep",
    "ok",
    "okay",
    "thanks",
    "thank",
    "greetings",
    "morning",
    "afternoon",
    "lunch",
    "evening",
    "night",
];

const DIALOGUE_MANAGEMENT_TOKENS: &[&str] = &[
    "ask",
    "browser",
    "casual",
    "check",
    "continue",
    "current",
    "date",
    "details",
    "do",
    "go",
    "greet",
    "greeting",
    "greetings",
    "handle",
    "hello",
    "hi",
    "it",
    "on",
    "open",
    "proceed",
    "reply",
    "respond",
    "say",
    "nothing",
    "else",
    "to",
    "user",
];

const GENERIC_WORKFLOW_TOKENS: &[&str] = &[
    "conversation",
    "guideline",
    "guidelines",
    "instruction",
    "instructions",
    "list",
    "review",
    "session",
    "thread",
    "uncommitted",
    "changes",
    "change",
    "diff",
    "status",
    "branch",
    "branches",
    "commit",
    "commits",
    "history",
];

const SESSION_CONTROL_ACTION_TOKENS: &[&str] = &[
    "clear",
    "clearing",
    "cleared",
    "exit",
    "exits",
    "exited",
    "quit",
    "quits",
    "quitting",
    "switch",
    "switching",
    "switched",
];

const PROVIDER_PLACEHOLDER_TOKENS: &[&str] = &["codex", "opencode", "claude", "grok"];

const PROVIDER_PLACEHOLDER_NOUNS: &[&str] = &["session", "task"];

pub(crate) const LOW_SIGNAL_PREFIXES: &[&str] = &[
    "your account does not have access",
    "api error:",
    "quota exhausted",
    "model switch due to quota exhaustion",
    "automation:",
    "the user interrupted the previous turn on purpose.",
    "the following is the codex agent history",
    "you are acting as a reviewer for a proposed code change made by another engineer",
    "new session -",
    "<environment_context>",
    "<codex_internal_context",
    "transcript delta start",
    "transcript delta end",
    "skills available",
    "how to use skills",
    "proactiveness strike a balance",
    "# files mentioned by the user",
    "files mentioned by the user",
    "# agents.md instructions for",
    "agents.md instructions for",
    "project-agnostic instructions for",
    "claude opus 4.5 guidelines",
    "last run:",
    "chunk id:",
    "wall time:",
    "process exited with code",
    "process running with session id",
    "original token count:",
    "success. updated the following files:",
    "updated the following files:",
    "output:",
    "usage:",
    "tokens used:",
    "cargo run -p",
    "running `target/debug/",
    "command line invocation:",
    "total output lines:",
    "reviewed codex session id:",
    "continue working toward the active thread goal.",
    "the objective below is user-provided data.",
    "tool web_search call:",
    "tool web_search result:",
    "tool apply_patch call:",
    "tool apply_patch result:",
    "coverage=",
    "f1_overlap=",
    "f1@",
    "avg_tiou=",
    "mae=",
    "titlef1=",
    "cider=",
    "score=",
    "fatal:",
    "single cell",
    "with repeats",
];

pub(crate) const LOW_SIGNAL_CONTAINS: &[&str] = &[
    "approval assessment",
    "@explore subagent",
    "@image subagent",
    "@build subagent",
    "review changes [commit|branch|pr]",
    "my request for codex",
    "my request for claude",
    "my request for opencode",
    "attachments/",
    "plugin://",
    "<cwd>",
    "<current_date>",
    "<timezone>",
    "skill.md",
    "a skill is a set of local instructions",
    "::code-comment{title=",
    "tool exec_command result",
    "tool write_stdin result",
    "tool exec_command",
    "tool write_stdin",
    "tool web_search call",
    "tool web_search result",
    "tool apply_patch call",
    "tool apply_patch result",
    "%%bash",
    "transcript delta start",
    "the list above is the skills available in this session",
    "skill bodies live on disk at the listed paths",
    "project-agnostic instructions for claude opus",
    "any running unified exec processes may still be running in the background",
    "automation id:",
    "$codex_home/automations/",
];

pub(crate) const META_WRAPPER_TOKENS: &[&str] = &[
    "implement",
    "implementation",
    "plan",
    "summary",
    "request",
    "objective",
    "goal",
    "task",
    "please",
];

pub(crate) const WRAPPER_FILLER_TOKENS: &[&str] =
    &["following", "below", "above", "current", "actual"];

const ABSTRACT_TASK_OBJECT_TOKENS: &[&str] = &[
    "goal",
    "goals",
    "issue",
    "issues",
    "item",
    "items",
    "objective",
    "objectives",
    "problem",
    "problems",
    "request",
    "requests",
    "result",
    "results",
    "task",
    "tasks",
    "thing",
    "things",
    "work",
];

const ABSTRACT_TASK_MODIFIER_TOKENS: &[&str] = &[
    "again",
    "all",
    "better",
    "best",
    "correct",
    "correctly",
    "existing",
    "fully",
    "more",
    "needed",
    "necessary",
    "proper",
    "properly",
    "real",
    "satisfy",
];

const DEICTIC_FOLLOWUP_TOKENS: &[&str] = &[
    "all",
    "anything",
    "everything",
    "it",
    "same",
    "something",
    "that",
    "them",
    "these",
    "this",
    "those",
];

const SHELL_ACTION_TOKENS: &[&str] = &[
    "build", "check", "compile", "deploy", "dev", "fmt", "format", "install", "lint", "preview",
    "run", "serve", "start", "test", "tests",
];

const COMMAND_TOKENS: &[&str] = &[
    "bash",
    "cargo",
    "cmake",
    "docker",
    "eslint",
    "git",
    "kubectl",
    "make",
    "node",
    "npm",
    "pip",
    "pnpm",
    "python",
    "python3",
    "sh",
    "swift",
    "tsc",
    "wrangler",
    "xcodebuild",
    "yarn",
    "zsh",
];

const INSTRUCTIONAL_LEAD_TOKENS: &[&str] = &[
    "if", "that", "the", "these", "this", "those", "unless", "when", "your",
];

const INSTRUCTIONAL_MODAL_TOKENS: &[&str] = &[
    "choose", "follow", "must", "need", "needs", "read", "required", "requires", "should", "use",
];

const INSTRUCTIONAL_CONTEXT_TOKENS: &[&str] = &[
    "api",
    "apis",
    "audit",
    "completion",
    "conventions",
    "guide",
    "instruction",
    "instructions",
    "objective",
    "policy",
    "prompt",
    "skill",
    "skills",
    "training",
    "version",
    "workflow",
];

pub(crate) const TITLE_TOPIC_STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "any",
    "at",
    "are",
    "as",
    "be",
    "build",
    "but",
    "can",
    "change",
    "changes",
    "check",
    "could",
    "debug",
    "did",
    "do",
    "does",
    "for",
    "from",
    "get",
    "had",
    "has",
    "have",
    "how",
    "i",
    "in",
    "into",
    "investigate",
    "is",
    "it",
    "its",
    "just",
    "lets",
    "let",
    "look",
    "maybe",
    "me",
    "my",
    "need",
    "now",
    "of",
    "okay",
    "ok",
    "or",
    "our",
    "please",
    "really",
    "results",
    "of",
    "on",
    "show",
    "so",
    "tell",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "to",
    "too",
    "review",
    "run",
    "same",
    "show",
    "still",
    "run",
    "task",
    "tell",
    "than",
    "there",
    "try",
    "update",
    "us",
    "use",
    "want",
    "was",
    "we",
    "what",
    "when",
    "where",
    "which",
    "why",
    "with",
    "would",
    "yes",
    "you",
    "your",
];

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

fn looks_like_short_dialogue_management_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.is_empty() || tokens.len() > 4 {
        return false;
    }
    let mut saw_signal = false;
    for token in tokens {
        if TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()) {
            continue;
        }
        if PHATIC_TOKENS.contains(&token.as_str())
            || DIALOGUE_MANAGEMENT_TOKENS.contains(&token.as_str())
        {
            saw_signal = true;
            continue;
        }
        return false;
    }
    saw_signal
}

fn looks_like_short_meta_instruction(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.is_empty() || tokens.len() > 4 {
        return false;
    }
    let mut saw_signal = false;
    for token in tokens {
        if TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()) {
            continue;
        }
        if DIALOGUE_MANAGEMENT_TOKENS.contains(&token.as_str()) {
            saw_signal = true;
            continue;
        }
        return false;
    }
    saw_signal
}

fn looks_like_provider_placeholder_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    matches!(
        tokens.as_slice(),
        [provider, noun]
            if PROVIDER_PLACEHOLDER_TOKENS.contains(&provider.as_str())
                && PROVIDER_PLACEHOLDER_NOUNS.contains(&noun.as_str())
    )
}

fn looks_like_meta_wrapper_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.is_empty() || tokens.len() > 5 {
        return false;
    }
    let mut saw_signal = false;
    for token in tokens {
        if TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()) {
            continue;
        }
        if META_WRAPPER_TOKENS.contains(&token.as_str())
            || WRAPPER_FILLER_TOKENS.contains(&token.as_str())
        {
            saw_signal = true;
            continue;
        }
        return false;
    }
    saw_signal
}

fn looks_like_review_guidance_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.len() < 2 || tokens.len() > 4 {
        return false;
    }
    let mut has_review = false;
    let mut has_guidance = false;
    for token in tokens {
        match token.as_str() {
            "code" => {}
            "review" => has_review = true,
            "guideline" | "guidelines" => has_guidance = true,
            token if TITLE_TOPIC_STOP_WORDS.contains(&token) => {}
            _ => return false,
        }
    }
    has_review && has_guidance
}

fn looks_like_presentational_wrapper_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    looks_like_presentational_wrapper_clause(&normalized)
        || normalized
            .split(" and ")
            .collect::<Vec<_>>()
            .as_slice()
            .iter()
            .copied()
            .all(looks_like_presentational_wrapper_clause)
}

fn looks_like_presentational_wrapper_clause(normalized: &str) -> bool {
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    let tail = match tokens.as_slice() {
        ["here", "is", tail @ ..]
        | ["here", "are", tail @ ..]
        | ["there", "is", tail @ ..]
        | ["there", "are", tail @ ..]
        | ["this", "is", tail @ ..]
        | ["these", "are", tail @ ..] => tail,
        _ => return false,
    };
    if tail.is_empty() || tail.len() > 6 {
        return false;
    }
    let topic_token_count = title_topic_tokens(&tail.join(" ")).len();
    let artifact_token_count = tail
        .iter()
        .filter(|token| {
            GENERIC_WORKFLOW_TOKENS.contains(token)
                || META_WRAPPER_TOKENS.contains(token)
                || matches!(
                    **token,
                    "code"
                        | "diff"
                        | "output"
                        | "report"
                        | "reports"
                        | "result"
                        | "results"
                        | "test"
                        | "tests"
                        | "case"
                        | "cases"
                )
        })
        .count();
    topic_token_count <= 1 || artifact_token_count >= tail.len().saturating_sub(1)
}

fn looks_like_abstract_followup_title(value: &str) -> bool {
    let content_tokens = normalized_content_tokens(value);
    if content_tokens.len() < 2 || content_tokens.len() > 6 {
        return false;
    }
    if !content_tokens
        .first()
        .is_some_and(|token| is_task_verb_token(token))
    {
        return false;
    }
    let concrete_count = content_tokens
        .iter()
        .filter(|token| is_concrete_task_topic_token(token))
        .count();
    let abstract_followup_count = content_tokens
        .iter()
        .skip(1)
        .filter(|token| {
            DEICTIC_FOLLOWUP_TOKENS.contains(&token.as_str())
                || ABSTRACT_TASK_OBJECT_TOKENS.contains(&token.as_str())
                || ABSTRACT_TASK_MODIFIER_TOKENS.contains(&token.as_str())
                || WRAPPER_FILLER_TOKENS.contains(&token.as_str())
        })
        .count();
    concrete_count == 0 && abstract_followup_count >= content_tokens.len().saturating_sub(1)
}

fn looks_like_abstract_objective_title(value: &str) -> bool {
    let content_tokens = normalized_content_tokens(value);
    if content_tokens.len() < 4 || content_tokens.len() > 14 {
        return false;
    }
    let verb_count = content_tokens
        .iter()
        .filter(|token| is_task_verb_token(token))
        .count();
    let abstract_count = content_tokens
        .iter()
        .filter(|token| {
            ABSTRACT_TASK_OBJECT_TOKENS.contains(&token.as_str())
                || ABSTRACT_TASK_MODIFIER_TOKENS.contains(&token.as_str())
                || WRAPPER_FILLER_TOKENS.contains(&token.as_str())
        })
        .count();
    let concrete_count = content_tokens
        .iter()
        .filter(|token| is_concrete_task_topic_token(token))
        .count();
    verb_count >= 2 && abstract_count >= 2 && concrete_count == 0
}

fn looks_like_generic_workflow_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.len() < 2 || tokens.len() > 4 {
        return false;
    }
    let mut saw_signal = false;
    for token in tokens {
        if TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()) {
            continue;
        }
        if GENERIC_WORKFLOW_TOKENS.contains(&token.as_str()) {
            saw_signal = true;
            continue;
        }
        return false;
    }
    saw_signal
}

fn looks_like_meta_conversation_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.is_empty() || tokens.len() > 8 {
        return false;
    }
    let has_continue = tokens
        .iter()
        .any(|token| matches!(token.as_str(), "continue" | "resume"));
    let has_conversation_boundary = tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "conversation" | "session" | "thread" | "review"
        )
    });
    has_continue && has_conversation_boundary
}

fn looks_like_session_control_meta_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    if tokens.is_empty() || tokens.len() > 6 {
        return false;
    }
    let content_tokens = tokens
        .iter()
        .filter(|token| !TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()))
        .collect::<Vec<_>>();
    let has_action = content_tokens
        .iter()
        .any(|token| SESSION_CONTROL_ACTION_TOKENS.contains(&token.as_str()));
    let has_boundary_object = content_tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "conversation" | "history" | "session" | "cli"
        )
    });
    if has_action && has_boundary_object {
        return true;
    }
    content_tokens.iter().any(|token| token.as_str() == "model")
        && content_tokens
            .iter()
            .any(|token| matches!(token.as_str(), "switch" | "switching" | "switched"))
        && content_tokens.iter().all(|token| {
            matches!(
                token.as_str(),
                "model"
                    | "switch"
                    | "switching"
                    | "switched"
                    | "quick"
                    | "exit"
                    | "exits"
                    | "exited"
                    | "quit"
                    | "quits"
                    | "quitting"
            )
        })
}

pub(crate) fn looks_like_command_or_output_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    if normalized.is_empty() {
        return false;
    }
    if looks_like_test_harness_output_title(value)
        || looks_like_bracketed_log_prefix_title(value)
        || looks_like_package_version_banner_title(value)
        || looks_like_progress_measurement_title(value)
        || looks_like_settings_banner_title(value)
        || looks_like_git_ref_review_title(value)
        || looks_like_shell_invocation_title(value)
        || looks_like_build_status_title(value)
        || looks_like_warning_banner_title(value)
        || looks_like_structured_key_value_title(value)
    {
        return true;
    }
    if normalized_matches_prefixes(
        &normalized,
        &[
            "command line invocation",
            "fatal",
            "total output lines",
            "process exited with code",
            "process running with session id",
            "reviewed codex session id",
            "running target debug",
            "blocking waiting for file lock",
        ],
    ) {
        return true;
    }

    let command_token_count = normalized
        .split_whitespace()
        .filter(|token| COMMAND_TOKENS.contains(token))
        .count();
    let flag_count = value
        .split_whitespace()
        .filter(|token| token.starts_with("--"))
        .count();
    let pathlike_token_count = value
        .split_whitespace()
        .filter(|token| token.contains('/') || token.contains('\\'))
        .count();
    let filelike_token_count = value
        .split_whitespace()
        .filter(|token| token.contains('.') && token.len() > 4)
        .count();
    let banner_char_count = value
        .chars()
        .filter(|character| matches!(character, '─' | '│' | '┌' | '┐' | '└' | '┘' | '⛅'))
        .count();

    banner_char_count >= 3
        || normalized.contains("update available")
        || (command_token_count >= 2 && (flag_count >= 1 || pathlike_token_count >= 1))
        || (pathlike_token_count >= 2 && command_token_count >= 1)
        || (filelike_token_count >= 2 && command_token_count >= 1)
        || flag_count >= 3
}

fn looks_like_bracketed_log_prefix_title(value: &str) -> bool {
    let trimmed = value.trim();
    let Some(rest) = trimmed.strip_prefix('[') else {
        return false;
    };
    let Some((label, remainder)) = rest.split_once(']') else {
        return false;
    };
    let normalized_label = basic_normalize_phrase(label);
    let label_tokens = normalized_label.split_whitespace().collect::<Vec<_>>();
    if label_tokens.is_empty() || label_tokens.len() > 2 {
        return false;
    }
    let is_log_label = label_tokens.iter().all(|token| {
        matches!(
            *token,
            "debug" | "info" | "warn" | "warning" | "error" | "trace" | "notice"
        )
    });
    is_log_label && !remainder.trim().is_empty()
}

fn looks_like_progress_measurement_title(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || basic_has_explicit_task_intent(trimmed) {
        return false;
    }
    let lowercase = trimmed.to_ascii_lowercase();
    let normalized = basic_normalize_phrase(trimmed);
    let has_timer = lowercase.contains("[00:")
        || lowercase.contains("[0:")
        || lowercase.contains("runtime:")
        || normalized.starts_with("total runtime")
        || normalized.starts_with("elapsed time");
    let has_rate = [
        "examples/s",
        "example/s",
        "steps/s",
        "step/s",
        "it/s",
        "tok/s",
        "tokens/s",
        "items/s",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker));
    has_timer && (has_rate || normalized.starts_with("total runtime"))
}

fn looks_like_shell_invocation_title(value: &str) -> bool {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() || tokens.len() > 5 {
        return false;
    }
    let first = normalize_shell_token(tokens[0]);
    let first_is_command =
        COMMAND_TOKENS.contains(&first.as_str()) || looks_like_package_script_token(&first);
    if !first_is_command {
        return false;
    }
    tokens.iter().skip(1).all(|token| {
        let normalized = normalize_shell_token(token);
        !normalized.is_empty()
            && (SHELL_ACTION_TOKENS.contains(&normalized.as_str())
                || COMMAND_TOKENS.contains(&normalized.as_str())
                || normalized.starts_with('-')
                || normalized
                    .chars()
                    .all(|character| character.is_ascii_digit()))
    })
}

fn normalize_shell_token(value: &str) -> String {
    value
        .trim_matches(|character: char| {
            matches!(
                character,
                ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']'
            )
        })
        .to_ascii_lowercase()
}

fn looks_like_package_script_token(value: &str) -> bool {
    let Some((name, suffix)) = value.rsplit_once('@') else {
        return false;
    };
    !name.is_empty()
        && suffix.contains('.')
        && suffix.chars().any(|character| character.is_ascii_digit())
}

fn looks_like_build_status_title(value: &str) -> bool {
    let trimmed = value.trim();
    let lowercase = trimmed.to_ascii_lowercase();
    lowercase.starts_with("blocking waiting for file lock")
        || lowercase.starts_with("compiling ")
        || lowercase.starts_with("finished ")
        || lowercase.starts_with("running `target/")
        || lowercase.starts_with("running target/")
}

fn looks_like_warning_banner_title(value: &str) -> bool {
    let trimmed = value.trim_start();
    if trimmed.is_empty() || basic_has_explicit_task_intent(trimmed) {
        return false;
    }
    let normalized = basic_normalize_phrase(trimmed);
    let token_count = normalized.split_whitespace().count();
    token_count <= 18
        && (trimmed.starts_with('⚠')
            || normalized.starts_with("warning")
            || normalized.starts_with("error"))
}

fn looks_like_test_harness_output_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    normalized.starts_with("test case ")
        && (normalized.contains(" failed ")
            || normalized.contains(" passed ")
            || value.contains("-["))
}

fn looks_like_package_version_banner_title(value: &str) -> bool {
    let mut tokens = value.split_whitespace();
    let Some(first_token) = tokens.next() else {
        return false;
    };
    if !first_token.starts_with('@')
        || first_token.matches('@').count() < 2
        || !first_token
            .chars()
            .any(|character| character.is_ascii_digit())
    {
        return false;
    }
    let remaining = tokens
        .map(|token| {
            token
                .trim_matches(|character: char| {
                    matches!(
                        character,
                        ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']'
                    )
                })
                .to_ascii_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    !remaining.is_empty()
        && remaining.len() <= 3
        && remaining.iter().all(|token| {
            COMMAND_TOKENS.contains(&token.as_str())
                || matches!(
                    token.as_str(),
                    "build" | "deploy" | "dev" | "run" | "start" | "test"
                )
        })
}

fn looks_like_settings_banner_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    tokens.len() <= 8
        && normalized.contains("command line")
        && tokens.iter().any(|token| {
            matches!(
                *token,
                "setting" | "settings" | "configuration" | "configurations"
            )
        })
}

fn looks_like_git_ref_review_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    if tokens.len() < 4 || tokens.len() > 10 {
        return false;
    }
    let workflow_token_count = tokens
        .iter()
        .filter(|token| {
            GENERIC_WORKFLOW_TOKENS.contains(token)
                || matches!(**token, "against" | "between" | "compare")
        })
        .count();
    let git_ref_token_count = tokens
        .iter()
        .filter(|token| looks_like_git_ref_token(token))
        .count();
    workflow_token_count >= 2
        && git_ref_token_count >= 2
        && !tokens.iter().any(|token| {
            matches!(
                token,
                &"fix" | &"implement" | &"track" | &"debug" | &"investigate"
            )
        })
}

fn looks_like_git_ref_token(token: &str) -> bool {
    matches!(
        token,
        "head" | "main" | "master" | "origin" | "upstream" | "develop" | "development" | "dev"
    ) || (token.contains('/')
        && token.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '/')
        }))
}

fn looks_like_instructional_preamble_title(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    if tokens.len() < 6 {
        return false;
    }

    let starts_with_instructional_lead = tokens
        .first()
        .is_some_and(|token| INSTRUCTIONAL_LEAD_TOKENS.contains(token));
    let has_modal = tokens
        .iter()
        .any(|token| INSTRUCTIONAL_MODAL_TOKENS.contains(token));
    let has_context = tokens
        .iter()
        .any(|token| INSTRUCTIONAL_CONTEXT_TOKENS.contains(token));

    (starts_with_instructional_lead && (has_modal || has_context))
        || normalized_contains_fragments(
            &normalized,
            &[
                "your training data",
                "read the relevant guide",
                "follow the instructions",
                "breaking changes",
            ],
        )
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

pub(crate) fn looks_like_path_stub_title(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || basic_has_explicit_task_intent(trimmed) {
        return false;
    }
    let tokens = trimmed.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() || tokens.len() > 3 {
        return false;
    }
    tokens.iter().all(|token| looks_like_pathish_token(token))
}

fn looks_like_pathish_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|character: char| {
        matches!(
            character,
            ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']'
        )
    });
    if trimmed.len() < 4 {
        return false;
    }
    let has_separator = trimmed.contains('/') || trimmed.contains('\\');
    let has_extension = trimmed.rsplit_once('.').is_some_and(|(_, suffix)| {
        !suffix.is_empty()
            && suffix.len() <= 12
            && suffix
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
    });
    (has_separator || trimmed.ends_with('/') || has_extension)
        && trimmed.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '/' | '\\' | '.' | '_' | '-' | '~')
        })
}

pub(crate) fn looks_like_metric_result_stub(original: &str, polished: &str) -> bool {
    let polished_lowercase = polished.to_ascii_lowercase();
    if polished_lowercase.is_empty() {
        return true;
    }
    if [
        "coverage=",
        "f1_overlap=",
        "f1@",
        "avg_tiou=",
        "mae=",
        "titlef1=",
        "cider=",
        "score=",
    ]
    .iter()
    .any(|prefix| polished_lowercase.starts_with(prefix))
    {
        return true;
    }

    let original_lowercase = original.to_ascii_lowercase();
    if !contains_metric_report_marker(&original_lowercase)
        || has_explicit_task_intent(&polished_lowercase)
    {
        return false;
    }

    let normalized = normalize_task_title(polished);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    tokens.len() <= 8
        && tokens.iter().any(|token| {
            token.contains("ckpt")
                || token.contains("checkpoint")
                || token.contains("adapter")
                || token.contains("4bit")
                || token.contains("8bit")
                || token.contains("16bit")
                || token.contains("bf16")
                || token.contains("fp16")
                || token.contains("mlx")
                || token.contains("lora")
        })
}

fn contains_metric_report_marker(value: &str) -> bool {
    [
        "coverage=",
        "f1_overlap=",
        "f1@",
        "avg_tiou",
        "mae=",
        "titlef1=",
        "cider=",
        "score=",
        "ueo(",
    ]
    .iter()
    .any(|marker| value.contains(marker))
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

pub(crate) fn looks_like_sensitive_locator_dump(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    lowercase.contains("$codex_home/automations/")
        || lowercase.contains("token=eyj")
        || ((lowercase.contains("http://") || lowercase.contains("https://"))
            && ["token=", "apikey=", "api_key=", "auth=", "signature="]
                .iter()
                .any(|marker| lowercase.contains(marker)))
}

fn looks_like_locator_stub(value: &str) -> bool {
    matches!(value.trim(), "colab" | "kaggle" | "automation")
}

fn normalized_content_tokens(value: &str) -> Vec<String> {
    normalized_title_tokens(value)
        .into_iter()
        .filter(|token| !TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()))
        .collect()
}

fn is_task_verb_token(token: &str) -> bool {
    explicit_task_verb_token(token)
}

fn explicit_task_verb_token(token: &str) -> bool {
    matches!(
        token,
        "add"
            | "analyze"
            | "audit"
            | "benchmark"
            | "build"
            | "choose"
            | "compare"
            | "create"
            | "debug"
            | "deploy"
            | "evaluate"
            | "explain"
            | "export"
            | "fix"
            | "implement"
            | "improve"
            | "investigate"
            | "locate"
            | "merge"
            | "remove"
            | "refactor"
            | "rename"
            | "replace"
            | "rescore"
            | "review"
            | "rebuild"
            | "split"
            | "summarize"
            | "test"
            | "track"
            | "train"
            | "verify"
    )
}

fn basic_has_explicit_task_intent(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    normalized
        .split_whitespace()
        .next()
        .is_some_and(explicit_task_verb_token)
}

fn is_concrete_task_topic_token(token: &str) -> bool {
    !TITLE_TOPIC_STOP_WORDS.contains(&token)
        && !META_WRAPPER_TOKENS.contains(&token)
        && !WRAPPER_FILLER_TOKENS.contains(&token)
        && !ABSTRACT_TASK_OBJECT_TOKENS.contains(&token)
        && !ABSTRACT_TASK_MODIFIER_TOKENS.contains(&token)
        && !DEICTIC_FOLLOWUP_TOKENS.contains(&token)
        && !is_task_verb_token(token)
}
