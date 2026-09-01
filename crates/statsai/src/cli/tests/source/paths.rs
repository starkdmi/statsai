use super::*;

#[test]
fn configured_claude_projects_path_normalizes_to_config_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(&projects).expect("projects");

    let normalized =
        normalize_configured_source_path("claude_code", &projects).expect("normalized path");

    assert_eq!(
        normalized,
        dir.path().canonicalize().expect("canonical dir")
    );
}

#[test]
fn configured_codex_sessions_path_normalizes_to_codex_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");

    let normalized = normalize_configured_source_path("codex", &sessions).expect("normalized path");

    assert_eq!(
        normalized,
        dir.path().canonicalize().expect("canonical dir")
    );
}

#[test]
fn configured_opencode_db_path_normalizes_to_data_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("opencode.db");
    std::fs::write(&db, "").expect("db");

    let normalized = normalize_configured_source_path("opencode", &db).expect("normalized path");

    assert_eq!(
        normalized,
        dir.path().canonicalize().expect("canonical dir")
    );
}

#[test]
fn configured_grok_sessions_path_normalizes_to_home_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");

    let normalized =
        normalize_configured_source_path("grok-build", &sessions).expect("normalized path");

    assert_eq!(
        normalized,
        dir.path().canonicalize().expect("canonical dir")
    );
}

#[test]
fn persist_source_upserts_into_store() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-preview-source"),
        LocationOrigin::Configured,
    );

    persist_source_after_preview(&store, &source).expect("persist");

    assert_eq!(store.list_sources().expect("sources").len(), 1);
}

#[test]
fn configured_source_overrides_discovered_source_for_same_path() {
    let discovered = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-merge"),
        LocationOrigin::Default,
    );
    let configured = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-merge"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![discovered],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let sources = scan_sources_for_adapter(&adapter, &[configured]);

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].location_origin, LocationOrigin::Configured);
}

#[test]
fn disabled_configured_source_suppresses_matching_discovered_source() {
    let matching = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-disabled"),
        LocationOrigin::Default,
    );
    let unrelated = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-enabled"),
        LocationOrigin::Default,
    );
    let mut disabled = SourceLocation::local_adapter(
        "claude",
        "test",
        "0",
        Path::new("/tmp/claude-disabled"),
        LocationOrigin::Configured,
    );
    disabled.enabled = false;
    let adapter = TestAdapter {
        provider: "claude_code",
        discovered: vec![matching, unrelated.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let sources = scan_sources_for_adapter(&adapter, &[disabled]);

    assert_eq!(sources, vec![unrelated]);
}

#[test]
fn configured_parent_source_suppresses_discovered_child_source() {
    let discovered = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/statsai-claude/projects"),
        LocationOrigin::Default,
    );
    let configured = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/statsai-claude"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "claude_code",
        discovered: vec![discovered],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let sources = scan_sources_for_adapter(&adapter, &[configured]);

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].location_origin, LocationOrigin::Configured);
    assert_eq!(
        sources[0].path_label.as_deref(),
        Some("/tmp/statsai-claude")
    );
}

#[test]
fn codex_nested_source_is_not_shadowed_by_parent_source() {
    let discovered_parent = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex"),
        LocationOrigin::Env,
    );
    let configured_child = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex/.codex"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![discovered_parent.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let mut sources = scan_sources_for_adapter(&adapter, std::slice::from_ref(&configured_child));
    sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
    assert_eq!(
        sources[1].path_label.as_deref(),
        Some("/tmp/statsai-codex/.codex")
    );
}

#[test]
fn codex_nested_sessions_source_is_shadowed_by_parent_source() {
    let discovered_parent = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex"),
        LocationOrigin::Env,
    );
    let configured_child = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex/sessions"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![discovered_parent.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let sources = scan_sources_for_adapter(&adapter, &[configured_child]);

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
}

#[test]
fn codex_source_under_nested_codex_root_is_not_shadowed_by_parent_source() {
    let discovered_parent = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex"),
        LocationOrigin::Env,
    );
    let configured_child = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex/.codex/sessions"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![discovered_parent.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let mut sources = scan_sources_for_adapter(&adapter, &[configured_child]);
    sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
    assert_eq!(
        sources[1].path_label.as_deref(),
        Some("/tmp/statsai-codex/.codex/sessions")
    );
}

#[test]
fn codex_custom_named_nested_root_is_not_shadowed_by_parent_source() {
    let discovered_parent = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex"),
        LocationOrigin::Env,
    );
    let configured_child = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/statsai-codex/project-codex-home"),
        LocationOrigin::Configured,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![discovered_parent.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let mut sources = scan_sources_for_adapter(&adapter, &[configured_child]);
    sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
    assert_eq!(
        sources[1].path_label.as_deref(),
        Some("/tmp/statsai-codex/project-codex-home")
    );
}

#[test]
fn non_local_sources_are_ignored_for_adapter_scans() {
    let configured_local = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-local"),
        LocationOrigin::Configured,
    );
    let configured_manual = SourceLocation::reported_usage(
        "codex",
        SourceKind::Manual,
        "reported-usage-summary",
        "0",
        "manual-note",
        None,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: Vec::new(),
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    let sources =
        scan_sources_for_adapter(&adapter, &[configured_local.clone(), configured_manual]);

    assert_eq!(sources, vec![configured_local]);
}
