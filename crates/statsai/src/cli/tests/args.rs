use super::support::*;
use super::*;

#[test]
fn privacy_filter_preview_is_exposed_by_the_cli() {
    let cli = Cli::try_parse_from(["statsai", "privacy", "filter", "--preview"])
        .expect("parse privacy preview");
    assert!(matches!(cli.command, Command::Privacy(_)));
}

#[test]
fn provider_aliases_match_canonical_provider() {
    assert!(provider_matches("claude_code", "claude"));
    assert!(provider_matches("claude-code", "claude_code"));
    assert!(provider_matches("codex", "codex"));
    assert_eq!(
        canonical_provider("claude").expect("provider"),
        "claude_code"
    );
    assert_eq!(canonical_provider_name("claude-code"), Some("claude_code"));
    assert_eq!(canonical_provider_name("grok"), Some("grok_build"));
    assert_eq!(canonical_provider_name("open-code"), Some("opencode"));
    assert_eq!(
        canonical_conversation_provider_filter(Some("claude")).expect("archive provider"),
        Some("claude_code")
    );
    assert_eq!(
        canonical_conversation_provider_filter(Some("grok")).expect("archive provider"),
        Some("grok_build")
    );
    assert_eq!(
        canonical_conversation_provider_filter(Some("open-code")).expect("archive provider"),
        Some("opencode")
    );
    assert!(canonical_conversation_provider_filter(Some("unknown")).is_err());
}

#[test]
fn task_status_and_verdict_parsers_reject_unknown_values() {
    assert_eq!(
        parse_task_status_filter("verified").expect("verified status"),
        TaskStatus::Verified
    );
    assert_eq!(
        parse_task_verdict("noise").expect("noise verdict"),
        TaskVerdict::Noise
    );
    assert!(parse_task_status_filter("mystery").is_err());
    assert!(parse_task_verdict("mystery").is_err());
}

#[test]
fn supported_store_schema_version_is_available_without_opening_a_store() {
    let cli = Cli::try_parse_from(["statsai", "store", "supported-schema-version"])
        .expect("parse supported schema query");

    assert!(matches!(
        cli.command,
        Command::Store(StoreAdminCommand {
            command: StoreAdminSubcommand::SupportedSchemaVersion,
        })
    ));

    let directory = tempfile::tempdir().expect("temporary directory");
    let store_path = directory.path().join("must-not-be-created.sqlite");
    store_admin(
        StoreAdminCommand {
            command: StoreAdminSubcommand::SupportedSchemaVersion,
        },
        &store_path,
    )
    .expect("print supported schema");
    assert!(!store_path.exists());
}

#[test]
fn supported_pricing_ruleset_version_is_available_without_opening_a_store() {
    let cli = Cli::try_parse_from(["statsai", "store", "supported-pricing-ruleset-version"])
        .expect("parse supported pricing query");

    assert!(matches!(
        cli.command,
        Command::Store(StoreAdminCommand {
            command: StoreAdminSubcommand::SupportedPricingRulesetVersion,
        })
    ));

    let directory = tempfile::tempdir().expect("temporary directory");
    let store_path = directory.path().join("must-not-be-created.sqlite");
    store_admin(
        StoreAdminCommand {
            command: StoreAdminSubcommand::SupportedPricingRulesetVersion,
        },
        &store_path,
    )
    .expect("print supported pricing ruleset");
    assert!(!store_path.exists());
}

#[test]
fn price_derived_commands_reprice_and_diagnostic_commands_do_not() {
    let reprice = |args: &[&str]| {
        let cli = Cli::try_parse_from(args).expect("parse");
        command_reprices_persisted_usage(&cli.command)
    };
    assert!(reprice(&["statsai", "scan"]));
    assert!(reprice(&["statsai", "report", "monthly"]));
    assert!(reprice(&["statsai", "sync"]));
    assert!(reprice(&["statsai", "export", "--json"]));
    assert!(reprice(&["statsai", "task", "list"]));
    assert!(!reprice(&["statsai", "status"]));
    assert!(!reprice(&["statsai", "quota", "status"]));
    assert!(!reprice(&["statsai", "conversation", "list"]));
    assert!(!reprice(&["statsai", "account", "list"]));
    assert!(!reprice(&["statsai", "source", "list"]));
    let doctor = Cli::try_parse_from(["statsai", "doctor"]).expect("parse doctor");
    assert!(!command_reprices_persisted_usage(&doctor.command));
}
