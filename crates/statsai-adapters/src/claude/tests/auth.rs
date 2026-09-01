use super::*;

#[test]
fn claude_cached_profile_infers_account_when_durable_settings_are_clear() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "file-only-account",
                "emailAddress": "file-only@example.com",
                "profileFetchedAt": 1_786_104_000_000_i64
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    let settings_path = root.join("settings.json");
    std::fs::write(&settings_path, r#"{"theme":"dark"}"#).expect("harmless settings");
    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    let VerifiedSourceObservation::Inferred {
        identity: state,
        basis,
        settings_modified_at,
    } = observation
    else {
        panic!("clean cached Claude profile must infer the account");
    };
    assert_eq!(basis, SourceIdentityInference::CachedLocalProfile);
    assert_eq!(settings_modified_at, file_modified_at(&settings_path));
    assert_eq!(state.provider_user_id.as_deref(), Some("file-only-account"));
    assert_eq!(state.email.as_deref(), Some("file-only@example.com"));
    assert_eq!(
        state.authenticated_at,
        Utc.timestamp_millis_opt(1_786_104_000_000_i64).single()
    );
}

#[test]
fn claude_default_profile_resolution_uses_home_sibling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join(".claude");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        dir.path().join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "active-home-account",
                "emailAddress": "home@example.com"
            }
        })
        .to_string(),
    )
    .expect("home profile");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "stale-nested-account",
                "emailAddress": "stale@example.com"
            }
        })
        .to_string(),
    )
    .expect("stale nested profile");

    let observation =
        claude_cached_profile_observation(&root, &LocationOrigin::Default, Some(&root), None);

    let VerifiedSourceObservation::Inferred {
        identity: state, ..
    } = observation
    else {
        panic!("default Claude source must use the home profile");
    };
    assert_eq!(
        state.provider_user_id.as_deref(),
        Some("active-home-account")
    );
}

#[test]
fn claude_environment_profile_resolution_uses_nested_profile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("custom").join(".claude");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "environment-account",
                "emailAddress": "environment@example.com"
            }
        })
        .to_string(),
    )
    .expect("nested profile");
    std::fs::write(
        root.parent().expect("custom parent").join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "unrelated-sibling-account",
                "emailAddress": "unrelated@example.com"
            }
        })
        .to_string(),
    )
    .expect("sibling profile");

    let observation = claude_cached_profile_observation(&root, &LocationOrigin::Env, None, None);

    let VerifiedSourceObservation::Inferred {
        identity: state, ..
    } = observation
    else {
        panic!("CLAUDE_CONFIG_DIR source must use its nested profile");
    };
    assert_eq!(
        state.provider_user_id.as_deref(),
        Some("environment-account")
    );
}

#[test]
fn claude_configured_source_with_conflicting_profiles_is_blocked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("mounted").join(".claude");
    std::fs::create_dir_all(&root).expect("config root");
    for (path, account) in [
        (root.join(".claude.json"), "nested-account"),
        (
            root.parent().expect("mounted parent").join(".claude.json"),
            "sibling-account",
        ),
    ] {
        std::fs::write(
            path,
            serde_json::json!({
                "oauthAccount": {
                    "accountUuid": account,
                    "emailAddress": format!("{account}@example.com")
                }
            })
            .to_string(),
        )
        .expect("profile");
    }

    assert_eq!(
        claude_cached_profile_observation(&root, &LocationOrigin::Configured, None, None),
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        }
    );
}

#[test]
fn claude_auth_dependencies_include_user_and_project_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("workspace");
    let workspace = dir.path().join("workspace");
    let project_settings_root = workspace.join(".claude");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&project_settings_root).expect("project settings root");
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

    let dependencies = claude_auth_dependency_paths(&root, &LocationOrigin::Configured);

    assert!(dependencies.contains(&root.join(".claude.json")));
    assert!(dependencies.contains(&root.join("settings.json")));
    assert!(dependencies.contains(&root.join("settings.local.json")));
    assert!(dependencies.contains(&project_settings_root));
}

