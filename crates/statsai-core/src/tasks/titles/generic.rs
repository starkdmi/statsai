use super::super::normalize::title_topic_tokens;
use super::{basic_normalize_phrase, normalized_title_tokens};

pub(crate) const GENERIC_PLACEHOLDER_EXACT: &[&str] = &[
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

pub(crate) const GENERIC_WORKFLOW_TOKENS: &[&str] = &[
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

pub(crate) fn looks_like_short_dialogue_management_title(value: &str) -> bool {
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

pub(crate) fn looks_like_short_meta_instruction(value: &str) -> bool {
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

pub(crate) fn looks_like_provider_placeholder_title(value: &str) -> bool {
    let tokens = normalized_title_tokens(value);
    matches!(
        tokens.as_slice(),
        [provider, noun]
            if PROVIDER_PLACEHOLDER_TOKENS.contains(&provider.as_str())
                && PROVIDER_PLACEHOLDER_NOUNS.contains(&noun.as_str())
    )
}

pub(crate) fn looks_like_meta_wrapper_title(value: &str) -> bool {
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

pub(crate) fn looks_like_review_guidance_title(value: &str) -> bool {
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

pub(crate) fn looks_like_presentational_wrapper_title(value: &str) -> bool {
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

pub(crate) fn looks_like_abstract_followup_title(value: &str) -> bool {
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

pub(crate) fn looks_like_abstract_objective_title(value: &str) -> bool {
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

pub(crate) fn looks_like_generic_workflow_title(value: &str) -> bool {
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

pub(crate) fn looks_like_meta_conversation_title(value: &str) -> bool {
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

pub(crate) fn looks_like_session_control_meta_title(value: &str) -> bool {
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

fn normalized_content_tokens(value: &str) -> Vec<String> {
    normalized_title_tokens(value)
        .into_iter()
        .filter(|token| !TITLE_TOPIC_STOP_WORDS.contains(&token.as_str()))
        .collect()
}

fn is_task_verb_token(token: &str) -> bool {
    explicit_task_verb_token(token)
}

pub(crate) fn explicit_task_verb_token(token: &str) -> bool {
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

fn is_concrete_task_topic_token(token: &str) -> bool {
    !TITLE_TOPIC_STOP_WORDS.contains(&token)
        && !META_WRAPPER_TOKENS.contains(&token)
        && !WRAPPER_FILLER_TOKENS.contains(&token)
        && !ABSTRACT_TASK_OBJECT_TOKENS.contains(&token)
        && !ABSTRACT_TASK_MODIFIER_TOKENS.contains(&token)
        && !DEICTIC_FOLLOWUP_TOKENS.contains(&token)
        && !is_task_verb_token(token)
}
