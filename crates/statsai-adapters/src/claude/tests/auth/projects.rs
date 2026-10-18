use super::*;

#[test]
fn claude_project_auth_override_and_dependencies_include_repository_ancestors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("workspace");
    let repository = dir.path().join("workspace");
    let nested_project = repository.join("crates").join("app");
    let repository_settings_root = repository.join(".claude");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(repository.join(".git")).expect("git directory");
    std::fs::create_dir_all(&nested_project).expect("nested project");
    std::fs::create_dir_all(&repository_settings_root).expect("project settings directory");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({
            "version": 1,
            "originalPath": nested_project,
            "entries": []
        })
        .to_string(),
    )
    .expect("session index");
    std::fs::write(
        repository_settings_root.join("settings.json"),
        serde_json::json!({
            "env": {"ANTHROPIC_API_KEY": "repository-api-key"}
        })
        .to_string(),
    )
    .expect("repository settings");

    let dependencies = claude_auth_dependency_paths(&root, &LocationOrigin::Configured);

    assert_eq!(
        claude_source_settings_have_auth_override(&root, &root),
        Some(true)
    );
    assert!(dependencies.contains(&repository_settings_root));
}

#[test]
fn claude_cached_profile_is_not_used_when_project_settings_select_api_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("workspace");
    let workspace = dir.path().join("workspace");
    let transcript = project_store.join("session.jsonl");
    let settings_path = workspace.join(".claude").join("settings.local.json");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(workspace.join(".claude")).expect("project settings directory");
    std::fs::write(&transcript, "{}\n").expect("transcript");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({
            "version": 1,
            "originalPath": workspace,
            "entries": [{
                "sessionId": "session",
                "fullPath": transcript,
                "projectPath": workspace
            }]
        })
        .to_string(),
    )
    .expect("session index");
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "env": {"ANTHROPIC_API_KEY": "project-api-key"}
        })
        .to_string(),
    )
    .expect("project settings");
    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&settings_path),
        }
    );
}

#[test]
fn claude_project_index_override_is_detected_without_transcript_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("workspace");
    let workspace = dir.path().join("workspace");
    let settings_path = workspace.join(".claude").join("settings.json");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(workspace.join(".claude")).expect("project settings directory");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({
            "version": 1,
            "originalPath": workspace,
            "entries": []
        })
        .to_string(),
    )
    .expect("session index");
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "env": {"CLAUDE_CODE_USE_VERTEX": "1"}
        })
        .to_string(),
    )
    .expect("project settings");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&settings_path),
        }
    );
}

#[test]
fn claude_project_without_session_index_still_uses_cached_profile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("unknown-workspace");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(
        project_store.join("session.jsonl"),
        format!("{}\n", serde_json::json!({"cwd": workspace})),
    )
    .expect("transcript");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    let VerifiedSourceObservation::Inferred {
        identity: state, ..
    } = observation
    else {
        panic!("missing optional session index must not disable account detection");
    };
    assert_eq!(state.provider_user_id.as_deref(), Some("cached-account"));
    assert_eq!(state.email.as_deref(), Some("cached@example.com"));
}

#[test]
fn claude_project_without_session_index_checks_recovered_project_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("unknown-workspace");
    let workspace = dir.path().join("workspace");
    let settings_path = workspace.join(".claude").join("settings.json");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(settings_path.parent().expect("settings parent"))
        .expect("project settings directory");
    std::fs::write(
        project_store.join("session.jsonl"),
        format!("{}\n", serde_json::json!({"cwd": workspace})),
    )
    .expect("transcript");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "env": {"ANTHROPIC_API_KEY": "project-api-key"}
        })
        .to_string(),
    )
    .expect("project settings");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&settings_path),
        }
    );
}

#[test]
fn claude_project_without_index_or_project_metadata_blocks_attribution() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("unknown-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::write(project_store.join("session.jsonl"), "{}\n").expect("transcript");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        }
    );
}

