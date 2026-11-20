use super::*;
use serde_json::{json, Value};

fn embedded_json() -> Value {
    serde_json::from_str(embedded_pricing_text()).expect("embedded pricing.json is JSON")
}

fn parse_value(value: &Value) -> Result<PricingTable, PricingFileError> {
    PricingTable::parse(&serde_json::to_string_pretty(value).expect("serialize"))
}

/// A small valid file to vary one field at a time.
fn minimal() -> Value {
    json!({
        "format": 1,
        "version": 3,
        "catalog": "test:2026-01-01",
        "models": {
            "model-a": {
                "note": "free text",
                "prices": [
                    { "from": null, "input": 1, "cache_write": 1.25, "cache_read": 0.1, "output": 5 },
                    { "from": "2026-03-01", "input": 2, "cache_write": 2.5, "cache_read": 0.2, "output": 10, "note": "rise" }
                ],
                "long_context": { "above": 1000, "input": 2, "output": 1.5 },
                "fast": { "multiplier": 2, "max_prompt": 5000 }
            },
            "model-b": {
                "source": "vendor_model_pricing",
                "prices": [
                    { "from": null, "input": 3, "cache_write": 3, "cache_read": 0.3, "output": 15 }
                ],
                "fast": {
                    "prices": [
                        { "from": null, "input": 6, "cache_write": 6, "cache_read": 0.6, "output": 30 }
                    ],
                    "long_context": { "at_least": 2000, "input": 1.5, "output": 1.5 }
                }
            }
        },
        "equivalents": {
            "alias-a": [
                { "from": null, "model": "model-a", "note": "first" },
                { "from": "2026-06-01", "model": "model-b" }
            ]
        },
        "names": {
            "model-a": ["model-a-thinking", "model.a"],
            "model-b": []
        },
        "patterns": [
            { "contains": ["model-a-2", "model.a.2"], "model": "model-a", "whole_version": true, "note": "why" },
            { "contains": ["model-b"], "model": "model-b" }
        ],
        "reversed": {
            "claude-9-opus": "model-a"
        }
    })
}

#[track_caller]
fn assert_invalid(value: &Value, needle: &str) {
    match parse_value(value) {
        Err(PricingFileError::Invalid(message)) => assert!(
            message.contains(needle),
            "expected an error mentioning {needle:?}, got {message:?}"
        ),
        Err(other) => panic!("expected Invalid mentioning {needle:?}, got {other:?}"),
        Ok(table) => panic!("expected Invalid mentioning {needle:?}, parsed {table:?}"),
    }
}

#[test]
fn embedded_file_parses_and_validates() {
    let text = embedded_pricing_text();
    let table = PricingTable::parse(text).expect("the embedded pricing.json must be valid");
    let header = PricingTable::header(text).expect("header");

    assert_eq!(u64::from(table.format()), header.format);
    assert_eq!(table.format(), PRICING_FORMAT);
    assert_eq!(table.version(), header.version);
    assert!(table.version() >= 1);
    assert!(table.catalog().contains(':'));
    assert_eq!(embedded_pricing().version(), table.version());
    assert_eq!(embedded_pricing().catalog(), table.catalog());
    assert!(table
        .pricing_for_model_on("claude-opus-5", chrono::NaiveDate::MIN)
        .is_some());
}

#[test]
fn minimal_file_parses() {
    let table = parse_value(&minimal()).expect("minimal file is valid");

    assert_eq!(table.format(), 1);
    assert_eq!(table.version(), 3);
    assert_eq!(table.catalog(), "test:2026-01-01");
    assert_eq!(table.normalize_model_name("Model.A"), "model-a");
    assert_eq!(table.normalize_model_name("x-model-a-2-y"), "model-a");
    assert_eq!(table.normalize_model_name("x-model-a-20"), "x-model-a-20");
    assert_eq!(table.normalize_model_name("claude-9-opus-max"), "model-a");
}

#[test]
fn source_text_round_trips_exactly() {
    let embedded = embedded_pricing();
    assert_eq!(embedded.source_text(), embedded_pricing_text());

    let compact = serde_json::to_string(&minimal()).expect("serialize");
    let text = format!("\n  {compact}\t\n");
    let table = PricingTable::parse(&text).expect("valid");
    assert_eq!(table.source_text(), text);

    let reparsed = PricingTable::parse(table.source_text()).expect("reparse");
    assert_eq!(reparsed.version(), table.version());
    assert_eq!(reparsed.catalog(), table.catalog());
    assert_eq!(reparsed.source_text(), text);
}

