use super::families::{family_for_name, GROK_FAMILY_ALIASES};
use super::{
    build_invocation, emit_kind_coverage, hashed_invocation_id_or_ordinal, parse_rfc3339_utc,
    push_invocation, source_file_hash,
};
use crate::read_bounded_jsonl_line;
use crate::AdapterScan;
use crate::BoundedLineRead;
use crate::GROK_BUILD_PROVIDER;
use crate::MAX_JSONL_RECORD_BYTES;
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use statsai_core::{
    activity_day_key, ActivityCoverageLevel, ActivityDurationKind, ActivityFamily, ActivityKind,
    ActivityOutcome, SourceLocation,
};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub(crate) const GROK_EVENTS_EVIDENCE: &str = "grok-events";
pub(crate) const GROK_CHAT_EVIDENCE: &str = "grok-chat-history";

#[derive(Deserialize)]
struct GrokToolCompleted {
    ts: Option<String>,
    tool_name: Option<String>,
    tool_call_id: Option<String>,
    duration_ms: Option<u64>,
    outcome: Option<String>,
}

pub(crate) fn extract_grok_session_activity(
    scan: &mut AdapterScan,
    source: &SourceLocation,
    session_dir: &Path,
    device_id: &str,
    fallback_timestamp: DateTime<Utc>,
) -> anyhow::Result<()> {
    let events_path = session_dir.join("events.jsonl");
    let session_name = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session");
    if events_path.is_file() {
        extract_grok_events(
            scan,
            source,
            &events_path,
            session_name,
            device_id,
            fallback_timestamp,
        )?;
    } else {
        extract_grok_chat_history(
            scan,
            source,
            &session_dir.join("chat_history.jsonl"),
            session_name,
            device_id,
            fallback_timestamp,
        )?;
    }
    Ok(())
}

fn extract_grok_events(
    scan: &mut AdapterScan,
    source: &SourceLocation,
    path: &Path,
    session_name: &str,
    device_id: &str,
    fallback_timestamp: DateTime<Utc>,
) -> anyhow::Result<()> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line_bytes = Vec::new();
    let file_hash = source_file_hash(path);
    let mut days = BTreeSet::new();
    let mut ordinal = 0usize;
    loop {
        let status = read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
        if status == BoundedLineRead::Eof {
            break;
        }
        ordinal = ordinal.saturating_add(1);
        if status == BoundedLineRead::Oversized {
            scan.diagnostics.invalid_rows += 1;
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            scan.diagnostics.invalid_rows += 1;
            continue;
        };
        if !line.contains("\"tool_completed\"") {
            continue;
        }
        let parsed: GrokToolCompleted = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => {
                scan.diagnostics.invalid_rows += 1;
                continue;
            }
        };
        let name = parsed.tool_name.as_deref().unwrap_or("tool");
        let call_id = parsed.tool_call_id.as_deref().unwrap_or("");
        let observed_at = parsed
            .ts
            .as_deref()
            .and_then(parse_rfc3339_utc)
            .unwrap_or(fallback_timestamp);
        days.insert(activity_day_key(observed_at));
        let outcome = match parsed.outcome.as_deref() {
            Some("success") => ActivityOutcome::Succeeded,
            Some("error") => ActivityOutcome::Failed,
            _ => ActivityOutcome::Unknown,
        };
        let family = family_for_name(GROK_FAMILY_ALIASES, name);
        let ordinal_key = ordinal.to_string();
        push_invocation(
            scan,
            build_invocation(
                hashed_invocation_id_or_ordinal(
                    &["grok-build", session_name, call_id],
                    &[&file_hash, &ordinal_key],
                ),
                GROK_BUILD_PROVIDER,
                source.source_id.clone(),
                file_hash.clone(),
                observed_at,
                ActivityKind::Tool,
                name.to_string(),
                family,
                None,
                None,
                None,
                None,
                outcome,
                parsed.duration_ms,
                parsed.duration_ms.map(|_| ActivityDurationKind::Reported),
                GROK_EVENTS_EVIDENCE,
            ),
        );
    }
    emit_grok_coverage(
        scan,
        source,
        device_id,
        &days,
        fallback_timestamp,
        ActivityCoverageLevel::Complete,
        ActivityCoverageLevel::Unavailable,
        ActivityCoverageLevel::Unavailable,
        GROK_EVENTS_EVIDENCE,
    );
    Ok(())
}