#[test]
fn claude_missing_index_checks_every_nested_transcript_project_scope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("mixed-workspaces");
    let oauth_workspace = dir.path().join("oauth-workspace");
    let api_workspace = dir.path().join("api-workspace");
    let api_settings = api_workspace.join(".claude").join("settings.json");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(api_settings.parent().expect("settings parent"))
        .expect("api project settings");
    std::fs::write(
        project_store.join("a-oauth-session.jsonl"),
        format!("{}\n", serde_json::json!({"cwd": oauth_workspace})),
    )
    .expect("oauth transcript");
    std::fs::write(
        project_store.join("z-api-session.jsonl"),
        format!("{}\n", serde_json::json!({"cwd": api_workspace})),
    )
    .expect("api parent transcript");
    let subagent = project_store
        .join("z-api-session")
        .join("subagents")
        .join("agent-a.jsonl");
    std::fs::create_dir_all(subagent.parent().expect("subagent parent"))
        .expect("subagent directory");
    std::fs::write(&subagent, "{}\n").expect("api subagent transcript");
    std::fs::write(
        &api_settings,
        serde_json::json!({"env": {"ANTHROPIC_API_KEY": "project-api-key"}}).to_string(),
    )
    .expect("api project settings");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("cached profile");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&api_settings),
        }
    );
}

#[test]
fn claude_project_local_settings_clear_user_auth_override_for_only_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("workspace");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(workspace.join(".claude")).expect("project settings directory");
    std::fs::write(
        root.join("settings.json"),
        serde_json::json!({
            "env": {"ANTHROPIC_AUTH_TOKEN": "stale-user-token"}
        })
        .to_string(),
    )
    .expect("user settings");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({
            "version": 1,
            "originalPath": workspace,
            "entries": []
        })
        .to_string(),
    )
    .expect("session index");
    std::fs::write(
        workspace.join(".claude").join("settings.local.json"),
        serde_json::json!({
            "env": {"ANTHROPIC_AUTH_TOKEN": ""}
        })
        .to_string(),
    )
    .expect("project local settings");

    assert_eq!(
        claude_source_settings_have_auth_override(&root, &root),
        Some(false)
    );
}

#[test]
fn claude_session_state_stub_does_not_veto_a_resolvable_project_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("unknown-workspace");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(
        project_store.join("session.jsonl"),
        format!("{}\n", serde_json::json!({"cwd": workspace})),
    )
    .expect("transcript");
    // A session abandoned before its first message: interface state only, so no
    // `cwd`, no messages, and no usage. It must not veto the store its siblings
    // already identify.
    std::fs::write(
        project_store.join("abandoned-session.jsonl"),
        [
            serde_json::json!({"type": "last-prompt", "value": ""}).to_string(),
            serde_json::json!({"type": "mode", "value": "default"}).to_string(),
            serde_json::json!({"type": "permission-mode", "value": "default"}).to_string(),
        ]
        .join("\n"),
    )
    .expect("session state stub");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    let VerifiedSourceObservation::Inferred {
        identity: state, ..
    } = observation
    else {
        panic!("a session-state stub must not block a store its siblings identify");
    };
    assert_eq!(state.provider_user_id.as_deref(), Some("cached-account"));
}

#[test]
fn claude_session_state_stub_still_checks_recovered_project_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("unknown-workspace");
    let workspace = dir.path().join("workspace");
    let settings_path = workspace.join(".claude").join("settings.json");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(settings_path.parent().expect("settings parent"))
        .expect("project settings directory");
    std::fs::write(
        project_store.join("session.jsonl"),
        format!("{}\n", serde_json::json!({"cwd": workspace})),
    )
    .expect("transcript");
    std::fs::write(
        project_store.join("abandoned-session.jsonl"),
        format!("{}\n", serde_json::json!({"type": "permission-mode"})),
    )
    .expect("session state stub");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(
        &settings_path,
        serde_json::json!({"env": {"ANTHROPIC_API_KEY": "project-api-key"}}).to_string(),
    )
    .expect("project settings");

    // Skipping the stub must not skip the store: the project it does resolve to
    // is still checked for a credential that overrides subscription OAuth.
    assert_eq!(
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None),
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&settings_path),
        }
    );
}

