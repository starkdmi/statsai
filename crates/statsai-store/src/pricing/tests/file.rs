//! The stored pricing file, the refresh state, and repricing with a table
//! other than the embedded one.
//!
//! Tables with prices different from the embedded file's are applied with
//! [`Store::apply_pricing_table`], which never installs them as the process's
//! active table: every test in this binary shares that table.

use super::support::*;
use super::*;
use crate::STORED_PRICING_FILE_KEY;
use serde_json::{json, Value};
use statsai_pricing::{embedded_pricing_text, refresh_disabled_by_env, PRICING_FORMAT};

const NEWER_CATALOG: &str = "test:newer-prices";

/// The embedded file with `edit` applied.
fn pricing_text_with(edit: impl FnOnce(&mut Value)) -> String {
    let mut file: Value = serde_json::from_str(embedded_pricing_text()).expect("embedded json");
    edit(&mut file);
    serde_json::to_string_pretty(&file).expect("serialize pricing file")
}

/// The embedded file at `version`, with `edit` applied.
fn pricing_text_at(version: u64, edit: impl FnOnce(&mut Value)) -> String {
    pricing_text_with(|file| {
        file["version"] = json!(version);
        edit(file);
    })
}

fn embedded_version() -> u64 {
    embedded_pricing().version()
}

/// GPT-5.5's input rate goes from 5 to 6 USD per million tokens.
fn raise_gpt_5_5_input(file: &mut Value) {
    file["catalog"] = json!(NEWER_CATALOG);
    file["models"]["gpt-5.5"]["prices"][0]["input"] = json!(6.0);
}

/// The embedded file one version up, with GPT-5.5's input rate raised.
fn newer_pricing_text() -> String {
    pricing_text_at(embedded_version() + 1, raise_gpt_5_5_input)
}

fn parsed(text: &str) -> Arc<PricingTable> {
    Arc::new(PricingTable::parse(text).expect("valid pricing file"))
}

fn gpt_5_5_event(source: &statsai_core::SourceLocation, started_at: &str) -> UsageEvent {
    test_event(
        source,
        parse_utc(started_at),
        "gpt-5-5",
        "gpt-5.5",
        million_token_usage(),
        missing_cost(),
    )
}

fn gpt_5_5_cost(table: &PricingTable, event: &UsageEvent) -> CostInfo {
    table.estimate_cost_at(
        "codex",
        Some(&test_model("gpt-5.5")),
        &million_token_usage(),
        &event.session.started_at,
    )
}

fn stored_pricing_text(store: &Store) -> Option<String> {
    store
        .metadata_value(STORED_PRICING_FILE_KEY)
        .expect("read stored pricing file")
}

fn mark_all_rollups_synced(store: &Store) {
    store.rebuild_sync_rollups().expect("rebuild");
    let rollups = store.all_sync_rollup_summaries().expect("rollups");
    store
        .mark_sync_rollups_synced(
            &rollups
                .iter()
                .map(|summary| summary.summary_id.clone())
                .collect::<Vec<_>>(),
        )
        .expect("mark synced");
    assert!(store
        .dirty_sync_rollup_summaries()
        .expect("clean")
        .is_empty());
}

