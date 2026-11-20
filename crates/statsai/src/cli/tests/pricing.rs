//! `statsai pricing status`, `refresh` and `auto-refresh`.
//!
//! The refresh state is passed explicitly: this test binary's environment
//! turns the refresh off (`.cargo/config.toml`). The only file a test lets a
//! command install as the process's active table has the embedded prices at a
//! higher version.

use super::support::*;
use super::*;
use statsai::pricing_refresh::{RefreshBookkeeping, ETAG_KEY};
use statsai_store::{embedded_pricing, PricingRefreshState, STORED_PRICING_FILE_KEY};

const UNREACHABLE_URL: &str = "http://127.0.0.1:1/pricing.json";

fn embedded_version() -> u64 {
    embedded_pricing().version()
}

fn status_of(store: &Store, disabled_by_env: bool) -> PricingStatus {
    pricing_status_with(store, disabled_by_env, UNREACHABLE_URL.to_string())
        .expect("pricing status")
}

#[test]
fn auto_refresh_off_then_status_reports_the_setting() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("statsai.sqlite");
    let cli = Cli::try_parse_from([
        "statsai",
        "--store",
        path.to_str().expect("utf-8 path"),
        "pricing",
        "auto-refresh",
        "off",
    ])
    .expect("parse auto-refresh off");
    let Command::Pricing(command) = cli.command else {
        panic!("expected a pricing command");
    };
    pricing(command, &cli.store.expect("store path")).expect("auto-refresh off");

    let store = Store::open(&path).expect("open store");
    assert!(!store.pricing_refresh_setting().expect("setting"));
    let status = status_of(&store, false);
    assert_eq!(
        status.refresh,
        RefreshStatus {
            on: false,
            off_by: Some("setting"),
            setting_on: false,
            disabled_by_environment: false,
        }
    );
    assert!(status.lines().contains(
        &"refresh: off: this store's setting is off; `statsai pricing auto-refresh on` turns it on"
            .to_string()
    ));
    let json = serde_json::to_value(&status).expect("status json");
    assert_eq!(json["refresh"]["on"], json!(false));
    assert_eq!(json["refresh"]["off_by"], json!("setting"));
    assert_eq!(json["refresh"]["setting_on"], json!(false));

    let lines = set_auto_refresh(&store, true, false).expect("auto-refresh on");
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("Pricing auto-refresh is on"));
    assert!(status_of(&store, false).refresh.on);
}

#[test]
fn auto_refresh_on_notes_an_environment_that_overrides_it() {
    let store = Store::in_memory().expect("store");

    let lines = set_auto_refresh(&store, true, true).expect("auto-refresh on");

    assert_eq!(lines.len(), 2);
    assert!(lines[1].contains("STATSAI_PRICING_REFRESH"), "{lines:?}");
    let status = status_of(&store, true);
    assert_eq!(status.refresh.off_by, Some("environment"));
    assert!(status.refresh.setting_on);
    assert!(status.lines().iter().any(|line| line
        == "refresh: off: STATSAI_PRICING_REFRESH turns it off in this environment; unset it to use the store's setting (on)"));
}

#[test]
fn status_of_a_new_store_prices_with_the_embedded_file() {
    let store = Store::in_memory().expect("store");
    let embedded = embedded_pricing();

    let status = status_of(&store, false);

    assert_eq!(
        status,
        PricingStatus {
            embedded: PricingFileStatus {
                version: embedded.version(),
                format: embedded.format(),
                catalog: embedded.catalog().to_string(),
            },
            stored: StoredPricingStatus::None,
            applied_version: None,
            applied_catalog: None,
            prices_with: PricesWith::File {
                version: embedded.version(),
                catalog: embedded.catalog().to_string(),
                source: "embedded",
            },
            refresh: RefreshStatus {
                on: true,
                off_by: None,
                setting_on: true,
                disabled_by_environment: false,
            },
            last_checked_at: None,
            last_attempt_at: None,
            etag_stored: false,
            url: UNREACHABLE_URL.to_string(),
        }
    );
    let lines = status.lines();
    assert!(lines.contains(&"stored pricing file: none".to_string()));
    assert!(lines.contains(&"store priced with: not priced yet".to_string()));
    assert!(lines.contains(&"last check: never".to_string()));
    assert!(lines.contains(&format!("url: {UNREACHABLE_URL}")));
}