#[test]
fn claude_dependency_topology_changes_only_for_project_indexes_and_directories() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let projects = root.join("projects");
    let project_store = projects.join("workspace");
    std::fs::create_dir_all(&project_store).expect("project store");

    assert!(claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&projects)
    ));
    assert!(claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&project_store)
    ));
    assert!(claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&project_store.join("sessions-index.json"))
    ));
    assert!(claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&project_store.join("session.jsonl"))
    ));
    std::fs::write(
        project_store.join("sessions-index.json"),
        r#"{"entries":[]}"#,
    )
    .expect("session index");
    assert!(!claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&project_store.join("session.jsonl"))
    ));
    assert!(!claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&root.join("settings.json"))
    ));
}

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
fn claude_cached_profile_is_not_used_when_settings_select_auth_token() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let settings_path = root.join("settings.json");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com",
                "organizationUuid": "cached-organization"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "env": {"ANTHROPIC_AUTH_TOKEN": "configured-token"}
        })
        .to_string(),
    )
    .expect("claude settings");

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
fn claude_custom_base_url_blocks_attribution_when_cached_profile_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let settings_path = root.join("settings.json");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "env": {"ANTHROPIC_BASE_URL": "https://gateway.example.com/anthropic"}
        })
        .to_string(),
    )
    .expect("claude settings");

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
fn claude_default_base_url_without_cached_profile_is_unavailable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        root.join("settings.json"),
        serde_json::json!({
            "env": {"ANTHROPIC_BASE_URL": "https://api.anthropic.com/"}
        })
        .to_string(),
    )
    .expect("claude settings");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(observation, VerifiedSourceObservation::Unavailable);
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
fn claude_cached_profile_is_not_used_when_settings_select_api_key_helper() {
    let settings = serde_json::json!({"apiKeyHelper": "/usr/local/bin/credential-helper"});

    assert!(claude_settings_value_has_auth_override(&settings));
}

#[test]
fn claude_local_settings_clear_lower_precedence_auth_overrides() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("settings.json"),
        serde_json::json!({
            "apiKeyHelper": "/usr/local/bin/stale-helper",
            "env": {"ANTHROPIC_API_KEY": "stale-key"}
        })
        .to_string(),
    )
    .expect("lower-precedence settings");
    std::fs::write(
        dir.path().join("settings.local.json"),
        serde_json::json!({
            "apiKeyHelper": "",
            "env": {"ANTHROPIC_API_KEY": ""}
        })
        .to_string(),
    )
    .expect("higher-precedence settings");

    assert_eq!(claude_settings_have_auth_override(dir.path()), Some(false));
}

#[test]
fn claude_settings_precedence_preserves_first_uninterrupted_enabled_timestamp() {
    let first_enabled_at = Utc
        .with_ymd_and_hms(2026, 5, 10, 0, 0, 0)
        .single()
        .expect("first enabled timestamp");
    let repeated_enabled_at = Utc
        .with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
        .single()
        .expect("repeated enabled timestamp");
    let reenabled_at = Utc
        .with_ymd_and_hms(2026, 5, 20, 0, 0, 0)
        .single()
        .expect("re-enabled timestamp");
    let enabled = serde_json::json!({
        "env": {"ANTHROPIC_API_KEY": "configured-key"}
    });
    let cleared = serde_json::json!({
        "env": {"ANTHROPIC_API_KEY": ""}
    });
    let mut state = ClaudeAuthOverrideState::default();

    state.apply(&enabled, false, Some(first_enabled_at));
    state.apply(&enabled, false, Some(repeated_enabled_at));

    assert_eq!(
        state.probe(),
        ClaudeAuthOverrideProbe::Blocked(ClaudeAuthBlock {
            blocked_since: Some(first_enabled_at),
        })
    );

    state.apply(&cleared, false, Some(repeated_enabled_at));
    assert_eq!(state.probe(), ClaudeAuthOverrideProbe::Clear);
    state.apply(&enabled, false, Some(reenabled_at));

    assert_eq!(
        state.probe(),
        ClaudeAuthOverrideProbe::Blocked(ClaudeAuthBlock {
            blocked_since: Some(reenabled_at),
        })
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
fn claude_settings_detect_every_higher_precedence_credential_override() {
    for name in CLAUDE_SETTINGS_AUTH_OVERRIDE_KEYS {
        let mut environment = serde_json::Map::new();
        environment.insert(
            (*name).to_owned(),
            Value::String(
                if claude_setting_is_provider_selector(name) {
                    "1"
                } else {
                    "configured-value"
                }
                .to_owned(),
            ),
        );
        let settings = serde_json::json!({"env": environment});

        assert!(
            claude_settings_value_has_auth_override(&settings),
            "expected {name} to suppress cached OAuth identity"
        );
    }
}

#[test]
fn claude_settings_detect_current_non_oauth_provider_selectors() {
    for name in [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_USE_MANTLE",
        "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    ] {
        for value in [
            Value::String("1".to_owned()),
            Value::String("true".to_owned()),
            Value::Bool(true),
            Value::Number(1.into()),
        ] {
            let mut environment = serde_json::Map::new();
            environment.insert(name.to_owned(), value);
            let settings = serde_json::json!({"env": environment});

            assert!(
                claude_settings_value_has_auth_override(&settings),
                "expected enabled {name} value to suppress cached OAuth identity"
            );
        }
    }
}

#[test]
fn claude_disabled_provider_selector_values_do_not_suppress_cached_oauth() {
    for name in [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_USE_MANTLE",
        "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    ] {
        for value in [
            Value::String("0".to_owned()),
            Value::String("false".to_owned()),
            Value::Bool(false),
            Value::Number(0.into()),
        ] {
            let mut environment = serde_json::Map::new();
            environment.insert(name.to_owned(), value);
            let settings = serde_json::json!({"env": environment});

            assert!(
                !claude_settings_value_has_auth_override(&settings),
                "expected disabled {name} value to preserve cached OAuth identity"
            );
        }
    }
}

#[test]
fn claude_settings_detect_descriptor_injected_credentials() {
    for name in [
        "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
        "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    ] {
        let mut environment = serde_json::Map::new();
        environment.insert(name.to_owned(), Value::String("3".to_owned()));
        let settings = serde_json::json!({"env": environment});

        assert!(
            claude_settings_value_has_auth_override(&settings),
            "expected {name} to suppress cached OAuth identity"
        );
    }
}

#[test]
fn claude_managed_settings_provider_override_suppresses_cached_oauth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let managed_settings_root = dir.path().join("managed");
    let managed_settings_path = managed_settings_root.join("managed-settings.json");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::create_dir_all(&managed_settings_root).expect("managed settings root");
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
        &managed_settings_path,
        serde_json::json!({
            "env": {"CLAUDE_CODE_USE_MANTLE": "1"}
        })
        .to_string(),
    )
    .expect("managed settings");

    let observation = claude_auth_snapshot_with_probe_context(
        &root,
        &LocationOrigin::Configured,
        Some(&managed_settings_root),
    );

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&managed_settings_path),
        }
    );
}