#[test]
fn a_newer_stored_file_reprices_changed_events_and_marks_their_rollups_dirty() {
    let (store, source) = store_with_source("/tmp/pricing-file-newer");
    let event = gpt_5_5_event(&source, "2026-09-01T12:00:00Z");
    store.insert_event(&event).expect("insert");
    let embedded = embedded_pricing();
    store.apply_pricing_table(&embedded).expect("embedded");
    let embedded_cost = stored_event(&store, &event.event_id.0).cost;
    assert_eq!(embedded_cost, gpt_5_5_cost(&embedded, &event));
    mark_all_rollups_synced(&store);

    let text = newer_pricing_text();
    let newer_version = embedded.version() + 1;
    assert_eq!(
        store.offer_pricing_file(&text).expect("offer"),
        PricingOffer::Stored {
            version: newer_version
        }
    );
    let table = store
        .current_pricing_table_with(PricingRefreshState::On)
        .expect("current table");
    assert_eq!(table.version(), newer_version);
    assert_eq!(table.source_text(), text);

    let report = store.apply_pricing_table(&table).expect("reprice");

    assert!(!report.already_current);
    assert_eq!(report.store_ahead, None);
    assert_eq!(report.version, newer_version);
    assert_eq!(report.catalog, NEWER_CATALOG);
    assert_eq!(report.changed_events, 1);
    assert_eq!(report.refreshed_rollups, 1);
    assert!(report.did_work());
    assert!(report.to_string().starts_with(&format!(
        "repriced store to pricing version {newer_version} ({NEWER_CATALOG}): examined_events=1 changed_events=1"
    )));
    let expected = gpt_5_5_cost(&table, &event);
    assert_ne!(expected, embedded_cost);
    assert_eq!(expected.pricing_version.as_deref(), Some(NEWER_CATALOG));
    assert_eq!(stored_event(&store, &event.event_id.0).cost, expected);
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        Some(newer_version)
    );
    assert_eq!(
        store
            .metadata_value(APPLIED_PRICING_CATALOG_VERSION_KEY)
            .expect("catalog"),
        Some(NEWER_CATALOG.to_string())
    );
    let dirty = store.dirty_sync_rollup_summaries().expect("dirty");
    assert_eq!(dirty.len(), 1);
    assert_eq!(
        dirty[0].cost.estimated_api_equivalent_micro_usd,
        expected.estimated_api_equivalent_micro_usd
    );
    assert_eq!(stored_pricing_text(&store), Some(text));

    // With the refresh off, this process prices with its embedded file, and
    // it does not take the store back to it.
    let off = store
        .current_pricing_table_with(PricingRefreshState::OffBySetting)
        .expect("refresh off");
    assert_eq!(off.version(), embedded.version());
    let report = store.apply_pricing_table(&off).expect("store ahead");
    assert_eq!(report.store_ahead, Some(newer_version));
    assert_eq!(stored_event(&store, &event.event_id.0).cost, expected);
}

#[test]
fn with_the_refresh_off_a_stored_newer_file_is_ignored() {
    let (store, source) = store_with_source("/tmp/pricing-file-refresh-off");
    let event = gpt_5_5_event(&source, "2026-09-01T12:00:00Z");
    store.insert_event(&event).expect("insert");
    let text = newer_pricing_text();
    store.offer_pricing_file(&text).expect("offer");
    store
        .set_pricing_refresh_setting(false)
        .expect("refresh off");
    let refresh =
        PricingRefreshState::resolve(false, store.pricing_refresh_setting().expect("setting"));
    assert_eq!(refresh, PricingRefreshState::OffBySetting);
    for off in [
        PricingRefreshState::OffBySetting,
        PricingRefreshState::OffByEnvironment,
    ] {
        assert_eq!(
            store
                .current_pricing_table_with(off)
                .expect("refresh off")
                .version(),
            embedded_version()
        );
    }

    // Installs the embedded table, which is already the active one.
    let report = store
        .ensure_current_pricing_with(refresh)
        .expect("reprice with the embedded file");

    assert_eq!(report.version, embedded_version());
    assert_eq!(report.changed_events, 1);
    assert_eq!(
        stored_event(&store, &event.event_id.0).cost,
        gpt_5_5_cost(&embedded_pricing(), &event)
    );
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        Some(embedded_version())
    );
    assert_eq!(
        stored_pricing_text(&store),
        Some(text),
        "the newer stored copy is kept for when the refresh is on again"
    );
}

#[test]
fn a_store_priced_with_a_newer_version_and_no_stored_file_is_used_unchanged() {
    let (store, source) = store_with_source("/tmp/pricing-file-store-ahead");
    let event = gpt_5_5_event(&source, "2026-09-01T12:00:00Z");
    store.insert_event(&event).expect("insert");
    store
        .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "99")
        .expect("future version");
    let payload_before =
        serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize");

    let report = store
        .ensure_current_pricing_with(PricingRefreshState::On)
        .expect("a newer store is not refused");

    assert_eq!(report.store_ahead, Some(99));
    assert_eq!(report.version, embedded_version());
    assert!(!report.did_work());
    assert!(report
        .to_string()
        .starts_with("store already priced with pricing version 99;"));
    assert_eq!(
        serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize"),
        payload_before
    );
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        Some(99)
    );
    assert_eq!(
        store
            .metadata_value(APPLIED_PRICING_CATALOG_VERSION_KEY)
            .expect("catalog"),
        None
    );
    assert_eq!(stored_pricing_text(&store), None);
}

