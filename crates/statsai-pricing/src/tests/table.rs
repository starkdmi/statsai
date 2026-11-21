use super::*;
use chrono::{NaiveDate, TimeZone, Utc};
use serde_json::{json, Value};
use statsai_core::UsageCounts;
use std::sync::Arc;

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
}

fn embedded_json() -> Value {
    serde_json::from_str(embedded_pricing_text()).expect("embedded pricing.json is JSON")
}

fn parse_value(value: &Value) -> PricingTable {
    PricingTable::parse(&serde_json::to_string_pretty(value).expect("serialize"))
        .expect("valid pricing file")
}

/// The embedded file with only its `version` changed, so installing it into
/// the process-global table cannot change any price other tests rely on.
fn embedded_with_version(version: u64) -> Arc<PricingTable> {
    let text = embedded_pricing_text();
    let current = format!("\"version\": {}", embedded_pricing().version());
    assert!(text.contains(&current), "embedded file spells {current}");
    let text = text.replacen(&current, &format!("\"version\": {version}"), 1);
    let table = PricingTable::parse(&text).expect("valid");
    assert_eq!(table.version(), version);
    Arc::new(table)
}

fn append_period(value: &mut Value, model: &str, period: Value) {
    value["models"][model]["prices"]
        .as_array_mut()
        .expect("prices")
        .push(period);
}

/// Adds `period` to `model`'s periods in date order, for a period that must
/// fall inside a window that has already passed.
fn insert_period(value: &mut Value, model: &str, period: Value) {
    let from = period["from"].as_str().expect("dated period").to_string();
    let periods = value["models"][model]["prices"]
        .as_array_mut()
        .expect("prices");
    let index = periods
        .iter()
        .position(|existing| {
            existing["from"]
                .as_str()
                .is_some_and(|existing| existing > from.as_str())
        })
        .unwrap_or(periods.len());
    periods.insert(index, period);
}

/// The dates of `periods` (a `prices`, `fast.prices` or `equivalents` list),
/// or none when `periods` is absent.
fn period_dates(periods: &Value) -> impl Iterator<Item = NaiveDate> + '_ {
    periods
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|period| period["from"].as_str())
        .map(|from| NaiveDate::parse_from_str(from, "%Y-%m-%d").expect("period date"))
}

/// The first day after every standard and fast period `model` has in
/// `value`, and no earlier than `at_least`. A period a test appends there
/// keeps the model's periods in order whatever periods later edits to
/// pricing.json give it.
fn day_after_last_period(value: &Value, model: &str, at_least: NaiveDate) -> NaiveDate {
    let entry = &value["models"][model];
    assert!(entry["prices"].is_array(), "{model} has prices");
    period_dates(&entry["prices"])
        .chain(period_dates(&entry["fast"]["prices"]))
        .map(|from| from + chrono::Duration::days(1))
        .fold(at_least, NaiveDate::max)
}

