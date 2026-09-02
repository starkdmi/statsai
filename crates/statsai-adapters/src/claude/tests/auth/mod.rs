pub(crate) use super::*;

mod overrides;
mod plans;
mod projects;

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