#[test]
fn claude_unread_transcript_records_still_block_attribution() {
    // Each of these leaves the scan unable to say the file names no project, so
    // the store must keep failing closed even though a sibling resolves.
    let oversized_record = format!(
        "{{\"cwd\":\"{}\"}}\n",
        "x".repeat(crate::MAX_JSONL_RECORD_BYTES)
    );
    let late_metadata = std::iter::repeat_n(
        serde_json::json!({"type": "mode"}).to_string(),
        CLAUDE_PROJECT_METADATA_SCAN_LINES + 4,
    )
    .collect::<Vec<_>>()
    .join("\n");
    for (label, contents) in [
        ("unparsable record", "{not valid json\n".to_string()),
        ("record past the scan window", late_metadata),
        ("record too large to read", oversized_record),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("claude-config");
        let project_store = root.join("projects").join("unknown-workspace");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&project_store).expect("project store");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(
            project_store.join("session.jsonl"),
            format!("{}\n", serde_json::json!({"cwd": workspace})),
        )
        .expect("resolvable sibling transcript");
        std::fs::write(project_store.join("unreadable.jsonl"), contents).expect("transcript");
        std::fs::write(
            root.join(".claude.json"),
            serde_json::json!({
                "oauthAccount": {
                    "accountUuid": "cached-account",
                    "emailAddress": "cached@example.com"
                }
            })
            .to_string(),
        )
        .expect("claude profile");

        assert_eq!(
            claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None),
            VerifiedSourceObservation::AttributionBlocked {
                blocked_since: None,
            },
            "expected a transcript with a {label} to keep failing closed"
        );
    }
}

#[test]
fn claude_session_state_stub_without_a_resolvable_sibling_blocks_attribution() {
    // Nothing else establishes the store's scope, so its settings were never
    // checked and the stub cannot be dismissed.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("unknown-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::write(
        project_store.join("abandoned-session.jsonl"),
        format!("{}\n", serde_json::json!({"type": "permission-mode"})),
    )
    .expect("session state stub");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");

    assert_eq!(
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None),
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        }
    );
}

#[test]
fn project_path_memo_serves_repeat_reads_and_expires_with_its_scope() {
    // Three probes derive the same source's project paths per scan, and deriving
    // them reads the project history. The memo has to answer the repeats without
    // touching the filesystem, and has to stop answering once the scope ends, so a
    // project added between daemon passes is still seen.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let projects_root = root.join("projects");
    let project_store = projects_root.join("workspace");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({ "originalPath": workspace.to_string_lossy() }).to_string(),
    )
    .expect("sessions index");

    let first = claude_project_paths_from_session_indexes(&projects_root).expect("first read");
    assert_eq!(first, vec![workspace.clone()]);

    {
        let _memo = ClaudeProjectPathMemo::begin();
        let memoized = claude_project_paths_from_session_indexes(&projects_root);
        assert_eq!(memoized.as_deref(), Some(first.as_slice()));

        // Removing the index would change the answer, so a fresh read cannot
        // return the old one -- only a memo that never looked can.
        std::fs::remove_file(project_store.join("sessions-index.json")).expect("remove index");
        assert_eq!(
            claude_project_paths_from_session_indexes(&projects_root).as_deref(),
            Some(first.as_slice()),
            "a held memo answers without re-reading the filesystem"
        );
    }

    // Scope gone: the same call now reports what is actually on disk. The store has
    // no index and no transcripts left, so it contributes no project path.
    assert_eq!(
        claude_project_paths_from_session_indexes(&projects_root),
        Some(Vec::new()),
        "the memo does not outlive its scope"
    );
}

#[test]
fn nested_project_path_memo_scopes_do_not_retire_the_outer_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects_root = dir.path().join("claude-config").join("projects");
    std::fs::create_dir_all(&projects_root).expect("projects root");

    let outer = ClaudeProjectPathMemo::begin();
    assert_eq!(
        claude_project_paths_from_session_indexes(&projects_root),
        Some(Vec::new())
    );
    {
        let _inner = ClaudeProjectPathMemo::begin();
        assert_eq!(
            claude_project_paths_from_session_indexes(&projects_root),
            Some(Vec::new())
        );
    }
    // The inner scope ending must restore the outer memo, not clear caching entirely.
    let project_store = projects_root.join("workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({ "originalPath": dir.path().join("workspace").to_string_lossy() })
            .to_string(),
    )
    .expect("sessions index");
    assert_eq!(
        claude_project_paths_from_session_indexes(&projects_root),
        Some(Vec::new()),
        "the outer memo still answers after the inner scope ended"
    );
    drop(outer);
    assert_eq!(
        claude_project_paths_from_session_indexes(&projects_root).map(|paths| paths.len()),
        Some(1),
        "and the filesystem is read again once every scope is gone"
    );
}

