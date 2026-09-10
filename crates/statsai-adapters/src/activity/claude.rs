use super::families::{family_for_name, CLAUDE_FAMILY_ALIASES};
use super::mcp::split_mcp_double_underscore;
use super::shell_cmd::{classify_shell_command, shell_command_is_write};
use super::skills::classify_claude_skill_input;
use super::{
    build_invocation, emit_kind_coverage, hashed_invocation_id, parse_rfc3339_utc,
    push_command_for_tool, push_invocation, signed_duration_ms, source_file_hash,
};
use crate::AdapterScan;
use crate::CLAUDE_CODE_PROVIDER;
use chrono::{DateTime, Utc};
use serde_json::Value;
use statsai_core::{
    activity_day_key, canonical_activity_display_name, ActivityCoverageLevel, ActivityDurationKind,
    ActivityFamily, ActivityKind, ActivityOutcome, SourceLocation,
};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

pub(crate) const CLAUDE_EVIDENCE: &str = "claude-tool-blocks";

#[derive(Debug, Default)]
pub(crate) struct ClaudeActivityExtractor {
    pending: HashMap<String, PendingTool>,
    completed: Vec<statsai_core::ActivityInvocationV1>,
    command_names: HashMap<String, (String, bool)>,
}

#[derive(Debug)]
struct PendingTool {
    invocation: statsai_core::ActivityInvocationV1,
}

impl ClaudeActivityExtractor {
    pub(crate) fn observe_value(
        &mut self,
        source: &SourceLocation,
        path: &Path,
        value: &Value,
        fallback_timestamp: DateTime<Utc>,
    ) {
        let record_type = value.get("type").and_then(Value::as_str);
        match record_type {
            Some("assistant") => self.observe_assistant(source, path, value, fallback_timestamp),
            Some("user") => self.observe_user(value, fallback_timestamp),
            _ => {}
        }
    }

    fn observe_assistant(
        &mut self,
        source: &SourceLocation,
        path: &Path,
        value: &Value,
        fallback_timestamp: DateTime<Utc>,
    ) {
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_utc)
            .unwrap_or(fallback_timestamp);
        let model = value
            .pointer("/message/model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        let Some(content) = value.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        let file_hash = source_file_hash(path);
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
            let invocation_id = hashed_invocation_id(&["claude-code", id]);
            if let Some((server, tool)) = split_mcp_double_underscore(name) {
                let mut invocation = build_invocation(
                    invocation_id,
                    CLAUDE_CODE_PROVIDER,
                    source.source_id.clone(),
                    file_hash.clone(),
                    timestamp,
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
                    CLAUDE_EVIDENCE,
                );
                invocation.model = model.clone();
                self.pending
                    .insert(id.to_string(), PendingTool { invocation });
                continue;
            }
            let family = family_for_name(CLAUDE_FAMILY_ALIASES, name);
            let display_name = canonical_activity_display_name(CLAUDE_CODE_PROVIDER, name);
            if family == ActivityFamily::Shell {
                if let Some(command) = block.pointer("/input/command").and_then(Value::as_str) {
                    self.command_names.insert(
                        invocation_id.clone(),
                        (
                            classify_shell_command(command),
                            shell_command_is_write(command),
                        ),
                    );
                }
            }
            let mut invocation = build_invocation(
                invocation_id.clone(),
                CLAUDE_CODE_PROVIDER,
                source.source_id.clone(),
                file_hash.clone(),
                timestamp,
                ActivityKind::Tool,
                display_name,
                family,
                None,
                None,
                None,
                None,
                ActivityOutcome::Unknown,
                None,
                None,
                CLAUDE_EVIDENCE,
            );
            invocation.model = model.clone();
            self.pending
                .insert(id.to_string(), PendingTool { invocation });
            if name == "Skill" {
                if let Some(skill_name) = block
                    .pointer("/input/skill")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    let classified = classify_claude_skill_input(skill_name);
                    let mut skill = build_invocation(
                        hashed_invocation_id(&["claude-code", id, "skill"]),
                        CLAUDE_CODE_PROVIDER,
                        source.source_id.clone(),
                        file_hash.clone(),
                        timestamp,
                        ActivityKind::Skill,
                        classified.name,
                        ActivityFamily::Other,
                        None,
                        None,
                        classified.plugin,
                        Some(classified.catalog),
                        ActivityOutcome::Unknown,
                        None,
                        None,
                        CLAUDE_EVIDENCE,
                    );
                    skill.model = model.clone();
                    self.completed.push(skill);
                }
            }
        }
        let _ = value;
    }

    fn observe_user(&mut self, value: &Value, fallback_timestamp: DateTime<Utc>) {
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_utc)
            .unwrap_or(fallback_timestamp);
        let Some(content) = value.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let Some(mut pending) = self.pending.remove(tool_use_id) else {
                continue;
            };
            pending.invocation.outcome = match block.get("is_error").and_then(Value::as_bool) {
                Some(true) => ActivityOutcome::Failed,
                Some(false) | None => ActivityOutcome::Succeeded,
            };
            pending.invocation.duration_ms =
                signed_duration_ms(pending.invocation.observed_at, timestamp);
            pending.invocation.duration_kind = pending
                .invocation
                .duration_ms
                .map(|_| ActivityDurationKind::WallClock);
            if pending.invocation.kind == ActivityKind::Skill {
                // skill rows are emitted separately at tool_use time
            }
            self.completed.push(pending.invocation);
        }
    }

    pub(crate) fn finish(
        mut self,
        scan: &mut AdapterScan,
        device_id: &str,
        source: &SourceLocation,
        fallback_timestamp: DateTime<Utc>,
    ) {
        for pending in self.pending.into_values() {
            self.completed.push(pending.invocation);
        }
        let mut days = BTreeSet::new();
        for invocation in self.completed {
            days.insert(activity_day_key(invocation.observed_at));
            if let Some((command_name, is_write)) =
                self.command_names.get(&invocation.invocation_id)
            {
                push_command_for_tool(scan, &invocation, command_name, *is_write);
            }
            push_invocation(scan, invocation);
        }
        let fallback_day = activity_day_key(fallback_timestamp);
        emit_kind_coverage(
            scan,
            device_id,
            source,
            CLAUDE_CODE_PROVIDER,
            &days,
            &fallback_day,
            ActivityKind::Tool,
            ActivityCoverageLevel::SampleBased,
            CLAUDE_EVIDENCE,
        );
        emit_kind_coverage(
            scan,
            device_id,
            source,
            CLAUDE_CODE_PROVIDER,
            &days,
            &fallback_day,
            ActivityKind::Mcp,
            ActivityCoverageLevel::SampleBased,
            CLAUDE_EVIDENCE,
        );
        emit_kind_coverage(
            scan,
            device_id,
            source,
            CLAUDE_CODE_PROVIDER,
            &days,
            &fallback_day,
            ActivityKind::Skill,
            ActivityCoverageLevel::SampleBased,
            CLAUDE_EVIDENCE,
        );
    }
}
