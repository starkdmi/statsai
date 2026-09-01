use super::*;
#[test]
fn sonnet_5_reports_aggregate_periods_that_cross_its_price_change() {
    let before = chrono::NaiveDate::from_ymd_opt(2026, 8, 31).expect("before boundary");
    let boundary = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).expect("boundary");
    let after = chrono::NaiveDate::from_ymd_opt(2026, 9, 2).expect("after boundary");

    assert!(pricing_changes_between("claude-sonnet-5", before, boundary));
    assert!(pricing_changes_between(
        "anthropic/claude-sonnet-5",
        after,
        before
    ));
    assert!(!pricing_changes_between("claude-sonnet-5", boundary, after));
    assert!(!pricing_changes_between("claude-opus-5", before, after));
}
#[test]
fn codex_auto_review_reports_aggregate_periods_that_cross_its_equivalent_change() {
    let before = chrono::NaiveDate::from_ymd_opt(2026, 7, 29).expect("before boundary");
    let boundary = chrono::NaiveDate::from_ymd_opt(2026, 7, 30).expect("boundary");
    let after = chrono::NaiveDate::from_ymd_opt(2026, 7, 31).expect("after boundary");

    assert!(pricing_changes_between(
        "codex-auto-review",
        before,
        boundary
    ));
    assert!(pricing_changes_between(
        "openai/codex-auto-review",
        after,
        before
    ));
    assert!(!pricing_changes_between(
        "codex-auto-review",
        boundary,
        after
    ));
    assert!(!pricing_changes_between("gpt-5.4", before, after));
}
#[test]
fn ruleset_version_is_numeric_and_catalog_is_descriptive() {
    const { assert!(PRICING_RULESET_VERSION >= 1) };
    assert!(PRICING_CATALOG_VERSION.contains(':'));
    assert_ne!(PRICING_RULESET_VERSION.to_string(), PRICING_CATALOG_VERSION);
}

#[test]
fn gpt_5_6_luna_and_terra_report_aggregate_periods_that_cross_the_july_30_cut() {
    let before = chrono::NaiveDate::from_ymd_opt(2026, 7, 29).expect("before boundary");
    let boundary = chrono::NaiveDate::from_ymd_opt(2026, 7, 30).expect("boundary");
    let after = chrono::NaiveDate::from_ymd_opt(2026, 7, 31).expect("after boundary");

    assert!(pricing_changes_between("gpt-5.6-luna", before, boundary));
    assert!(pricing_changes_between(
        "openai/gpt-5.6-luna",
        after,
        before
    ));
    assert!(pricing_changes_between("gpt-5.6-terra", before, boundary));
    assert!(pricing_changes_between(
        "openai/gpt-5.6-terra",
        after,
        before
    ));
    assert!(!pricing_changes_between("gpt-5.6-luna", boundary, after));
    assert!(!pricing_changes_between("gpt-5.6-terra", boundary, after));
    assert!(!pricing_changes_between("gpt-5.6-sol", before, after));
}