#[test]
fn project_path_memo_does_not_remember_a_read_it_could_not_complete() {
    // `None` means the directory could not be read conclusively, and the probes
    // fail closed on it -- they suppress automatic attribution rather than assume a
    // source is clear. Remembering it would let one unreadable moment settle the
    // question for every probe left in the pass.
    let dir = tempfile::tempdir().expect("tempdir");
    let projects_root = dir.path().join("projects");
    // A file where a directory belongs: `read_dir` fails with something other than
    // NotFound, which is what the derivation reports as inconclusive.
    std::fs::write(&projects_root, "not a directory").expect("blocking file");

    let _memo = ClaudeProjectPathMemo::begin();
    assert_eq!(
        claude_project_paths_from_session_indexes(&projects_root),
        None,
        "an unreadable projects root is inconclusive"
    );

    // Same scope, readable now. A memo that had remembered the failure would still
    // be answering None.
    std::fs::remove_file(&projects_root).expect("remove blocking file");
    let project_store = projects_root.join("workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({ "originalPath": dir.path().join("workspace").to_string_lossy() })
            .to_string(),
    )
    .expect("sessions index");

    assert_eq!(
        claude_project_paths_from_session_indexes(&projects_root).map(|paths| paths.len()),
        Some(1),
        "the next probe reads for itself rather than inheriting the failure"
    );
}

/// A Claude config root with a cached OAuth profile and one session index per
/// workspace, each workspace holding a harmless `.claude/settings.local.json`.
/// Returns the root and each workspace's settings file.
#[cfg(unix)]
fn claude_root_with_project_settings(dir: &Path, workspaces: &[&str]) -> (PathBuf, Vec<PathBuf>) {
    let root = dir.join("claude-config");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com",
                "profileFetchedAt": 1_786_104_000_000_i64,
                "organizationType": "claude_max",
                "organizationRateLimitTier": "default_claude_max_5x"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    let mut settings_paths = Vec::new();
    for name in workspaces {
        let project_store = root.join("projects").join(name);
        let workspace = dir.join(name);
        let settings_path = workspace.join(".claude").join("settings.local.json");
        std::fs::create_dir_all(&project_store).expect("project store");
        std::fs::create_dir_all(workspace.join(".claude")).expect("project settings directory");
        std::fs::write(
            project_store.join("sessions-index.json"),
            serde_json::json!({
                "version": 1,
                "originalPath": workspace,
                "entries": []
            })
            .to_string(),
        )
        .expect("session index");
        std::fs::write(&settings_path, r#"{"permissions":{"allow":[]}}"#)
            .expect("project settings");
        settings_paths.push(settings_path);
    }
    (root, settings_paths)
}

#[cfg(unix)]
#[test]
fn claude_unreadable_project_settings_do_not_block_the_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, settings_paths) =
        claude_root_with_project_settings(dir.path(), &["denied-workspace", "readable-workspace"]);
    let _deny = test_support::DenySettingsRead::new(&settings_paths[0]);

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    let VerifiedSourceObservation::Inferred {
        identity: state, ..
    } = observation
    else {
        panic!("a project settings file the scan may not open must not block the source: {observation:?}");
    };
    assert_eq!(state.provider_user_id.as_deref(), Some("cached-account"));
    assert!(claude_denied_project_settings_noted());
    // Plan evidence goes through the same settings check.
    let source = SourceLocation::local_adapter(
        crate::CLAUDE_CODE_PROVIDER,
        "claude-code-local-jsonl",
        "0",
        &root,
        LocationOrigin::Configured,
    );
    let evidence = collect_claude_account_evidence_with_probe_context(
        &source,
        &root,
        &LocationOrigin::Configured,
        None,
    );
    assert_eq!(evidence.plan_observations.len(), 1);
    assert_eq!(evidence.plan_observations[0].plan_name, "Max 5x");
}

#[cfg(unix)]
#[test]
fn claude_project_settings_denied_by_file_modes_still_block_the_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, settings_paths) =
        claude_root_with_project_settings(dir.path(), &["denied-workspace", "readable-workspace"]);
    // EACCES says the file is there, perhaps another user's with a credential.
    let _deny =
        test_support::DenySettingsRead::with_errno(&settings_paths[0], test_support::EACCES);

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None
        }
    );
}

