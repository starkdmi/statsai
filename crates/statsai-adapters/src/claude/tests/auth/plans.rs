use super::*;
use crate::{AccountEvidenceScan, CLAUDE_CODE_PROVIDER};
use statsai_core::{
    hash_text, provider_account_id_from_identity, AccountEvidenceKind, Confidence,
    ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION,
};

const PROFILE_FETCHED_AT_MILLIS: i64 = 1_786_104_000_000;

fn oauth_profile(organization_type: Option<&str>, rate_limit_tier: Option<&str>) -> Value {
    let mut oauth_account = serde_json::Map::new();
    oauth_account.insert(
        "accountUuid".to_owned(),
        Value::String("plan-account-uuid".to_owned()),
    );
    oauth_account.insert(
        "emailAddress".to_owned(),
        Value::String("plan-account@example.test".to_owned()),
    );
    oauth_account.insert(
        "profileFetchedAt".to_owned(),
        Value::Number(PROFILE_FETCHED_AT_MILLIS.into()),
    );
    if let Some(organization_type) = organization_type {
        oauth_account.insert(
            "organizationType".to_owned(),
            Value::String(organization_type.to_owned()),
        );
    }
    if let Some(rate_limit_tier) = rate_limit_tier {
        oauth_account.insert(
            "organizationRateLimitTier".to_owned(),
            Value::String(rate_limit_tier.to_owned()),
        );
    }
    serde_json::json!({ "oauthAccount": oauth_account })
}

fn write_profile(root: &Path, profile: &str) {
    std::fs::create_dir_all(root).expect("config root");
    std::fs::write(root.join(".claude.json"), profile).expect("claude profile");
}

fn plan_source(root: &Path) -> SourceLocation {
    SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "claude-code-local-jsonl",
        "0",
        root,
        LocationOrigin::Configured,
    )
}

fn collect_plan_evidence(root: &Path) -> AccountEvidenceScan {
    collect_claude_account_evidence_with_probe_context(
        &plan_source(root),
        root,
        &LocationOrigin::Configured,
        None,
    )
}

fn collect_plan_evidence_for_profile(root: &Path, profile: &str) -> AccountEvidenceScan {
    write_profile(root, profile);
    collect_plan_evidence(root)
}

fn expected_account_id() -> statsai_core::ProviderAccountId {
    provider_account_id_from_identity(
        CLAUDE_CODE_PROVIDER,
        Some("plan-account-uuid"),
        Some("plan-account@example.test"),
    )
    .expect("provider account id")
}

#[test]
fn claude_cached_organization_types_map_to_canonical_plans() {
    let expectations: &[(Option<&str>, Option<&str>, Option<&str>)] = &[
        (Some("claude_pro"), None, Some("Pro")),
        (Some(" CLAUDE_PRO "), Some("arbitrary_tier"), Some("Pro")),
        (
            Some("claude_max"),
            Some("default_claude_max_5x"),
            Some("Max 5x"),
        ),
        (
            Some("claude_max"),
            Some("DEFAULT_CLAUDE_MAX_20X"),
            Some("Max 20x"),
        ),
        (Some("claude_max"), None, Some("Max")),
        (Some("claude_max"), Some("   "), Some("Max")),
        (Some("claude_max"), Some("future_max_tier"), Some("Max")),
        (Some("claude_team"), None, Some("Team")),
        // A rate-limit tier never relabels a non-Max organization.
        (
            Some("claude_team"),
            Some("default_claude_max_5x"),
            Some("Team"),
        ),
        (Some("claude_enterprise"), None, Some("Enterprise")),
        (
            Some("claude_enterprise"),
            Some("enterprise_usage_based"),
            Some("Enterprise"),
        ),
        // Unknown organization types fail closed rather than becoming Free.
        (Some("claude_future"), Some("default_claude_max_5x"), None),
        (None, Some("default_claude_max_20x"), None),
        (Some("   "), Some("default_claude_max_5x"), None),
    ];

    for (organization_type, rate_limit_tier, expected_plan) in expectations {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("claude-config");
        let evidence = collect_plan_evidence_for_profile(
            &root,
            &oauth_profile(*organization_type, *rate_limit_tier).to_string(),
        );

        match expected_plan {
            Some(expected_plan) => {
                assert_eq!(
                    evidence.plan_observations.len(),
                    1,
                    "expected one plan for {organization_type:?}/{rate_limit_tier:?}"
                );
                let observation = &evidence.plan_observations[0];
                assert_eq!(observation.plan_name, *expected_plan);
                assert_eq!(
                    observation.raw_plan_name,
                    organization_type.expect("organization type").trim(),
                    "the raw plan keeps its original casing, trimmed"
                );
                assert_eq!(
                    evidence.accounts[0].plan_name.as_deref(),
                    Some(*expected_plan)
                );
            }
            None => assert!(
                evidence.plan_observations.is_empty() && evidence.accounts.is_empty(),
                "expected no evidence for {organization_type:?}/{rate_limit_tier:?}"
            ),
        }
    }
}