#[test]
fn a_stored_file_in_an_unknown_format_refuses_and_changes_nothing() {
    let (store, source) = store_with_source("/tmp/pricing-file-unknown-format");
    let event = gpt_5_5_event(&source, "2026-09-01T12:00:00Z");
    store.insert_event(&event).expect("insert");
    let future = r#"{"format": 2, "version": 99, "models": {"gpt-9": {"tiers": []}}}"#;
    store
        .set_metadata_value(STORED_PRICING_FILE_KEY, future)
        .expect("store a future file");
    let payload_before =
        serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize");

    assert!(matches!(
        store.stored_pricing().expect("stored"),
        StoredPricing::UnknownFormat {
            format: 2,
            version: 99
        }
    ));
    for refresh in [
        PricingRefreshState::On,
        PricingRefreshState::OffBySetting,
        PricingRefreshState::OffByEnvironment,
    ] {
        let error = store
            .ensure_current_pricing_with(refresh)
            .expect_err("an unknown format refuses");
        assert_eq!(
            error.to_string(),
            format!("this store's prices come from pricing file format 2, which this StatsAI binary cannot read (it reads format {PRICING_FORMAT}); upgrade StatsAI")
        );
        assert_eq!(
            error.downcast_ref::<UnsupportedPricingFormat>(),
            Some(&UnsupportedPricingFormat {
                format: 2,
                version: 99
            })
        );
    }
    // A command that does not reprice warns and prices with the embedded file.
    assert_eq!(
        store.use_current_pricing().expect("use").version(),
        embedded_version()
    );
    // The unknown copy's version still counts, so an older file is not newer.
    assert_eq!(
        store
            .offer_pricing_file(&newer_pricing_text())
            .expect("offer"),
        PricingOffer::NotNewer {
            version: embedded_version() + 1,
            newest: 99
        }
    );

    assert_eq!(
        serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize"),
        payload_before
    );
    assert_eq!(store.applied_pricing_ruleset_version().expect("meta"), None);
    assert_eq!(stored_pricing_text(&store).as_deref(), Some(future));
}

#[test]
fn an_unreadable_stored_file_is_ignored_and_replaced() {
    let invalid_known_format = pricing_text_at(99, |file| {
        file["models"]["gpt-5.5"]["prices"][0]["input"] = json!(-1.0);
    });
    for unreadable in ["not json", "{\"format\": 1}", invalid_known_format.as_str()] {
        let (store, source) = store_with_source("/tmp/pricing-file-unreadable");
        let event = gpt_5_5_event(&source, "2026-09-01T12:00:00Z");
        store.insert_event(&event).expect("insert");
        store
            .set_metadata_value(STORED_PRICING_FILE_KEY, unreadable)
            .expect("store an unreadable file");
        assert!(
            matches!(
                store.stored_pricing().expect("stored"),
                StoredPricing::Unreadable(_)
            ),
            "{unreadable}"
        );

        let report = store
            .ensure_current_pricing_with(PricingRefreshState::On)
            .expect("an unreadable copy is ignored");

        assert_eq!(report.version, embedded_version(), "{unreadable}");
        assert_eq!(report.changed_events, 1, "{unreadable}");
        assert_eq!(
            stored_event(&store, &event.event_id.0).cost,
            gpt_5_5_cost(&embedded_pricing(), &event)
        );
        assert_eq!(
            stored_pricing_text(&store).as_deref(),
            Some(embedded_pricing_text()),
            "{unreadable}"
        );
    }
}