#[cfg(unix)]
#[test]
fn claude_readable_override_still_blocks_beside_unreadable_project_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, settings_paths) = claude_root_with_project_settings(dir.path(), &["workspace"]);
    let shared_settings = settings_paths[0].with_file_name("settings.json");
    std::fs::write(
        &shared_settings,
        serde_json::json!({
            "env": {"ANTHROPIC_API_KEY": "project-api-key"}
        })
        .to_string(),
    )
    .expect("project settings");
    let _deny = test_support::DenySettingsRead::new(&settings_paths[0]);

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&shared_settings),
        }
    );
}

#[cfg(unix)]
#[test]
fn claude_unreadable_user_settings_still_block_the_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, _) = claude_root_with_project_settings(dir.path(), &["workspace"]);
    let user_settings = root.join("settings.json");
    std::fs::write(&user_settings, r#"{"theme":"dark"}"#).expect("user settings");
    let _deny = test_support::DenySettingsRead::new(&user_settings);

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None
        }
    );
}

#[cfg(unix)]
fn api_key_settings() -> String {
    serde_json::json!({
        "env": {"ANTHROPIC_API_KEY": "project-api-key"}
    })
    .to_string()
}

#[cfg(unix)]
#[test]
fn claude_override_after_unreadable_settings_in_the_same_project_still_blocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, settings_paths) = claude_root_with_project_settings(dir.path(), &["workspace"]);
    let local_settings = &settings_paths[0];
    let shared_settings = local_settings.with_file_name("settings.json");
    std::fs::write(&shared_settings, r#"{"permissions":{"allow":[]}}"#).expect("project settings");
    std::fs::write(local_settings, api_key_settings()).expect("project settings");
    // settings.json is applied first, so the override comes after the denial.
    let _deny = test_support::DenySettingsRead::new(&shared_settings);

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(local_settings),
        }
    );
}

#[cfg(unix)]
#[test]
fn claude_nested_project_override_still_blocks_beside_unreadable_repository_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, settings_paths) = claude_root_with_project_settings(dir.path(), &["workspace"]);
    let repository = dir.path().join("workspace");
    let nested_project = repository.join("crates").join("app");
    let nested_settings = nested_project.join(".claude").join("settings.json");
    std::fs::create_dir_all(repository.join(".git")).expect("git directory");
    std::fs::create_dir_all(nested_settings.parent().expect("parent"))
        .expect("nested settings directory");
    std::fs::write(&nested_settings, api_key_settings()).expect("nested settings");
    std::fs::write(
        root.join("projects")
            .join("workspace")
            .join("sessions-index.json"),
        serde_json::json!({
            "version": 1,
            "originalPath": nested_project,
            "entries": []
        })
        .to_string(),
    )
    .expect("session index");
    // The repository root's settings are applied before the nested project's.
    let _deny = test_support::DenySettingsRead::new(&settings_paths[0]);

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&nested_settings),
        }
    );
}

#[cfg(unix)]
#[test]
fn claude_other_projects_override_still_blocks_beside_unreadable_project_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, settings_paths) =
        claude_root_with_project_settings(dir.path(), &["denied-workspace", "override-workspace"]);
    std::fs::write(&settings_paths[1], api_key_settings()).expect("project settings");
    let _deny = test_support::DenySettingsRead::new(&settings_paths[0]);

    // Projects are checked in hash-map order, which changes from probe to
    // probe, so repeat until the denied project has very likely come first.
    for _ in 0..16 {
        let observation =
            claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

        assert_eq!(
            observation,
            VerifiedSourceObservation::AttributionBlocked {
                blocked_since: file_modified_at(&settings_paths[1]),
            }
        );
    }
}

