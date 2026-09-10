use super::*;
use chrono::Utc;
use statsai_core::{ActivityFamily, ActivityKind, ActivityOutcome, SkillCatalog, SourceId};
use std::collections::BTreeSet;

#[test]
fn hashed_ids_are_stable() {
    assert_eq!(
        hashed_invocation_id(&["codex", "thread", "item"]),
        hashed_invocation_id(&["codex", "thread", "item"])
    );
}

#[test]
fn missing_call_ids_use_ordinal_fallback() {
    let with_id = hashed_invocation_id_or_ordinal(&["codex", "thread", "item"], &["file", "1"]);
    let without_first = hashed_invocation_id_or_ordinal(&["codex", "", "item"], &["file", "1"]);
    let without_second = hashed_invocation_id_or_ordinal(&["codex", "", "item"], &["file", "2"]);
    assert_eq!(with_id, hashed_invocation_id(&["codex", "thread", "item"]));
    assert_ne!(with_id, without_first);
    assert_ne!(without_first, without_second);
}

#[test]
fn family_aliases_cover_documented_names() {
    assert_eq!(
        super::families::family_for_name(super::families::CLAUDE_FAMILY_ALIASES, "Bash"),
        ActivityFamily::Shell
    );
    assert_eq!(
        super::families::family_for_name(super::families::OPENCODE_FAMILY_ALIASES, "read"),
        ActivityFamily::FileRead
    );
    assert_eq!(
        super::families::family_for_name(super::families::GROK_FAMILY_ALIASES, "read_file"),
        ActivityFamily::FileRead
    );
    assert_eq!(
        super::families::family_for_name(super::families::CODEX_LEGACY_FAMILY_ALIASES, "run"),
        ActivityFamily::Shell
    );
    assert_eq!(
        super::families::family_for_name(super::families::CODEX_LEGACY_FAMILY_ALIASES, "exec"),
        ActivityFamily::Shell
    );
}

#[test]
fn jsonc_comment_stripping_keeps_strings() {
    let raw = r#"{
      // comment
      "mcp": { "example": { "type": "local" } },
      "url": "https://example.test/path" /* trailing */
    }"#;
    let stripped = super::opencode::strip_jsonc_comments(raw);
    let value: serde_json::Value = serde_json::from_str(&stripped).expect("jsonc");
    assert!(value["mcp"].get("example").is_some());
    assert_eq!(value["url"], "https://example.test/path");
}

#[test]
fn push_invocation_counts_unknown_tool_names() {
    let mut scan = AdapterScan::default();
    push_invocation(
        &mut scan,
        build_invocation(
            "id".to_string(),
            "codex",
            SourceId("src".to_string()),
            "hash".to_string(),
            Utc::now(),
            ActivityKind::Tool,
            "mystery".to_string(),
            ActivityFamily::Other,
            None,
            None,
            None,
            None,
            ActivityOutcome::Unknown,
            None,
            None,
            "test",
        ),
    );
    assert_eq!(scan.diagnostics.activity_rows, 1);
    assert_eq!(scan.diagnostics.activity_unknown_names, 1);
}

#[test]
fn command_invocation_marks_write_intent_as_file_write_family() {
    let tool = build_invocation(
        "id".to_string(),
        "claude_code",
        SourceId("src".to_string()),
        "hash".to_string(),
        Utc::now(),
        ActivityKind::Tool,
        "shell".to_string(),
        ActivityFamily::Shell,
        None,
        None,
        None,
        None,
        ActivityOutcome::Succeeded,
        Some(12),
        None,
        "test",
    );
    let write = command_invocation_for_tool(&tool, "tee", true);
    assert_eq!(write.kind, ActivityKind::Command);
    assert_eq!(write.family, ActivityFamily::FileWrite);
    assert_eq!(write.display_name, "tee");
    let read = command_invocation_for_tool(&tool, "git", false);
    assert_eq!(read.family, ActivityFamily::Shell);
}

