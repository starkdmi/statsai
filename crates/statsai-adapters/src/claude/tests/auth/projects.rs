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
