use super::families::{family_for_name, CODEX_LEGACY_FAMILY_ALIASES};
use super::mcp::split_mcp_double_underscore;
use super::skills::classify_skill_path;
use super::{
    build_invocation, duration_from_secs_nanos, emit_kind_coverage, hashed_invocation_id,
    hashed_invocation_id_or_ordinal, parse_rfc3339_utc, push_invocation, source_file_hash,
    timestamp_from_millis,
};
use crate::AdapterScan;
use crate::CODEX_PROVIDER;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use statsai_core::{
    activity_day_key, ActivityCoverageLevel, ActivityDurationKind, ActivityFamily, ActivityKind,
    ActivityOutcome, SourceLocation,
};
use std::collections::BTreeSet;
use std::path::Path;

pub(crate) const CODEX_NATIVE_EVIDENCE: &str = "codex-native-items";
pub(crate) const CODEX_LEGACY_EVIDENCE: &str = "codex-legacy-response-items";

#[derive(Debug, Default)]
pub(crate) struct CodexActivityExtractor {
    native: Vec<PendingNative>,
    legacy: Vec<PendingLegacy>,
    saw_native: bool,
}

#[derive(Debug)]
struct PendingNative {
    invocation: statsai_core::ActivityInvocationV1,
}

#[derive(Debug)]
struct PendingLegacy {
    invocation: statsai_core::ActivityInvocationV1,
}

impl CodexActivityExtractor {
    pub(crate) fn observe_native_line(
        &mut self,
        source: &SourceLocation,
        path: &Path,
        line: &str,
        ordinal: usize,
        fallback_timestamp: DateTime<Utc>,
    ) {
        let Some(parsed) = parse_native_line(source, path, line, ordinal, fallback_timestamp)
        else {
            return;
        };
        self.saw_native = true;
        for invocation in parsed {
            self.native.push(PendingNative { invocation });
        }
    }

    pub(crate) fn observe_legacy_line(
        &mut self,
        source: &SourceLocation,
        path: &Path,
        line: &str,
        session_id: &str,
        ordinal: usize,
        fallback_timestamp: DateTime<Utc>,
    ) {
        let Some(invocation) =
            parse_legacy_line(source, path, line, session_id, ordinal, fallback_timestamp)
        else {
            return;
        };
        self.legacy.push(PendingLegacy { invocation });
    }

    pub(crate) fn finish(
        self,
        scan: &mut AdapterScan,
        device_id: &str,
        source: &SourceLocation,
        fallback_timestamp: DateTime<Utc>,
    ) {
        let fallback_day = activity_day_key(fallback_timestamp);
        if self.saw_native {
            let mut days = BTreeSet::new();
            for pending in self.native {
                days.insert(activity_day_key(pending.invocation.observed_at));
                push_invocation(scan, pending.invocation);
            }
            emit_kind_coverage(
                scan,
                device_id,
                source,
                CODEX_PROVIDER,
                &days,
                &fallback_day,
                ActivityKind::Tool,
                ActivityCoverageLevel::Complete,
                CODEX_NATIVE_EVIDENCE,
            );
            emit_kind_coverage(
                scan,
                device_id,
                source,
                CODEX_PROVIDER,
                &days,
                &fallback_day,
                ActivityKind::Mcp,
                ActivityCoverageLevel::Complete,
                CODEX_NATIVE_EVIDENCE,
            );
            emit_kind_coverage(
                scan,
                device_id,
                source,
                CODEX_PROVIDER,
                &days,
                &fallback_day,
                ActivityKind::Skill,
                ActivityCoverageLevel::Complete,
                CODEX_NATIVE_EVIDENCE,
            );
        } else {
            let mut days = BTreeSet::new();
            for pending in self.legacy {
                days.insert(activity_day_key(pending.invocation.observed_at));
                push_invocation(scan, pending.invocation);
            }
            emit_kind_coverage(
                scan,
                device_id,
                source,
                CODEX_PROVIDER,
                &days,
                &fallback_day,
                ActivityKind::Tool,
                ActivityCoverageLevel::Partial,
                CODEX_LEGACY_EVIDENCE,
            );
            emit_kind_coverage(
                scan,
                device_id,
                source,
                CODEX_PROVIDER,
                &days,
                &fallback_day,
                ActivityKind::Mcp,
                ActivityCoverageLevel::Partial,
                CODEX_LEGACY_EVIDENCE,
            );
            emit_kind_coverage(
                scan,
                device_id,
                source,
                CODEX_PROVIDER,
                &days,
                &fallback_day,
                ActivityKind::Skill,
                ActivityCoverageLevel::Unavailable,
                CODEX_LEGACY_EVIDENCE,
            );
        }
    }
}