#[test]
fn claude_plan_detection_ignores_subscription_lifecycle_and_seat_fields() {
    let baseline = {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("claude-config");
        let evidence = collect_plan_evidence_for_profile(
            &root,
            &oauth_profile(Some("claude_max"), Some("default_claude_max_20x")).to_string(),
        );
        evidence.plan_observations[0].clone()
    };

    // Built as raw JSON so serde simply ignores the extra keys: they must not be
    // modelled at all, and unknown fields prove they cannot steer the output.
    let ignored_variants = [
        serde_json::json!({"hasAvailableSubscription": true}),
        serde_json::json!({"hasAvailableSubscription": false}),
        serde_json::json!({"billingType": "stripe_subscription"}),
        serde_json::json!({"billingType": "apple_iap"}),
        serde_json::json!({"seatTier": "team_standard"}),
        serde_json::json!({"seatTier": "team_tier_1"}),
        serde_json::json!({"seatTier": "enterprise_usage_based"}),
        serde_json::json!({"userRateLimitTier": "default_claude_max_5x"}),
        serde_json::json!({"hasAvailableSubscription": false, "billingType": "google_play"}),
    ];

    for extra in ignored_variants {
        let extra_fields = extra.as_object().expect("object");
        for placement in ["root", "oauthAccount"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().join("claude-config");
            let mut profile = oauth_profile(Some("claude_max"), Some("default_claude_max_20x"));
            let target = if placement == "root" {
                profile.as_object_mut().expect("profile object")
            } else {
                profile["oauthAccount"]
                    .as_object_mut()
                    .expect("oauth account object")
            };
            for (key, value) in extra_fields {
                target.insert(key.clone(), value.clone());
            }
            let evidence = collect_plan_evidence_for_profile(&root, &profile.to_string());

            assert_eq!(evidence.plan_observations.len(), 1);
            let observation = &evidence.plan_observations[0];
            assert_eq!(observation.plan_name, "Max 20x");
            assert_eq!(
                observation.record_fingerprint, baseline.record_fingerprint,
                "ignored {placement} fields {extra} must not change the fingerprint"
            );
        }
    }
}

#[test]
fn claude_plan_fingerprint_ignores_json_formatting_and_key_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let compact = collect_plan_evidence_for_profile(
        &root,
        &oauth_profile(Some("claude_max"), Some("default_claude_max_5x")).to_string(),
    );
    let reordered = collect_plan_evidence_for_profile(
        &root,
        "{\n  \"oauthAccount\" : {\n    \"organizationRateLimitTier\": \"default_claude_max_5x\",\n\
         \n    \"profileFetchedAt\": 1786104000000,\n    \"emailAddress\":\
         \n      \"plan-account@example.test\",\n    \"organizationType\": \"claude_max\",\n\
         \"accountUuid\": \"plan-account-uuid\"\n  }\n}\n",
    );

    assert_eq!(compact.plan_observations.len(), 1);
    assert_eq!(
        compact.plan_observations[0].record_fingerprint,
        reordered.plan_observations[0].record_fingerprint
    );
    assert_eq!(
        compact.plan_observations[0].observation_id,
        reordered.plan_observations[0].observation_id
    );
}