fn extract_grok_chat_history(
    scan: &mut AdapterScan,
    source: &SourceLocation,
    path: &Path,
    session_name: &str,
    device_id: &str,
    fallback_timestamp: DateTime<Utc>,
) -> anyhow::Result<()> {
    if !path.is_file() {
        emit_grok_coverage(
            scan,
            source,
            device_id,
            &BTreeSet::new(),
            fallback_timestamp,
            ActivityCoverageLevel::Unavailable,
            ActivityCoverageLevel::Unavailable,
            ActivityCoverageLevel::Unavailable,
            GROK_CHAT_EVIDENCE,
        );
        return Ok(());
    }
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line_bytes = Vec::new();
    let file_hash = source_file_hash(path);
    let mut pending: std::collections::HashMap<String, (String, DateTime<Utc>)> =
        std::collections::HashMap::new();
    let mut days = BTreeSet::new();
    loop {
        let status = read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)?;
        if status == BoundedLineRead::Eof {
            break;
        }
        if status == BoundedLineRead::Oversized {
            scan.diagnostics.invalid_rows += 1;
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            scan.diagnostics.invalid_rows += 1;
            continue;
        };
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => {
                scan.diagnostics.invalid_rows += 1;
                continue;
            }
        };
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let ts = value
                    .get("ts")
                    .or_else(|| value.get("timestamp"))
                    .and_then(Value::as_str)
                    .and_then(parse_rfc3339_utc)
                    .unwrap_or(fallback_timestamp);
                if let Some(calls) = value.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let id = call
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = call
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string();
                        pending.insert(id, (name, ts));
                    }
                }
            }
            Some("tool_result") => {
                let call_id = value
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let (name, observed_at) = pending
                    .remove(call_id)
                    .unwrap_or_else(|| ("tool".to_string(), fallback_timestamp));
                days.insert(activity_day_key(observed_at));
                let family = family_for_name(GROK_FAMILY_ALIASES, &name);
                push_invocation(
                    scan,
                    build_invocation(
                        hashed_invocation_id_or_ordinal(
                            &["grok-build", session_name, call_id],
                            &[&file_hash],
                        ),
                        GROK_BUILD_PROVIDER,
                        source.source_id.clone(),
                        file_hash.clone(),
                        observed_at,
                        ActivityKind::Tool,
                        name,
                        family,
                        None,
                        None,
                        None,
                        None,
                        ActivityOutcome::Unknown,
                        None,
                        None,
                        GROK_CHAT_EVIDENCE,
                    ),
                );
            }
            Some("backend_tool_call") => {
                let observed_at = value
                    .get("ts")
                    .or_else(|| value.get("timestamp"))
                    .and_then(Value::as_str)
                    .and_then(parse_rfc3339_utc)
                    .unwrap_or(fallback_timestamp);
                days.insert(activity_day_key(observed_at));
                let call_id = value.get("id").and_then(Value::as_str).unwrap_or("");
                push_invocation(
                    scan,
                    build_invocation(
                        hashed_invocation_id_or_ordinal(
                            &["grok-build", session_name, call_id],
                            &[&file_hash],
                        ),
                        GROK_BUILD_PROVIDER,
                        source.source_id.clone(),
                        file_hash.clone(),
                        observed_at,
                        ActivityKind::Tool,
                        "backend_tool_call".to_string(),
                        ActivityFamily::Other,
                        None,
                        None,
                        None,
                        None,
                        ActivityOutcome::Unknown,
                        None,
                        None,
                        GROK_CHAT_EVIDENCE,
                    ),
                );
            }
            _ => {}
        }
    }
    for (call_id, (name, observed_at)) in pending {
        days.insert(activity_day_key(observed_at));
        let family = family_for_name(GROK_FAMILY_ALIASES, &name);
        push_invocation(
            scan,
            build_invocation(
                hashed_invocation_id_or_ordinal(
                    &["grok-build", session_name, &call_id],
                    &[&file_hash],
                ),
                GROK_BUILD_PROVIDER,
                source.source_id.clone(),
                file_hash.clone(),
                observed_at,
                ActivityKind::Tool,
                name,
                family,
                None,
                None,
                None,
                None,
                ActivityOutcome::Unknown,
                None,
                None,
                GROK_CHAT_EVIDENCE,
            ),
        );
    }
    emit_grok_coverage(
        scan,
        source,
        device_id,
        &days,
        fallback_timestamp,
        ActivityCoverageLevel::Partial,
        ActivityCoverageLevel::Unavailable,
        ActivityCoverageLevel::Unavailable,
        GROK_CHAT_EVIDENCE,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_grok_coverage(
    scan: &mut AdapterScan,
    source: &SourceLocation,
    device_id: &str,
    days: &BTreeSet<String>,
    fallback_timestamp: DateTime<Utc>,
    tool_level: ActivityCoverageLevel,
    mcp_level: ActivityCoverageLevel,
    skill_level: ActivityCoverageLevel,
    evidence: &str,
) {
    let fallback_day = activity_day_key(fallback_timestamp);
    emit_kind_coverage(
        scan,
        device_id,
        source,
        GROK_BUILD_PROVIDER,
        days,
        &fallback_day,
        ActivityKind::Tool,
        tool_level,
        evidence,
    );
    emit_kind_coverage(
        scan,
        device_id,
        source,
        GROK_BUILD_PROVIDER,
        days,
        &fallback_day,
        ActivityKind::Mcp,
        mcp_level,
        evidence,
    );
    emit_kind_coverage(
        scan,
        device_id,
        source,
        GROK_BUILD_PROVIDER,
        days,
        &fallback_day,
        ActivityKind::Skill,
        skill_level,
        evidence,
    );
}
