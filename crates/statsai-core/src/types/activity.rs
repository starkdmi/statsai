//! Tool, MCP, and skill activity contracts.
//!
//! Invocation records stay on the device. Rollups and coverage rows are the only
//! activity shapes that may leave it, and only when `include_activity` is on.
//!
//! ## §3.1 Codex streams
//!
//! Per file, switch to the native `item_completed` stream only when a
//! **tool-bearing** item is present (`CommandExecution`, `FileChange`,
//! `McpToolCall`, and the other extracted item types). Prose-only
//! `item_completed` items (`Reasoning`, `AgentMessage`, `UserMessage`,
//! `ContextCompaction`) do not flip the switch. Files that emit those prose
//! items alongside legacy `function_call` records (class B) keep the legacy
//! stream; treating “any `item_completed` line” as native-only would drop
//! those calls.
//!
//! ## §3.2 Claude `is_error`
//!
//! `is_error` is an optional Anthropic tool-result field that defaults to
//! false. A paired `tool_result` with `is_error == true` is failed; a paired
//! result with `is_error == false` **or the field absent** is succeeded.
//! Reserve `unknown` for unpaired `tool_use` blocks.

use crate::ids::{ProviderAccountId, SourceId};
use crate::paths::hash_text;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const ACTIVITY_INVOCATION_SCHEMA_VERSION: &str = "activity_invocation.v1";
pub const ACTIVITY_ROLLUP_SCHEMA_VERSION: &str = "activity_rollup.v1";
pub const ACTIVITY_COVERAGE_SCHEMA_VERSION: &str = "activity_coverage.v1";
pub const ACTIVITY_PARSER_REVISION: &str = "activity.v3";
/// Identity table for provider-native tool names. Bump when a rename or
/// cross-provider alias is added; do not treat this as a parser revision of its own.
pub const ACTIVITY_OP_ALIAS_REVISION: &str = "activity-ops.v2";
/// Timestamps at or before Unix epoch, and any day before this instant, are
/// treated as absent so a zero `completed_at_ms` cannot key 1970-01-01.
pub const ACTIVITY_EARLIEST_PLAUSIBLE_MS: i64 = 1_704_067_200_000; // 2024-01-01T00:00:00Z

pub const ACTIVITY_DURATION_BUCKET_COUNT: usize = 8;
pub const ACTIVITY_DURATION_HISTOGRAM_EDGES_MS: [u64; 7] =
    [10, 100, 1_000, 10_000, 60_000, 600_000, 3_600_000];

pub const ACTIVITY_FAMILY_V1: [&str; 12] = [
    "file-read",
    "file-write",
    "code-search",
    "shell",
    "web",
    "browser",
    "computer",
    "agent",
    "planning",
    "media",
    "mcp",
    "other",
];

pub const ACTIVITY_ENTITY_KEY_SEPARATOR: char = '\u{0000}';
pub const ACTIVITY_IDENTIFIER_MAX_BYTES: usize = 256;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityKind {
    Tool,
    Mcp,
    Skill,
    Command,
}

impl ActivityKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Mcp => "mcp",
            Self::Skill => "skill",
            Self::Command => "command",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tool" => Some(Self::Tool),
            "mcp" => Some(Self::Mcp),
            "skill" => Some(Self::Skill),
            "command" => Some(Self::Command),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityFamily {
    FileRead,
    FileWrite,
    CodeSearch,
    Shell,
    Web,
    Browser,
    Computer,
    Agent,
    Planning,
    Media,
    Mcp,
    Other,
}

