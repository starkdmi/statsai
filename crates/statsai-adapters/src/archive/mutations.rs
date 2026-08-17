//! Reconstructs the file edits an agent applied from its recorded tool calls.
//!
//! The collectors in the parent module read a provider's transcript and hand
//! this module the tool call and the result it produced; this module decides
//! whether that pair describes a measurable edit and, if so, turns it into
//! [`TraceEdit`]s. Keeping that decision here is what lets the collectors stay
//! ignorant of `apply_patch`, `multiedit`, shell command classification, and
//! each provider's way of reporting failure.
//!
//! Every judgement is conservative in the same direction: a mutation that
//! cannot be measured degrades trace coverage rather than being counted as
//! zero lines, because an unmeasured edit is not the same as no edit.

use super::ArchiveScanDiagnostics;
use chrono::{DateTime, Utc};
use serde_json::Value;
use statsai_core::{
    parse_full_file_write, parse_structured_edit, parse_unified_patch, repository_relative_path,
    CoverageStatus, ParsedMutation, ProjectInfo, SourceLocation, TraceEdit, TraceEditContext,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(super) struct PendingMutation {
    tool_name: String,
    arguments: Value,
    source_record_id: String,
    occurred_at: Option<DateTime<Utc>>,
}

/// Records that some mutation was observed but cannot be measured.
///
/// Failed, rejected, truncated, and unsupported mutations must never inflate
/// applied-edit totals, but they do mean the trace no longer fully describes
/// the edits of that period.
pub(super) fn record_unmeasurable_mutation(counter: &mut u64, trace_coverage: &mut CoverageStatus) {
    *counter = counter.saturating_add(1);
    *trace_coverage = trace_coverage.combine(CoverageStatus::Partial);
}

pub(super) struct MutationInvocation<'a> {
    pub(super) call_key: String,
    pub(super) tool_name: Option<&'a str>,
    pub(super) arguments: &'a Value,
    pub(super) source_record_id: &'a str,
    pub(super) occurred_at: Option<DateTime<Utc>>,
}

pub(super) fn remember_original_mutation(
    pending: &mut HashMap<String, PendingMutation>,
    diagnostics: &mut ArchiveScanDiagnostics,
    trace_coverage: &mut CoverageStatus,
    invocation: MutationInvocation<'_>,
) {
    let Some(tool_name) = invocation.tool_name else {
        return;
    };
    if is_supported_mutation_tool(tool_name) {
        diagnostics.mutation_calls = diagnostics.mutation_calls.saturating_add(1);
        pending.insert(
            invocation.call_key,
            PendingMutation {
                tool_name: tool_name.to_string(),
                arguments: invocation.arguments.clone(),
                source_record_id: invocation.source_record_id.to_string(),
                occurred_at: invocation.occurred_at,
            },
        );
    } else if is_potentially_mutating_shell(tool_name)
        && !shell_invocation_is_read_only(invocation.arguments)
    {
        record_unmeasurable_mutation(&mut diagnostics.unsupported_mutations, trace_coverage);
    }
}

pub(super) struct MutationCompletion<'a> {
    pub(super) call_key: &'a str,
    pub(super) cache_key: &'a str,
    pub(super) result: &'a Value,
    pub(super) status: Option<&'a str>,
    pub(super) provider: &'a str,
    pub(super) source: &'a SourceLocation,
    pub(super) conversation_id: &'a str,
    pub(super) project: Option<&'a ProjectInfo>,
}