#[test]
fn claude_plan_evidence_is_stable_and_tracks_semantic_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let profile = oauth_profile(Some("claude_max"), Some("default_claude_max_5x")).to_string();
    let first = collect_plan_evidence_for_profile(&root, &profile);
    let repeat = collect_plan_evidence_for_profile(&root, &profile);

    assert_eq!(
        first.plan_observations[0].observation_id,
        repeat.plan_observations[0].observation_id
    );
    assert_eq!(
        first.plan_observations[0].record_fingerprint,
        repeat.plan_observations[0].record_fingerprint
    );

    let upgraded = collect_plan_evidence_for_profile(
        &root,
        &oauth_profile(Some("claude_max"), Some("default_claude_max_20x")).to_string(),
    );
    assert_eq!(upgraded.plan_observations[0].plan_name, "Max 20x");
    assert_ne!(
        upgraded.plan_observations[0].record_fingerprint,
        first.plan_observations[0].record_fingerprint,
        "the tier selects the canonical Max subtype, so it belongs in the fingerprint"
    );
    // The organization type is unchanged, so the canonical plan is the only thing
    // separating these two claims. It has to reach the observation id: the store
    // deduplicates on that id alone and would otherwise drop the newer tier.
    assert_ne!(
        upgraded.plan_observations[0].observation_id,
        first.plan_observations[0].observation_id
    );

    let mut refetched = oauth_profile(Some("claude_max"), Some("default_claude_max_20x"));
    refetched["oauthAccount"]["profileFetchedAt"] =
        Value::Number((PROFILE_FETCHED_AT_MILLIS + 86_400_000).into());
    let refetched = collect_plan_evidence_for_profile(&root, &refetched.to_string());
    assert_ne!(
        refetched.plan_observations[0].observation_id, upgraded.plan_observations[0].observation_id,
        "a later cached fetch is a new observation"
    );
    assert_ne!(
        refetched.plan_observations[0].record_fingerprint,
        upgraded.plan_observations[0].record_fingerprint
    );
}

#[test]
fn claude_plan_observation_reports_dated_medium_confidence_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let profile_path = root.join(".claude.json");
    let evidence = collect_plan_evidence_for_profile(
        &root,
        &oauth_profile(Some("claude_max"), Some("default_claude_max_5x")).to_string(),
    );

    assert_eq!(evidence.accounts.len(), 1);
    assert_eq!(evidence.plan_observations.len(), 1);
    assert!(
        evidence.identity_observations.is_empty(),
        "source identity state stays with the verified-source probe"
    );
    assert!(evidence.conversation_bindings.is_empty());
    assert!(
        evidence.checkpoints.is_empty(),
        "one bounded snapshot file has no incremental cursor"
    );

    let observed_at = Utc
        .timestamp_millis_opt(PROFILE_FETCHED_AT_MILLIS)
        .single()
        .expect("cached profile timestamp");
    let account = &evidence.accounts[0];
    assert_eq!(
        account.provider_user_id.as_deref(),
        Some("plan-account-uuid")
    );
    assert_eq!(account.email.as_deref(), Some("plan-account@example.test"));
    assert_eq!(account.plan_name.as_deref(), Some("Max 5x"));
    assert_eq!(account.observed_at, observed_at);

    let observation = &evidence.plan_observations[0];
    assert_eq!(
        observation.schema_version,
        ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION
    );
    assert_eq!(observation.provider, CLAUDE_CODE_PROVIDER);
    assert_eq!(observation.source_id, plan_source(&root).source_id);
    assert_eq!(
        observation.provider_account_id.as_ref(),
        Some(&expected_account_id())
    );
    assert_eq!(observation.raw_plan_name, "claude_max");
    assert_eq!(observation.plan_name, "Max 5x");
    assert_eq!(observation.observed_at, observed_at);
    assert_eq!(observation.active_from, None);
    assert_eq!(observation.active_until, None);
    assert!(observation.is_current_snapshot);
    assert_eq!(observation.evidence_kind, AccountEvidenceKind::AuthSnapshot);
    assert_eq!(observation.confidence, Confidence::Medium);
    assert_eq!(
        observation.parser_version,
        CLAUDE_ACCOUNT_EVIDENCE_PARSER_VERSION
    );
    assert_eq!(
        observation.artifact_path_hash,
        hash_text(&canonical_display(&profile_path))
    );
    let profile_path_label = canonical_display(&profile_path);
    let serialized = serde_json::to_string(observation).expect("serialize observation");
    assert!(
        !serialized.contains(&profile_path_label),
        "the artifact path is hashed, never persisted in the clear"
    );
}