#[test]
fn header_reads_only_format_and_version() {
    assert_eq!(
        PricingTable::header(r#"{"format": 9, "version": 12, "layout": {"anything": [1, 2]}}"#),
        Ok(PricingFileHeader {
            format: 9,
            version: 12
        })
    );
    assert!(matches!(
        PricingTable::header(r#"{"format": 1}"#),
        Err(PricingFileError::Invalid(_))
    ));
    assert!(matches!(
        PricingTable::header(r#"{"format": "1", "version": 2}"#),
        Err(PricingFileError::Invalid(_))
    ));
    assert!(matches!(
        PricingTable::header("not json"),
        Err(PricingFileError::Invalid(_))
    ));
}

#[test]
fn unknown_format_is_reported_as_unknown_not_invalid() {
    let mut value = embedded_json();
    value["format"] = json!(2);
    assert_eq!(
        parse_value(&value).unwrap_err(),
        PricingFileError::UnknownFormat {
            format: 2,
            supported: PRICING_FORMAT
        }
    );

    // A future format may reshape everything but `format` and `version`.
    assert_eq!(
        PricingTable::parse(r#"{"format": 7, "version": 40, "prices_v7": []}"#).unwrap_err(),
        PricingFileError::UnknownFormat {
            format: 7,
            supported: PRICING_FORMAT
        }
    );
    assert_eq!(
        PricingTable::parse(r#"{"format": 0, "version": 1}"#).unwrap_err(),
        PricingFileError::UnknownFormat {
            format: 0,
            supported: PRICING_FORMAT
        }
    );
    assert_eq!(
        PricingTable::parse(&format!(r#"{{"format": {}, "version": 1}}"#, u64::MAX)).unwrap_err(),
        PricingFileError::UnknownFormat {
            format: u64::MAX,
            supported: PRICING_FORMAT
        }
    );
    let message = PricingFileError::UnknownFormat {
        format: 2,
        supported: 1,
    }
    .to_string();
    assert!(message.contains('2') && message.contains('1'), "{message}");
}

#[test]
fn rejects_version_zero() {
    let mut value = embedded_json();
    value["version"] = json!(0);
    assert_invalid(&value, "version");

    value["version"] = json!(-1);
    assert!(matches!(
        parse_value(&value),
        Err(PricingFileError::Invalid(_))
    ));
}

#[test]
fn rejects_negative_and_out_of_bounds_rates() {
    let mut value = embedded_json();
    value["models"]["claude-opus-5"]["prices"][0]["input"] = json!(-0.01);
    assert_invalid(&value, "input");

    let mut value = embedded_json();
    value["models"]["claude-opus-5"]["prices"][0]["output"] = json!(1000.01);
    assert_invalid(&value, "output");

    let mut value = embedded_json();
    value["models"]["claude-opus-5"]["fast"]["prices"][0]["cache_read"] = json!(5000);
    assert_invalid(&value, "cache_read");

    let mut value = minimal();
    value["models"]["model-a"]["prices"][1]["cache_write"] = json!(-3);
    assert_invalid(&value, "cache_write");

    // The bounds themselves are allowed.
    let mut value = minimal();
    value["models"]["model-a"]["prices"][0]["input"] = json!(0);
    value["models"]["model-a"]["prices"][0]["output"] = json!(1000);
    parse_value(&value).expect("0 and 1000 are within bounds");
}

#[test]
fn rejects_out_of_range_multipliers() {
    for bad in [json!(0), json!(-2), json!(20.01), json!(1e9)] {
        let mut value = embedded_json();
        value["models"]["gpt-5"]["fast"]["multiplier"] = bad.clone();
        assert_invalid(&value, "multiplier");

        let mut value = embedded_json();
        value["models"]["gpt-5.5"]["long_context"]["input"] = bad.clone();
        assert_invalid(&value, "multiplier");

        let mut value = embedded_json();
        value["models"]["grok-4.7"]["fast"]["long_context"]["output"] = bad;
        assert_invalid(&value, "multiplier");
    }

    let mut value = minimal();
    value["models"]["model-a"]["fast"]["multiplier"] = json!(20);
    parse_value(&value).expect("20 is within bounds");
}

#[test]
fn rejects_out_of_range_token_thresholds() {
    for bad in [json!(0), json!(100_000_001)] {
        let mut value = minimal();
        value["models"]["model-a"]["long_context"]["above"] = bad.clone();
        assert_invalid(&value, "above");

        let mut value = minimal();
        value["models"]["model-b"]["fast"]["long_context"]["at_least"] = bad.clone();
        assert_invalid(&value, "at_least");

        let mut value = minimal();
        value["models"]["model-a"]["fast"]["max_prompt"] = bad;
        assert_invalid(&value, "max_prompt");
    }
}

#[test]
fn rejects_ambiguous_long_context_and_fast() {
    let mut value = minimal();
    value["models"]["model-a"]["long_context"]["at_least"] = json!(1000);
    assert_invalid(&value, "exactly one of `above` or `at_least`");

    let mut value = minimal();
    value["models"]["model-a"]["long_context"] = json!({ "input": 2, "output": 2 });
    assert_invalid(&value, "exactly one of `above` or `at_least`");

    let mut value = minimal();
    value["models"]["model-b"]["fast"]["multiplier"] = json!(2);
    assert_invalid(&value, "exactly one of `multiplier` or `prices`");

    let mut value = minimal();
    value["models"]["model-a"]["fast"] = json!({ "max_prompt": 10 });
    assert_invalid(&value, "exactly one of `multiplier` or `prices`");
}

#[test]
fn rejects_a_first_period_with_a_date() {
    let mut value = embedded_json();
    value["models"]["claude-opus-5"]["prices"][0]["from"] = json!("2026-01-01");
    assert_invalid(&value, "first period");

    let mut value = minimal();
    value["models"]["model-b"]["fast"]["prices"][0]["from"] = json!("2026-01-01");
    assert_invalid(&value, "first period");

    let mut value = minimal();
    value["equivalents"]["alias-a"][0]["from"] = json!("2026-01-01");
    assert_invalid(&value, "first period");
}

#[test]
fn rejects_missing_and_empty_periods() {
    let mut value = minimal();
    value["models"]["model-a"]["prices"] = json!([]);
    assert_invalid(&value, "at least one period");

    let mut value = minimal();
    value["models"]["model-a"]["prices"][1]["from"] = Value::Null;
    assert_invalid(&value, "needs a `from` date");

    let mut value = minimal();
    value["equivalents"]["alias-a"] = json!([]);
    assert_invalid(&value, "at least one period");
}

#[test]
fn rejects_non_increasing_dates() {
    for later in ["2026-03-01", "2026-02-28"] {
        let mut value = minimal();
        value["models"]["model-a"]["prices"]
            .as_array_mut()
            .expect("prices")
            .push(json!({ "from": later, "input": 3, "cache_write": 3, "cache_read": 0.3, "output": 15 }));
        assert_invalid(&value, "strictly increase");
    }

    for later in ["2026-06-01", "2026-05-31"] {
        let mut value = minimal();
        value["equivalents"]["alias-a"]
            .as_array_mut()
            .expect("equivalents")
            .push(json!({ "from": later, "model": "model-a" }));
        assert_invalid(&value, "strictly increase");
    }

    let mut value = minimal();
    value["equivalents"]["alias-a"]
        .as_array_mut()
        .expect("equivalents")
        .push(json!({ "from": "2026-06-02", "model": "model-a" }));
    parse_value(&value).expect("a strictly later date is valid");
}

#[test]
fn rejects_bad_date_strings() {
    for bad in [
        "2026-13-01",
        "2026-02-30",
        "2026-8-1",
        "2026/08/01",
        "20260801",
        "2026-08-01T00:00:00Z",
        " 2026-08-01",
        "yesterday",
        "",
    ] {
        let mut value = minimal();
        value["models"]["model-a"]["prices"][1]["from"] = json!(bad);
        assert_invalid(&value, "YYYY-MM-DD");
    }
}

#[test]
fn rejects_unknown_fields_everywhere() {
    let mut value = embedded_json();
    value["extra"] = json!(true);
    assert_invalid(&value, "unknown field");

    let mut value = minimal();
    value["models"]["model-a"]["rate"] = json!(1);
    assert_invalid(&value, "unknown field");

    let mut value = minimal();
    value["models"]["model-a"]["prices"][0]["batch"] = json!(0.5);
    assert_invalid(&value, "unknown field");

    let mut value = minimal();
    value["models"]["model-a"]["fast"]["note"] = json!("not allowed here");
    assert_invalid(&value, "unknown field");

    let mut value = minimal();
    value["models"]["model-a"]["long_context"]["note"] = json!("not allowed here");
    assert_invalid(&value, "unknown field");

    let mut value = minimal();
    value["equivalents"]["alias-a"][0]["source"] = json!("x");
    assert_invalid(&value, "unknown field");

    let mut value = minimal();
    value["patterns"][0]["exact"] = json!(true);
    assert_invalid(&value, "unknown field");
}

#[test]
fn rejects_duplicate_object_keys() {
    let text = serde_json::to_string(&minimal()).expect("serialize");
    let duplicated = text.replacen(
        r#""model-b":[]"#,
        r#""model-b":[],"model-b":["model-b-2"]"#,
        1,
    );
    assert_ne!(duplicated, text, "the names entry is present");
    match PricingTable::parse(&duplicated) {
        Err(PricingFileError::Invalid(message)) => {
            assert!(message.contains("duplicate key"), "{message}");
        }
        other => panic!("expected a duplicate-key error, got {other:?}"),
    }
}

#[test]
fn rejects_a_missing_equivalent_target() {
    let mut value = embedded_json();
    value["equivalents"]["codex-auto-review"][1]["model"] = json!("gpt-9-unpriced");
    assert_invalid(&value, "not in `models`");
}

#[test]
fn rejects_a_name_that_is_both_a_model_and_an_equivalent() {
    let mut value = minimal();
    value["equivalents"]["model-b"] = json!([{ "from": null, "model": "model-a" }]);
    assert_invalid(&value, "both `models` and `equivalents`");
}

#[test]
fn rejects_duplicate_spellings() {
    let mut value = embedded_json();
    value["names"]["claude-opus-5"]
        .as_array_mut()
        .expect("spellings")
        .push(json!("claude-opus-5-5-thinking"));
    assert_invalid(&value, "more than once");

    let mut value = minimal();
    value["names"]["model-a"]
        .as_array_mut()
        .expect("spellings")
        .push(json!("model.a"));
    assert_invalid(&value, "more than once");

    let mut value = minimal();
    value["names"]["model-a"]
        .as_array_mut()
        .expect("spellings")
        .push(json!("model-b"));
    assert_invalid(&value, "is itself a `names` key");
}

#[test]
fn rejects_malformed_names() {
    let long = "m".repeat(129);
    for bad in ["Model-A", "model a", "", "model-\u{e9}", long.as_str()] {
        let mut value = minimal();
        let entry = value["models"]["model-b"].clone();
        value["models"][bad] = entry;
        assert_invalid(&value, "lowercase ASCII");

        let mut value = minimal();
        value["names"]["model-a"] = json!([bad]);
        assert_invalid(&value, "lowercase ASCII");

        let mut value = minimal();
        value["patterns"][1]["model"] = json!(bad);
        assert_invalid(&value, "lowercase ASCII");
    }

    let mut value = minimal();
    value["patterns"][1]["contains"] = json!([]);
    assert_invalid(&value, "`contains` is empty");

    let mut value = minimal();
    value["reversed"]["opus-9-claude"] = json!("model-a");
    assert_invalid(&value, "claude-<version>-<family>");

    let mut value = minimal();
    value["reversed"]["claude-9-opus-max"] = json!("model-a");
    assert_invalid(&value, "claude-<version>-<family>");
}

#[test]
fn rejects_a_bad_catalog_or_source() {
    let long = format!("x:{}", "y".repeat(100));
    for bad in ["", "no-colon", long.as_str()] {
        let mut value = minimal();
        value["catalog"] = json!(bad);
        assert_invalid(&value, "catalog");
    }

    for bad in ["", "Cursor", "cursor-model", "cursor model"] {
        let mut value = minimal();
        value["models"]["model-b"]["source"] = json!(bad);
        assert_invalid(&value, "source");
    }
}

#[test]
fn rejects_an_oversized_file() {
    let mut value = minimal();
    value["models"]["model-a"]["note"] = json!("x".repeat(1024 * 1024));
    let text = serde_json::to_string(&value).expect("serialize");
    assert!(matches!(
        PricingTable::header(&text),
        Err(PricingFileError::Invalid(_))
    ));
    match PricingTable::parse(&text) {
        Err(PricingFileError::Invalid(message)) => assert!(message.contains("limit"), "{message}"),
        other => panic!("expected an oversize error, got {other:?}"),
    }
}