fn fixture_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn names(scan: &AdapterScan, kind: ActivityKind) -> Vec<String> {
    let mut names = scan
        .activity_invocations
        .iter()
        .filter(|row| row.kind == kind)
        .map(|row| row.display_name.clone())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn serialized_invocations_omit_secrets(scan: &AdapterScan) {
    let payload = serde_json::to_string(&scan.activity_invocations).expect("serialize");
    for forbidden in [
        "/fixture/project",
        "cat SKILL.md",
        "ls /fixture/project",
        "toolu_fixture",
        "call_fixture",
        "item_fixture",
        "thread_fixture",
        "tc_fixture",
        "aggregated_output",
    ] {
        assert!(
            !payload.contains(forbidden),
            "activity payload leaked {forbidden}: {payload}"
        );
    }
}

#[test]
fn extracts_codex_native_activity_fixture() {
    let root = fixture_root().join("codex/activity-native");
    let source = SourceLocation::local_adapter(
        crate::CODEX_PROVIDER,
        "test",
        "0",
        &root,
        statsai_core::LocationOrigin::Configured,
    );
    let scan =
        crate::codex::scan_codex_source(&crate::CodexAdapter, &source, &crate::tests::options())
            .expect("scan");
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| row.kind == ActivityKind::Command && row.display_name == "cat"));
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| { row.kind == ActivityKind::Tool && row.model.as_deref() == Some("gpt-5.4") }));
    let example_skills: Vec<_> = scan
        .activity_invocations
        .iter()
        .filter(|row| row.kind == ActivityKind::Skill && row.display_name == "example-skill")
        .collect();
    assert!(!example_skills.is_empty());
    assert!(example_skills
        .iter()
        .all(|row| row.skill_catalog == Some(SkillCatalog::Project)));
    assert!(scan.activity_invocations.iter().any(|row| {
        row.kind == ActivityKind::Mcp && row.mcp_server.as_deref() == Some("example_server")
    }));
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| row.display_name == "apply_patch"
            && row.outcome == statsai_core::ActivityOutcome::Unknown));
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| row.kind == ActivityKind::Tool && row.display_name == "update_plan"));
    assert!(scan
        .activity_invocations
        .iter()
        .all(|row| { !row.observed_at.to_rfc3339().starts_with("1970-01-01") }));
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| row.display_name == "web_search"));
    assert!(scan.activity_coverage.iter().any(|row| row.level
        == statsai_core::ActivityCoverageLevel::Complete
        && row.evidence == "codex-native-items"));
    serialized_invocations_omit_secrets(&scan);
}

#[test]
fn extracts_codex_legacy_activity_without_mixing_native() {
    let root = fixture_root().join("codex/activity-legacy");
    let source = SourceLocation::local_adapter(
        crate::CODEX_PROVIDER,
        "test",
        "0",
        &root,
        statsai_core::LocationOrigin::Configured,
    );
    let scan =
        crate::codex::scan_codex_source(&crate::CodexAdapter, &source, &crate::tests::options())
            .expect("scan");
    assert_eq!(
        names(&scan, ActivityKind::Mcp),
        vec!["example_tool".to_string()]
    );
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| row.display_name == "shell"));
    let apply_patch = scan
        .activity_invocations
        .iter()
        .find(|row| row.display_name == "apply_patch")
        .expect("apply_patch");
    assert_eq!(apply_patch.outcome, ActivityOutcome::Failed);
    let exec = scan
        .activity_invocations
        .iter()
        .find(|row| row.display_name == "shell")
        .expect("shell");
    assert_eq!(exec.outcome, ActivityOutcome::Unknown);
    assert!(scan.activity_coverage.iter().any(|row| {
        row.level == statsai_core::ActivityCoverageLevel::Partial
            && row.evidence == "codex-legacy-response-items"
    }));
    assert!(scan
        .activity_invocations
        .iter()
        .all(|row| row.duration_ms.is_none()));
    serialized_invocations_omit_secrets(&scan);
}

#[test]
fn extracts_codex_class_b_legacy_calls_when_item_completed_is_prose_only() {
    let root = fixture_root().join("codex/activity-class-b");
    let source = SourceLocation::local_adapter(
        crate::CODEX_PROVIDER,
        "test",
        "0",
        &root,
        statsai_core::LocationOrigin::Configured,
    );
    let scan =
        crate::codex::scan_codex_source(&crate::CodexAdapter, &source, &crate::tests::options())
            .expect("scan");
    assert!(
        scan.activity_invocations
            .iter()
            .any(|row| row.display_name == "shell" && row.kind == ActivityKind::Tool),
        "class-B files must count legacy tool calls"
    );
    assert!(scan.activity_invocations.iter().any(|row| {
        row.display_name == "apply_patch" && row.outcome == ActivityOutcome::Succeeded
    }));
    assert!(scan.activity_coverage.iter().any(|row| {
        row.level == statsai_core::ActivityCoverageLevel::Partial
            && row.evidence == "codex-legacy-response-items"
    }));
    assert!(scan
        .activity_invocations
        .iter()
        .all(|row| row.evidence == "codex-legacy-response-items"));
    serialized_invocations_omit_secrets(&scan);
}

#[test]
fn timestamp_from_millis_rejects_non_positive_and_pre_2024() {
    assert!(super::timestamp_from_millis(0).is_none());
    assert!(super::timestamp_from_millis(-1).is_none());
    assert!(super::timestamp_from_millis(1).is_none());
    assert!(super::timestamp_from_millis(1_767_225_600_000).is_some());
}