fn day(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

fn midnight(date: NaiveDate) -> chrono::DateTime<Utc> {
    date.and_hms_opt(0, 0, 0).expect("midnight").and_utc()
}

#[test]
fn an_event_is_priced_with_the_last_period_starting_on_or_before_its_day() {
    let table = embedded_pricing();

    let before = table
        .pricing_for_model_on("claude-sonnet-5-5", date(2026, 10, 6))
        .expect("priced");
    let on = table
        .pricing_for_model_on("claude-sonnet-5-5", date(2026, 10, 7))
        .expect("priced");
    assert!((before.cached_input_per_million - 0.2).abs() < f64::EPSILON);
    assert!((on.cached_input_per_million - 0.1).abs() < f64::EPSILON);
    assert!((before.input_per_million - on.input_per_million).abs() < f64::EPSILON);

    let sol_before = table
        .pricing_for_model_on("GPT-5.6-Sol", date(2026, 8, 20))
        .expect("priced");
    let sol_on = table
        .pricing_for_model_on("gpt-5.6-sol", date(2026, 8, 21))
        .expect("priced");
    assert!((sol_before.output_per_million - 30.0).abs() < f64::EPSILON);
    assert!((sol_on.output_per_million - 20.0).abs() < f64::EPSILON);

    // An equivalent switches target on its own boundary.
    assert_eq!(
        table.pricing_for_model_on("codex-auto-review", date(2026, 7, 29)),
        table.pricing_for_model_on("gpt-5.4", date(2026, 7, 29))
    );
    assert_eq!(
        table.pricing_for_model_on("codex-auto-review", date(2026, 7, 30)),
        table.pricing_for_model_on("gpt-5.6-luna", date(2026, 7, 30))
    );
    assert_ne!(
        table.pricing_for_model_on("codex-auto-review", date(2026, 7, 29)),
        table.pricing_for_model_on("codex-auto-review", date(2026, 7, 30))
    );
}

#[test]
fn a_middle_period_covers_the_days_up_to_the_next_start() {
    let mut value = embedded_json();
    // The test's own periods for Claude Opus 5, so a period a later edit to
    // pricing.json adds cannot change the rates it expects.
    value["models"]["claude-opus-5"]["prices"] = json!([
        { "from": null, "input": 5, "cache_write": 6.25, "cache_read": 0.5, "output": 25 }
    ]);
    let middle = date(2027, 2, 1);
    let last = middle + chrono::Duration::days(28);
    append_period(
        &mut value,
        "claude-opus-5",
        json!({ "from": day(middle), "input": 4, "cache_write": 5, "cache_read": 0.4, "output": 20 }),
    );
    append_period(
        &mut value,
        "claude-opus-5",
        json!({ "from": day(last), "input": 3, "cache_write": 3.75, "cache_read": 0.3, "output": 15 }),
    );
    let table = parse_value(&value);
    let input_on = |day: NaiveDate| {
        table
            .pricing_for_model_on("claude-opus-5", day)
            .expect("priced")
            .input_per_million
    };

    assert!((input_on(middle - chrono::Duration::days(1)) - 5.0).abs() < f64::EPSILON);
    assert!((input_on(middle) - 4.0).abs() < f64::EPSILON);
    assert!((input_on(last - chrono::Duration::days(1)) - 4.0).abs() < f64::EPSILON);
    assert!((input_on(last) - 3.0).abs() < f64::EPSILON);
    assert!((input_on(NaiveDate::MAX) - 3.0).abs() < f64::EPSILON);
    assert!((input_on(NaiveDate::MIN) - 5.0).abs() < f64::EPSILON);
}

#[test]
fn an_appended_price_period_reprices_later_events_without_touching_the_global() {
    let mut value = embedded_json();
    value["version"] = json!(embedded_pricing().version() + 1);
    let boundary = day_after_last_period(&value, "claude-opus-5", date(2027, 1, 1));
    let day_before = boundary - chrono::Duration::days(1);
    // The embedded rates on the day before the boundary (5 USD input per
    // million tokens in pricing version 7). The embedded file has no period
    // starting on the boundary, so it bills the boundary at them too.
    let embedded = embedded_pricing()
        .pricing_for_model_on("claude-opus-5", day_before)
        .expect("priced");
    assert_eq!(
        embedded_pricing().pricing_for_model_on("claude-opus-5", boundary),
        Some(embedded)
    );
    // The new period cuts every rate to a fifth: 1 USD input in version 7.
    let cut = |rate: f64| rate / 5.0;
    append_period(
        &mut value,
        "claude-opus-5",
        json!({
            "from": day(boundary),
            "input": cut(embedded.input_per_million),
            "cache_write": cut(embedded.cache_creation_per_million),
            "cache_read": cut(embedded.cached_input_per_million),
            "output": cut(embedded.output_per_million)
        }),
    );
    let table = parse_value(&value);
    let micro_usd_per_million = |rate: f64| (rate * 1_000_000.0).round() as i64;
    let old_micro_usd = micro_usd_per_million(embedded.input_per_million);
    let new_micro_usd = micro_usd_per_million(cut(embedded.input_per_million));
    assert_ne!(old_micro_usd, new_micro_usd);

    let model = statsai_core::ModelInfo {
        name: Some("claude-opus-5".to_string()),
        normalized_name: Some("claude-opus-5".to_string()),
        provider_model_id: Some("claude-opus-5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    };
    let usage = UsageCounts {
        input_tokens: Some(1_000_000),
        ..UsageCounts::default()
    };
    let after = midnight(boundary);
    let before = after - chrono::Duration::seconds(1);

    let old_rate = table.estimate_cost_at("claude_code", Some(&model), &usage, &before);
    let new_rate = table.estimate_cost_at("claude_code", Some(&model), &usage, &after);
    assert_eq!(
        old_rate.estimated_api_equivalent_micro_usd,
        Some(old_micro_usd)
    );
    assert_eq!(
        new_rate.estimated_api_equivalent_micro_usd,
        Some(new_micro_usd)
    );
    assert_eq!(new_rate.pricing_version.as_deref(), Some(table.catalog()));

    assert!(table.pricing_changes_between("claude-opus-5", day_before, boundary));
    assert!(table.pricing_changes_between(
        "anthropic/claude-opus-5-thinking",
        boundary,
        day_before
    ));
    assert!(!table.pricing_changes_between(
        "claude-opus-5",
        boundary,
        boundary + chrono::Duration::days(151)
    ));

    // Neither the embedded nor the process-global table saw the new period.
    assert!(!embedded_pricing().pricing_changes_between("claude-opus-5", day_before, boundary));
    assert!(!pricing_changes_between(
        "claude-opus-5",
        day_before,
        boundary
    ));
    assert_eq!(
        estimate_cost_at("claude_code", Some(&model), &usage, &after)
            .estimated_api_equivalent_micro_usd,
        Some(old_micro_usd)
    );
}

#[test]
fn fast_multiplier_follows_the_standard_period_in_effect() {
    let table = embedded_pricing();
    let model = statsai_core::ModelInfo {
        name: Some("gpt-5.6-luna".to_string()),
        normalized_name: None,
        provider_model_id: None,
        speed: Some("fast".to_string()),
        reasoning_level: None,
        reasoning_level_raw: None,
    };
    let usage = UsageCounts {
        input_tokens: Some(1_000_000),
        ..UsageCounts::default()
    };
    let before = Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).unwrap();
    let after = Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap();

    assert_eq!(
        table
            .estimate_cost_at("codex", Some(&model), &usage, &before)
            .estimated_api_equivalent_micro_usd,
        Some(2_000_000)
    );
    assert_eq!(
        table
            .estimate_cost_at("codex", Some(&model), &usage, &after)
            .estimated_api_equivalent_micro_usd,
        Some(400_000)
    );
}

#[test]
fn equivalent_boundaries_include_target_changes_only_inside_their_window() {
    let embedded = embedded_pricing();
    assert!(embedded.pricing_changes_between(
        "codex-auto-review",
        date(2026, 7, 29),
        date(2026, 7, 30)
    ));
    assert!(!embedded.pricing_changes_between(
        "codex-auto-review",
        date(2026, 1, 1),
        date(2026, 7, 29)
    ));
    // From the switch to GPT-5.6 Luna up to the day before the next dated
    // change the embedded file has for either (none in pricing version 7).
    let file = embedded_json();
    let switch = date(2026, 7, 30);
    let luna_window_end = period_dates(&file["equivalents"]["codex-auto-review"])
        .chain(period_dates(&file["models"]["gpt-5.6-luna"]["prices"]))
        .chain(period_dates(
            &file["models"]["gpt-5.6-luna"]["fast"]["prices"],
        ))
        .filter(|from| *from > switch)
        .map(|from| from - chrono::Duration::days(1))
        .fold(date(2027, 6, 1), NaiveDate::min);
    assert!(!embedded.pricing_changes_between("codex-auto-review", switch, luna_window_end));

    let mut value = file;
    // The test's own mapping: GPT-5.4 until 2026-07-30, then GPT-5.6 Luna
    // with no end, whatever later edits to pricing.json add.
    value["equivalents"]["codex-auto-review"] = json!([
        { "from": null, "model": "gpt-5.4" },
        { "from": "2026-07-30", "model": "gpt-5.6-luna" }
    ]);
    // Inside codex-auto-review's GPT-5.4 window, which closed on 2026-07-30:
    // a boundary for it too.
    insert_period(
        &mut value,
        "gpt-5.4",
        json!({ "from": "2026-05-01", "input": 2, "cache_write": 2, "cache_read": 0.2, "output": 12 }),
    );
    // After the window closes: GPT-5.4 changes, codex-auto-review does not.
    // The change comes after every GPT-5.6 Luna period too, so no Luna
    // boundary falls on it.
    let after_luna = day_after_last_period(&value, "gpt-5.6-luna", date(2026, 9, 1));
    let gpt_5_4_change = day_after_last_period(&value, "gpt-5.4", after_luna);
    append_period(
        &mut value,
        "gpt-5.4",
        json!({ "from": day(gpt_5_4_change), "input": 1, "cache_write": 1, "cache_read": 0.1, "output": 6 }),
    );
    // Inside the GPT-5.6 Luna window, which has no end, after the GPT-5.4
    // change.
    let luna_change = day_after_last_period(
        &value,
        "gpt-5.6-luna",
        date(2027, 1, 1).max(gpt_5_4_change + chrono::Duration::days(1)),
    );
    append_period(
        &mut value,
        "gpt-5.6-luna",
        json!({ "from": day(luna_change), "input": 0.1, "cache_write": 0.125, "cache_read": 0.01, "output": 0.6 }),
    );
    let table = parse_value(&value);
    let one_day = chrono::Duration::days(1);

    assert!(table.pricing_changes_between(
        "codex-auto-review",
        date(2026, 4, 30),
        date(2026, 5, 1)
    ));
    assert!(table.pricing_changes_between("gpt-5.4", gpt_5_4_change - one_day, gpt_5_4_change));
    assert!(!table.pricing_changes_between(
        "codex-auto-review",
        gpt_5_4_change - one_day,
        gpt_5_4_change
    ));
    assert!(table.pricing_changes_between("codex-auto-review", luna_change - one_day, luna_change));
    assert!(table.pricing_changes_between(
        "codex-auto-review",
        date(2026, 7, 29),
        date(2026, 7, 30)
    ));
}

#[test]
fn active_pricing_starts_from_the_embedded_file() {
    let active = active_pricing();
    let embedded = embedded_pricing();
    assert!(active.version() >= embedded.version());
    assert_eq!(active.catalog(), embedded.catalog());
    assert_eq!(active.format(), embedded.format());
}

#[test]
fn install_pricing_never_lowers_the_version() {
    let embedded_version = embedded_pricing().version();
    let base = active_pricing().version();
    assert!(base >= embedded_version);

    assert!(!install_pricing(embedded_with_version(base)));
    assert_eq!(active_pricing().version(), base);

    let higher = embedded_with_version(base + 1);
    assert!(install_pricing(Arc::clone(&higher)));
    assert!(Arc::ptr_eq(&active_pricing(), &higher));
    assert_eq!(active_pricing().version(), base + 1);

    assert!(!install_pricing(embedded_with_version(base + 1)));
    assert!(Arc::ptr_eq(&active_pricing(), &higher));

    assert!(!install_pricing(embedded_with_version(1)));
    assert!(!install_pricing(embedded_pricing()));
    assert_eq!(active_pricing().version(), base + 1);
    assert_eq!(active_pricing().catalog(), embedded_pricing().catalog());

    // The embedded table itself never changes.
    assert_eq!(embedded_pricing().version(), embedded_version);
}

#[test]
fn refresh_is_disabled_only_by_an_explicit_off_value() {
    for value in ["0", "off", "OFF", " Off ", "false", "FALSE", "no", "No\n"] {
        assert!(crate::table::refresh_value_disables(value), "{value:?}");
    }
    for value in ["", "1", "on", "true", "yes", "nope", "disabled", "o f f"] {
        assert!(!crate::table::refresh_value_disables(value), "{value:?}");
    }
    assert_eq!(PRICING_REFRESH_ENV, "STATSAI_PRICING_REFRESH");
}

#[test]
fn debug_output_names_the_table_without_its_text() {
    let debug = format!("{:?}", embedded_pricing());
    assert!(debug.contains("PricingTable"), "{debug}");
    assert!(debug.contains(embedded_pricing().catalog()), "{debug}");
    assert!(debug.len() < 300, "{debug}");
}
