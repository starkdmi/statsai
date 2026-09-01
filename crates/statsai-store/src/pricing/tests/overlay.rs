use super::support::*;
use super::*;

#[test]
fn apply_current_estimated_pricing_overlays_stale_estimated_only_summary() {
    let source = test_source("/tmp/codex-import-overlay");
    let start = parse_utc("2026-07-29T00:00:00Z");
    let end = parse_utc("2026-07-29T23:59:59Z");
    let mut summary = test_summary(&source, "codex-auto-review", start, end, missing_cost());
    summary.cost.estimated_api_equivalent_usd = Some(1);
    summary.cost.estimated_api_equivalent_micro_usd = Some(10_000);
    summary.cost.pricing_source = Some("official:stale".to_string());
    summary.cost.pricing_version = Some("official:stale".to_string());

    let priced = apply_current_estimated_pricing(summary);
    assert_eq!(priced.cost, expected_review_cost(end));
}

#[test]
fn apply_current_estimated_pricing_keeps_provider_reported_amount() {
    let source = test_source("/tmp/codex-import-provider-reported");
    let start = parse_utc("2026-07-29T00:00:00Z");
    let end = parse_utc("2026-07-29T23:59:59Z");
    let mut cost = missing_cost();
    cost.provider_reported_usd = Some(99);
    cost.provider_reported_micro_usd = Some(990_000);
    cost.pricing_source = Some("provider_invoice".to_string());
    cost.confidence = Confidence::High;
    let summary = test_summary(&source, "codex-auto-review", start, end, cost);

    let priced = apply_current_estimated_pricing(summary);
    assert_eq!(priced.cost.provider_reported_usd, Some(99));
    assert_eq!(priced.cost.provider_reported_micro_usd, Some(990_000));
    assert_eq!(
        priced.cost.pricing_source.as_deref(),
        Some("provider_invoice")
    );
    assert_eq!(
        priced.cost.estimated_api_equivalent_usd,
        expected_review_cost(end).estimated_api_equivalent_usd
    );
}
