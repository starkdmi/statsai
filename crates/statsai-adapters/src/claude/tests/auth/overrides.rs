use super::*;

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
