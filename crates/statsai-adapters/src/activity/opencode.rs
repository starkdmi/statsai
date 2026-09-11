use super::families::{family_for_name, OPENCODE_FAMILY_ALIASES};
use super::mcp::split_opencode_mcp_name;
use super::shell_cmd::{classify_shell_command, shell_command_is_write};
use super::{
    build_invocation, emit_kind_coverage, hashed_invocation_id, push_command_for_tool,
    push_invocation, source_file_hash, timestamp_from_millis,
};
use crate::sqlite_column_exists;
use crate::sqlite_table_exists;
use crate::AdapterScan;
use crate::OPENCODE_PROVIDER;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use statsai_core::{
    activity_day_key, canonical_activity_display_name, ActivityCoverageLevel, ActivityDurationKind,
    ActivityFamily, ActivityInvocationV1, ActivityKind, ActivityOutcome, SkillCatalog,
    SourceLocation,
};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

pub(crate) const OPENCODE_EVIDENCE: &str = "opencode-tool-parts";

#[derive(Debug, Clone, Default)]
pub struct OpenCodeActivityCursor {
    pub last_time_updated: i64,
    pub rows_returned: u64,
}

#[derive(Deserialize)]
struct OpenCodeToolPart {
    #[serde(rename = "type")]
    part_type: Option<String>,
    tool: Option<String>,
    #[serde(rename = "messageID", alias = "message_id")]
    message_id: Option<String>,
    state: Option<OpenCodeToolState>,
}

#[derive(Deserialize)]
struct OpenCodeToolState {
    status: Option<String>,
    input: Option<OpenCodeToolInput>,
    time: Option<OpenCodeToolTime>,
}

#[derive(Deserialize)]
struct OpenCodeToolInput {
    name: Option<String>,
    command: Option<String>,
}