impl ActivityFamily {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FileRead => "file-read",
            Self::FileWrite => "file-write",
            Self::CodeSearch => "code-search",
            Self::Shell => "shell",
            Self::Web => "web",
            Self::Browser => "browser",
            Self::Computer => "computer",
            Self::Agent => "agent",
            Self::Planning => "planning",
            Self::Media => "media",
            Self::Mcp => "mcp",
            Self::Other => "other",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "file-read" => Some(Self::FileRead),
            "file-write" => Some(Self::FileWrite),
            "code-search" => Some(Self::CodeSearch),
            "shell" => Some(Self::Shell),
            "web" => Some(Self::Web),
            "browser" => Some(Self::Browser),
            "computer" => Some(Self::Computer),
            "agent" => Some(Self::Agent),
            "planning" => Some(Self::Planning),
            "media" => Some(Self::Media),
            "mcp" => Some(Self::Mcp),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityOutcome {
    Succeeded,
    Failed,
    Unknown,
}

impl ActivityOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityDurationKind {
    Reported,
    WallClock,
}

impl ActivityDurationKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::WallClock => "wall-clock",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "reported" => Some(Self::Reported),
            "wall-clock" => Some(Self::WallClock),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SkillCatalog {
    User,
    Project,
    Plugin,
    System,
    Unknown,
}

impl SkillCatalog {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Plugin => "plugin",
            Self::System => "system",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "plugin" => Some(Self::Plugin),
            "system" => Some(Self::System),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityCoverageLevel {
    Complete,
    Partial,
    SampleBased,
    Unavailable,
}

impl ActivityCoverageLevel {
    /// Higher ranks disclose more uncertainty. Prefer this when two formats
    /// describe the same (source, day, kind).
    #[must_use]
    pub fn honesty_rank(self) -> u8 {
        match self {
            Self::Complete => 0,
            Self::SampleBased => 1,
            Self::Partial => 2,
            Self::Unavailable => 3,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::SampleBased => "sample-based",
            Self::Unavailable => "unavailable",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "complete" => Some(Self::Complete),
            "partial" => Some(Self::Partial),
            "sample-based" => Some(Self::SampleBased),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }
}

/// Local-only invocation. Arguments, outputs, paths, and raw call IDs are absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActivityInvocationV1 {
    pub schema_version: String,
    pub invocation_id: String,
    pub provider: String,
    pub source_id: SourceId,
    pub provider_account_id: Option<ProviderAccountId>,
    pub source_file_path_hash: String,
    pub observed_at: DateTime<Utc>,
    pub kind: ActivityKind,
    pub display_name: String,
    pub family: ActivityFamily,
    pub mcp_server: Option<String>,
    pub mcp_tool: Option<String>,
    pub plugin: Option<String>,
    pub skill_catalog: Option<SkillCatalog>,
    pub outcome: ActivityOutcome,
    pub duration_ms: Option<u64>,
    pub duration_kind: Option<ActivityDurationKind>,
    pub evidence: String,
    pub parser_revision: String,
}

/// Device-local / synced daily rollup. No invocation IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActivityRollupV1 {
    pub schema_version: String,
    pub rollup_id: String,
    pub device_id: String,
    pub source_id: SourceId,
    pub provider: String,
    pub provider_account_id: Option<ProviderAccountId>,
    pub day: String,
    pub kind: ActivityKind,
    /// Local rollup identity. Contains a NUL separator for MCP and plugin skills;
    /// omitted on the wire because the backend reconstructs it.
    #[serde(default, skip_serializing)]
    pub entity_key: String,
    pub display_name: String,
    pub family: ActivityFamily,
    pub mcp_server: Option<String>,
    pub mcp_tool: Option<String>,
    pub plugin: Option<String>,
    pub skill_catalog: Option<SkillCatalog>,
    pub calls: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub unknown: u64,
    pub duration_samples: u64,
    pub duration_sum_ms: u64,
    pub duration_max_ms: Option<u64>,
    pub duration_buckets: [u64; ACTIVITY_DURATION_BUCKET_COUNT],
    pub duration_kind: Option<ActivityDurationKind>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub evidence: String,
}

/// Per-source coverage for a contiguous run of days at one level.
///
/// Consecutive days that share `(source, kind, level, evidence)` collapse to
/// one row so a 256-day legacy stream is not 256 D1 writes. `day` is the
/// inclusive start; `day_end` is the inclusive end (`day` when the run is a
/// single day, or empty on payloads written before ranges existed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActivityCoverageV1 {
    pub schema_version: String,
    pub coverage_id: String,
    pub device_id: String,
    pub source_id: SourceId,
    pub provider: String,
    pub day: String,
    #[serde(default)]
    pub day_end: String,
    pub kind: ActivityKind,
    pub level: ActivityCoverageLevel,
    pub evidence: String,
    pub parser_revision: String,
}

impl ActivityCoverageV1 {
    #[must_use]
    pub fn effective_day_end(&self) -> &str {
        if self.day_end.is_empty() {
            &self.day
        } else {
            &self.day_end
        }
    }
}