#[derive(Deserialize)]
struct NativeLine {
    timestamp: Option<String>,
    payload: NativePayload,
}

#[derive(Deserialize)]
struct NativePayload {
    thread_id: Option<String>,
    started_at_ms: Option<i64>,
    completed_at_ms: Option<i64>,
    item: NativeItem,
}

#[derive(Deserialize)]
struct NativeItem {
    #[serde(rename = "type")]
    item_type: String,
    id: Option<String>,
    status: Option<String>,
    parsed_cmd: Option<Vec<ParsedCmd>>,
    duration: Option<NativeDuration>,
    server: Option<String>,
    tool: Option<String>,
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct ParsedCmd {
    #[serde(rename = "type")]
    cmd_type: Option<String>,
    path: Option<String>,
}

#[derive(Deserialize)]
struct NativeDuration {
    secs: Option<u64>,
    nanos: Option<u32>,
}

#[derive(Deserialize)]
struct LegacyLine {
    timestamp: Option<String>,
    payload: LegacyPayload,
}

#[derive(Deserialize)]
struct LegacyPayload {
    #[serde(rename = "type")]
    payload_type: Option<String>,
    name: Option<String>,
    call_id: Option<String>,
}

const IGNORED_NATIVE_TYPES: &[&str] = &[
    "Reasoning",
    "AgentMessage",
    "UserMessage",
    "ContextCompaction",
];

fn parse_native_line(
    source: &SourceLocation,
    path: &Path,
    line: &str,
    ordinal: usize,
    fallback_timestamp: DateTime<Utc>,
) -> Option<Vec<statsai_core::ActivityInvocationV1>> {
    let parsed: NativeLine = serde_json::from_str(line).ok()?;
    if IGNORED_NATIVE_TYPES.contains(&parsed.payload.item.item_type.as_str()) {
        return None;
    }
    let file_hash = source_file_hash(path);
    let observed_at = parsed
        .payload
        .completed_at_ms
        .and_then(timestamp_from_millis)
        .or_else(|| parsed.timestamp.as_deref().and_then(parse_rfc3339_utc))
        .unwrap_or(fallback_timestamp);
    let duration_ms = match (parsed.payload.started_at_ms, parsed.payload.completed_at_ms) {
        (Some(start), Some(end)) if end >= start => Some((end - start) as u64),
        _ => parsed
            .payload
            .item
            .duration
            .as_ref()
            .and_then(|duration| duration_from_secs_nanos(duration.secs, duration.nanos)),
    };
    let duration_kind = duration_ms.map(|_| ActivityDurationKind::Reported);
    let outcome = match parsed.payload.item.status.as_deref() {
        Some("completed") => ActivityOutcome::Succeeded,
        Some("failed") => ActivityOutcome::Failed,
        _ => ActivityOutcome::Unknown,
    };
    let item_id = parsed.payload.item.id.as_deref().unwrap_or("");
    let thread_id = parsed.payload.thread_id.as_deref().unwrap_or("");
    let ordinal_key = ordinal.to_string();
    let invocation_id = hashed_invocation_id_or_ordinal(
        &["codex", thread_id, item_id],
        &[&file_hash, &ordinal_key],
    );
    let mut invocations = Vec::new();

    match parsed.payload.item.item_type.as_str() {
        "CommandExecution" => {
            let family = command_family(parsed.payload.item.parsed_cmd.as_deref().unwrap_or(&[]));
            invocations.push(build_invocation(
                invocation_id.clone(),
                CODEX_PROVIDER,
                source.source_id.clone(),
                file_hash.clone(),
                observed_at,
                ActivityKind::Tool,
                "exec".to_string(),
                family,
                None,
                None,
                None,
                None,
                outcome,
                duration_ms,
                duration_kind,
                CODEX_NATIVE_EVIDENCE,
            ));
            if let Some(cmds) = parsed.payload.item.parsed_cmd.as_ref() {
                for cmd in cmds {
                    if cmd.cmd_type.as_deref() != Some("read") {
                        continue;
                    }
                    let Some(skill_path) = cmd.path.as_deref() else {
                        continue;
                    };
                    let Some(skill) = classify_skill_path(skill_path) else {
                        continue;
                    };
                    invocations.push(build_invocation(
                        hashed_invocation_id(&["codex", thread_id, item_id, "skill", &skill.name]),
                        CODEX_PROVIDER,
                        source.source_id.clone(),
                        file_hash.clone(),
                        observed_at,
                        ActivityKind::Skill,
                        skill.name,
                        ActivityFamily::Other,
                        None,
                        None,
                        skill.plugin,
                        Some(skill.catalog),
                        outcome,
                        duration_ms,
                        duration_kind,
                        CODEX_NATIVE_EVIDENCE,
                    ));
                }
            }
        }
        "FileChange" => invocations.push(build_invocation(
            invocation_id,
            CODEX_PROVIDER,
            source.source_id.clone(),
            file_hash,
            observed_at,
            ActivityKind::Tool,
            "apply_patch".to_string(),
            ActivityFamily::FileWrite,
            None,
            None,
            None,
            None,
            outcome,
            duration_ms,
            duration_kind,
            CODEX_NATIVE_EVIDENCE,
        )),
        "McpToolCall" => {
            let server = parsed
                .payload
                .item
                .server
                .as_deref()
                .filter(|value| !value.is_empty());
            let tool = parsed
                .payload
                .item
                .tool
                .as_deref()
                .filter(|value| !value.is_empty());
            let (Some(server), Some(tool)) = (server, tool) else {
                return if invocations.is_empty() {
                    None
                } else {
                    Some(invocations)
                };
            };
            invocations.push(build_invocation(
                invocation_id,
                CODEX_PROVIDER,
                source.source_id.clone(),
                file_hash,
                observed_at,
                ActivityKind::Mcp,
                tool.to_string(),
                ActivityFamily::Mcp,
                Some(server.to_string()),
                Some(tool.to_string()),
                None,
                None,
                outcome,
                duration_ms,
                duration_kind,
                CODEX_NATIVE_EVIDENCE,
            ));
        }
        "WebSearch" => invocations.push(build_invocation(
            invocation_id,
            CODEX_PROVIDER,
            source.source_id.clone(),
            file_hash,
            observed_at,
            ActivityKind::Tool,
            "web_search".to_string(),
            ActivityFamily::Web,
            None,
            None,
            None,
            None,
            ActivityOutcome::Unknown,
            duration_ms,
            duration_kind,
            CODEX_NATIVE_EVIDENCE,
        )),
        "DynamicToolCall" => {
            let display = match (
                parsed.payload.item.namespace.as_deref(),
                parsed.payload.item.tool.as_deref(),
            ) {
                (Some(namespace), Some(tool)) => format!("{namespace}/{tool}"),
                (None, Some(tool)) => tool.to_string(),
                (Some(namespace), None) => namespace.to_string(),
                _ => "dynamic".to_string(),
            };
            invocations.push(build_invocation(
                invocation_id,
                CODEX_PROVIDER,
                source.source_id.clone(),
                file_hash,
                observed_at,
                ActivityKind::Tool,
                display,
                ActivityFamily::Other,
                None,
                None,
                None,
                None,
                outcome,
                duration_ms,
                duration_kind,
                CODEX_NATIVE_EVIDENCE,
            ));
        }
        "ImageView" => invocations.push(build_invocation(
            invocation_id,
            CODEX_PROVIDER,
            source.source_id.clone(),
            file_hash,
            observed_at,
            ActivityKind::Tool,
            "view_image".to_string(),
            ActivityFamily::Media,
            None,
            None,
            None,
            None,
            ActivityOutcome::Unknown,
            duration_ms,
            duration_kind,
            CODEX_NATIVE_EVIDENCE,
        )),
        "Extension" => invocations.push(build_invocation(
            invocation_id,
            CODEX_PROVIDER,
            source.source_id.clone(),
            file_hash,
            observed_at,
            ActivityKind::Tool,
            "extension".to_string(),
            ActivityFamily::Media,
            None,
            None,
            None,
            None,
            outcome,
            duration_ms,
            duration_kind,
            CODEX_NATIVE_EVIDENCE,
        )),
        "Plan" => invocations.push(build_invocation(
            invocation_id,
            CODEX_PROVIDER,
            source.source_id.clone(),
            file_hash,
            observed_at,
            ActivityKind::Tool,
            "update_plan".to_string(),
            ActivityFamily::Planning,
            None,
            None,
            None,
            None,
            ActivityOutcome::Unknown,
            duration_ms,
            duration_kind,
            CODEX_NATIVE_EVIDENCE,
        )),
        "SubAgentActivity" => invocations.push(build_invocation(
            invocation_id,
            CODEX_PROVIDER,
            source.source_id.clone(),
            file_hash,
            observed_at,
            ActivityKind::Tool,
            "subagent".to_string(),
            ActivityFamily::Agent,
            None,
            None,
            None,
            None,
            ActivityOutcome::Unknown,
            duration_ms,
            duration_kind,
            CODEX_NATIVE_EVIDENCE,
        )),
        _ => {}
    }
    (!invocations.is_empty()).then_some(invocations)
}

fn command_family(cmds: &[ParsedCmd]) -> ActivityFamily {
    if cmds.is_empty() {
        return ActivityFamily::Shell;
    }
    if cmds
        .iter()
        .all(|cmd| matches!(cmd.cmd_type.as_deref(), Some("read") | Some("list_files")))
    {
        return ActivityFamily::FileRead;
    }
    if cmds
        .iter()
        .all(|cmd| cmd.cmd_type.as_deref() == Some("search"))
    {
        return ActivityFamily::CodeSearch;
    }
    ActivityFamily::Shell
}

fn parse_legacy_line(
    source: &SourceLocation,
    path: &Path,
    line: &str,
    session_id: &str,
    ordinal: usize,
    fallback_timestamp: DateTime<Utc>,
) -> Option<statsai_core::ActivityInvocationV1> {
    let parsed: LegacyLine = serde_json::from_str(line).ok()?;
    let payload_type = parsed.payload.payload_type.as_deref()?;
    if !matches!(
        payload_type,
        "function_call" | "custom_tool_call" | "web_search_call" | "tool_search_call"
    ) {
        return None;
    }
    let observed_at = parsed
        .timestamp
        .as_deref()
        .and_then(parse_rfc3339_utc)
        .unwrap_or(fallback_timestamp);
    let file_hash = source_file_hash(path);
    if payload_type == "web_search_call" {
        return Some(build_invocation(
            hashed_invocation_id(&["codex", session_id, &ordinal.to_string()]),
            CODEX_PROVIDER,
            source.source_id.clone(),
            file_hash,
            observed_at,
            ActivityKind::Tool,
            "web_search_call".to_string(),
            ActivityFamily::Web,
            None,
            None,
            None,
            None,
            ActivityOutcome::Unknown,
            None,
            None,
            CODEX_LEGACY_EVIDENCE,
        ));
    }
    let name = parsed.payload.name.as_deref().unwrap_or(payload_type);
    let call_id = parsed.payload.call_id.as_deref().unwrap_or("");
    let ordinal_key = ordinal.to_string();
    let invocation_id = hashed_invocation_id_or_ordinal(
        &["codex", session_id, call_id],
        &[&file_hash, &ordinal_key],
    );
    if let Some((server, tool)) = split_mcp_double_underscore(name) {
        return Some(build_invocation(
            invocation_id,
            CODEX_PROVIDER,
            source.source_id.clone(),
            file_hash,
            observed_at,
            ActivityKind::Mcp,
            tool.to_string(),
            ActivityFamily::Mcp,
            Some(server.to_string()),
            Some(tool.to_string()),
            None,
            None,
            ActivityOutcome::Unknown,
            None,
            None,
            CODEX_LEGACY_EVIDENCE,
        ));
    }
    let family = family_for_name(CODEX_LEGACY_FAMILY_ALIASES, name);
    Some(build_invocation(
        invocation_id,
        CODEX_PROVIDER,
        source.source_id.clone(),
        file_hash,
        observed_at,
        ActivityKind::Tool,
        name.to_string(),
        family,
        None,
        None,
        None,
        None,
        ActivityOutcome::Unknown,
        None,
        None,
        CODEX_LEGACY_EVIDENCE,
    ))
}