#[test]
fn repricing_stores_the_applied_file_only_over_an_older_or_missing_copy() {
    let embedded = embedded_pricing();
    let older = pricing_text_at(embedded.version() - 1, |_| {});
    let newer = pricing_text_at(embedded.version() + 1, |_| {});
    // Same version, different text: not newer, so not replaced.
    let same_version = pricing_text_with(|_| {});
    assert_ne!(same_version, embedded_pricing_text());
    let cases = [
        (None, Some(embedded_pricing_text())),
        (Some(older.as_str()), Some(embedded_pricing_text())),
        (Some(newer.as_str()), Some(newer.as_str())),
        (Some(same_version.as_str()), Some(same_version.as_str())),
    ];
    for (stored, expected) in cases {
        let (store, _source) = store_with_source("/tmp/pricing-file-write-back");
        if let Some(stored) = stored {
            store
                .set_metadata_value(STORED_PRICING_FILE_KEY, stored)
                .expect("store a copy");
        }

        let report = store.apply_pricing_table(&embedded).expect("reprice");

        assert!(!report.already_current);
        assert_eq!(
            store.applied_pricing_ruleset_version().expect("applied"),
            Some(embedded.version())
        );
        assert_eq!(
            stored_pricing_text(&store).as_deref(),
            expected,
            "stored before: {:?}",
            stored.map(|text| PricingTable::header(text).expect("header"))
        );
    }
}

