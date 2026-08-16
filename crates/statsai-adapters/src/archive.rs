use super::{
    canonical_display, codex_project_context_from_value, codex_usage_roots, collect_jsonl_files,
    expand_home_path, grok_sessions_root, model_from_nested_value, open_sqlite_readonly,
    read_bounded_jsonl_line, resolve_project_context, source_root_path,
    timestamp_from_nested_value, BoundedLineRead, ProjectContextCache, CLAUDE_CODE_PROVIDER,
    CODEX_PROVIDER, GROK_BUILD_PROVIDER, MAX_JSONL_RECORD_BYTES, OPENCODE_PROVIDER,
};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::Value;
use statsai_core::{
    archive_artifact_metadata_signature, archive_content_id, archive_conversation_id,
    archive_item_id, hash_text, parse_full_file_write, parse_structured_edit, parse_unified_patch,
    ArchiveArtifactDependency, ArchiveCompleteness, ArchiveContentKind, ArchiveContentPart,
    ArchiveConversation, ArchiveItem, ArchiveItemKind, ArchiveRole, CoverageStatus, ModelInfo,
    ParsedMutation, ProjectInfo, SourceLocation, TraceEdit, TraceEditContext, UsageCounts,
    ARCHIVE_CONVERSATION_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use url::Url;

const MAX_TOOL_CALL_TEXT_BYTES: usize = 32 * 1024;
const MAX_TOOL_RESULT_TEXT_BYTES: usize = 64 * 1024;
const TOOL_RESULT_TAIL_BYTES: usize = 16 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ArchiveScanDiagnostics {
    pub files_scanned: u64,
    pub records_scanned: u64,
    pub conversations: u64,
    pub items: u64,
    pub content_parts: u64,
    pub binary_bytes: u64,
    pub missing_content: u64,
    pub invalid_records: u64,
    pub mutation_calls: u64,
    pub applied_mutations: u64,
    pub failed_mutations: u64,
    pub unsupported_mutations: u64,
    pub truncated_mutations: u64,
}

#[derive(Debug, Clone)]
pub struct ArchiveScan {
    pub conversations: Vec<ArchiveConversation>,
    pub artifact_dependencies: Vec<ArchiveArtifactDependency>,
    pub trace_edits: Vec<TraceEdit>,
    pub trace_coverage: CoverageStatus,
    pub diagnostics: ArchiveScanDiagnostics,
}

impl Default for ArchiveScan {
    fn default() -> Self {
        Self {
            conversations: Vec::new(),
            artifact_dependencies: Vec::new(),
            trace_edits: Vec::new(),
            // A scan that reconstructed nothing has measured nothing. Only a
            // provider whose mutations this module actually parses may claim
            // complete trace coverage, so that a new adapter cannot silently
            // over-report by inheriting the default.
            trace_coverage: CoverageStatus::Unavailable,
            diagnostics: ArchiveScanDiagnostics::default(),
        }
    }
}

impl ArchiveScan {
    /// Empty scan for a provider whose tool mutations this module parses.
    ///
    /// Coverage starts complete and degrades as unmeasurable mutations are
    /// observed, so an archive with no edits at all reads as "nothing to
    /// measure" rather than "not measured".
    fn measured() -> Self {
        Self {
            trace_coverage: CoverageStatus::Complete,
            ..Self::default()
        }
    }
}

#[derive(Debug)]
struct PendingMutation {
    tool_name: String,
    arguments: Value,
    source_record_id: String,
    occurred_at: Option<DateTime<Utc>>,
}

type ArtifactDependencyMap = BTreeMap<(String, PathBuf), String>;

#[derive(Debug)]
struct ConversationBuilder {
    provider: String,
    source_id: statsai_core::SourceId,
    native_id: String,
    title: Option<String>,
    project: Option<ProjectInfo>,
    started_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    missing_content: u64,
    missing_content_scope_id: String,
    discarded_source_record_ids: Vec<String>,
    superseded_conversation_ids: Vec<String>,
    items: Vec<ArchiveItem>,
}

impl ConversationBuilder {
    fn new(provider: &str, source: &SourceLocation, native_id: String, scope_path: &Path) -> Self {
        Self {
            provider: provider.to_string(),
            source_id: source.source_id.clone(),
            native_id,
            title: None,
            project: None,
            started_at: None,
            updated_at: None,
            missing_content: 0,
            missing_content_scope_id: hash_text(&canonical_display(scope_path)),
            discarded_source_record_ids: Vec::new(),
            superseded_conversation_ids: Vec::new(),
            items: Vec::new(),
        }
    }

    fn push(&mut self, item: ArchiveItem) {
        if let Some(created_at) = item.created_at {
            self.started_at = Some(
                self.started_at
                    .map_or(created_at, |current| current.min(created_at)),
            );
            self.updated_at = Some(
                self.updated_at
                    .map_or(created_at, |current| current.max(created_at)),
            );
        }
        self.items.push(item);
    }

    fn finish(mut self) -> ArchiveConversation {
        self.items
            .sort_by_key(|item| (item.ordinal, item.item_id.clone()));
        ArchiveConversation {
            schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversation_id: archive_conversation_id(&self.provider, &self.native_id),
            provider: self.provider,
            source_id: self.source_id,
            native_conversation_id: self.native_id,
            title: self.title,
            project: self.project,
            started_at: self.started_at,
            updated_at: self.updated_at,
            completeness: if self.missing_content > 0 {
                ArchiveCompleteness::Partial
            } else if self.items.is_empty() {
                ArchiveCompleteness::MetadataOnly
            } else {
                ArchiveCompleteness::Complete
            },
            missing_content_count: self.missing_content,
            missing_content_scope_id: Some(self.missing_content_scope_id),
            discarded_source_record_ids: self.discarded_source_record_ids,
            superseded_conversation_ids: self.superseded_conversation_ids,
            items: self.items,
        }
    }
}

pub(super) fn collect_provider_archive(
    provider: &str,
    source: &SourceLocation,
    selected_cache_keys: Option<&HashSet<String>>,
) -> Result<ArchiveScan> {
    match provider {
        CODEX_PROVIDER => collect_codex(source, selected_cache_keys),
        CLAUDE_CODE_PROVIDER => collect_claude(source, selected_cache_keys),
        OPENCODE_PROVIDER => collect_opencode(source, selected_cache_keys),
        GROK_BUILD_PROVIDER => collect_grok(source, selected_cache_keys),
        _ => Ok(ArchiveScan::default()),
    }
}

/// Records that some mutation was observed but cannot be measured.
///
/// Failed, rejected, truncated, and unsupported mutations must never inflate
/// applied-edit totals, but they do mean the trace no longer fully describes
/// the edits of that period.
fn record_unmeasurable_mutation(counter: &mut u64, trace_coverage: &mut CoverageStatus) {
    *counter = counter.saturating_add(1);
    *trace_coverage = trace_coverage.combine(CoverageStatus::Partial);
}

struct MutationInvocation<'a> {
    call_key: String,
    tool_name: Option<&'a str>,
    arguments: &'a Value,
    source_record_id: &'a str,
    occurred_at: Option<DateTime<Utc>>,
}

fn remember_original_mutation(
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

struct MutationCompletion<'a> {
    call_key: &'a str,
    cache_key: &'a str,
    result: &'a Value,
    status: Option<&'a str>,
    provider: &'a str,
    source: &'a SourceLocation,
    conversation_id: &'a str,
    project: Option<&'a ProjectInfo>,
}

