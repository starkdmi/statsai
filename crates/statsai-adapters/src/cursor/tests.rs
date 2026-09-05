use super::*;
use crate::tests::options;
use statsai_core::{ReasoningLevel, SourceKind};

fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cursor")
        .join(relative)
}

fn parse(relative: &str) -> CursorImportReport {
    parse_cursor_usage_csv(&fixture(relative), &options()).expect("parse cursor csv")
}

fn event_for<'a>(report: &'a CursorImportReport, model: &str) -> &'a UsageEvent {
    report
        .events
        .iter()
        .find(|event| {
            event
                .model
                .as_ref()
                .and_then(|model| model.name.as_deref())
                .is_some_and(|name| name == model)
        })
        .unwrap_or_else(|| panic!("no event for model {model}"))
}

fn total_tokens(report: &CursorImportReport) -> u64 {
    report
        .events
        .iter()
        .map(|event| event.usage.computed_total())
        .sum()
}

#[test]
fn parses_token_columns_timestamps_and_sessions() {
    let report = parse("basic/usage-events-basic.csv");

    assert_eq!(report.events.len(), 6);
    assert_eq!(report.rows_read, 6);
    assert_eq!(report.rows_skipped, 0);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);

    let event = event_for(&report, "cursor-grok-4.6-high");
    assert_eq!(event.usage.input_tokens, Some(2000));
    assert_eq!(event.usage.cache_creation_tokens, Some(1000));
    assert_eq!(event.usage.cache_read_tokens, Some(3000));
    assert_eq!(event.usage.output_tokens, Some(4000));
    assert_eq!(event.usage.total_tokens, Some(10000));
    assert_eq!(
        event
            .created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "2026-01-05T10:20:00.000Z"
    );
    assert_eq!(event.provider, CURSOR_PROVIDER);
    assert_eq!(event.source.source_kind, SourceKind::Manual);

    // Rows from one cloud agent share a session; a local row does not join it.
    let fast = event_for(&report, "cursor-grok-4.6-high-fast");
    assert_eq!(event.session.session_id, fast.session.session_id);
    let local = event_for(&report, "claude-4.5-sonnet");
    assert_ne!(event.session.session_id, local.session.session_id);
}

#[test]
fn reads_speed_and_reasoning_from_the_model_suffix() {
    let report = parse("basic/usage-events-basic.csv");

    let fast = event_for(&report, "cursor-grok-4.6-high-fast")
        .model
        .as_ref();
    let fast = fast.expect("model");
    assert_eq!(fast.speed.as_deref(), Some("fast"));
    assert_eq!(fast.reasoning_level, Some(ReasoningLevel::High));
    assert_eq!(fast.normalized_name.as_deref(), Some("grok-4.6"));

    let standard = event_for(&report, "cursor-grok-4.6-high").model.as_ref();
    let standard = standard.expect("model");
    assert_eq!(standard.speed, None);
    assert_eq!(standard.reasoning_level, Some(ReasoningLevel::High));

    // Cursor's `-max` is its 1M-context tier, not a fast-mode or price flag.
    let max = event_for(&report, "claude-fable-5-1-thinking-max")
        .model
        .as_ref()
        .expect("model");
    assert_eq!(max.speed, None);
    assert_eq!(max.normalized_name.as_deref(), Some("claude-fable-5-1"));
}

#[test]
fn fast_mode_costs_twice_the_standard_rate() {
    let report = parse("basic/usage-events-basic.csv");

    let standard = event_for(&report, "cursor-grok-4.6-high");
    let fast = event_for(&report, "cursor-grok-4.6-high-fast");

    let standard_cost = standard
        .cost
        .estimated_api_equivalent_micro_usd
        .expect("standard cost");
    let fast_cost = fast
        .cost
        .estimated_api_equivalent_micro_usd
        .expect("fast cost");
    assert_eq!(fast_cost, standard_cost * 2);
}

#[test]
fn reports_unpriced_models_without_dropping_their_tokens() {
    let report = parse("basic/usage-events-basic.csv");

    let unpriced = unpriced_model_tokens(&report.events);
    assert_eq!(unpriced.get("grok-bot-automation"), Some(&8000));
    assert_eq!(unpriced.get("gemini-3.1-pro"), Some(&1000));
    assert!(!unpriced.contains_key("cursor-grok-4.6-high"));
    assert!(!unpriced.contains_key("claude-4.5-sonnet"));

    let bot = event_for(&report, "grok-bot-automation");
    assert_eq!(bot.usage.total_tokens, Some(8000));
    assert_eq!(bot.cost.estimated_api_equivalent_micro_usd, None);
    assert_eq!(bot.cost.pricing_source.as_deref(), Some("unknown"));
}

#[test]
fn records_a_reported_charge_alongside_the_estimate() {
    let report = parse("usage-based/usage-events-charged.csv");

    let charged = event_for(&report, "cursor-grok-4.6-high");
    assert_eq!(charged.cost.provider_reported_micro_usd, Some(123_400));
    assert_eq!(
        charged.cost.pricing_source.as_deref(),
        Some("cursor.usage_event.cost")
    );
    assert_eq!(charged.cost.confidence, Confidence::High);
    // The API-equivalent estimate is still computed next to the real charge.
    assert!(charged.cost.estimated_api_equivalent_micro_usd.is_some());

    // A bare decimal is a charge too.
    let bare = event_for(&report, "claude-4.5-sonnet");
    assert_eq!(bare.cost.provider_reported_micro_usd, Some(2_500_000));
}