#[test]
fn offering_a_file_stores_only_a_valid_newer_one_and_reprices_nothing() {
    let (store, source) = store_with_source("/tmp/pricing-file-offer");
    let event = gpt_5_5_event(&source, "2026-09-01T12:00:00Z");
    store.insert_event(&event).expect("insert");
    let payload_before =
        serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize");
    let embedded = embedded_version();
    let one_up = newer_pricing_text();
    let two_up = pricing_text_at(embedded + 2, raise_gpt_5_5_input);
    let one_up_again = pricing_text_at(embedded + 1, |file| {
        file["catalog"] = json!("test:another");
    });

    assert_eq!(
        store
            .offer_pricing_file(embedded_pricing_text())
            .expect("equal"),
        PricingOffer::NotNewer {
            version: embedded,
            newest: embedded
        }
    );
    assert_eq!(
        store
            .offer_pricing_file(&pricing_text_at(embedded - 1, |_| {}))
            .expect("older"),
        PricingOffer::NotNewer {
            version: embedded - 1,
            newest: embedded
        }
    );
    assert_eq!(stored_pricing_text(&store), None);

    assert_eq!(
        store.offer_pricing_file(&one_up).expect("newer"),
        PricingOffer::Stored {
            version: embedded + 1
        }
    );
    assert_eq!(stored_pricing_text(&store), Some(one_up.clone()));
    assert_eq!(
        store
            .offer_pricing_file(&one_up_again)
            .expect("equal to stored"),
        PricingOffer::NotNewer {
            version: embedded + 1,
            newest: embedded + 1
        }
    );
    assert_eq!(
        store.offer_pricing_file(&two_up).expect("newer again"),
        PricingOffer::Stored {
            version: embedded + 2
        }
    );
    assert_eq!(
        store
            .offer_pricing_file(&one_up_again)
            .expect("older than stored"),
        PricingOffer::NotNewer {
            version: embedded + 1,
            newest: embedded + 2
        }
    );

    assert_eq!(
        store
            .offer_pricing_file(r#"{"format": 2, "version": 100, "rates": []}"#)
            .expect("unknown format"),
        PricingOffer::UnknownFormat {
            format: 2,
            version: 100
        }
    );
    for (label, rate) in [("negative", -1.0), ("out of range", 5000.0)] {
        let invalid = pricing_text_at(embedded + 3, |file| {
            file["models"]["gpt-5.5"]["prices"][0]["input"] = json!(rate);
        });
        let offer = store.offer_pricing_file(&invalid).expect(label);
        assert!(
            matches!(&offer, PricingOffer::Invalid(message) if message.contains("gpt-5.5")),
            "{label}: {offer:?}"
        );
    }
    assert!(matches!(
        store.offer_pricing_file("{").expect("not json"),
        PricingOffer::Invalid(_)
    ));

    assert_eq!(stored_pricing_text(&store), Some(two_up));
    assert_eq!(
        serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize"),
        payload_before
    );
    assert_eq!(store.applied_pricing_ruleset_version().expect("meta"), None);
    assert_eq!(
        store
            .metadata_value(APPLIED_PRICING_CATALOG_VERSION_KEY)
            .expect("catalog"),
        None
    );
}

#[test]
fn the_environment_wins_over_the_setting_and_the_setting_round_trips() {
    use PricingRefreshState::{OffByEnvironment, OffBySetting, On};
    assert_eq!(PricingRefreshState::resolve(false, true), On);
    assert_eq!(PricingRefreshState::resolve(false, false), OffBySetting);
    assert_eq!(PricingRefreshState::resolve(true, true), OffByEnvironment);
    assert_eq!(PricingRefreshState::resolve(true, false), OffByEnvironment);
    assert!(On.is_on());
    assert!(!OffBySetting.is_on());
    assert!(!OffByEnvironment.is_on());

    let store = Store::in_memory().expect("store");
    assert!(store.pricing_refresh_setting().expect("absent means on"));
    store.set_pricing_refresh_setting(false).expect("off");
    assert_eq!(
        store
            .metadata_value(crate::PRICING_REFRESH_SETTING_KEY)
            .expect("setting"),
        Some("off".to_string())
    );
    assert!(!store.pricing_refresh_setting().expect("off"));
    // Whatever this process's environment says, the state combines it with
    // the setting the same way.
    assert_eq!(
        store.pricing_refresh_state().expect("state"),
        PricingRefreshState::resolve(refresh_disabled_by_env(), false)
    );
    store.set_pricing_refresh_setting(true).expect("on");
    assert_eq!(
        store
            .metadata_value(crate::PRICING_REFRESH_SETTING_KEY)
            .expect("setting"),
        Some("on".to_string())
    );
    assert!(store.pricing_refresh_setting().expect("on"));
    store
        .set_metadata_value(crate::PRICING_REFRESH_SETTING_KEY, "sometimes")
        .expect("odd value");
    assert!(store.pricing_refresh_setting().expect("only off is off"));
}

#[test]
fn a_summary_spanning_a_boundary_only_the_newer_file_has_becomes_unknown() {
    let (store, source) = store_with_source("/tmp/pricing-file-new-boundary");
    let start = parse_utc("2026-09-01T00:00:00Z");
    let end = parse_utc("2026-09-03T00:00:00Z");
    store
        .upsert_summary(&test_summary(
            &source,
            "gpt-5.5",
            start,
            end,
            missing_cost(),
        ))
        .expect("summary");
    let newer = parsed(&pricing_text_at(embedded_version() + 1, |file| {
        file["models"]["gpt-5.5"]["prices"]
            .as_array_mut()
            .expect("periods")
            .push(json!({
                "from": "2026-09-02",
                "input": 6,
                "cache_write": 6,
                "cache_read": 0.6,
                "output": 36
            }));
    }));
    let (start_day, end_day) = (start.date_naive(), end.date_naive());
    assert!(!embedded_pricing().pricing_changes_between("gpt-5.5", start_day, end_day));
    assert!(newer.pricing_changes_between("gpt-5.5", start_day, end_day));

    store
        .apply_pricing_table(&embedded_pricing())
        .expect("embedded");
    let priced = store.summaries().expect("summaries").remove(0);
    assert!(priced.cost.estimated_api_equivalent_usd.is_some());

    let report = store.apply_pricing_table(&newer).expect("newer");

    assert_eq!(report.changed_summaries, 1);
    let stored = store.summaries().expect("summaries").remove(0);
    assert!(stored.cost.estimated_api_equivalent_usd.is_none());
    assert_eq!(stored.cost.pricing_source.as_deref(), Some("unknown"));
}

#[test]
fn a_stored_copy_equal_to_a_known_table_is_not_parsed_again() {
    let store = Store::in_memory().expect("store");
    store
        .ensure_current_pricing_with(PricingRefreshState::On)
        .expect("reprice an empty store");
    assert_eq!(
        stored_pricing_text(&store).as_deref(),
        Some(embedded_pricing_text())
    );

    let StoredPricing::Usable(table) = store.stored_pricing().expect("stored") else {
        panic!("the embedded copy is usable");
    };
    assert!(Arc::ptr_eq(&table, &embedded_pricing()) || Arc::ptr_eq(&table, &active_pricing()));
    let report = store
        .ensure_current_pricing_with(PricingRefreshState::On)
        .expect("again");
    assert!(report.already_current);
    assert_eq!(
        report.to_string(),
        format!(
            "pricing already at version {} ({})",
            embedded_version(),
            embedded_pricing().catalog()
        )
    );
}
