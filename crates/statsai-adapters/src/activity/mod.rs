//! Activity extraction: tool calls, MCP calls, and observed skill loads.

mod claude;
mod codex;
#[cfg(test)]
mod corpus;
mod families;
mod grok;
mod mcp;
mod opencode;
mod skills;

pub(crate) use claude::*;
pub(crate) use codex::*;
pub(crate) use grok::*;
pub(crate) use opencode::*;

use crate::AdapterScan;
use chrono::{DateTime, TimeZone, Utc};
use statsai_core::{
    activity_coverage_id, activity_invocation_id, canonical_display, coalesce_iso_day_ranges,
    hash_text, sanitize_activity_invocation, ActivityCoverageLevel, ActivityCoverageV1,
    ActivityDurationKind, ActivityFamily, ActivityInvocationV1, ActivityKind, ActivityOutcome,
    SourceId, SourceLocation, ACTIVITY_COVERAGE_SCHEMA_VERSION, ACTIVITY_EARLIEST_PLAUSIBLE_MS,
    ACTIVITY_INVOCATION_SCHEMA_VERSION, ACTIVITY_PARSER_REVISION,
};
use std::collections::BTreeSet;
use std::path::Path;

pub(crate) fn source_file_hash(path: &Path) -> String {
    hash_text(&canonical_display(path))
}

pub(crate) fn parse_rfc3339_utc(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|ts| ts.with_timezone(&Utc))
}

pub(crate) fn timestamp_from_millis(value: i64) -> Option<DateTime<Utc>> {
    if value <= 0 {
        return None;
    }
    Utc.timestamp_millis_opt(value)
        .single()
        .filter(|ts| ts.timestamp_millis() >= ACTIVITY_EARLIEST_PLAUSIBLE_MS)
}

pub(crate) fn duration_from_secs_nanos(secs: Option<u64>, nanos: Option<u32>) -> Option<u64> {
    match (secs, nanos) {
        (None, None) => None,
        (secs, nanos) => {
            let ms = secs.unwrap_or(0).saturating_mul(1000);
            let nano_ms = u64::from(nanos.unwrap_or(0)) / 1_000_000;
            Some(ms.saturating_add(nano_ms))
        }
    }
}

pub(crate) fn signed_duration_ms(start: DateTime<Utc>, end: DateTime<Utc>) -> Option<u64> {
    let ms = end.signed_duration_since(start).num_milliseconds();
    (ms >= 0).then_some(ms as u64)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_invocation(
    invocation_id: String,
    provider: &str,
    source_id: SourceId,
    source_file_path_hash: String,
    observed_at: DateTime<Utc>,
    kind: ActivityKind,
    display_name: String,
    family: ActivityFamily,
    mcp_server: Option<String>,
    mcp_tool: Option<String>,
    plugin: Option<String>,
    skill_catalog: Option<statsai_core::SkillCatalog>,
    outcome: ActivityOutcome,
    duration_ms: Option<u64>,
    duration_kind: Option<ActivityDurationKind>,
    evidence: &str,
) -> ActivityInvocationV1 {
    ActivityInvocationV1 {
        schema_version: ACTIVITY_INVOCATION_SCHEMA_VERSION.to_string(),
        invocation_id,
        provider: provider.to_string(),
        source_id,
        provider_account_id: None,
        source_file_path_hash,
        observed_at,
        kind,
        display_name,
        family,
        mcp_server,
        mcp_tool,
        plugin,
        skill_catalog,
        outcome,
        duration_ms,
        duration_kind,
        evidence: evidence.to_string(),
        parser_revision: ACTIVITY_PARSER_REVISION.to_string(),
    }
}

pub(crate) fn push_invocation(scan: &mut AdapterScan, invocation: ActivityInvocationV1) {
    let Some(invocation) = sanitize_activity_invocation(invocation) else {
        return;
    };
    if invocation.family == ActivityFamily::Other && invocation.kind == ActivityKind::Tool {
        scan.diagnostics.activity_unknown_names += 1;
    }
    scan.diagnostics.activity_rows += 1;
    scan.activity_invocations.push(invocation);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn push_coverage(
    scan: &mut AdapterScan,
    device_id: &str,
    source: &SourceLocation,
    provider: &str,
    day: &str,
    day_end: &str,
    kind: ActivityKind,
    level: ActivityCoverageLevel,
    evidence: &str,
) {
    let coverage_id = activity_coverage_id(device_id, &source.source_id.0, day, kind);
    if let Some(existing) = scan
        .activity_coverage
        .iter_mut()
        .find(|row| row.coverage_id == coverage_id)
    {
        if level.honesty_rank() > existing.level.honesty_rank() {
            existing.level = level;
            existing.evidence = evidence.to_string();
        }
        if day_end > existing.effective_day_end() {
            existing.day_end = day_end.to_string();
        }
        return;
    }
    scan.activity_coverage.push(ActivityCoverageV1 {
        schema_version: ACTIVITY_COVERAGE_SCHEMA_VERSION.to_string(),
        coverage_id,
        device_id: device_id.to_string(),
        source_id: source.source_id.clone(),
        provider: provider.to_string(),
        day: day.to_string(),
        day_end: day_end.to_string(),
        kind,
        level,
        evidence: evidence.to_string(),
        parser_revision: ACTIVITY_PARSER_REVISION.to_string(),
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_kind_coverage(
    scan: &mut AdapterScan,
    device_id: &str,
    source: &SourceLocation,
    provider: &str,
    days: &BTreeSet<String>,
    fallback_day: &str,
    kind: ActivityKind,
    level: ActivityCoverageLevel,
    evidence: &str,
) {
    let ranges = if days.is_empty() {
        vec![(fallback_day.to_string(), fallback_day.to_string())]
    } else {
        coalesce_iso_day_ranges(days.iter().map(String::as_str))
    };
    for (day, day_end) in ranges {
        push_coverage(
            scan, device_id, source, provider, &day, &day_end, kind, level, evidence,
        );
    }
}

#[must_use]
pub(crate) fn hashed_invocation_id(parts: &[&str]) -> String {
    activity_invocation_id(parts)
}

/// When a provider omits the call id, fold in extra disambiguators so two
/// incomplete records in the same session do not share an invocation id.
#[must_use]
pub(crate) fn hashed_invocation_id_or_ordinal(parts: &[&str], fallback: &[&str]) -> String {
    if parts.iter().any(|part| part.is_empty()) {
        let mut combined = Vec::with_capacity(parts.len() + fallback.len());
        combined.extend_from_slice(parts);
        combined.extend_from_slice(fallback);
        activity_invocation_id(&combined)
    } else {
        activity_invocation_id(parts)
    }
}

#[cfg(test)]
mod tests;