pub(super) fn finish_original_mutation(
    pending: &mut HashMap<String, PendingMutation>,
    diagnostics: &mut ArchiveScanDiagnostics,
    trace_edits: &mut Vec<TraceEdit>,
    trace_coverage: &mut CoverageStatus,
    completion: MutationCompletion<'_>,
) {
    let Some(call) = pending.remove(completion.call_key) else {
        return;
    };
    if mutation_result_failed(completion.result, completion.status, &call.tool_name) {
        record_unmeasurable_mutation(&mut diagnostics.failed_mutations, trace_coverage);
        return;
    }
    let repository_path = completion
        .project
        .and_then(|project| project.path_label.as_deref())
        .map(Path::new);
    let context = TraceEditContext {
        provider: completion.provider,
        source_id: &completion.source.source_id,
        cache_key: completion.cache_key,
        conversation_id: completion.conversation_id,
        source_record_id: &call.source_record_id,
        occurred_at: call.occurred_at,
        project: completion.project,
        repository_path,
    };
    // Whether a whole-file write created the file is reported in the outcome,
    // not in the call, so the result text is read before parsing.
    let decoded_result = decoded_result_payload(completion.result);
    let outcome = result_text(completion.result, decoded_result.as_ref());
    match parse_original_mutation(
        &context,
        &call.tool_name,
        &call.arguments,
        outcome.as_deref(),
    ) {
        Some(parsed) => {
            *trace_coverage = trace_coverage.combine(parsed.coverage);
            if !parsed.edits.is_empty() {
                diagnostics.applied_mutations = diagnostics.applied_mutations.saturating_add(1);
            }
            diagnostics.unsupported_mutations = diagnostics
                .unsupported_mutations
                .saturating_add(parsed.unsupported_sections);
            trace_edits.extend(parsed.edits);
        }
        None => {
            record_unmeasurable_mutation(&mut diagnostics.unsupported_mutations, trace_coverage);
        }
    }
}

fn parse_original_mutation(
    context: &TraceEditContext<'_>,
    tool_name: &str,
    arguments: &Value,
    result_text: Option<&str>,
) -> Option<ParsedMutation> {
    let normalized_name = normalized_tool_name(tool_name);
    let decoded = decoded_tool_arguments(arguments);
    let arguments = decoded.as_ref().unwrap_or(arguments);
    if normalized_name == "apply_patch" || normalized_name == "patch" {
        let patch = arguments
            .as_str()
            .or_else(|| string_argument(arguments, &["patch", "input", "diff"]))?;
        return Some(parse_unified_patch(context, patch));
    }
    if normalized_name == "multiedit" {
        let path = path_argument(arguments)?;
        let relative_path = repository_relative_path(context.repository_path, path);
        let replacements = arguments.get("edits")?.as_array()?;
        if replacements.is_empty() {
            return Some(ParsedMutation {
                coverage: CoverageStatus::Unavailable,
                edits: Vec::new(),
                unsupported_sections: 1,
            });
        }
        let mut coverage = None;
        let mut edits = Vec::new();
        let mut unsupported_sections = 0_u64;
        for (section_index, replacement) in replacements.iter().enumerate() {
            let Some(parsed) =
                parse_structured_replacement(context, &relative_path, replacement, section_index)
            else {
                coverage = Some(
                    coverage.map_or(CoverageStatus::Partial, |current: CoverageStatus| {
                        current.combine(CoverageStatus::Partial)
                    }),
                );
                unsupported_sections = unsupported_sections.saturating_add(1);
                continue;
            };
            coverage = Some(coverage.map_or(parsed.coverage, |current: CoverageStatus| {
                current.combine(parsed.coverage)
            }));
            unsupported_sections = unsupported_sections.saturating_add(parsed.unsupported_sections);
            edits.extend(parsed.edits);
        }
        return Some(ParsedMutation {
            coverage: coverage.unwrap_or(CoverageStatus::Unavailable),
            edits,
            unsupported_sections,
        });
    }
    if matches!(
        normalized_name.as_str(),
        "edit" | "str_replace" | "replace" | "str_replace_editor"
    ) {
        let path = path_argument(arguments)?;
        return parse_structured_replacement(
            context,
            &repository_relative_path(context.repository_path, path),
            arguments,
            0,
        );
    }
    if matches!(
        normalized_name.as_str(),
        "write" | "write_file" | "create_file"
    ) {
        let path = path_argument(arguments)?;
        let content = string_argument(arguments, &["content", "text", "file_text"])?;
        let creation_known = normalized_name == "create_file"
            || arguments
                .get("create")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || arguments
                .get("is_new_file")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || result_reports_file_creation(result_text);
        return Some(parse_full_file_write(
            context,
            &repository_relative_path(context.repository_path, path),
            content,
            creation_known,
        ));
    }
    None
}