#[cfg(unix)]
#[test]
fn claude_unreadable_project_settings_keep_the_sources_verified_assignment() {
    use statsai_store::{reconcile_verified_source_state, verified_source_observation_hash, Store};

    let dir = tempfile::tempdir().expect("tempdir");
    let (root, settings_paths) =
        claude_root_with_project_settings(dir.path(), &["denied-workspace", "readable-workspace"]);
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        crate::CLAUDE_CODE_PROVIDER,
        "claude-code-local-jsonl",
        "0",
        &root,
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let reconcile = |source: &mut SourceLocation| {
        let observation =
            claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);
        let hash = verified_source_observation_hash(&observation).expect("observation hash");
        reconcile_verified_source_state(&store, source, &observation, hash).expect("reconcile");
        store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments")
    };

    let assignments = reconcile(&mut source);
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].ended_at, None);

    // The system stops letting scans into one project's folder.
    let _deny = test_support::DenySettingsRead::new(&settings_paths[0]);
    let after_denial = reconcile(&mut source);

    assert_eq!(after_denial, assignments);
}

#[test]
fn privacy_protected_folders_are_the_ones_a_denial_covers_whole() {
    let home = Path::new("/Users/someone");
    for (path, folder) in [
        (
            "/Users/someone/Documents/app/.claude",
            Some("/Users/someone/Documents"),
        ),
        (
            "/Users/someone/Downloads/app/.claude",
            Some("/Users/someone/Downloads"),
        ),
        ("/Users/someone/Desktop/app", Some("/Users/someone/Desktop")),
        (
            "/Users/someone/Library/Mobile Documents/com~apple~CloudDocs/app",
            Some("/Users/someone/Library/Mobile Documents"),
        ),
        ("/Volumes/External/app/.claude", Some("/Volumes/External")),
        ("/Users/someone/work/app/.claude", None),
        ("/Users/someone/Library/Caches/app", None),
        ("/Users/someone/Documents", Some("/Users/someone/Documents")),
        ("/Users/someone2/Documents/app", None),
        ("/opt/work/app", None),
    ] {
        assert_eq!(
            privacy_protected_folder(Path::new(path), Some(home)),
            folder.map(PathBuf::from),
            "{path}"
        );
    }
}

/// A Claude config root whose session indexes name `projects`, which need not
/// exist: opening their settings is what the tests watch.
#[cfg(unix)]
fn claude_root_naming_projects(dir: &Path, projects: &[PathBuf]) -> PathBuf {
    let root = dir.join("claude-config");
    for (index, project) in projects.iter().enumerate() {
        let project_store = root.join("projects").join(format!("project-{index}"));
        std::fs::create_dir_all(&project_store).expect("project store");
        std::fs::write(
            project_store.join("sessions-index.json"),
            serde_json::json!({"version": 1, "originalPath": project, "entries": []}).to_string(),
        )
        .expect("session index");
    }
    root
}

#[cfg(unix)]
#[test]
fn a_denied_protected_folder_is_asked_about_once_per_probe() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Not under the home directory, so the volume is the protected folder and
    // the repository search stops at its root.
    let volume = PathBuf::from("/Volumes/statsai-test-denied-volume");
    let projects = ["first", "second", "third"].map(|name| volume.join(name));
    let root = claude_root_naming_projects(dir.path(), &projects);
    let _denials = projects
        .iter()
        .flat_map(|project| claude_settings_paths(&project.join(".claude")))
        .map(|path| test_support::DenySettingsRead::new(&path))
        .collect::<Vec<_>>();
    test_support::take_opened_settings();

    let probe = claude_source_settings_auth_override(&root, &root);

    assert_eq!(probe, Some(ClaudeAuthOverrideProbe::Clear));
    let opened_on_volume = test_support::take_opened_settings()
        .into_iter()
        .filter(|path| path.starts_with(&volume))
        .count();
    assert_eq!(
        opened_on_volume, 1,
        "the first denial stands for the whole volume"
    );
    assert!(claude_denied_project_settings_folders().contains(&volume));
}

#[cfg(unix)]
#[test]
fn a_denied_project_outside_protected_folders_does_not_excuse_the_others() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, settings_paths) =
        claude_root_with_project_settings(dir.path(), &["denied-workspace", "other-workspace"]);
    let _deny = test_support::DenySettingsRead::new(&settings_paths[0]);
    // The other project's settings exist and are another user's: only a
    // protected folder's denial may stand for its neighbours.
    let _other =
        test_support::DenySettingsRead::with_errno(&settings_paths[1], test_support::EACCES);

    assert_eq!(claude_source_settings_auth_override(&root, &root), None);
}

