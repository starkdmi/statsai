use super::super::normalize::normalize_task_title;
use super::generic::{explicit_task_verb_token, GENERIC_WORKFLOW_TOKENS};
use super::{
    basic_normalize_phrase, has_explicit_task_intent, looks_like_structured_key_value_title,
    normalized_contains_fragments, normalized_matches_prefixes,
};

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

pub(crate) fn looks_like_instructional_preamble_title(value: &str) -> bool {
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

pub(crate) fn looks_like_sensitive_locator_dump(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    lowercase.contains("$codex_home/automations/")
        || lowercase.contains("token=eyj")
        || ((lowercase.contains("http://") || lowercase.contains("https://"))
            && ["token=", "apikey=", "api_key=", "auth=", "signature="]
                .iter()
                .any(|marker| lowercase.contains(marker)))
}

pub(crate) fn looks_like_locator_stub(value: &str) -> bool {
    matches!(value.trim(), "colab" | "kaggle" | "automation")
}

fn basic_has_explicit_task_intent(value: &str) -> bool {
    let normalized = basic_normalize_phrase(value);
    normalized
        .split_whitespace()
        .next()
        .is_some_and(explicit_task_verb_token)
}