fn finish_original_mutation(
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
        let relative_path = repository_relative_path(context.repository_path, &path);
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
            &repository_relative_path(context.repository_path, &path),
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
            &repository_relative_path(context.repository_path, &path),
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

fn repository_relative_path(repository_path: Option<&Path>, path: &Path) -> PathBuf {
    repository_path
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .to_path_buf()
}

fn mark_unresolved_mutations(
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

fn collect_codex(
    source: &SourceLocation,
    selected_cache_keys: Option<&HashSet<String>>,
) -> Result<ArchiveScan> {
    let Some(path_label) = source.path_label.as_deref() else {
        return Ok(ArchiveScan::default());
    };
    let source_path = PathBuf::from(path_label);
    let paths = selected_jsonl_paths(selected_cache_keys, || {
        codex_usage_roots(&source_path)
            .into_iter()
            .flat_map(|path| collect_jsonl_files(&path).unwrap_or_default())
            .collect()
    });
    let mut scan = ArchiveScan::measured();
    let mut artifact_dependencies = ArtifactDependencyMap::new();
    for path in paths {
        scan.diagnostics.files_scanned += 1;
        scan.conversations.push(collect_codex_file(
            source,
            &path,
            &mut scan.diagnostics,
            &mut artifact_dependencies,
            &mut scan.trace_edits,
            &mut scan.trace_coverage,
        )?);
    }
    scan.artifact_dependencies = finish_artifact_dependencies(artifact_dependencies);
    finish_diagnostics(&mut scan);
    Ok(scan)
}

fn collect_codex_file(
    source: &SourceLocation,
    path: &Path,
    diagnostics: &mut ArchiveScanDiagnostics,
    artifact_dependencies: &mut ArtifactDependencyMap,
    trace_edits: &mut Vec<TraceEdit>,
    trace_coverage: &mut CoverageStatus,
) -> Result<ArchiveConversation> {
    let fallback_id = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string();
    let (native_id, title, project) = codex_archive_header(path, fallback_id)?;
    let mut builder = ConversationBuilder::new(CODEX_PROVIDER, source, native_id.clone(), path);
    builder.title = title;
    builder.project = project.clone();
    let mut current_model = None::<ModelInfo>;
    let mut structured_user_fingerprints = HashSet::new();
    let mut fallback_user_items = Vec::new();
    let mut pending_mutations = HashMap::new();
    let source_record_path = canonical_display(path);

    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line_bytes = Vec::new();
    let mut line_number = 0usize;
    loop {
        let status = read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
        if status == BoundedLineRead::Eof {
            break;
        }
        line_number = line_number.saturating_add(1);
        if status == BoundedLineRead::Oversized {
            diagnostics.records_scanned += 1;
            diagnostics.invalid_records += 1;
            record_unmeasurable_mutation(&mut diagnostics.truncated_mutations, trace_coverage);
            builder.missing_content += 1;
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            diagnostics.records_scanned += 1;
            diagnostics.invalid_records += 1;
            builder.missing_content += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        diagnostics.records_scanned += 1;
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                diagnostics.invalid_records += 1;
                builder.missing_content += 1;
                continue;
            }
        };
        if value.get("type").and_then(Value::as_str) == Some("turn_context") {
            current_model = model_from_nested_value(&value, None);
            continue;
        }
        let timestamp = timestamp_from_nested_value(&value);
        let record_id = format!("{source_record_path}:{line_number}");
        let top_type = value.get("type").and_then(Value::as_str);
        if top_type == Some("response_item") {
            let payload = value.get("payload").unwrap_or(&Value::Null);
            let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
            match payload_type {
                "message" => {
                    let role = payload
                        .get("role")
                        .and_then(Value::as_str)
                        .map(ArchiveRole::parse);
                    let native_item_id = native_id_from_value(payload)
                        .unwrap_or_else(|| format!("line:{line_number}"));
                    let content = payload.get("content").unwrap_or(&Value::Null);
                    if local_artifacts_allowed(ArchiveItemKind::Message, role) {
                        collect_artifact_dependencies(content, path, artifact_dependencies);
                    }
                    let (item, missing) = item_from_value(ItemInput {
                        provider: CODEX_PROVIDER,
                        conversation_native_id: &native_id,
                        native_item_id: &native_item_id,
                        source_record_id: &record_id,
                        ordinal: line_number as u64,
                        kind: ArchiveItemKind::Message,
                        role,
                        created_at: timestamp,
                        model: current_model.clone(),
                        tool_name: None,
                        tool_call_id: None,
                        status: None,
                        usage: None,
                        content,
                    });
                    builder.missing_content += missing;
                    if role == Some(ArchiveRole::User) {
                        structured_user_fingerprints.insert(item_fingerprint(&item));
                    }
                    builder.push(item);
                }
                "reasoning" => {
                    let summary = payload.get("summary").unwrap_or(&Value::Null);
                    if value_has_readable_content(summary) {
                        let native_item_id = native_id_from_value(payload)
                            .unwrap_or_else(|| format!("reasoning:{line_number}"));
                        let (item, missing) = item_from_value(ItemInput {
                            provider: CODEX_PROVIDER,
                            conversation_native_id: &native_id,
                            native_item_id: &native_item_id,
                            source_record_id: &record_id,
                            ordinal: line_number as u64,
                            kind: ArchiveItemKind::ReasoningSummary,
                            role: Some(ArchiveRole::Assistant),
                            created_at: timestamp,
                            model: current_model.clone(),
                            tool_name: None,
                            tool_call_id: None,
                            status: None,
                            usage: None,
                            content: summary,
                        });
                        builder.missing_content += missing;
                        builder.push(item);
                    }
                }
                "function_call" | "tool_call" | "custom_tool_call" => {
                    let native_item_id = native_id_from_value(payload)
                        .or_else(|| {
                            payload
                                .get("call_id")
                                .and_then(Value::as_str)
                                .map(|call_id| format!("tool-call:{call_id}"))
                        })
                        .unwrap_or_else(|| format!("tool-call:{line_number}"));
                    let content = payload
                        .get("arguments")
                        .or_else(|| payload.get("input"))
                        .unwrap_or(&Value::Null);
                    let call_key = payload
                        .get("call_id")
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or(&native_item_id)
                        .to_string();
                    remember_original_mutation(
                        &mut pending_mutations,
                        diagnostics,
                        trace_coverage,
                        MutationInvocation {
                            call_key,
                            tool_name: payload.get("name").and_then(Value::as_str),
                            arguments: content,
                            source_record_id: &record_id,
                            occurred_at: timestamp,
                        },
                    );
                    let (item, missing) = item_from_value(ItemInput {
                        provider: CODEX_PROVIDER,
                        conversation_native_id: &native_id,
                        native_item_id: &native_item_id,
                        source_record_id: &record_id,
                        ordinal: line_number as u64,
                        kind: ArchiveItemKind::ToolCall,
                        role: Some(ArchiveRole::Assistant),
                        created_at: timestamp,
                        model: current_model.clone(),
                        tool_name: payload.get("name").and_then(Value::as_str),
                        tool_call_id: payload.get("call_id").and_then(Value::as_str),
                        status: payload.get("status").and_then(Value::as_str),
                        usage: None,
                        content,
                    });
                    builder.missing_content += missing;
                    builder.push(item);
                }
                "function_call_output" | "tool_result" | "custom_tool_call_output" => {
                    let native_item_id = native_id_from_value(payload)
                        .or_else(|| {
                            payload
                                .get("call_id")
                                .and_then(Value::as_str)
                                .map(|call_id| format!("tool-result:{call_id}"))
                        })
                        .unwrap_or_else(|| format!("tool-result:{line_number}"));
                    let content = payload
                        .get("output")
                        .or_else(|| payload.get("content"))
                        .unwrap_or(&Value::Null);
                    let call_key = payload
                        .get("call_id")
                        .or_else(|| payload.get("tool_use_id"))
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or(&native_item_id);
                    finish_original_mutation(
                        &mut pending_mutations,
                        diagnostics,
                        trace_edits,
                        trace_coverage,
                        MutationCompletion {
                            call_key,
                            cache_key: &source_record_path,
                            result: payload,
                            status: payload.get("status").and_then(Value::as_str),
                            provider: CODEX_PROVIDER,
                            source,
                            conversation_id: &archive_conversation_id(CODEX_PROVIDER, &native_id),
                            project: project.as_ref(),
                        },
                    );
                    let (item, missing) = item_from_value(ItemInput {
                        provider: CODEX_PROVIDER,
                        conversation_native_id: &native_id,
                        native_item_id: &native_item_id,
                        source_record_id: &record_id,
                        ordinal: line_number as u64,
                        kind: ArchiveItemKind::ToolResult,
                        role: Some(ArchiveRole::Tool),
                        created_at: timestamp,
                        model: current_model.clone(),
                        tool_name: payload.get("name").and_then(Value::as_str),
                        tool_call_id: payload.get("call_id").and_then(Value::as_str),
                        status: payload.get("status").and_then(Value::as_str),
                        usage: None,
                        content,
                    });
                    builder.missing_content += missing;
                    builder.push(item);
                }
                _ => {}
            }
        } else if top_type == Some("event_msg")
            && value.pointer("/payload/type").and_then(Value::as_str) == Some("user_message")
        {
            let payload = value.get("payload").unwrap_or(&Value::Null);
            let content = payload
                .get("message")
                .or_else(|| payload.get("text"))
                .unwrap_or(&Value::Null);
            collect_artifact_dependencies(content, path, artifact_dependencies);
            let native_item_id = format!("user-event:{line_number}");
            let (item, missing) = item_from_value(ItemInput {
                provider: CODEX_PROVIDER,
                conversation_native_id: &native_id,
                native_item_id: &native_item_id,
                source_record_id: &record_id,
                ordinal: line_number as u64,
                kind: ArchiveItemKind::Message,
                role: Some(ArchiveRole::User),
                created_at: timestamp,
                model: current_model.clone(),
                tool_name: None,
                tool_call_id: None,
                status: None,
                usage: None,
                content,
            });
            fallback_user_items.push((item, missing));
        }
    }
    for (item, missing) in fallback_user_items {
        if !structured_user_fingerprints.contains(&item_fingerprint(&item)) {
            builder.missing_content += missing;
            builder.push(item);
        }
    }
    mark_unresolved_mutations(&pending_mutations, diagnostics, trace_coverage);
    Ok(builder.finish())
}

fn codex_archive_header(
    path: &Path,
    fallback_id: String,
) -> Result<(String, Option<String>, Option<ProjectInfo>)> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut project_cache = ProjectContextCache::new();
    let mut reader = BufReader::new(file);
    let mut line_bytes = Vec::new();
    loop {
        let status = read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
        if status == BoundedLineRead::Eof {
            break;
        }
        if status == BoundedLineRead::Oversized {
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            continue;
        };
        if !line.contains("session_meta") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let native_id = value
            .pointer("/payload/id")
            .and_then(Value::as_str)
            .unwrap_or(&fallback_id)
            .to_string();
        let title = value
            .pointer("/payload/thread_name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let project = codex_project_context_from_value(&value, &mut project_cache);
        return Ok((native_id, title, project));
    }
    Ok((fallback_id, None, None))
}

fn collect_claude(
    source: &SourceLocation,
    selected_cache_keys: Option<&HashSet<String>>,
) -> Result<ArchiveScan> {
    let Some(root) = source_root_path(source) else {
        return Ok(ArchiveScan::default());
    };
    let projects = root.join("projects");
    let paths = selected_jsonl_paths(selected_cache_keys, || {
        collect_jsonl_files(&projects).unwrap_or_default()
    });
    let mut scan = ArchiveScan::measured();
    let mut artifact_dependencies = ArtifactDependencyMap::new();
    for path in paths {
        if path.file_name().and_then(|name| name.to_str()) == Some("sessions-index.json") {
            continue;
        }
        scan.diagnostics.files_scanned += 1;
        scan.conversations.push(collect_claude_file(
            source,
            &path,
            &mut scan.diagnostics,
            &mut artifact_dependencies,
            &mut scan.trace_edits,
            &mut scan.trace_coverage,
        )?);
    }
    scan.artifact_dependencies = finish_artifact_dependencies(artifact_dependencies);
    finish_diagnostics(&mut scan);
    Ok(scan)
}

fn collect_claude_file(
    source: &SourceLocation,
    path: &Path,
    diagnostics: &mut ArchiveScanDiagnostics,
    artifact_dependencies: &mut ArtifactDependencyMap,
    trace_edits: &mut Vec<TraceEdit>,
    trace_coverage: &mut CoverageStatus,
) -> Result<ArchiveConversation> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let fallback_id = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string();
    let is_subagent_file = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        == Some("subagents");
    let fallback_agent_id = fallback_id
        .strip_prefix("agent-")
        .unwrap_or(&fallback_id)
        .to_string();
    let mut builder =
        ConversationBuilder::new(CLAUDE_CODE_PROVIDER, source, fallback_id.clone(), path);
    let mut reader = BufReader::new(file);
    let mut line_bytes = Vec::new();
    let mut line_number = 0usize;
    let mut pending_mutations = HashMap::new();
    let source_record_path = canonical_display(path);
    loop {
        let status = read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
        if status == BoundedLineRead::Eof {
            break;
        }
        line_number = line_number.saturating_add(1);
        if status == BoundedLineRead::Oversized {
            diagnostics.records_scanned += 1;
            diagnostics.invalid_records += 1;
            record_unmeasurable_mutation(&mut diagnostics.truncated_mutations, trace_coverage);
            builder.missing_content += 1;
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            diagnostics.records_scanned += 1;
            diagnostics.invalid_records += 1;
            builder.missing_content += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        diagnostics.records_scanned += 1;
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                diagnostics.invalid_records += 1;
                builder.missing_content += 1;
                continue;
            }
        };
        if let Some(session_id) = value
            .get("sessionId")
            .or_else(|| value.get("session_id"))
            .and_then(Value::as_str)
        {
            let native_id =
                claude_archive_native_id(&value, session_id, &fallback_agent_id, is_subagent_file);
            if native_id != session_id {
                let superseded_id = archive_conversation_id(CLAUDE_CODE_PROVIDER, session_id);
                if !builder.superseded_conversation_ids.contains(&superseded_id) {
                    builder.superseded_conversation_ids.push(superseded_id);
                }
            }
            builder.native_id = native_id;
        }
        builder.title = builder.title.or_else(|| {
            value
                .get("summary")
                .or_else(|| value.get("title"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
        if builder.project.is_none() {
            builder.project = value
                .get("cwd")
                .and_then(Value::as_str)
                .map(expand_home_path)
                .and_then(|path| resolve_project_context(Some(path), None, None));
        }
        let message = value.get("message").unwrap_or(&value);
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .or_else(|| value.get("type").and_then(Value::as_str))
            .map(ArchiveRole::parse);
        let Some(content) = message.get("content").or_else(|| value.get("content")) else {
            continue;
        };
        let native_item_id = native_id_from_value(&value)
            .or_else(|| native_id_from_value(message))
            .unwrap_or_else(|| format!("line:{line_number}"));
        let created_at = timestamp_from_nested_value(&value);
        let model = model_from_nested_value(&value, None);
        let usage = message
            .get("usage")
            .map(super::claude_usage_counts_from_value);
        let content_blocks = content
            .as_array()
            .map_or_else(|| vec![content], |blocks| blocks.iter().collect());
        for (part_index, block) in content_blocks.into_iter().enumerate() {
            let content_type = block.get("type").and_then(Value::as_str).unwrap_or("");
            let source_record_id = format!("{source_record_path}:{line_number}:{part_index}");
            if claude_block_is_opaque_reasoning(block, content_type) {
                builder.discarded_source_record_ids.push(source_record_id);
                continue;
            }
            let (kind, item_role) = match content_type {
                "tool_use" | "tool_call" => {
                    (ArchiveItemKind::ToolCall, Some(ArchiveRole::Assistant))
                }
                "tool_result" => (ArchiveItemKind::ToolResult, Some(ArchiveRole::Tool)),
                "thinking" | "reasoning" | "reasoning_summary" => (
                    ArchiveItemKind::ReasoningSummary,
                    Some(ArchiveRole::Assistant),
                ),
                _ => (ArchiveItemKind::Message, role),
            };
            let block_native_item_id = native_id_from_value(block)
                .unwrap_or_else(|| format!("{native_item_id}:part:{part_index}:{}", kind.as_str()));
            let tool_call_id = block
                .get("tool_use_id")
                .or_else(|| block.get("call_id"))
                .or_else(|| block.get("id"))
                .and_then(Value::as_str);
            let archive_content = match kind {
                ArchiveItemKind::ToolCall => block.get("input").unwrap_or(block),
                ArchiveItemKind::ToolResult => block
                    .get("content")
                    .or_else(|| block.get("output"))
                    .unwrap_or(block),
                ArchiveItemKind::Message
                | ArchiveItemKind::ReasoningSummary
                | ArchiveItemKind::Artifact => block,
            };
            if kind == ArchiveItemKind::ToolCall {
                remember_original_mutation(
                    &mut pending_mutations,
                    diagnostics,
                    trace_coverage,
                    MutationInvocation {
                        call_key: tool_call_id.unwrap_or(&block_native_item_id).to_string(),
                        tool_name: block
                            .get("name")
                            .or_else(|| block.get("tool_name"))
                            .and_then(Value::as_str),
                        arguments: archive_content,
                        source_record_id: &source_record_id,
                        occurred_at: created_at,
                    },
                );
            } else if kind == ArchiveItemKind::ToolResult {
                finish_original_mutation(
                    &mut pending_mutations,
                    diagnostics,
                    trace_edits,
                    trace_coverage,
                    MutationCompletion {
                        call_key: tool_call_id.unwrap_or(&block_native_item_id),
                        cache_key: &source_record_path,
                        result: block,
                        status: block.get("status").and_then(Value::as_str),
                        provider: CLAUDE_CODE_PROVIDER,
                        source,
                        conversation_id: &archive_conversation_id(
                            CLAUDE_CODE_PROVIDER,
                            &builder.native_id,
                        ),
                        project: builder.project.as_ref(),
                    },
                );
            }
            if local_artifacts_allowed(kind, item_role) {
                collect_artifact_dependencies(archive_content, path, artifact_dependencies);
            }
            let (item, missing) = item_from_value(ItemInput {
                provider: CLAUDE_CODE_PROVIDER,
                conversation_native_id: &builder.native_id,
                native_item_id: &block_native_item_id,
                source_record_id: &source_record_id,
                ordinal: (line_number as u64) << 32 | part_index as u64,
                kind,
                role: item_role,
                created_at,
                model: model.clone(),
                tool_name: block
                    .get("name")
                    .or_else(|| block.get("tool_name"))
                    .and_then(Value::as_str),
                tool_call_id,
                status: block.get("status").and_then(Value::as_str),
                usage: (part_index == 0).then(|| usage.clone()).flatten(),
                content: archive_content,
            });
            builder.missing_content += missing;
            builder.push(item);
        }
    }
    mark_unresolved_mutations(&pending_mutations, diagnostics, trace_coverage);
    Ok(builder.finish())
}

fn claude_archive_native_id(
    value: &Value,
    session_id: &str,
    fallback_agent_id: &str,
    is_subagent_file: bool,
) -> String {
    let is_sidechain = value
        .get("isSidechain")
        .or_else(|| value.get("is_sidechain"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let agent_id = value
        .get("agentId")
        .or_else(|| value.get("agent_id"))
        .and_then(Value::as_str);
    if is_subagent_file {
        format!("{session_id}:agent:{fallback_agent_id}")
    } else if is_sidechain || agent_id.is_some() {
        format!(
            "{session_id}:agent:{}",
            agent_id.unwrap_or(fallback_agent_id)
        )
    } else {
        session_id.to_string()
    }
}

fn claude_block_is_opaque_reasoning(block: &Value, content_type: &str) -> bool {
    if matches!(content_type, "redacted_thinking" | "encrypted_thinking") {
        return true;
    }
    matches!(content_type, "thinking" | "reasoning" | "reasoning_summary")
        && !["text", "thinking", "summary", "content"]
            .into_iter()
            .filter_map(|key| block.get(key))
            .any(value_has_readable_content)
}

fn collect_grok(
    source: &SourceLocation,
    selected_cache_keys: Option<&HashSet<String>>,
) -> Result<ArchiveScan> {
    // Grok sessions record no tool mutations this module can reconstruct.
    let mut scan = ArchiveScan::default();
    let Some(root) = source_root_path(source) else {
        return Ok(scan);
    };
    let mut session_dirs = Vec::new();
    if let Some(selected) = selected_cache_keys {
        session_dirs.extend(
            selected
                .iter()
                .filter_map(|key| Path::new(key).parent().map(Path::to_path_buf)),
        );
    } else {
        let sessions = grok_sessions_root(&root);
        if sessions.is_dir() {
            session_dirs.extend(
                std::fs::read_dir(sessions)?
                    .filter_map(std::result::Result::ok)
                    .filter(|entry| entry.path().is_dir())
                    .map(|entry| entry.path()),
            );
        }
    }
    session_dirs.sort();
    session_dirs.dedup();
    let mut artifact_dependencies = ArtifactDependencyMap::new();
    for session_dir in session_dirs {
        let chat_path = session_dir.join("chat_history.jsonl");
        if !chat_path.is_file() {
            continue;
        }
        scan.diagnostics.files_scanned += 1;
        let native_id = session_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();
        let mut builder =
            ConversationBuilder::new(GROK_BUILD_PROVIDER, source, native_id.clone(), &chat_path);
        if let Ok(summary) = std::fs::read_to_string(session_dir.join("summary.json")) {
            if let Ok(value) = serde_json::from_str::<Value>(&summary) {
                builder.title = value
                    .get("title")
                    .or_else(|| value.pointer("/info/title"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                builder.project = value
                    .pointer("/info/cwd")
                    .and_then(Value::as_str)
                    .map(expand_home_path)
                    .and_then(|path| resolve_project_context(Some(path), None, None));
            }
        }
        let file = File::open(&chat_path)?;
        let mut reader = BufReader::new(file);
        let mut line_bytes = Vec::new();
        let mut line_number = 0usize;
        loop {
            let status =
                read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
            if status == BoundedLineRead::Eof {
                break;
            }
            line_number = line_number.saturating_add(1);
            if status == BoundedLineRead::Oversized {
                scan.diagnostics.records_scanned += 1;
                scan.diagnostics.invalid_records += 1;
                scan.diagnostics.truncated_mutations =
                    scan.diagnostics.truncated_mutations.saturating_add(1);
                builder.missing_content += 1;
                continue;
            }
            let Ok(line) = std::str::from_utf8(&line_bytes) else {
                scan.diagnostics.records_scanned += 1;
                scan.diagnostics.invalid_records += 1;
                builder.missing_content += 1;
                continue;
            };
            if line.trim().is_empty() {
                continue;
            }
            scan.diagnostics.records_scanned += 1;
            let value = match serde_json::from_str::<Value>(line) {
                Ok(value) => value,
                Err(_) => {
                    scan.diagnostics.invalid_records += 1;
                    builder.missing_content += 1;
                    continue;
                }
            };
            let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
            let (kind, role) = match record_type {
                "user" => (ArchiveItemKind::Message, ArchiveRole::User),
                "assistant" => (ArchiveItemKind::Message, ArchiveRole::Assistant),
                "reasoning" => (ArchiveItemKind::ReasoningSummary, ArchiveRole::Assistant),
                "tool_result" => (ArchiveItemKind::ToolResult, ArchiveRole::Tool),
                "system" => (ArchiveItemKind::Message, ArchiveRole::System),
                _ => continue,
            };
            let content = if kind == ArchiveItemKind::ToolResult {
                &value
            } else {
                value
                    .get("content")
                    .or_else(|| value.get("text"))
                    .or_else(|| value.get("message"))
                    .or_else(|| value.get("summary"))
                    .unwrap_or(&Value::Null)
            };
            if !value_has_readable_content(content) {
                continue;
            }
            if local_artifacts_allowed(kind, Some(role)) {
                collect_artifact_dependencies(content, &chat_path, &mut artifact_dependencies);
            }
            let native_item_id =
                native_id_from_value(&value).unwrap_or_else(|| format!("line:{line_number}"));
            let source_record_id = format!("{}:{line_number}", chat_path.display());
            let (item, missing) = item_from_value(ItemInput {
                provider: GROK_BUILD_PROVIDER,
                conversation_native_id: &native_id,
                native_item_id: &native_item_id,
                source_record_id: &source_record_id,
                ordinal: line_number as u64,
                kind,
                role: Some(role),
                created_at: timestamp_from_nested_value(&value),
                model: model_from_nested_value(&value, None),
                tool_name: value.get("name").and_then(Value::as_str),
                tool_call_id: value.get("tool_call_id").and_then(Value::as_str),
                status: value.get("status").and_then(Value::as_str),
                usage: None,
                content,
            });
            builder.missing_content += missing;
            builder.push(item);
        }
        scan.conversations.push(builder.finish());
    }
    scan.artifact_dependencies = finish_artifact_dependencies(artifact_dependencies);
    finish_diagnostics(&mut scan);
    Ok(scan)
}

fn collect_opencode(
    source: &SourceLocation,
    selected_cache_keys: Option<&HashSet<String>>,
) -> Result<ArchiveScan> {
    let Some(root) = source_root_path(source) else {
        return Ok(ArchiveScan::default());
    };
    let db_path = root.join("opencode.db");
    if !db_path.is_file()
        || selected_cache_keys.is_some_and(|keys| !keys.contains(&canonical_display(&db_path)))
    {
        return Ok(ArchiveScan::default());
    }
    let connection = open_sqlite_readonly(&db_path)?;
    let mut builders = BTreeMap::<String, ConversationBuilder>::new();
    let mut artifact_dependencies = ArtifactDependencyMap::new();
    let mut session_statement = connection.prepare(
        "SELECT id, title, time_created, time_updated, directory FROM session ORDER BY time_created, id",
    )?;
    let sessions = session_statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    for row in sessions {
        let (id, title, created, updated, directory) = row?;
        let mut builder = ConversationBuilder::new(OPENCODE_PROVIDER, source, id.clone(), &db_path);
        builder.title = title;
        builder.started_at = timestamp_from_epoch(created);
        builder.updated_at = timestamp_from_epoch(updated);
        builder.project = directory
            .map(PathBuf::from)
            .and_then(|path| resolve_project_context(Some(path), None, None));
        builders.insert(id, builder);
    }

    let mut message_context = HashMap::<
        String,
        (
            String,
            ArchiveRole,
            Option<DateTime<Utc>>,
            Option<ModelInfo>,
        ),
    >::new();
    let mut records_scanned = 0u64;
    let mut invalid_records = 0u64;
    let mut message_statement = connection.prepare(
        "SELECT id, session_id, time_created, data FROM message ORDER BY session_id, time_created, id",
    )?;
    let messages = message_statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in messages {
        let (message_id, session_id, created, data) = row?;
        records_scanned += 1;
        let value = match serde_json::from_str::<Value>(&data) {
            Ok(value) => value,
            Err(_) => {
                invalid_records += 1;
                if let Some(builder) = builders.get_mut(&session_id) {
                    builder.missing_content += 1;
                }
                continue;
            }
        };
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .map(ArchiveRole::parse)
            .unwrap_or(ArchiveRole::Unknown);
        let model = super::opencode_message_model_info(&value);
        message_context.insert(
            message_id.clone(),
            (
                session_id.clone(),
                role,
                timestamp_from_epoch(created),
                model.clone(),
            ),
        );
        if let Some(content) = value
            .get("content")
            .filter(|content| value_has_readable_content(content))
        {
            if let Some(builder) = builders.get_mut(&session_id) {
                if local_artifacts_allowed(ArchiveItemKind::Message, Some(role)) {
                    collect_artifact_dependencies(content, &db_path, &mut artifact_dependencies);
                }
                let (item, missing) = item_from_value(ItemInput {
                    provider: OPENCODE_PROVIDER,
                    conversation_native_id: &session_id,
                    native_item_id: &message_id,
                    source_record_id: &format!("message:{message_id}"),
                    ordinal: created.max(0) as u64,
                    kind: ArchiveItemKind::Message,
                    role: Some(role),
                    created_at: timestamp_from_epoch(created),
                    model,
                    tool_name: None,
                    tool_call_id: None,
                    status: None,
                    usage: Some(super::opencode_message_usage_counts(&value)),
                    content,
                });
                builder.missing_content += missing;
                builder.push(item);
            }
        }
    }

    let part_table_exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'part'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if part_table_exists {
        let mut part_statement = connection.prepare(
            "SELECT id, message_id, session_id, time_created, data FROM part ORDER BY session_id, time_created, id",
        )?;
        let parts = part_statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        for row in parts {
            let (part_id, message_id, session_id, created, data) = row?;
            records_scanned += 1;
            let value = match serde_json::from_str::<Value>(&data) {
                Ok(value) => value,
                Err(_) => {
                    invalid_records += 1;
                    if let Some(builder) = builders.get_mut(&session_id) {
                        builder.missing_content += 1;
                    }
                    continue;
                }
            };
            let part_type = value.get("type").and_then(Value::as_str).unwrap_or("");
            let Some(builder) = builders.get_mut(&session_id) else {
                continue;
            };
            if part_type == "tool" {
                let state = value.get("state").unwrap_or(&Value::Null);
                let input = state
                    .get("input")
                    .or_else(|| value.get("input"))
                    .unwrap_or(&Value::Null);
                let output = state
                    .get("output")
                    .filter(|value| !value.is_null())
                    .or_else(|| value.get("output"))
                    .filter(|value| !value.is_null())
                    .or_else(|| state.get("error"))
                    .or_else(|| value.get("error"));
                let tool_name = value
                    .get("tool")
                    .or_else(|| value.get("name"))
                    .and_then(Value::as_str);
                let tool_call_id = value
                    .get("callID")
                    .or_else(|| value.get("call_id"))
                    .and_then(Value::as_str);
                let status = state
                    .get("status")
                    .or_else(|| value.get("status"))
                    .and_then(Value::as_str);
                let model = message_context
                    .get(&message_id)
                    .and_then(|value| value.3.clone());
                let source_record_id = format!("part:{part_id}");
                let (call, call_missing) = item_from_value(ItemInput {
                    provider: OPENCODE_PROVIDER,
                    conversation_native_id: &session_id,
                    native_item_id: &part_id,
                    source_record_id: &source_record_id,
                    ordinal: created.max(0) as u64,
                    kind: ArchiveItemKind::ToolCall,
                    role: Some(ArchiveRole::Assistant),
                    created_at: timestamp_from_epoch(created),
                    model: model.clone(),
                    tool_name,
                    tool_call_id,
                    status,
                    usage: None,
                    content: input,
                });
                builder.missing_content += call_missing;
                builder.push(call);
                if let Some(output) = output {
                    let result_native_id = format!("{part_id}:result");
                    let (result, result_missing) = item_from_value(ItemInput {
                        provider: OPENCODE_PROVIDER,
                        conversation_native_id: &session_id,
                        native_item_id: &result_native_id,
                        source_record_id: &format!("{source_record_id}:result"),
                        ordinal: created.max(0) as u64,
                        kind: ArchiveItemKind::ToolResult,
                        role: Some(ArchiveRole::Tool),
                        created_at: timestamp_from_epoch(created),
                        model,
                        tool_name,
                        tool_call_id,
                        status,
                        usage: None,
                        content: output,
                    });
                    builder.missing_content += result_missing;
                    builder.push(result);
                }
                continue;
            }
            let (kind, role) = match part_type {
                "reasoning" => (
                    ArchiveItemKind::ReasoningSummary,
                    Some(ArchiveRole::Assistant),
                ),
                "tool_call" => (ArchiveItemKind::ToolCall, Some(ArchiveRole::Assistant)),
                "tool_result" => (ArchiveItemKind::ToolResult, Some(ArchiveRole::Tool)),
                "file" | "image" => (
                    ArchiveItemKind::Artifact,
                    message_context.get(&message_id).map(|value| value.1),
                ),
                "text" => (
                    ArchiveItemKind::Message,
                    message_context.get(&message_id).map(|value| value.1),
                ),
                _ => continue,
            };
            let content = if matches!(
                kind,
                ArchiveItemKind::ToolCall | ArchiveItemKind::ToolResult
            ) {
                &value
            } else {
                value
                    .get("text")
                    .or_else(|| value.get("content"))
                    .or_else(|| value.get("output"))
                    .unwrap_or(&value)
            };
            if local_artifacts_allowed(kind, role) {
                collect_artifact_dependencies(content, &db_path, &mut artifact_dependencies);
            }
            let (item, missing) = item_from_value(ItemInput {
                provider: OPENCODE_PROVIDER,
                conversation_native_id: &session_id,
                native_item_id: &part_id,
                source_record_id: &format!("part:{part_id}"),
                ordinal: created.max(0) as u64,
                kind,
                role,
                created_at: timestamp_from_epoch(created),
                model: message_context
                    .get(&message_id)
                    .and_then(|value| value.3.clone()),
                tool_name: value
                    .get("tool")
                    .or_else(|| value.get("name"))
                    .and_then(Value::as_str),
                tool_call_id: value
                    .get("callID")
                    .or_else(|| value.get("call_id"))
                    .and_then(Value::as_str),
                status: value
                    .pointer("/state/status")
                    .or_else(|| value.get("status"))
                    .and_then(Value::as_str),
                usage: None,
                content,
            });
            builder.missing_content += missing;
            builder.push(item);
        }
    }
    let mut scan = ArchiveScan {
        conversations: builders
            .into_values()
            .map(ConversationBuilder::finish)
            .collect(),
        artifact_dependencies: finish_artifact_dependencies(artifact_dependencies),
        trace_edits: Vec::new(),
        trace_coverage: CoverageStatus::Unavailable,
        diagnostics: ArchiveScanDiagnostics {
            files_scanned: 1,
            records_scanned,
            invalid_records,
            ..ArchiveScanDiagnostics::default()
        },
    };
    finish_diagnostics(&mut scan);
    Ok(scan)
}

struct ItemInput<'a> {
    provider: &'a str,
    conversation_native_id: &'a str,
    native_item_id: &'a str,
    source_record_id: &'a str,
    ordinal: u64,
    kind: ArchiveItemKind,
    role: Option<ArchiveRole>,
    created_at: Option<DateTime<Utc>>,
    model: Option<ModelInfo>,
    tool_name: Option<&'a str>,
    tool_call_id: Option<&'a str>,
    status: Option<&'a str>,
    usage: Option<UsageCounts>,
    content: &'a Value,
}

fn item_from_value(input: ItemInput<'_>) -> (ArchiveItem, u64) {
    let fingerprint = hash_text(&input.content.to_string());
    let item_id = archive_item_id(
        input.provider,
        input.conversation_native_id,
        Some(input.native_item_id),
        input.ordinal,
        &fingerprint,
    );
    let mut missing = 0;
    let mut parts = Vec::new();
    let materialize_local_artifacts = local_artifacts_allowed(input.kind, input.role);
    if matches!(
        input.kind,
        ArchiveItemKind::ToolCall | ArchiveItemKind::ToolResult
    ) {
        if !input.content.is_null() {
            let text = match input.content {
                Value::String(value) => value.clone(),
                value => value.to_string(),
            };
            if !text.trim().is_empty() && text != "null" {
                push_text_part(&item_id, ArchiveContentKind::Json, text, &mut parts);
            }
        }
        extract_binary_content_parts(
            input.content,
            &item_id,
            &mut parts,
            &mut missing,
            materialize_local_artifacts,
        );
    } else {
        extract_content_parts(
            input.content,
            &item_id,
            &mut parts,
            &mut missing,
            materialize_local_artifacts,
        );
    }
    if parts.is_empty() && missing > 0 {
        push_text_part(
            &item_id,
            ArchiveContentKind::Json,
            r#"{"omitted_content":"invalid, unavailable, or oversized artifact"}"#.to_string(),
            &mut parts,
        );
    } else if parts.is_empty() && !input.content.is_null() {
        let text = match input.content {
            Value::String(value) => value.clone(),
            value => value.to_string(),
        };
        if !text.trim().is_empty() && text != "null" {
            push_text_part(&item_id, ArchiveContentKind::Json, text, &mut parts);
        }
    }
    match input.kind {
        ArchiveItemKind::ToolCall => {
            bound_text_parts(&mut parts, MAX_TOOL_CALL_TEXT_BYTES, 0);
        }
        ArchiveItemKind::ToolResult => {
            bound_text_parts(
                &mut parts,
                MAX_TOOL_RESULT_TEXT_BYTES,
                TOOL_RESULT_TAIL_BYTES,
            );
        }
        ArchiveItemKind::Message
        | ArchiveItemKind::ReasoningSummary
        | ArchiveItemKind::Artifact => {}
    }
    let parts_authoritative = missing == 0 && parts.iter().all(|part| !part.truncated);
    (
        ArchiveItem {
            item_id,
            native_item_id: Some(input.native_item_id.to_string()),
            source_record_id: Some(input.source_record_id.to_string()),
            ordinal: input.ordinal,
            kind: input.kind,
            role: input.role,
            created_at: input.created_at,
            model: input.model,
            tool_name: input.tool_name.map(ToOwned::to_owned),
            tool_call_id: input.tool_call_id.map(ToOwned::to_owned),
            status: input.status.map(ToOwned::to_owned),
            usage: input.usage,
            parts_authoritative,
            parts,
        },
        missing,
    )
}

fn local_artifacts_allowed(kind: ArchiveItemKind, role: Option<ArchiveRole>) -> bool {
    role == Some(ArchiveRole::User)
        && matches!(kind, ArchiveItemKind::Message | ArchiveItemKind::Artifact)
}

fn extract_binary_content_parts(
    value: &Value,
    item_id: &str,
    parts: &mut Vec<ArchiveContentPart>,
    missing: &mut u64,
    materialize_local_artifacts: bool,
) {
    match value {
        Value::String(value) => {
            if let Some((mime_type, encoded)) = parse_data_url(value) {
                push_binary_part(item_id, mime_type, None, encoded, parts, missing);
            }
        }
        Value::Array(values) => {
            for value in values {
                extract_binary_content_parts(
                    value,
                    item_id,
                    parts,
                    missing,
                    materialize_local_artifacts,
                );
            }
        }
        Value::Object(object) => {
            let content_type = object.get("type").and_then(Value::as_str).unwrap_or("");
            if let Some(source) = object.get("source").and_then(Value::as_object) {
                if source.get("type").and_then(Value::as_str) == Some("base64") {
                    let mime_type = artifact_mime_type(object, content_type, None);
                    if let Some(data) = source.get("data").and_then(Value::as_str) {
                        push_binary_part(
                            item_id,
                            &mime_type,
                            object.get("name").and_then(Value::as_str),
                            data,
                            parts,
                            missing,
                        );
                        return;
                    }
                    *missing += 1;
                    return;
                }
            }
            if matches!(
                content_type,
                "image" | "input_image" | "file" | "input_file"
            ) {
                if let Some(artifact) = artifact_reference(object) {
                    if let Some((mime_type, encoded)) = parse_data_url(artifact) {
                        push_binary_part(
                            item_id,
                            mime_type,
                            object.get("name").and_then(Value::as_str),
                            encoded,
                            parts,
                            missing,
                        );
                    } else {
                        let bytes = materialize_local_artifacts
                            .then(|| read_explicit_local_artifact(artifact))
                            .flatten();
                        if let Some(bytes) = bytes {
                            let mime_type =
                                artifact_mime_type(object, content_type, Some(artifact));
                            push_binary_bytes(
                                item_id,
                                &mime_type,
                                object.get("name").and_then(Value::as_str),
                                &bytes,
                                parts,
                            );
                        } else {
                            push_external_part(item_id, content_type, artifact, parts);
                            *missing += 1;
                        }
                    }
                    return;
                }
                *missing += 1;
                return;
            }
            for value in object.values() {
                extract_binary_content_parts(
                    value,
                    item_id,
                    parts,
                    missing,
                    materialize_local_artifacts,
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn extract_content_parts(
    value: &Value,
    item_id: &str,
    parts: &mut Vec<ArchiveContentPart>,
    missing: &mut u64,
    materialize_local_artifacts: bool,
) {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
        Value::String(text) => {
            if let Some((mime_type, encoded)) = parse_data_url(text) {
                push_binary_part(item_id, mime_type, None, encoded, parts, missing);
            } else if !text.trim().is_empty() {
                push_text_part(item_id, ArchiveContentKind::Text, text.clone(), parts);
            }
        }
        Value::Array(values) => {
            for value in values {
                extract_content_parts(value, item_id, parts, missing, materialize_local_artifacts);
            }
        }
        Value::Object(object) => {
            let content_type = object.get("type").and_then(Value::as_str).unwrap_or("");
            if let Some(source) = object.get("source").and_then(Value::as_object) {
                if source.get("type").and_then(Value::as_str) == Some("base64") {
                    let mime_type = artifact_mime_type(object, content_type, None);
                    if let Some(data) = source.get("data").and_then(Value::as_str) {
                        push_binary_part(
                            item_id,
                            &mime_type,
                            object.get("name").and_then(Value::as_str),
                            data,
                            parts,
                            missing,
                        );
                        return;
                    }
                    *missing += 1;
                    return;
                }
            }
            if matches!(
                content_type,
                "image" | "input_image" | "file" | "input_file"
            ) {
                if let Some(value) = artifact_reference(object) {
                    if let Some((mime_type, encoded)) = parse_data_url(value) {
                        push_binary_part(
                            item_id,
                            mime_type,
                            object.get("name").and_then(Value::as_str),
                            encoded,
                            parts,
                            missing,
                        );
                    } else {
                        let bytes = materialize_local_artifacts
                            .then(|| read_explicit_local_artifact(value))
                            .flatten();
                        if let Some(bytes) = bytes {
                            let mime_type = artifact_mime_type(object, content_type, Some(value));
                            push_binary_bytes(
                                item_id,
                                &mime_type,
                                object.get("name").and_then(Value::as_str),
                                &bytes,
                                parts,
                            );
                        } else {
                            push_external_part(item_id, content_type, value, parts);
                            *missing += 1;
                        }
                    }
                    return;
                }
                *missing += 1;
                return;
            }
            if matches!(content_type, "text" | "input_text" | "output_text") {
                if let Some(text) = object.get("text").and_then(Value::as_str) {
                    push_text_part(item_id, ArchiveContentKind::Text, text.to_string(), parts);
                    return;
                }
            }
            if matches!(content_type, "thinking" | "reasoning" | "reasoning_summary") {
                for key in ["text", "thinking", "summary"] {
                    if let Some(value) = object.get(key) {
                        extract_content_parts(
                            value,
                            item_id,
                            parts,
                            missing,
                            materialize_local_artifacts,
                        );
                    }
                }
                return;
            }
            if matches!(content_type, "tool_use" | "tool_call" | "tool_result") {
                let compact = Value::Object(object.clone()).to_string();
                push_text_part(item_id, ArchiveContentKind::Json, compact, parts);
                return;
            }
            for key in ["text", "content", "message", "output"] {
                if let Some(value) = object.get(key) {
                    extract_content_parts(
                        value,
                        item_id,
                        parts,
                        missing,
                        materialize_local_artifacts,
                    );
                }
            }
        }
    }
}

fn artifact_reference(object: &serde_json::Map<String, Value>) -> Option<&str> {
    ["image_url", "file_data", "url", "data", "source"]
        .into_iter()
        .filter_map(|key| object.get(key))
        .find_map(|value| match value {
            Value::String(value) => Some(value.as_str()),
            Value::Object(value) => value
                .get("url")
                .or_else(|| value.get("data"))
                .and_then(Value::as_str),
            _ => None,
        })
}

fn artifact_mime_type(
    object: &serde_json::Map<String, Value>,
    content_type: &str,
    reference: Option<&str>,
) -> String {
    if let Some(mime_type) = object
        .get("mime_type")
        .or_else(|| object.get("media_type"))
        .and_then(Value::as_str)
        .or_else(|| {
            object
                .get("source")
                .and_then(Value::as_object)
                .and_then(|source| source.get("media_type"))
                .and_then(Value::as_str)
        })
    {
        return mime_type.to_string();
    }
    if let Some(mime_type) = reference
        .and_then(mime_type_from_artifact_reference)
        .or_else(|| {
            object
                .get("name")
                .and_then(Value::as_str)
                .and_then(mime_type_from_path)
        })
    {
        return mime_type.to_string();
    }
    if content_type.contains("image") {
        "image/unknown".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

fn mime_type_from_artifact_reference(reference: &str) -> Option<&'static str> {
    let path = if reference.starts_with("file:") {
        Url::parse(reference).ok()?.to_file_path().ok()?
    } else {
        PathBuf::from(reference)
    };
    mime_type_from_path(path.to_str()?)
}

fn mime_type_from_path(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "avif" => Some("image/avif"),
        "bmp" => Some("image/bmp"),
        "gif" => Some("image/gif"),
        "heic" => Some("image/heic"),
        "heif" => Some("image/heif"),
        "ico" => Some("image/x-icon"),
        "jpeg" | "jpg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "svg" | "svgz" => Some("image/svg+xml"),
        "tif" | "tiff" => Some("image/tiff"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn collect_artifact_dependencies(
    value: &Value,
    candidate_path: &Path,
    dependencies: &mut ArtifactDependencyMap,
) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_artifact_dependencies(value, candidate_path, dependencies);
            }
        }
        Value::Object(object) => {
            let is_embedded_base64 = object
                .get("source")
                .and_then(Value::as_object)
                .and_then(|source| source.get("type"))
                .and_then(Value::as_str)
                == Some("base64");
            let content_type = object.get("type").and_then(Value::as_str).unwrap_or("");
            if is_embedded_base64 {
                return;
            }
            if matches!(
                content_type,
                "image" | "input_image" | "file" | "input_file"
            ) {
                if let Some(path) =
                    artifact_reference(object).and_then(explicit_local_artifact_path)
                {
                    let signature = archive_artifact_metadata_signature(&path);
                    dependencies.insert((canonical_display(candidate_path), path), signature);
                }
                return;
            }
            if matches!(
                content_type,
                "text" | "input_text" | "output_text" | "tool_use" | "tool_call" | "tool_result"
            ) {
                return;
            }
            let keys = if matches!(content_type, "thinking" | "reasoning" | "reasoning_summary") {
                &["text", "thinking", "summary"][..]
            } else {
                &["text", "content", "message", "output"][..]
            };
            for value in keys.iter().filter_map(|key| object.get(*key)) {
                collect_artifact_dependencies(value, candidate_path, dependencies);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn finish_artifact_dependencies(
    dependencies: ArtifactDependencyMap,
) -> Vec<ArchiveArtifactDependency> {
    dependencies
        .into_iter()
        .map(
            |((cache_key, path), metadata_signature)| ArchiveArtifactDependency {
                cache_key,
                path,
                metadata_signature,
            },
        )
        .collect()
}

fn push_text_part(
    item_id: &str,
    kind: ArchiveContentKind,
    text: String,
    parts: &mut Vec<ArchiveContentPart>,
) {
    if text.trim().is_empty() {
        return;
    }
    let ordinal = parts.len() as u64;
    parts.push(ArchiveContentPart::text(
        archive_content_id(item_id, ordinal),
        ordinal,
        kind,
        text,
    ));
}

fn bound_text_parts(parts: &mut Vec<ArchiveContentPart>, max_bytes: usize, tail_bytes: usize) {
    let text_values = parts
        .iter()
        .filter_map(|part| part.text.as_deref())
        .collect::<Vec<_>>();
    let total_bytes = text_values.iter().map(|text| text.len()).sum::<usize>();
    if total_bytes <= max_bytes {
        return;
    }

    let original = text_values.join("\n");
    let Some(first_text_index) = parts.iter().position(|part| part.text.is_some()) else {
        return;
    };
    let marker = if tail_bytes == 0 {
        "\n[truncated]"
    } else {
        "\n[... truncated ...]\n"
    };
    let content_budget = max_bytes.saturating_sub(marker.len());
    let retained_tail_bytes = tail_bytes.min(content_budget);
    let retained_head_bytes = content_budget.saturating_sub(retained_tail_bytes);
    let head_end = previous_char_boundary(&original, retained_head_bytes);
    let tail_start = next_char_boundary(
        &original,
        original.len().saturating_sub(retained_tail_bytes),
    );
    let bounded = if retained_tail_bytes == 0 {
        format!("{}{}", &original[..head_end], marker)
    } else {
        format!(
            "{}{}{}",
            &original[..head_end],
            marker,
            &original[tail_start..]
        )
    };
    let first = &mut parts[first_text_index];
    first.text = Some(bounded);
    first.content_hash = hash_text(&original);
    first.original_bytes = total_bytes as u64;
    first.truncated = true;

    let mut retained_first_text = false;
    parts.retain(|part| {
        if part.text.is_none() {
            return true;
        }
        if retained_first_text {
            false
        } else {
            retained_first_text = true;
            true
        }
    });
}

fn previous_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn next_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn push_binary_part(
    item_id: &str,
    mime_type: &str,
    name: Option<&str>,
    encoded: &str,
    parts: &mut Vec<ArchiveContentPart>,
    missing: &mut u64,
) {
    push_binary_part_with_limit(
        item_id,
        mime_type,
        name,
        encoded,
        MAX_ARTIFACT_BYTES,
        parts,
        missing,
    );
}

fn push_binary_part_with_limit(
    item_id: &str,
    mime_type: &str,
    name: Option<&str>,
    encoded: &str,
    max_bytes: u64,
    parts: &mut Vec<ArchiveContentPart>,
    missing: &mut u64,
) {
    let Some(expected_bytes) = decoded_base64_len(encoded) else {
        *missing += 1;
        return;
    };
    if expected_bytes > max_bytes {
        *missing += 1;
        return;
    }
    let Ok(bytes) = BASE64.decode(encoded) else {
        *missing += 1;
        return;
    };
    if bytes.len() as u64 != expected_bytes || bytes.len() as u64 > max_bytes {
        *missing += 1;
        return;
    }
    let ordinal = parts.len() as u64;
    let kind = content_kind_for_mime(mime_type);
    parts.push(ArchiveContentPart::binary_bytes(
        archive_content_id(item_id, ordinal),
        ordinal,
        kind,
        Some(mime_type.to_string()),
        name.map(ToOwned::to_owned),
        &bytes,
    ));
}

fn decoded_base64_len(encoded: &str) -> Option<u64> {
    let encoded_len = u64::try_from(encoded.len()).ok()?;
    if encoded_len % 4 != 0 {
        return None;
    }
    let padding = if encoded.ends_with("==") {
        2
    } else if encoded.ends_with('=') {
        1
    } else {
        0
    };
    encoded_len
        .checked_div(4)?
        .checked_mul(3)?
        .checked_sub(padding)
}

fn push_binary_bytes(
    item_id: &str,
    mime_type: &str,
    name: Option<&str>,
    bytes: &[u8],
    parts: &mut Vec<ArchiveContentPart>,
) {
    let ordinal = parts.len() as u64;
    parts.push(ArchiveContentPart::binary_bytes(
        archive_content_id(item_id, ordinal),
        ordinal,
        content_kind_for_mime(mime_type),
        Some(mime_type.to_string()),
        name.map(ToOwned::to_owned),
        bytes,
    ));
}

fn push_external_part(
    item_id: &str,
    content_type: &str,
    uri: &str,
    parts: &mut Vec<ArchiveContentPart>,
) {
    let ordinal = parts.len() as u64;
    let kind = if content_type.contains("image") {
        ArchiveContentKind::Image
    } else {
        ArchiveContentKind::File
    };
    parts.push(ArchiveContentPart {
        content_id: archive_content_id(item_id, ordinal),
        ordinal,
        kind,
        mime_type: None,
        name: None,
        text: None,
        data_base64: None,
        external_uri: Some(uri.to_string()),
        content_hash: hash_text(uri),
        original_bytes: 0,
        truncated: false,
    });
}

fn parse_data_url(value: &str) -> Option<(&str, &str)> {
    let value = value.strip_prefix("data:")?;
    let (metadata, encoded) = value.split_once(',')?;
    let mime_type = metadata.strip_suffix(";base64")?;
    Some((mime_type, encoded))
}

fn read_explicit_local_artifact(value: &str) -> Option<Vec<u8>> {
    let path = explicit_local_artifact_path(value)?;
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ARTIFACT_BYTES {
        return None;
    }
    let file = File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_ARTIFACT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_ARTIFACT_BYTES).then_some(bytes)
}

fn explicit_local_artifact_path(value: &str) -> Option<PathBuf> {
    if value.starts_with("file:") {
        return Url::parse(value).ok()?.to_file_path().ok();
    }
    Path::new(value).is_absolute().then(|| PathBuf::from(value))
}

fn content_kind_for_mime(mime_type: &str) -> ArchiveContentKind {
    if mime_type.starts_with("image/") {
        ArchiveContentKind::Image
    } else if mime_type.starts_with("audio/") {
        ArchiveContentKind::Audio
    } else {
        ArchiveContentKind::File
    }
}

fn value_has_readable_content(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(values) => values.iter().any(value_has_readable_content),
        Value::Object(object) => {
            object.get("source").is_some()
                || object.get("image_url").is_some()
                || [
                    "text", "content", "message", "output", "summary", "thinking",
                ]
                .into_iter()
                .filter_map(|key| object.get(key))
                .any(value_has_readable_content)
        }
        Value::Bool(_) | Value::Number(_) => false,
    }
}

fn native_id_from_value(value: &Value) -> Option<String> {
    ["id", "uuid", "message_id", "messageId"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn item_fingerprint(item: &ArchiveItem) -> String {
    hash_text(
        &item
            .parts
            .iter()
            .map(|part| part.content_hash.as_str())
            .collect::<Vec<_>>()
            .join(":"),
    )
}

fn timestamp_from_epoch(value: i64) -> Option<DateTime<Utc>> {
    let millis = if value.abs() < 10_000_000_000 {
        value.saturating_mul(1000)
    } else {
        value
    };
    DateTime::from_timestamp_millis(millis)
}

fn selected_jsonl_paths<F>(
    selected_cache_keys: Option<&HashSet<String>>,
    all_paths: F,
) -> Vec<PathBuf>
where
    F: FnOnce() -> Vec<PathBuf>,
{
    match selected_cache_keys {
        Some(selected) => selected
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .collect(),
        None => all_paths(),
    }
}

fn finish_diagnostics(scan: &mut ArchiveScan) {
    if scan.trace_coverage != CoverageStatus::Unavailable {
        let diagnostic_coverage = if scan.diagnostics.failed_mutations > 0
            || scan.diagnostics.unsupported_mutations > 0
            || scan.diagnostics.truncated_mutations > 0
            || scan.diagnostics.invalid_records > 0
        {
            CoverageStatus::Partial
        } else {
            CoverageStatus::Complete
        };
        scan.trace_coverage = scan.trace_coverage.combine(diagnostic_coverage);
    }
    scan.diagnostics.conversations = scan.conversations.len() as u64;
    for conversation in &scan.conversations {
        scan.diagnostics.items += conversation.items.len() as u64;
        scan.diagnostics.missing_content += conversation.missing_content_count;
        for item in &conversation.items {
            scan.diagnostics.content_parts += item.parts.len() as u64;
            scan.diagnostics.binary_bytes += item
                .parts
                .iter()
                .filter(|part| part.data_base64.is_some())
                .map(|part| part.original_bytes)
                .sum::<u64>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GrokBuildAdapter, OpenCodeAdapter, ProviderAdapter};
    use statsai_core::LocationOrigin;
    use std::io::Write;
    use tempfile::tempdir;

    fn source(provider: &str, path: &Path) -> SourceLocation {
        SourceLocation::local_adapter(provider, "test", "1", path, LocationOrigin::Configured)
    }

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
    fn claude_subagents_with_a_shared_session_remain_distinct_conversations() {
        let dir = tempdir().unwrap();
        let subagents = dir
            .path()
            .join("projects")
            .join("workspace")
            .join("parent-session")
            .join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();

        for (agent_id, text) in [("alpha", "alpha work"), ("beta", "beta work")] {
            let path = subagents.join(format!("agent-{agent_id}.jsonl"));
            std::fs::write(
                path,
                serde_json::json!({
                    "sessionId": "parent-session",
                    "isSidechain": true,
                    "agentId": agent_id,
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": text}]
                    }
                })
                .to_string()
                    + "\n",
            )
            .unwrap();
        }

        let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
        assert_eq!(scan.conversations.len(), 2);
        let mut native_ids = scan
            .conversations
            .iter()
            .map(|conversation| conversation.native_conversation_id.as_str())
            .collect::<Vec<_>>();
        native_ids.sort_unstable();
        assert_eq!(
            native_ids,
            ["parent-session:agent:alpha", "parent-session:agent:beta"]
        );
        assert_ne!(
            scan.conversations[0].conversation_id,
            scan.conversations[1].conversation_id
        );
        let parent_id = archive_conversation_id(CLAUDE_CODE_PROVIDER, "parent-session");
        assert!(scan.conversations.iter().all(|conversation| {
            conversation.superseded_conversation_ids == [parent_id.clone()]
        }));
        assert_eq!(
            claude_archive_native_id(&serde_json::json!({}), "main-session", "fallback", false),
            "main-session"
        );
    }

    #[test]
    fn codex_collects_visible_reasoning_and_exact_embedded_image() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{{"id":"thread-1","thread_name":"Image work"}}}}"#).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{{"id":"m1","type":"message","role":"user","content":[{{"type":"input_text","text":"inspect this"}},{{"type":"input_image","image_url":"data:image/png;base64,AAEC/w=="}}]}}}}"#).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{{"id":"r1","type":"reasoning","summary":[{{"type":"summary_text","text":"The image is readable."}}],"encrypted_content":"opaque"}}}}"#).unwrap();

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
        assert_eq!(scan.conversations.len(), 1);
        let conversation = &scan.conversations[0];
        assert_eq!(conversation.completeness, ArchiveCompleteness::Complete);
        assert_eq!(conversation.project, None);
        assert!(conversation
            .items
            .iter()
            .any(|item| item.kind == ArchiveItemKind::ReasoningSummary));
        let image = conversation
            .items
            .iter()
            .flat_map(|item| &item.parts)
            .find(|part| part.kind == ArchiveContentKind::Image)
            .unwrap();
        assert_eq!(
            BASE64.decode(image.data_base64.as_ref().unwrap()).unwrap(),
            [0, 1, 2, 255]
        );
    }

    #[test]
    fn codex_keeps_tool_call_and_result_with_the_same_call_id() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"thread-1"}}}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"type":"response_item","payload":{{"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{{\"path\":\"README.md\"}}"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"response_item","payload":{{"type":"function_call_output","call_id":"call-1","output":"file contents"}}}}"#).unwrap();

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
        let items = &scan.conversations[0].items;
        assert_eq!(items.len(), 2);
        assert_ne!(items[0].item_id, items[1].item_id);
        assert!(items
            .iter()
            .any(|item| item.kind == ArchiveItemKind::ToolCall));
        assert!(items
            .iter()
            .any(|item| item.kind == ArchiveItemKind::ToolResult));
    }

    #[test]
    fn codex_malformed_json_record_makes_trace_coverage_partial() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"thread-1"}}}}"#
        )
        .unwrap();
        writeln!(file, "{{malformed json").unwrap();

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();

        assert_eq!(scan.diagnostics.invalid_records, 1);
        assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
    }

    #[test]
    fn claude_invalid_utf8_record_makes_trace_coverage_partial() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects/example");
        std::fs::create_dir_all(&projects).unwrap();
        let path = projects.join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "sessionId":"session-1",
                "message":{"role":"user","content":"hello"}
            })
        )
        .unwrap();
        file.write_all(&[0xff, b'\n']).unwrap();

        let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();

        assert_eq!(scan.diagnostics.invalid_records, 1);
        assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
    }

    #[test]
    fn codex_counts_original_patch_before_archive_tool_argument_truncation() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "timestamp": "2026-08-01T10:00:00Z",
                "type": "session_meta",
                "payload": {"id": "thread-1", "cwd": dir.path()}
            })
        )
        .unwrap();
        let mut patch = "*** Begin Patch\n*** Add File: src/generated.rs\n".to_string();
        for index in 0..5_000 {
            patch.push_str(&format!("+line {index}\n"));
        }
        patch.push_str("*** End Patch\n");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "timestamp": "2026-08-01T10:00:01Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "apply_patch",
                    "arguments": patch,
                }
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "timestamp": "2026-08-01T10:00:02Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call-1",
                    "output": "Done!",
                }
            })
        )
        .unwrap();

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
        assert_eq!(scan.trace_coverage, CoverageStatus::Complete);
        assert_eq!(scan.trace_edits.len(), 1);
        assert_eq!(scan.trace_edits[0].counts.source_additions, 5_000);
        let call = scan.conversations[0]
            .items
            .iter()
            .find(|item| item.kind == ArchiveItemKind::ToolCall)
            .unwrap();
        assert!(call.parts.iter().any(|part| part.truncated));
    }

    #[test]
    fn codex_custom_tool_records_collect_successful_apply_patch_edits() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        for value in [
            serde_json::json!({
                "timestamp":"2026-08-01T10:00:00Z",
                "type":"session_meta",
                "payload":{"id":"thread-1","cwd":dir.path()}
            }),
            serde_json::json!({
                "timestamp":"2026-08-01T10:00:01Z",
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call",
                    "call_id":"call-1",
                    "name":"apply_patch",
                    "input":"*** Begin Patch\n*** Add File: src/lib.rs\n+pub fn added() {}\n*** End Patch\n"
                }
            }),
            serde_json::json!({
                "timestamp":"2026-08-01T10:00:02Z",
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call_output",
                    "call_id":"call-1",
                    "output":"Done!"
                }
            }),
        ] {
            writeln!(file, "{value}").unwrap();
        }

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();

        assert_eq!(scan.trace_coverage, CoverageStatus::Complete);
        assert_eq!(scan.trace_edits.len(), 1);
        assert_eq!(scan.trace_edits[0].counts.source_additions, 1);
        assert_eq!(scan.diagnostics.applied_mutations, 1);
        let items = &scan.conversations[0].items;
        assert!(items
            .iter()
            .any(|item| item.kind == ArchiveItemKind::ToolCall));
        assert!(items
            .iter()
            .any(|item| item.kind == ArchiveItemKind::ToolResult));
    }

    #[cfg(unix)]
    #[test]
    fn codex_trace_record_ids_use_the_canonical_archive_path() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let real_root = dir.path().join("real");
        let sessions = real_root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        for value in [
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-1"}}),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "call_id":"call-1",
                    "name":"apply_patch",
                    "arguments":"*** Begin Patch\n*** Add File: src/lib.rs\n+line\n*** End Patch\n"
                }
            }),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call_output",
                    "call_id":"call-1",
                    "output":"Done!"
                }
            }),
        ] {
            writeln!(file, "{value}").unwrap();
        }

        let linked_root = dir.path().join("linked");
        symlink(&real_root, &linked_root).unwrap();
        let scan = collect_codex(&source(CODEX_PROVIDER, &linked_root), None).unwrap();

        assert_eq!(scan.trace_edits.len(), 1);
        assert!(scan.trace_edits[0]
            .source_record_id
            .starts_with(&format!("{}:", canonical_display(&path))));
    }

    #[test]
    fn failed_mutations_reduce_coverage_without_counting_lines() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        for value in [
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-1"}}),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "call_id":"call-1",
                    "name":"apply_patch",
                    "arguments":"*** Begin Patch\n*** Add File: src/lib.rs\n+line\n*** End Patch\n"
                }
            }),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call_output",
                    "call_id":"call-1",
                    "output":"patch rejected"
                }
            }),
        ] {
            writeln!(file, "{value}").unwrap();
        }

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
        assert!(scan.trace_edits.is_empty());
        assert_eq!(scan.diagnostics.failed_mutations, 1);
        assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
    }

    #[test]
    fn unparseable_successful_mutation_is_not_applied_or_complete() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        for value in [
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-1"}}),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "call_id":"call-1",
                    "name":"apply_patch",
                    "arguments":"*** Begin Patch\n*** End Patch\n"
                }
            }),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call_output",
                    "call_id":"call-1",
                    "output":"Done!"
                }
            }),
        ] {
            writeln!(file, "{value}").unwrap();
        }

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();

        assert!(scan.trace_edits.is_empty());
        assert_eq!(scan.diagnostics.applied_mutations, 0);
        assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
    }

    #[test]
    fn codex_invalid_context_patch_is_not_counted_as_applied() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        for value in [
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-1"}}),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "call_id":"call-1",
                    "name":"apply_patch",
                    "arguments":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch\n"
                }
            }),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call_output",
                    "call_id":"call-1",
                    "output":"Invalid Context 0:\nold"
                }
            }),
        ] {
            writeln!(file, "{value}").unwrap();
        }

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
        assert!(scan.trace_edits.is_empty());
        assert_eq!(scan.diagnostics.failed_mutations, 1);
        assert_eq!(scan.diagnostics.applied_mutations, 0);
        assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
    }

    #[test]
    fn codex_shell_execution_makes_trace_coverage_partial() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-1"}})
        )
        .unwrap();
        for (index, command) in [
            "git apply changes.patch",
            "python -c \"from pathlib import Path; Path('x').write_text('x')\"",
            "truncate -s 0 generated.txt",
            "printf x>generated.txt",
        ]
        .into_iter()
        .enumerate()
        {
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type":"response_item",
                    "payload":{
                        "type":"function_call",
                        "call_id":format!("call-{index}"),
                        "name":"exec_command",
                        "arguments":{"cmd":command}
                    }
                })
            )
            .unwrap();
        }

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
        assert_eq!(scan.diagnostics.unsupported_mutations, 4);
        assert_eq!(scan.trace_coverage, CoverageStatus::Partial);
    }

    #[test]
    fn read_only_shell_execution_keeps_trace_coverage_complete() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-1"}})
        )
        .unwrap();
        for (index, arguments) in [
            serde_json::json!({"cmd": "ls -la src"}),
            serde_json::json!({"cmd": "git status --short && git diff --stat"}),
            serde_json::json!({"cmd": "cargo test -p statsai-core 2>&1 | tail -20"}),
            serde_json::json!({"cmd": "RUST_LOG=debug cargo clippy --all-targets"}),
            serde_json::json!({"command": ["bash", "-lc", "rg --files-with-matches TODO crates"]}),
            serde_json::json!({"command": ["ls", "-la"]}),
        ]
        .into_iter()
        .enumerate()
        {
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type":"response_item",
                    "payload":{
                        "type":"function_call",
                        "call_id":format!("call-{index}"),
                        "name":"exec_command",
                        "arguments":arguments
                    }
                })
            )
            .unwrap();
        }

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
        assert_eq!(scan.diagnostics.unsupported_mutations, 0);
        assert_eq!(scan.trace_coverage, CoverageStatus::Complete);
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

    #[test]
    fn providers_without_mutation_parsing_report_unavailable_trace_coverage() {
        let dir = tempdir().unwrap();
        let scan =
            collect_provider_archive("unwired-provider", &source("unwired", dir.path()), None)
                .unwrap();
        assert_eq!(scan.trace_coverage, CoverageStatus::Unavailable);
    }

    #[test]
    fn claude_structured_edit_is_counted_after_successful_tool_result() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects/example");
        std::fs::create_dir_all(&projects).unwrap();
        let path = projects.join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        for value in [
            serde_json::json!({
                "sessionId":"session-1",
                "cwd":dir.path(),
                "message":{"role":"assistant","content":[{
                    "type":"tool_use",
                    "id":"tool-1",
                    "name":"Edit",
                    "input":{"file_path":dir.path().join("src/lib.rs"),"old_string":"old","new_string":"new"}
                }]}
            }),
            serde_json::json!({
                "sessionId":"session-1",
                "message":{"role":"user","content":[{
                    "type":"tool_result",
                    "tool_use_id":"tool-1",
                    "content":"updated"
                }]}
            }),
        ] {
            writeln!(file, "{value}").unwrap();
        }

        let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
        assert_eq!(scan.trace_edits.len(), 1);
        assert_eq!(
            scan.trace_edits[0].relative_path,
            PathBuf::from("src/lib.rs")
        );
        assert_eq!(scan.trace_edits[0].counts.source_additions, 1);
        assert_eq!(scan.trace_edits[0].counts.source_deletions, 1);
    }

    #[test]
    fn claude_write_is_counted_as_a_creation_when_the_result_says_so() {
        let write_session = |result: &str| {
            let dir = tempdir().unwrap();
            let projects = dir.path().join("projects/example");
            std::fs::create_dir_all(&projects).unwrap();
            let mut file = File::create(projects.join("session.jsonl")).unwrap();
            for value in [
                serde_json::json!({
                    "sessionId":"session-1",
                    "cwd":dir.path(),
                    "message":{"role":"assistant","content":[{
                        "type":"tool_use",
                        "id":"tool-1",
                        "name":"Write",
                        "input":{
                            "file_path":dir.path().join("src/lib.rs"),
                            "content":"one\ntwo\nthree\n"
                        }
                    }]}
                }),
                serde_json::json!({
                    "sessionId":"session-1",
                    "message":{"role":"user","content":[{
                        "type":"tool_result",
                        "tool_use_id":"tool-1",
                        "content":result
                    }]}
                }),
            ] {
                writeln!(file, "{value}").unwrap();
            }
            let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
            scan.trace_edits.into_iter().next().expect("one trace edit")
        };

        // The Write tool never declares creation in its arguments, so a created
        // file is only distinguishable from an overwrite through the outcome.
        let created = write_session("File created successfully at: /workspace/src/lib.rs");
        assert_eq!(
            created.mutation_kind,
            statsai_core::MutationKind::FileCreation
        );
        assert_eq!(created.counts.source_additions, 3);
        assert_eq!(created.counts.unclassified_lines_written, 0);
        assert_eq!(
            created.added_line_fingerprints.len(),
            3,
            "a creation carries fingerprints, so it can be trace-matched"
        );

        // An overwrite mixes new and replaced lines, which stays unclassified.
        let overwritten = write_session("The file /workspace/src/lib.rs has been updated.");
        assert_eq!(
            overwritten.mutation_kind,
            statsai_core::MutationKind::FileWrite
        );
        assert_eq!(overwritten.counts.source_additions, 0);
        assert_eq!(overwritten.counts.unclassified_lines_written, 3);
    }

    #[test]
    fn claude_multiedit_collects_each_nested_edit_after_successful_result() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects/example");
        std::fs::create_dir_all(&projects).unwrap();
        let path = projects.join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        for value in [
            serde_json::json!({
                "sessionId":"session-1",
                "cwd":dir.path(),
                "message":{"role":"assistant","content":[{
                    "type":"tool_use",
                    "id":"tool-1",
                    "name":"MultiEdit",
                    "input":{
                        "file_path":dir.path().join("src/lib.rs"),
                        "edits":[
                            {"old_string":"old","new_string":"new"},
                            {"old_string":"old","new_string":"new"}
                        ]
                    }
                }]}
            }),
            serde_json::json!({
                "sessionId":"session-1",
                "message":{"role":"user","content":[{
                    "type":"tool_result",
                    "tool_use_id":"tool-1",
                    "content":"updated"
                }]}
            }),
        ] {
            writeln!(file, "{value}").unwrap();
        }

        let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();

        assert_eq!(scan.trace_coverage, CoverageStatus::Complete);
        assert_eq!(scan.trace_edits.len(), 2);
        assert_eq!(scan.diagnostics.applied_mutations, 1);
        assert_ne!(
            scan.trace_edits[0].trace_edit_id,
            scan.trace_edits[1].trace_edit_id
        );
        assert!(scan.trace_edits.iter().all(|edit| {
            edit.relative_path == Path::new("src/lib.rs")
                && edit.counts.source_additions == 1
                && edit.counts.source_deletions == 1
        }));
    }

    #[test]
    fn external_artifact_marks_archive_partial_instead_of_silently_dropping_it() {
        let content = serde_json::json!({
            "type": "input_image",
            "image_url": {"url": "https://example.test/image.png"}
        });
        let (item, missing) = item_from_value(ItemInput {
            provider: "test",
            conversation_native_id: "c1",
            native_item_id: "i1",
            source_record_id: "line:1",
            ordinal: 1,
            kind: ArchiveItemKind::Message,
            role: Some(ArchiveRole::User),
            created_at: None,
            model: None,
            tool_name: None,
            tool_call_id: None,
            status: None,
            usage: None,
            content: &content,
        });
        assert_eq!(missing, 1);
        assert_eq!(
            item.parts[0].external_uri.as_deref(),
            Some("https://example.test/image.png")
        );
    }

    #[test]
    fn unmaterialized_artifacts_are_missing_for_messages_and_tools() {
        for kind in [ArchiveItemKind::Message, ArchiveItemKind::ToolResult] {
            let content = serde_json::json!({
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png"}
            });
            let (item, missing) = item_from_value(ItemInput {
                provider: "test",
                conversation_native_id: "c1",
                native_item_id: kind.as_str(),
                source_record_id: "line:1",
                ordinal: 1,
                kind,
                role: Some(ArchiveRole::Assistant),
                created_at: None,
                model: None,
                tool_name: None,
                tool_call_id: None,
                status: None,
                usage: None,
                content: &content,
            });

            assert_eq!(missing, 1);
            assert!(item.parts.iter().any(|part| part.text.is_some()));
            if kind == ArchiveItemKind::Message {
                assert!(item.parts.iter().any(|part| part
                    .text
                    .as_deref()
                    .is_some_and(|text| text.contains("omitted_content"))));
            }
            assert!(item.parts.iter().all(|part| part.data_base64.is_none()));
        }
    }

    #[test]
    fn local_artifact_reads_require_bounded_regular_files() {
        let dir = tempdir().unwrap();
        let small_path = dir.path().join("small.bin");
        std::fs::write(&small_path, [0, 1, 2, 255]).unwrap();
        assert_eq!(
            read_explicit_local_artifact(small_path.to_str().unwrap()),
            Some(vec![0, 1, 2, 255])
        );
        assert_eq!(
            read_explicit_local_artifact(dir.path().to_str().unwrap()),
            None
        );

        let spaced_path = dir.path().join("My Image.bin");
        std::fs::write(&spaced_path, [4, 5, 6]).unwrap();
        let file_url = Url::from_file_path(&spaced_path).unwrap().to_string();
        assert!(file_url.contains("My%20Image.bin"));
        assert_eq!(
            explicit_local_artifact_path(&file_url),
            Some(spaced_path.clone())
        );
        assert_eq!(read_explicit_local_artifact(&file_url), Some(vec![4, 5, 6]));
        let encoded_content = serde_json::json!({
            "type": "file",
            "url": file_url
        });
        let (encoded_item, encoded_missing) = item_from_value(ItemInput {
            provider: "test",
            conversation_native_id: "c1",
            native_item_id: "encoded-file",
            source_record_id: "line:encoded",
            ordinal: 2,
            kind: ArchiveItemKind::Message,
            role: Some(ArchiveRole::User),
            created_at: None,
            model: None,
            tool_name: None,
            tool_call_id: None,
            status: None,
            usage: None,
            content: &encoded_content,
        });
        assert_eq!(encoded_missing, 0);
        assert_eq!(
            BASE64
                .decode(encoded_item.parts[0].data_base64.as_ref().unwrap())
                .unwrap(),
            [4, 5, 6]
        );

        let oversized_path = dir.path().join("oversized.bin");
        File::create(&oversized_path)
            .unwrap()
            .set_len(MAX_ARTIFACT_BYTES + 1)
            .unwrap();
        assert_eq!(
            read_explicit_local_artifact(oversized_path.to_str().unwrap()),
            None
        );

        let artifact = oversized_path.to_string_lossy().into_owned();
        let content = serde_json::json!({"type": "file", "url": artifact.clone()});
        let (item, missing) = item_from_value(ItemInput {
            provider: "test",
            conversation_native_id: "c1",
            native_item_id: "i1",
            source_record_id: "line:1",
            ordinal: 1,
            kind: ArchiveItemKind::Message,
            role: Some(ArchiveRole::User),
            created_at: None,
            model: None,
            tool_name: None,
            tool_call_id: None,
            status: None,
            usage: None,
            content: &content,
        });
        assert_eq!(missing, 1);
        assert_eq!(
            item.parts[0].external_uri.as_deref(),
            Some(artifact.as_str())
        );
    }

    #[test]
    fn local_image_paths_keep_image_kind_without_provider_mime_metadata() {
        let dir = tempdir().unwrap();
        let png_path = dir.path().join("photo.png");
        std::fs::write(&png_path, [0x89, b'P', b'N', b'G']).unwrap();
        let content = serde_json::json!({
            "type": "input_image",
            "image_url": png_path.to_str().unwrap()
        });
        let (item, missing) = item_from_value(ItemInput {
            provider: "test",
            conversation_native_id: "c1",
            native_item_id: "image-1",
            source_record_id: "line:1",
            ordinal: 1,
            kind: ArchiveItemKind::Message,
            role: Some(ArchiveRole::User),
            created_at: None,
            model: None,
            tool_name: None,
            tool_call_id: None,
            status: None,
            usage: None,
            content: &content,
        });
        assert_eq!(missing, 0);
        assert_eq!(item.parts[0].kind, ArchiveContentKind::Image);
        assert_eq!(item.parts[0].mime_type.as_deref(), Some("image/png"));

        let extensionless_path = dir.path().join("attachment");
        std::fs::write(&extensionless_path, [1, 2, 3]).unwrap();
        let extensionless = serde_json::json!({
            "type": "image",
            "source": extensionless_path.to_str().unwrap()
        });
        let mut parts = Vec::new();
        let mut binary_missing = 0;
        extract_binary_content_parts(
            &extensionless,
            "item-binary",
            &mut parts,
            &mut binary_missing,
            true,
        );
        assert_eq!(binary_missing, 0);
        assert_eq!(parts[0].kind, ArchiveContentKind::Image);
        assert_eq!(parts[0].mime_type.as_deref(), Some("image/unknown"));
    }

    #[test]
    fn codex_tool_results_cannot_materialize_local_files() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "do not archive").unwrap();
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "session_meta",
                "payload": {"id": "thread-1"}
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call-1",
                    "output": {
                        "type": "file",
                        "source": secret.to_str().unwrap()
                    }
                }
            })
        )
        .unwrap();

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
        assert!(scan.artifact_dependencies.is_empty());
        assert_eq!(scan.conversations[0].missing_content_count, 1);
        let result = scan.conversations[0]
            .items
            .iter()
            .find(|item| item.kind == ArchiveItemKind::ToolResult)
            .unwrap();
        assert!(result.parts.iter().all(|part| part.data_base64.is_none()));
        assert!(result
            .parts
            .iter()
            .any(|part| part.external_uri.as_deref() == secret.to_str()));
    }

    #[test]
    fn codex_scan_tracks_explicit_local_artifact_dependencies() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let artifact = dir.path().join("missing.png");
        let path = sessions.join("thread.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "session_meta",
                "payload": {"id": "thread-1"}
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "id": "m1",
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_image",
                        "image_url": artifact.to_str().unwrap()
                    }]
                }
            })
        )
        .unwrap();

        let selected = HashSet::from([canonical_display(&path)]);
        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), Some(&selected)).unwrap();
        assert_eq!(scan.artifact_dependencies.len(), 1);
        assert_eq!(
            scan.artifact_dependencies[0].cache_key,
            canonical_display(&path)
        );
        assert_eq!(scan.artifact_dependencies[0].path, artifact);
        assert_eq!(scan.artifact_dependencies[0].metadata_signature, "missing");
    }

    #[test]
    fn codex_streaming_collection_uses_late_session_metadata() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("fallback-name.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"id":"m1","type":"message","role":"user","content":[{{"type":"input_text","text":"before metadata"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"thread-late","thread_name":"Late metadata"}}}}"#
        )
        .unwrap();

        let scan = collect_codex(&source(CODEX_PROVIDER, dir.path()), None).unwrap();
        assert_eq!(scan.conversations.len(), 1);
        let conversation = &scan.conversations[0];
        assert_eq!(conversation.native_conversation_id, "thread-late");
        assert_eq!(conversation.title.as_deref(), Some("Late metadata"));
        assert_eq!(conversation.items.len(), 1);
        assert_eq!(
            conversation.items[0].parts[0].text.as_deref(),
            Some("before metadata")
        );
    }

    #[test]
    fn selected_jsonl_paths_skip_discovery_for_explicit_cache_keys() {
        let path = PathBuf::from("selected.jsonl");
        let selected = HashSet::from([path.to_string_lossy().into_owned()]);
        let discovery_called = std::cell::Cell::new(false);

        let paths = selected_jsonl_paths(Some(&selected), || {
            discovery_called.set(true);
            vec![PathBuf::from("discovered.jsonl")]
        });

        assert_eq!(paths, vec![path]);
        assert!(!discovery_called.get());
    }

    #[test]
    fn embedded_artifact_decode_is_bounded_and_marks_missing() {
        let encoded = "AAEC/w==";
        assert_eq!(decoded_base64_len(encoded), Some(4));

        let mut parts = Vec::new();
        let mut missing = 0;
        push_binary_part_with_limit(
            "item-1",
            "image/png",
            None,
            encoded,
            3,
            &mut parts,
            &mut missing,
        );
        assert!(parts.is_empty());
        assert_eq!(missing, 1);

        push_binary_part_with_limit(
            "item-1",
            "image/png",
            None,
            encoded,
            4,
            &mut parts,
            &mut missing,
        );
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].original_bytes, 4);
        assert_eq!(missing, 1);

        push_binary_part_with_limit(
            "item-1",
            "image/png",
            None,
            "invalid!",
            4,
            &mut parts,
            &mut missing,
        );
        assert_eq!(parts.len(), 1);
        assert_eq!(missing, 2);
    }

    #[test]
    fn tool_results_are_bounded_but_keep_full_hash_and_size() {
        let original = "a".repeat(MAX_TOOL_RESULT_TEXT_BYTES + 1024);
        let content = Value::String(original.clone());
        let (item, missing) = item_from_value(ItemInput {
            provider: "test",
            conversation_native_id: "c1",
            native_item_id: "i1",
            source_record_id: "line:1",
            ordinal: 1,
            kind: ArchiveItemKind::ToolResult,
            role: Some(ArchiveRole::Tool),
            created_at: None,
            model: None,
            tool_name: Some("exec"),
            tool_call_id: None,
            status: None,
            usage: None,
            content: &content,
        });
        assert_eq!(missing, 0);
        assert!(!item.parts_authoritative);
        assert!(item.parts[0].truncated);
        assert_eq!(item.parts[0].original_bytes, original.len() as u64);
        assert_eq!(item.parts[0].content_hash, hash_text(&original));
        assert!(item.parts[0].text.as_ref().unwrap().contains("truncated"));
        assert!(item.parts[0].text.as_ref().unwrap().len() <= MAX_TOOL_RESULT_TEXT_BYTES);
    }

    #[test]
    fn tool_calls_preserve_complete_structured_arguments() {
        let content = serde_json::json!({
            "path": "/tmp/a",
            "content": "replacement",
            "options": {"recursive": true, "limit": 25}
        });
        let (item, missing) = item_from_value(ItemInput {
            provider: "test",
            conversation_native_id: "c1",
            native_item_id: "i1",
            source_record_id: "line:1",
            ordinal: 1,
            kind: ArchiveItemKind::ToolCall,
            role: Some(ArchiveRole::Assistant),
            created_at: None,
            model: None,
            tool_name: Some("replace"),
            tool_call_id: Some("call-1"),
            status: None,
            usage: None,
            content: &content,
        });

        assert_eq!(missing, 0);
        assert!(item.parts_authoritative);
        assert_eq!(item.parts.len(), 1);
        let archived = serde_json::from_str::<Value>(item.parts[0].text.as_ref().unwrap()).unwrap();
        assert_eq!(archived, content);
        assert_eq!(
            item.parts[0].original_bytes,
            content.to_string().len() as u64
        );
        assert!(!item.parts[0].truncated);
    }

    #[test]
    fn claude_collects_text_thinking_and_embedded_images() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects/project-a");
        std::fs::create_dir_all(&projects).unwrap();
        let path = projects.join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"sessionId":"claude-1","type":"assistant","timestamp":"2026-01-01T00:00:00Z","message":{{"role":"assistant","content":[{{"type":"text","text":"answer"}},{{"type":"thinking","thinking":"Readable reasoning"}},{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"AAEC/w=="}}}}]}}}}"#
        )
        .unwrap();

        let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
        let conversation = &scan.conversations[0];
        let parts = conversation
            .items
            .iter()
            .flat_map(|item| &item.parts)
            .collect::<Vec<_>>();
        assert!(parts
            .iter()
            .any(|part| part.text.as_deref() == Some("answer")));
        assert!(parts
            .iter()
            .any(|part| part.text.as_deref() == Some("Readable reasoning")));
        let image = parts
            .iter()
            .find(|part| part.kind == ArchiveContentKind::Image)
            .unwrap();
        assert_eq!(
            BASE64.decode(image.data_base64.as_ref().unwrap()).unwrap(),
            [0, 1, 2, 255]
        );
    }

    #[test]
    fn claude_discards_redacted_and_encrypted_only_thinking_blocks() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects/project-a");
        std::fs::create_dir_all(&projects).unwrap();
        let path = projects.join("session.jsonl");
        let record = serde_json::json!({
            "sessionId": "claude-1",
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "visible answer"},
                    {"type": "redacted_thinking", "data": "opaque-redacted-payload"},
                    {"type": "thinking", "data": "opaque-encrypted-payload"},
                    {"type": "thinking", "thinking": "readable reasoning"}
                ]
            }
        });
        std::fs::write(&path, record.to_string() + "\n").unwrap();

        let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
        let conversation = &scan.conversations[0];
        let archived_text = conversation
            .items
            .iter()
            .flat_map(|item| &item.parts)
            .filter_map(|part| part.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(archived_text.contains("visible answer"));
        assert!(archived_text.contains("readable reasoning"));
        assert!(!archived_text.contains("opaque-redacted-payload"));
        assert!(!archived_text.contains("opaque-encrypted-payload"));
        assert_eq!(conversation.items.len(), 2);
        assert_eq!(conversation.discarded_source_record_ids.len(), 2);
        assert_eq!(conversation.completeness, ArchiveCompleteness::Complete);
    }

    #[test]
    fn claude_classifies_and_bounds_tool_blocks() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects/project-a");
        std::fs::create_dir_all(&projects).unwrap();
        let path = projects.join("session.jsonl");
        let output = "x".repeat(MAX_TOOL_RESULT_TEXT_BYTES + 1024);
        let record = serde_json::json!({
            "sessionId": "claude-1",
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": "tool-1",
                        "name": "shell",
                        "input": {"command": "build"}
                    },
                    {
                        "type": "tool_result",
                        "tool_use_id": "tool-1",
                        "content": [
                            {"type": "text", "text": output},
                            {
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": "image/png",
                                    "data": "AAEC/w=="
                                }
                            }
                        ]
                    }
                ]
            }
        });
        let mut file = File::create(&path).unwrap();
        writeln!(file, "{record}").unwrap();

        let scan = collect_claude(&source(CLAUDE_CODE_PROVIDER, dir.path()), None).unwrap();
        let conversation = &scan.conversations[0];
        assert!(conversation
            .items
            .iter()
            .any(|item| item.kind == ArchiveItemKind::ToolCall));
        let result = conversation
            .items
            .iter()
            .find(|item| item.kind == ArchiveItemKind::ToolResult)
            .unwrap();
        assert_eq!(result.role, Some(ArchiveRole::Tool));
        assert_eq!(result.tool_call_id.as_deref(), Some("tool-1"));
        assert!(result.parts[0].truncated);
        assert!(result.parts[0].original_bytes > MAX_TOOL_RESULT_TEXT_BYTES as u64);
        assert!(result.parts[0].text.as_ref().unwrap().contains("truncated"));
        assert!(result.parts[0].text.as_ref().unwrap().len() <= MAX_TOOL_RESULT_TEXT_BYTES);
        let image = result
            .parts
            .iter()
            .find(|part| part.kind == ArchiveContentKind::Image)
            .unwrap();
        assert_eq!(
            BASE64.decode(image.data_base64.as_ref().unwrap()).unwrap(),
            [0, 1, 2, 255]
        );
    }

    #[test]
    fn grok_collects_readable_chat_history() {
        let dir = tempdir().unwrap();
        let session = dir.path().join("sessions/grok-1");
        std::fs::create_dir_all(&session).unwrap();
        let mut file = File::create(session.join("chat_history.jsonl")).unwrap();
        writeln!(file, r#"{{"id":"u1","type":"user","content":"hello"}}"#).unwrap();
        writeln!(
            file,
            r#"{{"id":"r1","type":"reasoning","summary":"thinking"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"id":"a1","type":"assistant","content":"world"}}"#
        )
        .unwrap();

        let scan = collect_grok(&source(GROK_BUILD_PROVIDER, dir.path()), None).unwrap();
        let conversation = &scan.conversations[0];
        assert_eq!(conversation.items.len(), 3);
        let reasoning = conversation
            .items
            .iter()
            .find(|item| item.kind == ArchiveItemKind::ReasoningSummary)
            .expect("reasoning summary");
        assert_eq!(reasoning.parts.len(), 1);
        assert_eq!(reasoning.parts[0].text.as_deref(), Some("thinking"));
        assert_eq!(conversation.completeness, ArchiveCompleteness::Complete);
        assert_eq!(scan.trace_coverage, CoverageStatus::Unavailable);
    }

    #[test]
    fn grok_archive_candidates_include_chat_without_summary() {
        let dir = tempdir().unwrap();
        let session = dir.path().join("sessions/grok-1");
        std::fs::create_dir_all(&session).unwrap();
        let chat_path = session.join("chat_history.jsonl");
        let mut file = File::create(&chat_path).unwrap();
        writeln!(file, r#"{{"id":"u1","type":"user","content":"hello"}}"#).unwrap();

        let source = source(GROK_BUILD_PROVIDER, dir.path());
        let adapter = GrokBuildAdapter;
        let candidates = adapter.archive_scan_candidates(&source).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, chat_path);

        let selected = candidates
            .into_iter()
            .map(|candidate| candidate.cache_key)
            .collect::<HashSet<_>>();
        let scan = adapter.collect_archive(&source, Some(&selected)).unwrap();
        assert_eq!(scan.conversations.len(), 1);
        assert_eq!(scan.conversations[0].items.len(), 1);
    }

    #[test]
    fn opencode_collects_part_text_and_binary_artifacts() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE session (
                  id TEXT PRIMARY KEY,
                  title TEXT,
                  time_created INTEGER NOT NULL,
                  time_updated INTEGER NOT NULL,
                  directory TEXT
                );
                CREATE TABLE message (
                  id TEXT PRIMARY KEY,
                  session_id TEXT NOT NULL,
                  time_created INTEGER NOT NULL,
                  data TEXT NOT NULL
                );
                CREATE TABLE part (
                  id TEXT PRIMARY KEY,
                  message_id TEXT NOT NULL,
                  session_id TEXT NOT NULL,
                  time_created INTEGER NOT NULL,
                  data TEXT NOT NULL
                );
                INSERT INTO session VALUES ('s1', 'OpenCode thread', 1000, 2000, '/tmp/project');
                INSERT INTO message VALUES ('m1', 's1', 1000, '{"role":"user"}');
                INSERT INTO part VALUES ('p1', 'm1', 's1', 1001, '{"type":"text","text":"hello from opencode"}');
                INSERT INTO part VALUES ('p2', 'm1', 's1', 1002, '{"type":"file","url":"data:image/png;base64,AAEC/w=="}');
                "#,
            )
            .unwrap();
        let tool_output = format!(
            "output-head-{}-output-tail",
            "x".repeat(MAX_TOOL_RESULT_TEXT_BYTES + 1024)
        );
        let tool_state = serde_json::json!({
            "type": "tool",
            "callID": "call-1",
            "tool": "shell",
            "state": {
                "status": "completed",
                "input": {"command": "build", "cwd": "/tmp/project"},
                "output": tool_output.clone()
            }
        });
        connection
            .execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["p3", "m1", "s1", 1003, tool_state.to_string()],
            )
            .unwrap();
        drop(connection);

        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let noncanonical_root = nested.join("..");
        let mut source = source(OPENCODE_PROVIDER, &noncanonical_root);
        source.path_label = Some(noncanonical_root.to_string_lossy().into_owned());
        let adapter = OpenCodeAdapter;
        let selected = adapter
            .archive_scan_candidates(&source)
            .unwrap()
            .into_iter()
            .map(|candidate| candidate.cache_key)
            .collect::<HashSet<_>>();
        assert_eq!(selected, HashSet::from([canonical_display(&db_path)]));

        let scan = collect_opencode(&source, Some(&selected)).unwrap();
        let conversation = &scan.conversations[0];
        assert_eq!(conversation.items.len(), 4);
        assert!(conversation
            .items
            .iter()
            .flat_map(|item| &item.parts)
            .any(|part| part.text.as_deref() == Some("hello from opencode")));
        assert!(conversation
            .items
            .iter()
            .flat_map(|item| &item.parts)
            .any(|part| part.kind == ArchiveContentKind::Image && part.original_bytes == 4));
        let call = conversation
            .items
            .iter()
            .find(|item| item.kind == ArchiveItemKind::ToolCall)
            .expect("tool call");
        let result = conversation
            .items
            .iter()
            .find(|item| item.kind == ArchiveItemKind::ToolResult)
            .expect("tool result");
        assert_ne!(call.item_id, result.item_id);
        assert_eq!(call.native_item_id.as_deref(), Some("p3"));
        assert_eq!(result.native_item_id.as_deref(), Some("p3:result"));
        assert_eq!(call.tool_name.as_deref(), Some("shell"));
        assert_eq!(call.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(call.status.as_deref(), Some("completed"));
        assert_eq!(
            serde_json::from_str::<Value>(call.parts[0].text.as_ref().unwrap()).unwrap(),
            serde_json::json!({"command": "build", "cwd": "/tmp/project"})
        );
        assert_eq!(result.role, Some(ArchiveRole::Tool));
        assert_eq!(result.tool_name.as_deref(), Some("shell"));
        assert_eq!(result.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(result.status.as_deref(), Some("completed"));
        assert!(result.parts[0].truncated);
        assert_eq!(result.parts[0].original_bytes, tool_output.len() as u64);
        assert_eq!(result.parts[0].content_hash, hash_text(&tool_output));
        let retained_output = result.parts[0].text.as_ref().unwrap();
        assert!(retained_output.starts_with("output-head-"));
        assert!(retained_output.contains("[... truncated ...]"));
        assert!(retained_output.ends_with("-output-tail"));
        assert!(retained_output.len() <= MAX_TOOL_RESULT_TEXT_BYTES);
        assert_eq!(scan.trace_coverage, CoverageStatus::Unavailable);
    }
}