#[cfg(unix)]
#[test]
fn a_denied_project_outside_protected_folders_is_reported_by_its_folder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, settings_paths) =
        claude_root_with_project_settings(dir.path(), &["denied-workspace", "other-workspace"]);
    let _deny = test_support::DenySettingsRead::new(&settings_paths[0]);
    test_support::take_opened_settings();

    assert_eq!(
        claude_source_settings_auth_override(&root, &root),
        Some(ClaudeAuthOverrideProbe::Clear)
    );
    assert!(test_support::take_opened_settings().contains(&settings_paths[1]));
    assert!(claude_denied_project_settings_folders().contains(&dir.path().join("denied-workspace")));
}

#[cfg(unix)]
#[test]
fn deferred_project_settings_are_neither_opened_nor_answered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, settings_paths) =
        claude_root_with_project_settings(dir.path(), &["first-workspace", "second-workspace"]);
    let source = SourceLocation::local_adapter(
        crate::CLAUDE_CODE_PROVIDER,
        "claude-code-local-jsonl",
        "0",
        &root,
        LocationOrigin::Configured,
    );
    let adapter = ClaudeCodeAdapter;
    test_support::take_opened_settings();

    {
        let _deferral = ClaudeProjectSettingsDeferral::begin();
        assert!(matches!(
            adapter
                .probe_verified_source_state(&source)
                .expect("deferred probe"),
            VerifiedSourceObservation::Unavailable
        ));
        let evidence = adapter
            .collect_account_evidence(&source, &[])
            .expect("deferred evidence");
        assert!(evidence.plan_observations.is_empty());
        let opened = test_support::take_opened_settings();
        assert!(
            settings_paths.iter().all(|path| !opened.contains(path)),
            "{opened:?}"
        );
    }

    // The next pass that is not deferred answers as before.
    assert!(matches!(
        adapter.probe_verified_source_state(&source).expect("probe"),
        VerifiedSourceObservation::Inferred { .. }
    ));
    let evidence = adapter
        .collect_account_evidence(&source, &[])
        .expect("evidence");
    assert_eq!(evidence.plan_observations.len(), 1);
    assert!(test_support::take_opened_settings().contains(&settings_paths[0]));
}

#[cfg(unix)]
#[test]
fn account_dependencies_leave_out_every_project_folder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, settings_paths) =
        claude_root_with_project_settings(dir.path(), &["first-workspace", "second-workspace"]);
    let source = SourceLocation::local_adapter(
        crate::CLAUDE_CODE_PROVIDER,
        "claude-code-local-jsonl",
        "0",
        &root,
        LocationOrigin::Configured,
    );
    let adapter = ClaudeCodeAdapter;

    let all = adapter.verification_dependency_paths(&source);
    let account = adapter.account_verification_dependency_paths(&source);

    let project_settings_roots = settings_paths
        .iter()
        .map(|path| path.parent().expect("settings root").to_path_buf())
        .collect::<Vec<_>>();
    assert!(project_settings_roots.iter().all(|root| all.contains(root)));
    assert!(account
        .iter()
        .all(|path| !path.starts_with(dir.path().join("first-workspace"))
            && !path.starts_with(dir.path().join("second-workspace"))));
    assert!(account.iter().all(|path| all.contains(path)));
    assert!(account.contains(&root.join(".claude.json")));
}

#[test]
fn a_source_with_no_project_is_answered_while_deferred() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(root.join("projects")).expect("projects root");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com",
                "profileFetchedAt": 1_786_104_000_000_i64,
                "organizationType": "claude_max",
                "organizationRateLimitTier": "default_claude_max_5x"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    let source = SourceLocation::local_adapter(
        crate::CLAUDE_CODE_PROVIDER,
        "claude-code-local-jsonl",
        "0",
        &root,
        LocationOrigin::Configured,
    );
    let adapter = ClaudeCodeAdapter;

    // Answering reads no project folder, so nothing is deferred: a source's
    // account and plan are recorded before it has any session.
    let _deferral = ClaudeProjectSettingsDeferral::begin();
    assert!(matches!(
        adapter.probe_verified_source_state(&source).expect("probe"),
        VerifiedSourceObservation::Inferred { .. }
    ));
    let evidence = adapter
        .collect_account_evidence(&source, &[])
        .expect("evidence");
    assert_eq!(evidence.plan_observations.len(), 1);
}