#[test]
fn claude_managed_settings_drop_in_provider_override_suppresses_cached_oauth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let drop_ins = dir.path().join("managed-settings.d");
    std::fs::create_dir_all(&drop_ins).expect("managed settings drop-ins");
    std::fs::write(
        drop_ins.join("20-provider.json"),
        serde_json::json!({
            "env": {"CLAUDE_CODE_USE_ANTHROPIC_AWS": "1"}
        })
        .to_string(),
    )
    .expect("managed settings drop-in");

    assert_eq!(
        claude_managed_settings_have_auth_override_in(dir.path()),
        Some(true)
    );
}

#[test]
fn claude_managed_drop_ins_clear_base_auth_overrides_in_filename_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let drop_ins = dir.path().join("managed-settings.d");
    std::fs::create_dir_all(&drop_ins).expect("managed settings drop-ins");
    std::fs::write(
        dir.path().join("managed-settings.json"),
        serde_json::json!({
            "apiKeyHelper": "/usr/local/bin/stale-helper",
            "policyHelper": {"path": "/usr/local/bin/stale-policy"},
            "env": {"CLAUDE_CODE_USE_VERTEX": "1"}
        })
        .to_string(),
    )
    .expect("managed settings base");
    std::fs::write(
        drop_ins.join("10-keep-enabled.json"),
        serde_json::json!({
            "env": {"CLAUDE_CODE_USE_VERTEX": "1"}
        })
        .to_string(),
    )
    .expect("earlier managed settings drop-in");
    std::fs::write(
        drop_ins.join("20-clear-auth.json"),
        serde_json::json!({
            "apiKeyHelper": "",
            "policyHelper": null,
            "env": {"CLAUDE_CODE_USE_VERTEX": ""}
        })
        .to_string(),
    )
    .expect("later managed settings drop-in");

    assert_eq!(
        claude_managed_settings_have_auth_override_in(dir.path()),
        Some(false)
    );
}

#[test]
fn claude_managed_policy_helper_suppresses_cached_oauth_without_execution() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("managed-settings.json"),
        serde_json::json!({
            "policyHelper": {"path": "/usr/local/bin/managed-claude-policy"}
        })
        .to_string(),
    )
    .expect("managed settings");

    assert_eq!(
        claude_managed_settings_have_auth_override_in(dir.path()),
        Some(true)
    );
}

#[test]
fn claude_unrelated_managed_settings_do_not_suppress_cached_oauth() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("managed-settings.json"),
        serde_json::json!({
            "permissions": {"deny": ["Read(//etc/secrets/**)"]}
        })
        .to_string(),
    )
    .expect("managed settings");

    assert_eq!(
        claude_managed_settings_have_auth_override_in(dir.path()),
        Some(false)
    );
}

#[test]
fn claude_malformed_managed_settings_block_cached_profile_attribution() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(&root).expect("config root");
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
    std::fs::write(dir.path().join("managed-settings.json"), "{not valid json")
        .expect("malformed managed settings");

    assert_eq!(
        claude_managed_settings_have_auth_override_in(dir.path()),
        None
    );
    assert_eq!(
        claude_auth_snapshot_with_probe_context(
            &root,
            &LocationOrigin::Configured,
            Some(dir.path()),
        ),
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        }
    );
}

#[test]
fn claude_malformed_settings_block_cached_profile_attribution() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(&root).expect("config root");
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
    std::fs::write(root.join("settings.json"), "{not valid json")
        .expect("malformed claude settings");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        }
    );
}