#[test]
fn claude_plan_evidence_accepts_email_only_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let evidence = collect_plan_evidence_for_profile(
        &root,
        &serde_json::json!({
            "oauthAccount": {
                "emailAddress": "  Plan-Account@Example.Test  ",
                "profileFetchedAt": PROFILE_FETCHED_AT_MILLIS,
                "organizationType": "claude_team"
            }
        })
        .to_string(),
    );

    assert_eq!(evidence.plan_observations.len(), 1);
    assert_eq!(
        evidence.accounts[0].email.as_deref(),
        Some("plan-account@example.test")
    );
    assert!(evidence.accounts[0].provider_user_id.is_none());
    assert_eq!(
        evidence.plan_observations[0].provider_account_id,
        provider_account_id_from_identity(
            CLAUDE_CODE_PROVIDER,
            None,
            Some("plan-account@example.test"),
        )
    );
    assert_eq!(evidence.plan_observations[0].plan_name, "Team");
}

#[test]
fn claude_plan_evidence_fails_closed_on_unusable_profiles() {
    let cases: &[(&str, Option<&str>)] = &[
        ("missing profile", None),
        ("invalid json", Some("{not valid json")),
        ("missing oauth account", Some(r#"{"theme":"dark"}"#)),
        (
            "missing identity",
            Some(
                r#"{"oauthAccount":{"profileFetchedAt":1786104000000,"organizationType":"claude_pro"}}"#,
            ),
        ),
        (
            "blank identity",
            Some(
                r#"{"oauthAccount":{"accountUuid":"  ","emailAddress":"","profileFetchedAt":1786104000000,"organizationType":"claude_pro"}}"#,
            ),
        ),
        (
            "missing fetch time",
            Some(r#"{"oauthAccount":{"accountUuid":"a","organizationType":"claude_pro"}}"#),
        ),
        (
            "blank fetch time",
            Some(
                r#"{"oauthAccount":{"accountUuid":"a","profileFetchedAt":"","organizationType":"claude_pro"}}"#,
            ),
        ),
        (
            "non-string fetch time",
            Some(
                r#"{"oauthAccount":{"accountUuid":"a","profileFetchedAt":true,"organizationType":"claude_pro"}}"#,
            ),
        ),
        (
            "malformed fetch time",
            Some(
                r#"{"oauthAccount":{"accountUuid":"a","profileFetchedAt":"yesterday","organizationType":"claude_pro"}}"#,
            ),
        ),
        (
            "missing organization type",
            Some(r#"{"oauthAccount":{"accountUuid":"a","profileFetchedAt":1786104000000}}"#),
        ),
        (
            "blank organization type",
            Some(
                r#"{"oauthAccount":{"accountUuid":"a","profileFetchedAt":1786104000000,"organizationType":"   "}}"#,
            ),
        ),
        (
            "unknown organization type",
            Some(
                r#"{"oauthAccount":{"accountUuid":"a","profileFetchedAt":1786104000000,"organizationType":"claude_future"}}"#,
            ),
        ),
    ];

    for (label, profile) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("claude-config");
        std::fs::create_dir_all(&root).expect("config root");
        if let Some(profile) = profile {
            std::fs::write(root.join(".claude.json"), profile).expect("claude profile");
        }
        let evidence = collect_plan_evidence(&root);

        assert!(
            evidence.accounts.is_empty() && evidence.plan_observations.is_empty(),
            "expected no plan evidence for {label}"
        );
    }
}

#[test]
fn claude_ambiguous_profile_topology_suppresses_plan_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("mounted").join(".claude");
    std::fs::create_dir_all(&root).expect("config root");
    let profile = oauth_profile(Some("claude_pro"), None).to_string();
    std::fs::write(root.join(".claude.json"), &profile).expect("nested profile");
    std::fs::write(
        root.parent().expect("mounted parent").join(".claude.json"),
        &profile,
    )
    .expect("sibling profile");

    let evidence = collect_plan_evidence(&root);

    assert!(evidence.accounts.is_empty() && evidence.plan_observations.is_empty());
}

#[test]
fn claude_unreadable_settings_suppress_plan_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    write_profile(&root, &oauth_profile(Some("claude_pro"), None).to_string());
    std::fs::write(root.join("settings.json"), "{not valid json")
        .expect("malformed claude settings");

    assert!(collect_plan_evidence(&root).plan_observations.is_empty());

    std::fs::write(root.join("settings.json"), r#"{"theme":"dark"}"#).expect("harmless settings");
    assert_eq!(collect_plan_evidence(&root).plan_observations.len(), 1);
}

#[test]
fn claude_malformed_managed_settings_suppress_plan_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let managed_settings_root = dir.path().join("managed");
    std::fs::create_dir_all(&managed_settings_root).expect("managed settings root");
    write_profile(&root, &oauth_profile(Some("claude_pro"), None).to_string());
    std::fs::write(
        managed_settings_root.join("managed-settings.json"),
        "{not valid json",
    )
    .expect("malformed managed settings");

    let evidence = collect_claude_account_evidence_with_probe_context(
        &plan_source(&root),
        &root,
        &LocationOrigin::Configured,
        Some(&managed_settings_root),
    );

    assert!(evidence.accounts.is_empty() && evidence.plan_observations.is_empty());
}

#[test]
fn claude_managed_provider_override_suppresses_plan_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let managed_settings_root = dir.path().join("managed");
    std::fs::create_dir_all(&managed_settings_root).expect("managed settings root");
    write_profile(&root, &oauth_profile(Some("claude_max"), None).to_string());
    std::fs::write(
        managed_settings_root.join("managed-settings.json"),
        serde_json::json!({"env": {"CLAUDE_CODE_USE_BEDROCK": "1"}}).to_string(),
    )
    .expect("managed settings");

    let evidence = collect_claude_account_evidence_with_probe_context(
        &plan_source(&root),
        &root,
        &LocationOrigin::Configured,
        Some(&managed_settings_root),
    );

    assert!(evidence.accounts.is_empty() && evidence.plan_observations.is_empty());
}