/// Collapse a set of ISO days into inclusive `[start, end]` runs.
#[must_use]
pub fn coalesce_iso_day_ranges<'a, I>(days: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut parsed = days
        .into_iter()
        .filter_map(|day| chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok())
        .collect::<Vec<_>>();
    parsed.sort_unstable();
    parsed.dedup();
    let mut ranges = Vec::new();
    let mut start = None;
    let mut end = None;
    for day in parsed {
        match (start, end) {
            (None, _) => {
                start = Some(day);
                end = Some(day);
            }
            (Some(_), Some(prev)) if prev.succ_opt() == Some(day) => {
                end = Some(day);
            }
            (Some(range_start), Some(range_end)) => {
                ranges.push((range_start.to_string(), range_end.to_string()));
                start = Some(day);
                end = Some(day);
            }
            _ => {}
        }
    }
    if let (Some(range_start), Some(range_end)) = (start, end) {
        ranges.push((range_start.to_string(), range_end.to_string()));
    }
    ranges
}

#[must_use]
pub fn activity_invocation_id(parts: &[&str]) -> String {
    hash_text(&parts.join("\0"))
}

/// Strip control characters and truncate to the sync identifier bound.
#[must_use]
pub fn sanitize_activity_identifier(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(ACTIVITY_IDENTIFIER_MAX_BYTES));
    for ch in value.chars() {
        if ch.is_control() {
            continue;
        }
        if out.len().saturating_add(ch.len_utf8()) > ACTIVITY_IDENTIFIER_MAX_BYTES {
            break;
        }
        out.push(ch);
    }
    out
}

/// Drop invocations the hosted validator would reject as a whole batch.
#[must_use]
pub fn sanitize_activity_invocation(
    mut invocation: ActivityInvocationV1,
) -> Option<ActivityInvocationV1> {
    invocation.display_name = sanitize_activity_identifier(&invocation.display_name);
    invocation.mcp_server = invocation.mcp_server.and_then(|value| {
        let value = sanitize_activity_identifier(&value);
        (!value.is_empty()).then_some(value)
    });
    invocation.mcp_tool = invocation.mcp_tool.and_then(|value| {
        let value = sanitize_activity_identifier(&value);
        (!value.is_empty()).then_some(value)
    });
    invocation.plugin = invocation.plugin.and_then(|value| {
        let value = sanitize_activity_identifier(&value);
        (!value.is_empty()).then_some(value)
    });
    if invocation.display_name.is_empty() {
        return None;
    }
    if invocation.kind == ActivityKind::Mcp
        && (invocation.mcp_server.is_none() || invocation.mcp_tool.is_none())
    {
        return None;
    }
    Some(invocation)
}

#[must_use]
pub fn activity_entity_key(
    kind: ActivityKind,
    display_name: &str,
    mcp_server: Option<&str>,
    mcp_tool: Option<&str>,
    plugin: Option<&str>,
) -> String {
    match kind {
        ActivityKind::Mcp => format!(
            "{}{}{}",
            mcp_server.unwrap_or(""),
            ACTIVITY_ENTITY_KEY_SEPARATOR,
            mcp_tool.unwrap_or(display_name)
        ),
        ActivityKind::Skill => match plugin {
            Some(plugin) if !plugin.is_empty() => {
                format!("{plugin}{ACTIVITY_ENTITY_KEY_SEPARATOR}{display_name}")
            }
            _ => display_name.to_string(),
        },
        ActivityKind::Tool | ActivityKind::Command => display_name.to_string(),
    }
}

#[must_use]
pub fn activity_day_key(observed_at: DateTime<Utc>) -> String {
    observed_at.date_naive().to_string()
}