#[test]
fn extracts_claude_tool_blocks_including_fork_and_subagent() {
    let root = fixture_root().join("claude/activity");
    let source = SourceLocation::local_adapter(
        crate::CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        &root,
        statsai_core::LocationOrigin::Configured,
    );
    let scan = crate::claude::scan_claude_source(
        &crate::ClaudeCodeAdapter,
        &source,
        &crate::tests::options(),
    )
    .expect("scan");
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| row.display_name == "shell"));
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| { row.kind == ActivityKind::Skill && row.display_name == "example-skill" }));
    assert!(scan.activity_invocations.iter().any(|row| {
        row.kind == ActivityKind::Skill && row.plugin.as_deref() == Some("acme-plugin")
    }));
    assert!(scan.activity_invocations.iter().any(|row| {
        row.kind == ActivityKind::Mcp && row.mcp_server.as_deref() == Some("Server_Name")
    }));
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| row.display_name == "glob"));
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| row.kind == ActivityKind::Command && row.display_name == "ls"));
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| row.model.as_deref() == Some("claude-fixture")));
    let bash_ids = scan
        .activity_invocations
        .iter()
        .filter(|row| row.display_name == "shell" && row.kind == ActivityKind::Tool)
        .map(|row| row.invocation_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        bash_ids.len(),
        1,
        "forked duplicate tool_use.id shares one invocation id"
    );
    assert_eq!(
        scan.activity_invocations
            .iter()
            .find(|row| row.display_name == "shell" && row.kind == ActivityKind::Tool)
            .map(|row| row.outcome),
        Some(ActivityOutcome::Succeeded)
    );
    assert_eq!(
        scan.activity_invocations
            .iter()
            .find(|row| row.display_name == "read")
            .map(|row| row.outcome),
        Some(ActivityOutcome::Failed)
    );
    assert!(
        scan.activity_invocations.iter().any(|row| {
            row.kind == ActivityKind::Tool
                && row.display_name == "Skill"
                && row.outcome == ActivityOutcome::Succeeded
        }),
        "paired Skill tool_result without is_error is succeeded"
    );
    assert_eq!(
        scan.activity_invocations
            .iter()
            .find(|row| row.display_name == "grep")
            .map(|row| row.outcome),
        Some(ActivityOutcome::Unknown)
    );
    serialized_invocations_omit_secrets(&scan);
}

#[test]
fn extracts_grok_events_and_chat_fallback() {
    let events_root = fixture_root().join("grok/activity-events");
    let events_source = SourceLocation::local_adapter(
        crate::GROK_BUILD_PROVIDER,
        "test",
        "0",
        &events_root,
        statsai_core::LocationOrigin::Configured,
    );
    let events_scan = crate::grok::scan_grok_build_source(
        &crate::GrokBuildAdapter,
        &events_source,
        &crate::tests::options(),
    )
    .expect("events scan");
    assert!(events_scan
        .activity_invocations
        .iter()
        .any(|row| row.display_name == "read" && row.outcome == ActivityOutcome::Succeeded));
    assert!(events_scan.activity_coverage.iter().any(|row| {
        row.evidence == "grok-events" && row.level == statsai_core::ActivityCoverageLevel::Complete
    }));

    let chat_root = fixture_root().join("grok/activity-chat");
    let chat_source = SourceLocation::local_adapter(
        crate::GROK_BUILD_PROVIDER,
        "test",
        "0",
        &chat_root,
        statsai_core::LocationOrigin::Configured,
    );
    let chat_scan = crate::grok::scan_grok_build_source(
        &crate::GrokBuildAdapter,
        &chat_source,
        &crate::tests::options(),
    )
    .expect("chat scan");
    assert!(chat_scan
        .activity_invocations
        .iter()
        .any(|row| row.display_name == "read"));
    assert!(chat_scan
        .activity_invocations
        .iter()
        .any(|row| row.display_name == "backend_tool_call"));
    assert!(chat_scan.activity_coverage.iter().any(|row| {
        row.evidence == "grok-chat-history"
            && row.level == statsai_core::ActivityCoverageLevel::Partial
    }));
    serialized_invocations_omit_secrets(&events_scan);
    serialized_invocations_omit_secrets(&chat_scan);
}

#[test]
fn extracts_opencode_tool_parts_and_skips_noop_cursor() {
    let root = fixture_root().join("opencode/activity");
    let db_path = root.join("opencode.db");
    let source = SourceLocation::local_adapter(
        crate::OPENCODE_PROVIDER,
        "test",
        "0",
        &root,
        statsai_core::LocationOrigin::Configured,
    );
    let connection = crate::open_sqlite_readonly(&db_path).expect("db");
    let servers = load_opencode_mcp_servers(&root.join("opencode.json"));
    let mut scan = AdapterScan::default();
    let first = extract_opencode_activity(
        &mut scan,
        &connection,
        &source,
        &db_path,
        "device",
        None,
        &servers,
        Utc::now(),
    )
    .expect("first extract");
    assert!(first.rows_returned > 0);
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| row.display_name == "read"));
    assert!(scan
        .activity_invocations
        .iter()
        .any(|row| { row.kind == ActivityKind::Skill && row.display_name == "example-skill" }));
    assert!(scan.activity_invocations.iter().any(|row| {
        row.kind == ActivityKind::Mcp && row.mcp_server.as_deref() == Some("example_server")
    }));
    serialized_invocations_omit_secrets(&scan);

    let mut noop = AdapterScan::default();
    let second = extract_opencode_activity(
        &mut noop,
        &connection,
        &source,
        &db_path,
        "device",
        Some(first.last_time_updated),
        &servers,
        Utc::now(),
    )
    .expect("cursor extract");
    assert_eq!(second.rows_returned, 0);
    assert!(noop.activity_invocations.is_empty());
}