#[test]
fn treats_cost_labels_including_unknown_ones_as_no_charge() {
    let report = parse("usage-based/usage-events-charged.csv");

    for model in [
        "cursor-grok-4.6-medium",
        "cursor-grok-4.6-low",
        "cursor-grok-4.5-high",
    ] {
        let event = event_for(&report, model);
        assert_eq!(event.cost.provider_reported_micro_usd, None, "{model}");
        assert_eq!(event.cost.provider_reported_usd, None, "{model}");
    }
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[test]
fn sums_rows_that_share_every_immutable_field() {
    let report = parse("collision/usage-events-collision.csv");

    // Three rows, two of which collide, so two events.
    assert_eq!(report.rows_read, 3);
    assert_eq!(report.events.len(), 2);

    let merged = event_for(&report, "cursor-grok-4.6-medium");
    assert_eq!(merged.usage.input_tokens, Some(2400));
    assert_eq!(merged.usage.cache_creation_tokens, Some(1200));
    assert_eq!(merged.usage.cache_read_tokens, Some(3600));
    assert_eq!(merged.usage.output_tokens, Some(4800));
    assert_eq!(merged.usage.total_tokens, Some(12000));
}

#[test]
fn colliding_row_order_does_not_change_the_result() {
    let ordered = parse("collision/usage-events-collision.csv");
    let swapped = parse("collision/usage-events-collision-swapped.csv");

    let ids = |report: &CursorImportReport| {
        let mut ids: Vec<String> = report
            .events
            .iter()
            .map(|event| event.event_id.0.clone())
            .collect();
        ids.sort();
        ids
    };
    let usage = |report: &CursorImportReport| {
        let mut usage: Vec<(String, u64)> = report
            .events
            .iter()
            .map(|event| (event.event_id.0.clone(), event.usage.computed_total()))
            .collect();
        usage.sort();
        usage
    };

    assert_eq!(ids(&ordered), ids(&swapped));
    assert_eq!(usage(&ordered), usage(&swapped));
}

#[test]
fn event_ids_ignore_token_counts_so_a_growing_row_keeps_its_identity() {
    let early = parse("snapshots/export-early.csv");
    let late = parse("snapshots/export-late.csv");

    let early_grok = event_for(&early, "cursor-grok-4.6-high");
    let late_grok = event_for(&late, "cursor-grok-4.6-high");

    assert_eq!(early_grok.event_id, late_grok.event_id);
    assert_eq!(early_grok.usage.total_tokens, Some(10000));
    assert_eq!(late_grok.usage.total_tokens, Some(40000));
    assert_eq!(early.events.len(), 2);
    assert_eq!(late.events.len(), 3);
}

#[test]
fn skips_unusable_rows_with_warnings_and_keeps_the_rest() {
    let report = parse("malformed/usage-events-malformed.csv");

    assert_eq!(report.rows_read, 5);
    assert_eq!(report.rows_skipped, 3);
    assert_eq!(report.events.len(), 2);
    assert_eq!(report.warnings.len(), 3);
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("unparseable date")));
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("unreadable token counts")));
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("missing model")));

    // An unknown trailing column does not disturb the rows that do parse.
    assert_eq!(total_tokens(&report), 11000);
}

#[test]
fn parses_a_header_without_the_optional_agent_columns() {
    let report = parse("malformed/usage-events-legacy-header.csv");

    assert_eq!(report.events.len(), 2);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let event = event_for(&report, "cursor-grok-4.5-high");
    assert_eq!(event.usage.total_tokens, Some(10000));
}

#[test]
fn a_header_without_a_required_column_is_an_error() {
    let error = parse_cursor_usage_csv(&fixture("malformed/usage-events-no-date.csv"), &options())
        .expect_err("missing Date column");

    assert!(
        error.to_string().contains("Date"),
        "unexpected error: {error}"
    );
}

#[test]
fn every_import_writes_to_one_stable_source() {
    let early = parse("snapshots/export-early.csv");
    let late = parse("snapshots/export-late.csv");

    assert_eq!(early.source.source_id, late.source.source_id);
    assert_eq!(early.source.source_id, cursor_import_source().source_id);
}

#[test]
fn reports_the_date_range_the_file_covers() {
    let report = parse("snapshots/export-late.csv");

    let (start, end) = report.range.expect("range");
    assert_eq!(
        start.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "2026-01-06T20:00:00.000Z"
    );
    assert_eq!(
        end.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "2026-01-07T11:00:00.000Z"
    );
}

#[test]
fn a_directory_path_collects_every_export_in_sorted_order() {
    let directory = fixture("collision");
    let paths = cursor_csv_paths(&directory).expect("collect paths");

    assert_eq!(paths.len(), 2);
    assert!(paths[0].ends_with("usage-events-collision-swapped.csv"));
    assert!(paths[1].ends_with("usage-events-collision.csv"));

    let file = fixture("basic/usage-events-basic.csv");
    assert_eq!(cursor_csv_paths(&file).expect("single file"), vec![file]);
}

#[test]
fn the_cursor_importer_is_not_a_discoverable_adapter() {
    assert!(default_adapters()
        .iter()
        .all(|adapter| adapter.provider() != CURSOR_PROVIDER));
    assert!(adapter_for_provider(CURSOR_PROVIDER).is_none());
    assert!(CursorCsvImport.discover().is_empty());
}

#[test]
fn a_charged_row_is_not_counted_as_unpriced() {
    let report = parse("usage-based/usage-events-charged.csv");

    let unpriced = unpriced_model_tokens(&report.events);

    // Every model here either prices from the catalog or carries a real charge.
    assert!(unpriced.is_empty(), "{unpriced:?}");
}