#[test]
fn claude_every_durable_auth_override_suppresses_plan_evidence() {
    // The same key table that governs identity attribution, so plan evidence can
    // never survive a credential that identity inference already refuses.
    for name in CLAUDE_SETTINGS_AUTH_OVERRIDE_KEYS {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("claude-config");
        write_profile(&root, &oauth_profile(Some("claude_pro"), None).to_string());
        let mut environment = serde_json::Map::new();
        environment.insert(
            (*name).to_owned(),
            Value::String(
                if claude_setting_is_provider_selector(name) {
                    "1"
                } else if *name == "ANTHROPIC_BASE_URL" {
                    "https://gateway.example.test/anthropic"
                } else {
                    "configured-value"
                }
                .to_owned(),
            ),
        );
        std::fs::write(
            root.join("settings.json"),
            serde_json::json!({"env": environment}).to_string(),
        )
        .expect("claude settings");

        let evidence = collect_plan_evidence(&root);

        assert!(
            evidence.accounts.is_empty() && evidence.plan_observations.is_empty(),
            "expected {name} to suppress cached plan evidence"
        );
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    write_profile(&root, &oauth_profile(Some("claude_pro"), None).to_string());
    std::fs::write(
        root.join("settings.json"),
        serde_json::json!({"apiKeyHelper": "/usr/local/bin/credential-helper"}).to_string(),
    )
    .expect("claude settings");

    assert!(collect_plan_evidence(&root).plan_observations.is_empty());
}