#[test]
fn status_uses_a_newer_stored_file_only_while_the_refresh_is_on() {
    let store = Store::in_memory().expect("store");
    let newer = pricing_file_at(embedded_version() + 1, |_| {});
    assert!(matches!(
        store.offer_pricing_file(&newer).expect("offer"),
        statsai_store::PricingOffer::Stored { .. }
    ));
    store.set_metadata_value(ETAG_KEY, "\"v2\"").expect("etag");

    let on = status_of(&store, false);
    assert!(matches!(
        on.stored,
        StoredPricingStatus::Usable { version, .. } if version == embedded_version() + 1
    ));
    assert!(matches!(
        on.prices_with,
        PricesWith::File { version, source: "stored", .. } if version == embedded_version() + 1
    ));
    assert!(on.etag_stored);

    let off = status_of(&store, true);
    assert!(matches!(
        off.prices_with,
        PricesWith::File { version, source: "embedded", .. } if version == embedded_version()
    ));

    // Once the store is priced with the stored file, a process with the
    // refresh off keeps pricing with it.
    store
        .set_metadata_value(
            statsai_store::APPLIED_PRICING_RULESET_VERSION_KEY,
            &(embedded_version() + 1).to_string(),
        )
        .expect("applied");
    let off = status_of(&store, true);
    assert!(matches!(
        off.prices_with,
        PricesWith::File { version, source: "stored", .. } if version == embedded_version() + 1
    ));
    assert!(off
        .lines()
        .iter()
        .any(|line| line.starts_with("commands price with: version ")
            && line.ends_with(", the stored file")));
}

#[test]
fn auto_refresh_off_says_a_store_priced_with_a_stored_file_keeps_it() {
    let store = Store::in_memory().expect("store");
    let lines = set_auto_refresh(&store, false, false).expect("auto-refresh off");
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with("Pricing auto-refresh is off for this store: StatsAI does not fetch the pricing file and prices with the file built into the binary."),
        "{lines:?}"
    );
    assert!(
        lines[0].ends_with("A store already priced with a newer stored file keeps that file, so its prices never go back."),
        "{lines:?}"
    );
}

#[test]
fn status_works_on_a_store_whose_file_has_an_unknown_format() {
    let store = Store::in_memory().expect("store");
    let future = json!({ "format": 99, "version": 1234, "models": {} }).to_string();
    store
        .set_metadata_value(STORED_PRICING_FILE_KEY, &future)
        .expect("future file");

    let status = status_of(&store, false);

    assert_eq!(
        status.stored,
        StoredPricingStatus::UnknownFormat {
            version: 1234,
            format: 99
        }
    );
    let PricesWith::Refused { reason } = &status.prices_with else {
        panic!(
            "an unknown format must refuse, got {:?}",
            status.prices_with
        );
    };
    assert!(reason.contains("upgrade StatsAI"), "{reason}");
    let lines = status.lines();
    assert!(lines
        .iter()
        .any(|line| line.starts_with("stored pricing file: version 1234, format 99")));
    assert!(lines
        .iter()
        .any(|line| line.starts_with("commands price with: none: ")));
    serde_json::to_string(&status).expect("status json");
}