/// Map a provider-native tool name onto a stable identity.
///
/// `activity-ops.v2` (2026-09):
/// - Shell: Codex `shell` → `shell_command` → `exec_command` → `exec`/`run`,
///   Claude `Bash`, OpenCode `bash`, Grok `run_terminal_command` all map to
///   `shell`. `write_stdin` is the unified_exec stdin companion, not a rename.
/// - Read/edit/write/grep/glob/web names collapse across Claude, OpenCode, and
///   Grok. Codex `apply_patch` stays distinct from `edit`/`write`.
#[must_use]
pub fn canonical_activity_display_name(provider: &str, native_name: &str) -> String {
    match (provider, native_name) {
        ("codex", "shell" | "shell_command" | "exec_command" | "exec" | "run") => {
            "shell".to_string()
        }
        ("claude_code", "Bash") | ("opencode", "bash") | ("grok_build", "run_terminal_command") => {
            "shell".to_string()
        }
        ("claude_code", "Read" | "NotebookRead")
        | ("opencode", "read")
        | ("grok_build", "read_file") => "read".to_string(),
        ("claude_code", "Edit" | "MultiEdit" | "NotebookEdit")
        | ("opencode", "edit" | "multiedit")
        | ("grok_build", "search_replace") => "edit".to_string(),
        ("claude_code", "Write") | ("opencode", "write") | ("grok_build", "write") => {
            "write".to_string()
        }
        ("claude_code", "Grep") | ("opencode", "grep") | ("grok_build", "grep") => {
            "grep".to_string()
        }
        ("claude_code", "Glob") | ("opencode", "glob") => "glob".to_string(),
        ("claude_code", "WebFetch") | ("opencode", "webfetch") => "web_fetch".to_string(),
        ("claude_code", "WebSearch")
        | ("opencode", "websearch" | "google_search")
        | ("codex", "web_search_call") => "web_search".to_string(),
        _ => native_name.to_string(),
    }
}

#[must_use]
pub fn activity_duration_bucket_index(duration_ms: u64) -> usize {
    ACTIVITY_DURATION_HISTOGRAM_EDGES_MS
        .iter()
        .position(|&edge| duration_ms < edge)
        .unwrap_or(ACTIVITY_DURATION_BUCKET_COUNT - 1)
}

/// Upper edge of a histogram bucket, used as the p50/p90 estimate.
///
/// The last bucket is unbounded (`≥1h`); its reported edge is the 1h threshold.
#[must_use]
pub fn activity_duration_bucket_upper_edge_ms(index: usize) -> Option<u64> {
    if index >= ACTIVITY_DURATION_BUCKET_COUNT {
        return None;
    }
    Some(
        ACTIVITY_DURATION_HISTOGRAM_EDGES_MS
            .get(index)
            .copied()
            .unwrap_or(*ACTIVITY_DURATION_HISTOGRAM_EDGES_MS.last().expect("edges")),
    )
}

/// Bucket-estimate percentile: the upper edge of the first bucket whose
/// cumulative count reaches `percentile * samples`.
#[must_use]
pub fn activity_duration_percentile_ms(
    buckets: &[u64; ACTIVITY_DURATION_BUCKET_COUNT],
    samples: u64,
    percentile: f64,
) -> Option<u64> {
    if samples == 0 || !(0.0..=1.0).contains(&percentile) {
        return None;
    }
    let target = ((percentile * samples as f64).ceil() as u64).max(1);
    let mut cumulative = 0u64;
    for (index, count) in buckets.iter().enumerate() {
        cumulative = cumulative.saturating_add(*count);
        if cumulative >= target {
            return activity_duration_bucket_upper_edge_ms(index);
        }
    }
    activity_duration_bucket_upper_edge_ms(ACTIVITY_DURATION_BUCKET_COUNT - 1)
}

#[must_use]
pub fn activity_rollup_id(
    device_id: &str,
    source_id: &str,
    account_key: &str,
    day: &str,
    kind: ActivityKind,
    entity_key: &str,
) -> String {
    hash_text(&format!(
        "activity_rollup.v1\0{device_id}\0{source_id}\0{account_key}\0{day}\0{}\0{entity_key}",
        kind.as_str()
    ))
}

#[must_use]
pub fn activity_coverage_id(
    device_id: &str,
    source_id: &str,
    day: &str,
    kind: ActivityKind,
) -> String {
    hash_text(&format!(
        "activity_coverage.v1\0{device_id}\0{source_id}\0{day}\0{}",
        kind.as_str()
    ))
}