#[derive(Deserialize)]
struct OpenCodeToolTime {
    start: Option<i64>,
    end: Option<i64>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_opencode_activity(
    scan: &mut AdapterScan,
    connection: &Connection,
    source: &SourceLocation,
    db_path: &Path,
    device_id: &str,
    cursor: Option<i64>,
    mcp_servers: &[String],
    fallback_timestamp: DateTime<Utc>,
) -> anyhow::Result<OpenCodeActivityCursor> {
    if !crate::sqlite_table_exists(connection, "part")? {
        let fallback_day = activity_day_key(fallback_timestamp);
        emit_kind_coverage(
            scan,
            device_id,
            source,
            OPENCODE_PROVIDER,
            &BTreeSet::new(),
            &fallback_day,
            ActivityKind::Tool,
            ActivityCoverageLevel::Unavailable,
            OPENCODE_EVIDENCE,
        );
        return Ok(OpenCodeActivityCursor::default());
    }

    let last_time_updated = cursor.unwrap_or(0);
    let message_models = load_opencode_message_models(connection)?;
    let part_sql = if sqlite_column_exists(connection, "part", "message_id")? {
        "SELECT id, time_created, time_updated, data, message_id FROM part WHERE time_updated > ?1"
    } else {
        "SELECT id, time_created, time_updated, data, NULL FROM part WHERE time_updated > ?1"
    };
    let mut statement = connection.prepare(part_sql)?;
    let mut rows = statement.query([last_time_updated])?;
    let file_hash = source_file_hash(db_path);
    let mcp_classified = !mcp_servers.is_empty();
    let mut max_updated = last_time_updated;
    let mut rows_returned = 0u64;
    let mut days = BTreeSet::new();

    while let Some(row) = rows.next()? {
        rows_returned += 1;
        let part_id: String = row.get(0)?;
        let time_created: i64 = row.get(1)?;
        let time_updated: i64 = row.get(2)?;
        max_updated = max_updated.max(time_updated);
        let data_text: String = row.get(3)?;
        let part_message_id: Option<String> = row.get(4)?;
        let parsed: OpenCodeToolPart = match serde_json::from_str(&data_text) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if parsed.part_type.as_deref() != Some("tool") {
            continue;
        }
        let tool_name = parsed.tool.as_deref().unwrap_or("tool");
        let status = parsed
            .state
            .as_ref()
            .and_then(|state| state.status.as_deref());
        let outcome = match status {
            Some("completed") => ActivityOutcome::Succeeded,
            Some("error") => ActivityOutcome::Failed,
            _ => ActivityOutcome::Unknown,
        };
        let duration_ms = parsed.state.as_ref().and_then(|state| {
            let time = state.time.as_ref()?;
            match (time.start, time.end) {
                (Some(start), Some(end)) if end >= start => Some((end - start) as u64),
                _ => None,
            }
        });
        let observed_at = parsed
            .state
            .as_ref()
            .and_then(|state| state.time.as_ref()?.start)
            .or(Some(time_created))
            .and_then(timestamp_from_millis)
            .unwrap_or(fallback_timestamp);
        days.insert(activity_day_key(observed_at));
        let invocation_id = hashed_invocation_id(&["opencode", &part_id]);
        let model_key = part_message_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .or(parsed.message_id.as_deref());
        let model = model_key.and_then(|id| message_models.get(id).cloned());

        if tool_name == "skill" {
            let skill_name = parsed
                .state
                .as_ref()
                .and_then(|state| state.input.as_ref()?.name.clone())
                .unwrap_or_else(|| "skill".to_string());
            push_invocation(
                scan,
                with_model(
                    build_invocation(
                        hashed_invocation_id(&["opencode", &part_id, "skill"]),
                        OPENCODE_PROVIDER,
                        source.source_id.clone(),
                        file_hash.clone(),
                        observed_at,
                        ActivityKind::Skill,
                        skill_name,
                        ActivityFamily::Other,
                        None,
                        None,
                        None,
                        Some(SkillCatalog::User),
                        outcome,
                        duration_ms,
                        duration_ms.map(|_| ActivityDurationKind::Reported),
                        OPENCODE_EVIDENCE,
                    ),
                    model.clone(),
                ),
            );
            push_invocation(
                scan,
                with_model(
                    build_invocation(
                        invocation_id,
                        OPENCODE_PROVIDER,
                        source.source_id.clone(),
                        file_hash.clone(),
                        observed_at,
                        ActivityKind::Tool,
                        "skill".to_string(),
                        ActivityFamily::Other,
                        None,
                        None,
                        None,
                        None,
                        outcome,
                        duration_ms,
                        duration_ms.map(|_| ActivityDurationKind::Reported),
                        OPENCODE_EVIDENCE,
                    ),
                    model.clone(),
                ),
            );
            continue;
        }

        if mcp_classified {
            if let Some((server, tool)) = split_opencode_mcp_name(tool_name, mcp_servers) {
                push_invocation(
                    scan,
                    with_model(
                        build_invocation(
                            invocation_id,
                            OPENCODE_PROVIDER,
                            source.source_id.clone(),
                            file_hash.clone(),
                            observed_at,
                            ActivityKind::Mcp,
                            tool.to_string(),
                            ActivityFamily::Mcp,
                            Some(server),
                            Some(tool.to_string()),
                            None,
                            None,
                            outcome,
                            duration_ms,
                            duration_ms.map(|_| ActivityDurationKind::Reported),
                            OPENCODE_EVIDENCE,
                        ),
                        model.clone(),
                    ),
                );
                continue;
            }
        }

        let family = family_for_name(OPENCODE_FAMILY_ALIASES, tool_name);
        let mut tool = build_invocation(
            invocation_id,
            OPENCODE_PROVIDER,
            source.source_id.clone(),
            file_hash.clone(),
            observed_at,
            ActivityKind::Tool,
            canonical_activity_display_name(OPENCODE_PROVIDER, tool_name),
            family,
            None,
            None,
            None,
            None,
            outcome,
            duration_ms,
            duration_ms.map(|_| ActivityDurationKind::Reported),
            OPENCODE_EVIDENCE,
        );
        tool.model = model.clone();
        if family == ActivityFamily::Shell {
            if let Some(command) = parsed
                .state
                .as_ref()
                .and_then(|state| state.input.as_ref()?.command.as_deref())
            {
                push_command_for_tool(
                    scan,
                    &tool,
                    &classify_shell_command(command),
                    shell_command_is_write(command),
                );
            }
        }
        push_invocation(scan, tool);
    }

    let fallback_day = activity_day_key(fallback_timestamp);
    emit_kind_coverage(
        scan,
        device_id,
        source,
        OPENCODE_PROVIDER,
        &days,
        &fallback_day,
        ActivityKind::Tool,
        ActivityCoverageLevel::Complete,
        OPENCODE_EVIDENCE,
    );
    emit_kind_coverage(
        scan,
        device_id,
        source,
        OPENCODE_PROVIDER,
        &days,
        &fallback_day,
        ActivityKind::Mcp,
        if mcp_classified {
            ActivityCoverageLevel::Complete
        } else {
            ActivityCoverageLevel::Partial
        },
        OPENCODE_EVIDENCE,
    );
    emit_kind_coverage(
        scan,
        device_id,
        source,
        OPENCODE_PROVIDER,
        &days,
        &fallback_day,
        ActivityKind::Skill,
        ActivityCoverageLevel::Complete,
        OPENCODE_EVIDENCE,
    );

    Ok(OpenCodeActivityCursor {
        last_time_updated: max_updated,
        rows_returned,
    })
}

fn with_model(mut invocation: ActivityInvocationV1, model: Option<String>) -> ActivityInvocationV1 {
    invocation.model = model;
    invocation
}

fn load_opencode_message_models(
    connection: &Connection,
) -> anyhow::Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    if !sqlite_table_exists(connection, "message")? {
        return Ok(map);
    }
    let mut statement = connection.prepare("SELECT id, data FROM message")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let data: String = row.get(1)?;
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if let Some(model) = crate::model::opencode_message_model_id(&value) {
            let model = model.trim();
            if !model.is_empty() {
                map.insert(id, model.to_string());
            }
        }
    }
    Ok(map)
}

pub(crate) fn load_opencode_mcp_servers(config_path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(config_path) else {
        return Vec::new();
    };
    let stripped = strip_jsonc_comments(&text);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&stripped) else {
        return Vec::new();
    };
    value
        .get("mcp")
        .and_then(|mcp| mcp.as_object())
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}

pub(crate) fn strip_jsonc_comments(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            output.push(ch);
            continue;
        }
        if ch == '/' {
            match chars.peek() {
                Some('/') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next == '\n' {
                            output.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut prev_star = false;
                    for next in chars.by_ref() {
                        if prev_star && next == '/' {
                            break;
                        }
                        prev_star = next == '*';
                    }
                }
                _ => output.push(ch),
            }
            continue;
        }
        output.push(ch);
    }
    output
}