#[test]
fn refresh_refuses_when_the_refresh_is_off_and_names_the_cause() {
    let store = Store::in_memory().expect("store");

    let by_setting = run_pricing_refresh(
        &store,
        PricingRefreshState::OffBySetting,
        UNREACHABLE_URL,
        StdDuration::from_secs(1),
    )
    .expect_err("off by the setting");
    assert!(
        by_setting
            .to_string()
            .contains("run `statsai pricing auto-refresh on`"),
        "{by_setting}"
    );

    let by_environment = run_pricing_refresh(
        &store,
        PricingRefreshState::OffByEnvironment,
        UNREACHABLE_URL,
        StdDuration::from_secs(1),
    )
    .expect_err("off by the environment");
    assert!(
        by_environment
            .to_string()
            .contains("unset STATSAI_PRICING_REFRESH"),
        "{by_environment}"
    );

    assert_eq!(
        RefreshBookkeeping::read(&store).expect("bookkeeping"),
        RefreshBookkeeping::default()
    );
}

#[test]
fn refresh_reports_current_pricing_when_the_file_has_not_changed() {
    let store = Store::in_memory().expect("store");
    let server = PricingFileServer::start(304, String::new(), None);

    let lines = run_pricing_refresh(
        &store,
        PricingRefreshState::On,
        &server.url,
        StdDuration::from_secs(5),
    )
    .expect("refresh");
    server.finish();

    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with(&format!(
            "Pricing is current at version {}",
            embedded_version()
        )),
        "{lines:?}"
    );
    assert!(lines[0].contains("has not changed"), "{lines:?}");
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        Some(embedded_version())
    );
}

#[test]
fn refresh_adopts_a_newer_file_and_reprices_at_once() {
    let store = Store::in_memory().expect("store");
    // The embedded prices at a higher version: the refresh installs it as the
    // process's active table, which every test in this binary shares.
    let newer = pricing_file_at(embedded_version() + 1, |_| {});
    let server = PricingFileServer::start(200, newer.clone(), Some("\"v2\""));
    let url = server.url.clone();

    let lines = run_pricing_refresh(
        &store,
        PricingRefreshState::On,
        &url,
        StdDuration::from_secs(5),
    )
    .expect("refresh");
    server.finish();

    assert_eq!(
        lines[0],
        format!(
            "Stored pricing file version {} from {url}.",
            embedded_version() + 1
        )
    );
    assert!(
        lines[1].starts_with(&format!(
            "repriced store to pricing version {}",
            embedded_version() + 1
        )),
        "{lines:?}"
    );
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        Some(embedded_version() + 1)
    );
    assert_eq!(
        store
            .metadata_value(STORED_PRICING_FILE_KEY)
            .expect("stored file"),
        Some(newer)
    );
}

#[test]
fn refresh_fails_on_a_server_error_or_a_rejected_file() {
    let store = Store::in_memory().expect("store");

    let server = PricingFileServer::start(500, String::new(), None);
    let failed = run_pricing_refresh(
        &store,
        PricingRefreshState::On,
        &server.url,
        StdDuration::from_secs(5),
    )
    .expect_err("a server error fails");
    server.finish();
    assert!(
        failed
            .to_string()
            .starts_with("could not fetch the pricing file from "),
        "{failed}"
    );
    assert!(failed.to_string().contains("HTTP 500"), "{failed}");

    let negative = pricing_file_at(embedded_version() + 1, |file| {
        file["models"]["gpt-5.5"]["prices"][0]["output"] = json!(-30);
    });
    let server = PricingFileServer::start(200, negative, None);
    let rejected = run_pricing_refresh(
        &store,
        PricingRefreshState::On,
        &server.url,
        StdDuration::from_secs(5),
    )
    .expect_err("a negative rate is rejected");
    server.finish();
    assert!(
        rejected
            .to_string()
            .starts_with("did not use the pricing file from "),
        "{rejected}"
    );
    assert_eq!(
        store
            .metadata_value(STORED_PRICING_FILE_KEY)
            .expect("stored file"),
        None
    );
    assert_eq!(
        store.applied_pricing_ruleset_version().expect("applied"),
        None
    );
}