/// Whether a whole-file write's outcome says it created the file.
///
/// A creation makes every written line an addition; an overwrite mixes new and
/// replaced lines with no way to tell them apart, so those lines stay
/// unclassified and unmatchable. Tools that never declare this in their
/// arguments — Claude Code's `Write` among them — do say it in the result, so
/// reading the outcome is what keeps a created file out of the unclassified
/// bucket.
fn result_reports_file_creation(result_text: Option<&str>) -> bool {
    result_text.is_some_and(|text| {
        let opening = text.trim_start().to_ascii_lowercase();
        opening.starts_with("file created")
            || opening.starts_with("created file")
            || opening.starts_with("successfully created")
    })
}

fn parse_structured_replacement(
    context: &TraceEditContext<'_>,
    path: &Path,
    arguments: &Value,
    section_index: usize,
) -> Option<ParsedMutation> {
    let old_string = string_argument(arguments, &["old_string", "old_str", "old_text", "oldText"])?;
    let new_string = string_argument(arguments, &["new_string", "new_str", "new_text", "newText"])?;
    let mut parsed = parse_structured_edit(context, path, old_string, new_string, section_index);
    if arguments
        .get("replace_all")
        .or_else(|| arguments.get("replaceAll"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        parsed.coverage = CoverageStatus::Partial;
        parsed.unsupported_sections = parsed.unsupported_sections.saturating_add(1);
    }
    Some(parsed)
}

fn decoded_tool_arguments(arguments: &Value) -> Option<Value> {
    arguments
        .as_str()
        .and_then(|value| serde_json::from_str(value).ok())
}

fn normalized_tool_name(tool_name: &str) -> String {
    tool_name
        .rsplit(['.', ':'])
        .next()
        .unwrap_or(tool_name)
        .to_ascii_lowercase()
}

fn is_supported_mutation_tool(tool_name: &str) -> bool {
    matches!(
        normalized_tool_name(tool_name).as_str(),
        "apply_patch"
            | "patch"
            | "edit"
            | "multiedit"
            | "str_replace"
            | "replace"
            | "str_replace_editor"
            | "write"
            | "write_file"
            | "create_file"
    )
}

fn is_potentially_mutating_shell(tool_name: &str) -> bool {
    matches!(
        normalized_tool_name(tool_name).as_str(),
        "exec"
            | "exec_command"
            | "shell"
            | "shell_command"
            | "local_shell"
            | "write_stdin"
            | "bash"
            | "terminal"
            | "run_command"
    )
}

/// Programs that read the working tree without being able to rewrite it.
///
/// Membership is deliberately narrow: claiming a command is read-only when it
/// is not would overstate trace coverage, while leaving one out only repeats
/// the conservative "some edit went unmeasured" answer. Interpreters are
/// excluded on principle. `sed`, `awk`, and `perl` all write files from inside
/// their own program text — `sed -n 'w out.rs'`, `awk '{ system("...") }'` —
/// so no flag inspection can prove one of them read-only, and `xxd` takes an
/// output file as a bare positional argument.
const READ_ONLY_SHELL_PROGRAMS: &[&str] = &[
    "basename", "cat", "cd", "cksum", "cmp", "column", "comm", "cut", "date", "diff", "dirname",
    "du", "echo", "false", "fd", "file", "grep", "head", "hostname", "id", "jq", "ls", "md5",
    "md5sum", "nl", "od", "printenv", "printf", "ps", "pwd", "readlink", "realpath", "rg",
    "shasum", "sleep", "stat", "tail", "tr", "tree", "true", "type", "uname", "uniq", "wc",
    "which", "whoami",
];

/// `find` actions that write, delete, or run an arbitrary program.
const WRITING_FIND_ACTIONS: &[&str] = &[
    "-delete", "-exec", "-execdir", "-fls", "-fprint", "-fprint0", "-fprintf", "-ok", "-okdir",
];

/// `git` subcommands that only read the object database and working tree.
const READ_ONLY_GIT_SUBCOMMANDS: &[&str] = &[
    "blame",
    "cat-file",
    "describe",
    "diff",
    "diff-tree",
    "grep",
    "log",
    "ls-files",
    "ls-tree",
    "name-rev",
    "reflog",
    "rev-list",
    "rev-parse",
    "shortlog",
    "show",
    "status",
    "whatchanged",
];

/// `cargo` subcommands that write only to the build directory.
///
/// Build artifacts live under `target/`, which code classification already
/// excludes, so they cannot change a measured line count.
const READ_ONLY_CARGO_SUBCOMMANDS: &[&str] = &[
    "bench", "build", "check", "clippy", "metadata", "test", "tree",
];

/// Whether a shell tool call is confidently incapable of editing source files.
///
/// Read-only inspection is by far the most common use of a shell tool. Marking
/// every one of them as an unmeasurable mutation would leave trace coverage
/// permanently partial for any session that ran `ls` once, which says nothing
/// about how completely the agent's edits were reconstructed.
fn shell_invocation_is_read_only(arguments: &Value) -> bool {
    let decoded = decoded_tool_arguments(arguments);
    let arguments = decoded.as_ref().unwrap_or(arguments);
    shell_command_script(arguments).is_some_and(|script| is_read_only_shell_script(&script))
}

/// Flattens the argument shapes providers record for a shell tool into a script.
fn shell_command_script(arguments: &Value) -> Option<String> {
    if let Some(script) = arguments.as_str() {
        return Some(script.to_string());
    }
    let command = ["command", "cmd", "script", "shell_command", "input"]
        .into_iter()
        .find_map(|key| arguments.get(key))?;
    if let Some(script) = command.as_str() {
        return Some(script.to_string());
    }
    let argv = command
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()?;
    // `["bash", "-lc", "<script>"]` is the shape Codex records; anything else
    // is already a tokenized command whose program is its first element.
    let (program, rest) = argv.split_first()?;
    if matches!(
        shell_program_name(program).as_str(),
        "bash" | "sh" | "zsh" | "dash"
    ) {
        if let Some(index) = rest
            .iter()
            .position(|argument| matches!(*argument, "-c" | "-lc" | "-ic" | "-lic"))
        {
            return rest.get(index + 1).map(|script| (*script).to_string());
        }
    }
    Some(argv.join(" "))
}

fn is_read_only_shell_script(script: &str) -> bool {
    // Stderr redirection carries no filename; every other redirection, command
    // substitution, or privilege escalation can reach a file this module never
    // sees, so the command is treated as potentially mutating.
    let script = script.replace("2>&1", "").replace("1>&2", "");
    if script.trim().is_empty()
        || script.contains('>')
        || script.contains('<')
        || script.contains("$(")
        || script.contains('`')
    {
        return false;
    }
    let mut segments = script
        .split(['\n', ';', '|', '&'])
        .map(|segment| segment.trim().trim_matches(['(', ')']).trim())
        .filter(|segment| !segment.is_empty())
        .peekable();
    if segments.peek().is_none() {
        return false;
    }
    segments.all(is_read_only_shell_segment)
}

fn is_read_only_shell_segment(segment: &str) -> bool {
    let mut tokens = segment
        .split_whitespace()
        // A leading `NAME=value` sets the environment for the command itself.
        .skip_while(|token| !token.starts_with('-') && token.contains('='))
        .collect::<Vec<_>>();
    // `env` only prepares the environment for the program that follows it, so
    // that program is what decides whether anything can be written.
    let mut stripped_env_prefix = false;
    while tokens
        .first()
        .is_some_and(|token| shell_program_name(token) == "env")
    {
        stripped_env_prefix = true;
        tokens = tokens
            .split_off(1)
            .into_iter()
            .skip_while(|token| !token.starts_with('-') && token.contains('='))
            .collect();
    }
    let Some((program, arguments)) = tokens.split_first() else {
        // A bare `env` names no program and only prints the environment.
        return stripped_env_prefix;
    };
    let program = shell_program_name(program);
    let subcommand = arguments
        .iter()
        .find(|token| !token.starts_with('-'))
        .map(|token| token.to_ascii_lowercase());
    let has_flag = |flags: &[&str]| {
        arguments
            .iter()
            .any(|token| flags.contains(&token.to_ascii_lowercase().as_str()))
    };
    match program.as_str() {
        "git" => subcommand
            .is_some_and(|subcommand| READ_ONLY_GIT_SUBCOMMANDS.contains(&subcommand.as_str())),
        "cargo" => subcommand
            .is_some_and(|subcommand| READ_ONLY_CARGO_SUBCOMMANDS.contains(&subcommand.as_str())),
        // `find` walks read-only until given an action that writes or executes.
        "find" => !arguments
            .iter()
            .any(|token| WRITING_FIND_ACTIONS.contains(&token.to_ascii_lowercase().as_str())),
        // `fd` likewise only reaches a file through an exec action.
        "fd" => !has_flag(&["-x", "--exec", "--exec-batch"]),
        // `sort` writes only through an explicit output file.
        "sort" => {
            !has_flag(&["-o", "--output"])
                && !arguments.iter().any(|token| token.starts_with("--output="))
        }
        program => READ_ONLY_SHELL_PROGRAMS.contains(&program),
    }
}

/// Reduces an invoked program to the executable name it dispatches on.
fn shell_program_name(program: &str) -> String {
    program
        .trim_matches('"')
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase()
}

fn mutation_result_failed(result: &Value, status: Option<&str>, tool_name: &str) -> bool {
    if status.is_some_and(|status| {
        matches!(
            status.to_ascii_lowercase().as_str(),
            "failed" | "error" | "rejected" | "cancelled" | "canceled"
        )
    }) {
        return true;
    }
    if result
        .get("is_error")
        .or_else(|| result.get("isError"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    if result
        .get("success")
        .and_then(Value::as_bool)
        .is_some_and(|success| !success)
    {
        return true;
    }
    // Codex wraps tool output in a JSON-encoded object whose exit code is
    // authoritative; text heuristics only apply when no exit code is recorded.
    let decoded = decoded_result_payload(result);
    if let Some(exit_code) = decoded
        .as_ref()
        .and_then(|payload| payload.pointer("/metadata/exit_code"))
        .and_then(Value::as_i64)
    {
        return exit_code != 0;
    }
    is_supported_mutation_tool(tool_name)
        && result_text(result, decoded.as_ref())
            .is_some_and(|text| mutation_result_text_reports_failure(&text))
}

/// Generic markers that only mean failure when a result opens with them.
const MUTATION_FAILURE_PREFIXES: &[&str] =
    &["<tool_use_error>", "error:", "failed", "invalid context"];

/// Phrases specific enough to mean failure anywhere in a result's first line.
const MUTATION_FAILURE_PHRASES: &[&str] = &[
    "failed to apply",
    "invalid patch",
    "patch rejected",
    "permission denied",
    "verification failed",
];

/// Whether a tool result's text reports that the mutation did not apply.
///
/// Only the opening line is inspected, because providers put their outcome
/// there. A successful structured edit then echoes the edited region back, so
/// the file's own contents routinely contain phrases like `permission denied`;
/// searching the whole body would discard the very edits to error handling and
/// permission checks that the result is describing.
fn mutation_result_text_reports_failure(text: &str) -> bool {
    let Some(first_line) = text.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return false;
    };
    let first_line = first_line.to_ascii_lowercase();
    MUTATION_FAILURE_PREFIXES
        .iter()
        .any(|marker| first_line.starts_with(marker))
        || MUTATION_FAILURE_PHRASES
            .iter()
            .any(|phrase| first_line.contains(phrase))
}

/// Decodes the JSON-encoded object Codex stores as a tool output string.
fn decoded_result_payload(result: &Value) -> Option<Value> {
    let raw = result
        .get("output")
        .or_else(|| result.get("content"))?
        .as_str()?;
    serde_json::from_str::<Value>(raw)
        .ok()
        .filter(Value::is_object)
}

/// Extracts human-readable result text from the shapes providers record:
/// a bare string, a keyed string, a JSON-encoded payload, or text blocks.
fn result_text(result: &Value, decoded: Option<&Value>) -> Option<String> {
    if let Some(text) = decoded
        .and_then(|payload| payload.get("output"))
        .and_then(Value::as_str)
    {
        return Some(text.to_string());
    }
    if let Some(text) = result.as_str() {
        return Some(text.to_string());
    }
    for key in ["output", "content", "error", "message"] {
        let Some(value) = result.get(key) else {
            continue;
        };
        if let Some(text) = value.as_str() {
            return Some(text.to_string());
        }
        if let Some(blocks) = value.as_array() {
            let joined = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            if !joined.is_empty() {
                return Some(joined);
            }
        }
    }
    None
}

fn string_argument<'a>(arguments: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
}

fn path_argument(arguments: &Value) -> Option<PathBuf> {
    string_argument(arguments, &["file_path", "path", "file", "filename"]).map(PathBuf::from)
}

pub(super) fn mark_unresolved_mutations(
    pending: &HashMap<String, PendingMutation>,
    diagnostics: &mut ArchiveScanDiagnostics,
    trace_coverage: &mut CoverageStatus,
) {
    if pending.is_empty() {
        return;
    }
    diagnostics.truncated_mutations = diagnostics
        .truncated_mutations
        .saturating_add(pending.len() as u64);
    *trace_coverage = trace_coverage.combine(CoverageStatus::Partial);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_apply_patch_failures_are_detected_in_every_recorded_output_shape() {
        // Plain-text output, as written for `custom_tool_call_output`.
        assert!(mutation_result_failed(
            &serde_json::json!({
                "output": "apply_patch verification failed: Failed to read file to update \
                           /workspace/src/generate.py: No such file or directory (os error 2)"
            }),
            None,
            "apply_patch"
        ));
        // JSON-encoded output carrying an exit code.
        assert!(mutation_result_failed(
            &serde_json::json!({
                "output": "{\"output\":\"apply_patch: cannot apply\",\
                            \"metadata\":{\"exit_code\":1,\"duration_seconds\":0.1}}"
            }),
            None,
            "apply_patch"
        ));
        // Claude records failures on the tool_result block itself.
        assert!(mutation_result_failed(
            &serde_json::json!({"is_error": true, "content": [{"type": "text", "text": "no match"}]}),
            None,
            "edit"
        ));
    }

    #[test]
    fn codex_apply_patch_successes_are_not_treated_as_failures() {
        // A successful patch lists the files it touched; those paths must not
        // trip the failure heuristics.
        assert!(!mutation_result_failed(
            &serde_json::json!({
                "output": "{\"output\":\"Success. Updated the following files:\\n\
                            A tests/failed_login_case.rs\\n\",\
                            \"metadata\":{\"exit_code\":0,\"duration_seconds\":0.1}}"
            }),
            None,
            "apply_patch"
        ));
        assert!(!mutation_result_failed(
            &serde_json::json!({
                "output": "Success. Updated the following files:\nM src/lib.rs\n"
            }),
            None,
            "apply_patch"
        ));
    }

    #[test]
    fn read_only_programs_with_writing_flags_stay_unmeasurable() {
        // `find`, `sort`, `fd`, and `env` read by default but each has a
        // writing form, and `env` merely prepares the environment for the real
        // program.
        for command in [
            "find . -name '*.rs' -delete",
            "find . -type f -exec touch {} +",
            "sort -o sorted.txt input.txt",
            "sort --output=sorted.txt input.txt",
            "env cargo fmt",
            "env FOO=bar cargo fmt",
            "fd -e rs -x touch",
        ] {
            assert!(
                !shell_invocation_is_read_only(&serde_json::json!({ "cmd": command })),
                "{command} must stay unmeasurable"
            );
        }
        // Their read-only forms still keep coverage intact.
        for command in [
            "find . -name '*.rs'",
            "sort input.txt",
            "env FOO=bar cargo test",
            "env",
        ] {
            assert!(
                shell_invocation_is_read_only(&serde_json::json!({ "cmd": command })),
                "{command} reads only"
            );
        }
    }

    #[test]
    fn interpreters_are_never_classified_read_only() {
        // These reach the filesystem from inside their own program text, with
        // no flag a classifier could key on: `sed` has the `w` command, `awk`
        // has output redirection and `system()`, `perl` is a general-purpose
        // language, and `xxd` takes its output file as a bare positional
        // argument. Even their innocent-looking invocations stay unmeasurable,
        // because proving one safe would mean interpreting the program.
        for command in [
            "sed -n 'w src/copied.rs' README.md",
            "sed -n '1,5p' src/lib.rs",
            "sed -i '' 's/a/b/' src/lib.rs",
            "perl -pe 's/a/b/' src/lib.rs",
            "perl -I lib -e 'print 1'",
            "awk '{ system(\"touch src/lib.rs\") }' input",
            "awk '{print $1}' input",
            "xxd -r dump.txt src/lib.rs",
            "python3 -c 'print(1)'",
            "ruby -e 'puts 1'",
            "node -e 'console.log(1)'",
        ] {
            assert!(
                !shell_invocation_is_read_only(&serde_json::json!({ "cmd": command })),
                "{command} cannot be proven read-only"
            );
        }
    }

    #[test]
    fn shell_commands_that_can_reach_a_file_stay_unmeasurable() {
        for arguments in [
            serde_json::json!({"cmd": "cat template.rs > src/lib.rs"}),
            serde_json::json!({"cmd": "ls && cargo fmt"}),
            serde_json::json!({"cmd": "sed -i '' 's/a/b/' src/lib.rs"}),
            serde_json::json!({"cmd": "eval $(cat script.sh)"}),
            serde_json::json!({"cmd": "git checkout -- src/lib.rs"}),
            serde_json::json!({"command": ["bash", "-lc", "npm run build"]}),
            serde_json::json!({"description": "no command recorded"}),
        ] {
            assert!(
                !shell_invocation_is_read_only(&arguments),
                "{arguments} must stay unmeasurable"
            );
        }
    }

    #[test]
    fn successful_edits_echoing_error_text_are_not_discarded() {
        // A structured edit echoes the edited region back. Source lines that
        // happen to mention a failure must not retire the edit that wrote them.
        let echoed = "The file /workspace/src/auth.rs has been updated.\n\
             Here's the result of running `cat -n` on a snippet:\n\
                12\tbail!(\"permission denied\");\n\
                13\treturn Err(Error::VerificationFailed);\n";
        assert!(!mutation_result_failed(
            &serde_json::json!({"content": echoed}),
            None,
            "edit"
        ));
        // The same phrase in the opening line is still a failure report.
        assert!(mutation_result_failed(
            &serde_json::json!({"content": "permission denied: /workspace/src/auth.rs"}),
            None,
            "edit"
        ));
        assert!(mutation_result_failed(
            &serde_json::json!({"content": "<tool_use_error>String to replace not found</tool_use_error>"}),
            None,
            "edit"
        ));
    }
}