#[must_use]
pub fn activity_account_key(account_id: Option<&ProviderAccountId>) -> &str {
    account_id.map(|id| id.0.as_str()).unwrap_or("unlinked")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn canonical_ops_collapse_shell_read_and_write_aliases() {
        for native in ["shell", "shell_command", "exec_command", "exec", "run"] {
            assert_eq!(canonical_activity_display_name("codex", native), "shell");
        }
        assert_eq!(
            canonical_activity_display_name("codex", "write_stdin"),
            "write_stdin"
        );
        assert_eq!(
            canonical_activity_display_name("codex", "apply_patch"),
            "apply_patch"
        );
        assert_eq!(
            canonical_activity_display_name("claude_code", "Bash"),
            "shell"
        );
        assert_eq!(canonical_activity_display_name("opencode", "bash"), "shell");
        assert_eq!(
            canonical_activity_display_name("claude_code", "Read"),
            "read"
        );
        assert_eq!(canonical_activity_display_name("opencode", "read"), "read");
        assert_eq!(
            canonical_activity_display_name("grok_build", "run_terminal_command"),
            "shell"
        );
    }

    #[test]
    fn skill_catalog_unknown_round_trips() {
        assert_eq!(SkillCatalog::Unknown.as_str(), "unknown");
        assert_eq!(SkillCatalog::parse("unknown"), Some(SkillCatalog::Unknown));
        assert_eq!(
            serde_json::to_string(&SkillCatalog::Unknown).unwrap(),
            "\"unknown\""
        );
    }

    #[test]
    fn family_round_trips_kebab_case() {
        for name in ACTIVITY_FAMILY_V1 {
            let family = ActivityFamily::parse(name).expect(name);
            assert_eq!(family.as_str(), name);
            let json = serde_json::to_string(&family).unwrap();
            assert_eq!(json, format!("\"{name}\""));
        }
    }

    #[test]
    fn duration_buckets_split_on_documented_edges() {
        assert_eq!(activity_duration_bucket_index(0), 0);
        assert_eq!(activity_duration_bucket_index(9), 0);
        assert_eq!(activity_duration_bucket_index(10), 1);
        assert_eq!(activity_duration_bucket_index(99), 1);
        assert_eq!(activity_duration_bucket_index(100), 2);
        assert_eq!(activity_duration_bucket_index(999), 2);
        assert_eq!(activity_duration_bucket_index(1_000), 3);
        assert_eq!(activity_duration_bucket_index(9_999), 3);
        assert_eq!(activity_duration_bucket_index(10_000), 4);
        assert_eq!(activity_duration_bucket_index(59_999), 4);
        assert_eq!(activity_duration_bucket_index(60_000), 5);
        assert_eq!(activity_duration_bucket_index(599_999), 5);
        assert_eq!(activity_duration_bucket_index(600_000), 6);
        assert_eq!(activity_duration_bucket_index(3_599_999), 6);
        assert_eq!(activity_duration_bucket_index(3_600_000), 7);
        assert_eq!(activity_duration_bucket_index(u64::MAX), 7);
    }

    #[test]
    fn percentile_reports_bucket_upper_edge() {
        let mut buckets = [0u64; 8];
        buckets[0] = 1;
        buckets[2] = 1;
        buckets[4] = 1;
        assert_eq!(
            activity_duration_percentile_ms(&buckets, 3, 0.5),
            Some(1_000)
        );
        assert_eq!(
            activity_duration_percentile_ms(&buckets, 3, 0.9),
            Some(60_000)
        );
        assert_eq!(activity_duration_percentile_ms(&buckets, 0, 0.5), None);
    }

    #[test]
    fn entity_key_uses_nul_for_mcp_and_plugin_skills() {
        assert_eq!(
            activity_entity_key(ActivityKind::Tool, "Bash", None, None, None),
            "Bash"
        );
        assert_eq!(
            activity_entity_key(
                ActivityKind::Mcp,
                "example_tool",
                Some("example_server"),
                Some("example_tool"),
                None
            ),
            "example_server\u{0000}example_tool"
        );
        assert_eq!(
            activity_entity_key(
                ActivityKind::Skill,
                "example-skill",
                None,
                None,
                Some("acme-plugin")
            ),
            "acme-plugin\u{0000}example-skill"
        );
        assert_eq!(
            activity_entity_key(ActivityKind::Skill, "example-skill", None, None, None),
            "example-skill"
        );
    }

    #[test]
    fn day_key_is_utc_naive_date() {
        let ts = Utc.with_ymd_and_hms(2026, 1, 1, 23, 59, 59).unwrap();
        assert_eq!(activity_day_key(ts), "2026-01-01");
    }

    #[test]
    fn invocation_ids_are_stable_and_delimiter_safe() {
        let a = activity_invocation_id(&["codex", "thread", "item"]);
        let b = activity_invocation_id(&["codex", "thread", "item"]);
        let c = activity_invocation_id(&["codex", "threaditem"]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn sanitize_strips_controls_truncates_and_drops_incomplete_mcp() {
        assert_eq!(sanitize_activity_identifier("exec\npatch"), "execpatch");
        assert_eq!(
            sanitize_activity_identifier(&"a".repeat(300)).len(),
            ACTIVITY_IDENTIFIER_MAX_BYTES
        );
        assert!(sanitize_activity_invocation(ActivityInvocationV1 {
            schema_version: ACTIVITY_INVOCATION_SCHEMA_VERSION.to_string(),
            invocation_id: "id".to_string(),
            provider: "codex".to_string(),
            source_id: crate::SourceId("src".to_string()),
            provider_account_id: None,
            source_file_path_hash: "hash".to_string(),
            observed_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            kind: ActivityKind::Mcp,
            display_name: "mcp".to_string(),
            family: ActivityFamily::Mcp,
            mcp_server: Some("server".to_string()),
            mcp_tool: None,
            plugin: None,
            skill_catalog: None,
            outcome: ActivityOutcome::Unknown,
            duration_ms: None,
            duration_kind: None,
            evidence: "codex-native-items".to_string(),
            parser_revision: ACTIVITY_PARSER_REVISION.to_string(),
        })
        .is_none());
        let kept = sanitize_activity_invocation(ActivityInvocationV1 {
            schema_version: ACTIVITY_INVOCATION_SCHEMA_VERSION.to_string(),
            invocation_id: "id".to_string(),
            provider: "codex".to_string(),
            source_id: crate::SourceId("src".to_string()),
            provider_account_id: None,
            source_file_path_hash: "hash".to_string(),
            observed_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            kind: ActivityKind::Tool,
            display_name: "exec\u{0007}".to_string(),
            family: ActivityFamily::Shell,
            mcp_server: None,
            mcp_tool: None,
            plugin: None,
            skill_catalog: None,
            outcome: ActivityOutcome::Unknown,
            duration_ms: None,
            duration_kind: None,
            evidence: "codex-native-items".to_string(),
            parser_revision: ACTIVITY_PARSER_REVISION.to_string(),
        })
        .expect("kept");
        assert_eq!(kept.display_name, "exec");
    }

    #[test]
    fn coverage_honesty_prefers_partial_over_complete() {
        assert!(
            ActivityCoverageLevel::Partial.honesty_rank()
                > ActivityCoverageLevel::Complete.honesty_rank()
        );
        assert!(
            ActivityCoverageLevel::Unavailable.honesty_rank()
                > ActivityCoverageLevel::Partial.honesty_rank()
        );
    }

    #[test]
    fn coalesce_iso_day_ranges_merges_contiguous_days_and_keeps_gaps() {
        assert_eq!(
            coalesce_iso_day_ranges(["2026-01-01", "2026-01-02", "2026-01-03"]),
            vec![("2026-01-01".to_string(), "2026-01-03".to_string())]
        );
        assert_eq!(
            coalesce_iso_day_ranges(["2026-01-01", "2026-01-03", "2026-01-02", "2026-01-10"]),
            vec![
                ("2026-01-01".to_string(), "2026-01-03".to_string()),
                ("2026-01-10".to_string(), "2026-01-10".to_string()),
            ]
        );
        assert!(coalesce_iso_day_ranges(Vec::<&str>::new()).is_empty());
    }

    #[test]
    fn coverage_payloads_without_day_end_treat_the_start_as_the_end() {
        let row = ActivityCoverageV1 {
            schema_version: ACTIVITY_COVERAGE_SCHEMA_VERSION.to_string(),
            coverage_id: "id".to_string(),
            device_id: "device".to_string(),
            source_id: crate::SourceId("src".to_string()),
            provider: "codex".to_string(),
            day: "2026-01-01".to_string(),
            day_end: String::new(),
            kind: ActivityKind::Tool,
            level: ActivityCoverageLevel::Partial,
            evidence: "legacy".to_string(),
            parser_revision: ACTIVITY_PARSER_REVISION.to_string(),
        };
        assert_eq!(row.effective_day_end(), "2026-01-01");
    }
}
