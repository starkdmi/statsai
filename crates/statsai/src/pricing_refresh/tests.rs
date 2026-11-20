//! The pricing refresh against a local HTTP server.
//!
//! Every test passes the refresh state explicitly: the environment of a test
//! binary turns the refresh off (`.cargo/config.toml`). Fetched files are only
//! ever offered to a test's own store, never installed as the process's
//! active table.

use super::*;
use chrono::TimeZone;
use serde_json::{json, Value};
use statsai_store::{embedded_pricing, StoredPricing, STORED_PRICING_FILE_KEY};
use std::net::TcpListener;
use std::thread;
use std::time::Instant;
use tiny_http::{Header, Response, Server};

/// What the test server answers one request with.
struct Reply {
    status: u16,
    body: Vec<u8>,
    etag: Option<&'static str>,
}

impl Reply {
    fn ok(body: impl Into<Vec<u8>>, etag: Option<&'static str>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            etag,
        }
    }

    fn status(status: u16) -> Self {
        Self {
            status,
            body: Vec::new(),
            etag: None,
        }
    }
}

/// The headers of one request the test server received.
#[derive(Debug)]
struct Seen {
    if_none_match: Option<String>,
    user_agent: Option<String>,
}

/// A server on 127.0.0.1 that answers one request with each reply in order,
/// then stops.
struct TestServer {
    url: String,
    thread: thread::JoinHandle<Vec<Seen>>,
}

impl TestServer {
    fn start(replies: Vec<Reply>) -> Self {
        let server = Server::http("127.0.0.1:0").expect("bind test server");
        let address = server.server_addr().to_ip().expect("ip address");
        let url = format!("http://{address}/crates/statsai-pricing/pricing.json");
        let thread = thread::spawn(move || {
            let mut seen = Vec::new();
            for reply in replies {
                let Ok(Some(request)) = server.recv_timeout(Duration::from_secs(10)) else {
                    break;
                };
                let header = |name: &'static str| {
                    request
                        .headers()
                        .iter()
                        .find(|header| header.field.equiv(name))
                        .map(|header| header.value.as_str().to_string())
                };
                seen.push(Seen {
                    if_none_match: header("If-None-Match"),
                    user_agent: header("User-Agent"),
                });
                let mut response = Response::from_data(reply.body).with_status_code(reply.status);
                if let Some(etag) = reply.etag {
                    response.add_header(Header::from_bytes("ETag", etag).expect("etag header"));
                }
                let _ = request.respond(response);
            }
            seen
        });
        Self { url, thread }
    }

    /// Waits for every reply to be sent and returns what the server saw.
    fn requests(self) -> Vec<Seen> {
        self.thread.join().expect("test server thread")
    }
}

/// A URL nothing listens on, for tests that must not make a request: a
/// request there fails, which a test would see as `Failed`.
const UNREACHABLE_URL: &str = "http://127.0.0.1:1/pricing.json";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
}

fn hours(count: i64) -> TimeDelta {
    TimeDelta::hours(count)
}

fn minutes(count: i64) -> TimeDelta {
    TimeDelta::minutes(count)
}

fn embedded_version() -> u64 {
    embedded_pricing().version()
}

/// The embedded file at `version`, with `edit` applied.
fn pricing_text_at(version: u64, edit: impl FnOnce(&mut Value)) -> String {
    let mut file: Value =
        serde_json::from_str(embedded_pricing().source_text()).expect("embedded json");
    file["version"] = json!(version);
    edit(&mut file);
    serde_json::to_string_pretty(&file).expect("serialize pricing file")
}

fn newer_pricing_text() -> String {
    pricing_text_at(embedded_version() + 1, |_| {})
}

fn store() -> Store {
    Store::in_memory().expect("store")
}

fn stored_file(store: &Store) -> Option<String> {
    store
        .metadata_value(STORED_PRICING_FILE_KEY)
        .expect("stored pricing file")
}

fn bookkeeping(store: &Store) -> RefreshBookkeeping {
    RefreshBookkeeping::read(store).expect("refresh bookkeeping")
}

fn refresh(store: &Store, url: &str, force: bool) -> RefreshOutcome {
    refresh_pricing_with(
        store,
        url,
        now(),
        force,
        PricingRefreshState::On,
        Duration::from_secs(5),
    )
    .expect("refresh")
}

#[test]
fn a_fetched_file_comes_with_its_etag() {
    let server = TestServer::start(vec![Reply::ok("pricing text", Some("\"v1\""))]);

    let outcome = fetch_pricing_file(&server.url, None, Duration::from_secs(5)).expect("fetch");

    assert_eq!(
        outcome,
        FetchOutcome::Fetched {
            text: "pricing text".to_string(),
            etag: Some("\"v1\"".to_string()),
        }
    );
    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].if_none_match, None);
    assert_eq!(
        seen[0].user_agent.as_deref(),
        Some(format!("statsai/{}", env!("CARGO_PKG_VERSION")).as_str())
    );
}

#[test]
fn the_etag_is_sent_and_not_modified_is_reported() {
    let server = TestServer::start(vec![Reply::status(304)]);

    let outcome =
        fetch_pricing_file(&server.url, Some("\"v1\""), Duration::from_secs(5)).expect("fetch");

    assert_eq!(outcome, FetchOutcome::NotModified);
    let seen = server.requests();
    assert_eq!(seen[0].if_none_match.as_deref(), Some("\"v1\""));
}

#[test]
fn an_error_status_fails_the_fetch() {
    for status in [404, 500] {
        let server = TestServer::start(vec![Reply::status(status)]);
        let error = fetch_pricing_file(&server.url, None, Duration::from_secs(5))
            .expect_err("an error status fails");
        assert!(
            error.to_string().contains(&format!("HTTP {status}")),
            "{error:#}"
        );
        server.requests();
    }
}

#[test]
fn a_body_over_one_mebibyte_fails_the_fetch() {
    let limit = usize::try_from(MAX_PRICING_FILE_BYTES).expect("limit fits");
    let server = TestServer::start(vec![
        Reply::ok(vec![b'a'; limit], None),
        Reply::ok(vec![b'a'; limit + 1], None),
    ]);

    let at_limit = fetch_pricing_file(&server.url, None, Duration::from_secs(5))
        .expect("a body of exactly the limit is read");
    assert!(matches!(at_limit, FetchOutcome::Fetched { text, .. } if text.len() == limit));
    let error = fetch_pricing_file(&server.url, None, Duration::from_secs(5))
        .expect_err("a body over the limit fails");
    assert!(error.to_string().contains("larger than"), "{error:#}");
    server.requests();
}

#[test]
fn a_body_that_is_not_utf8_fails_the_fetch() {
    let server = TestServer::start(vec![Reply::ok(vec![0xff, 0xfe, 0x00], None)]);
    let error =
        fetch_pricing_file(&server.url, None, Duration::from_secs(5)).expect_err("not UTF-8 fails");
    assert!(error.to_string().contains("UTF-8"), "{error:#}");
    server.requests();
}

#[test]
fn a_server_that_never_answers_fails_within_the_timeout() {
    // The listener accepts connections into its backlog and never answers.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let url = format!(
        "http://{}/pricing.json",
        listener.local_addr().expect("address")
    );
    let timeout = Duration::from_millis(300);

    let started = Instant::now();
    let result = fetch_pricing_file(&url, None, timeout);
    let elapsed = started.elapsed();

    assert!(result.is_err(), "a silent server must time out");
    assert!(
        elapsed < Duration::from_secs(5),
        "timed out after {elapsed:?}, not about {timeout:?}"
    );
    drop(listener);
}

#[test]
fn only_https_or_loopback_http_is_fetched() {
    for url in [
        "http://example.com/pricing.json",
        "http://127.0.0.1.example.com/pricing.json",
        "http://10.0.0.1/pricing.json",
        "ftp://example.com/pricing.json",
        "file:///etc/hosts",
    ] {
        let error = fetch_pricing_file(url, None, Duration::from_secs(1))
            .expect_err("an insecure URL is refused");
        assert!(error.to_string().contains("only https"), "{url}: {error:#}");
    }
    assert!(fetch_pricing_file("not a url", None, Duration::from_secs(1)).is_err());

    assert_eq!(
        require_secure_pricing_url(DEFAULT_PRICING_URL).ok(),
        Some(true)
    );
    for url in [
        "http://127.0.0.1:8080/pricing.json",
        "http://localhost/pricing.json",
        "http://[::1]:8080/pricing.json",
    ] {
        assert_eq!(require_secure_pricing_url(url).ok(), Some(false), "{url}");
    }
}

#[test]
fn a_refresh_is_due_once_a_day_outside_the_attempt_lease() {
    let now = now();
    let cases = [
        // (checked_at, attempt_at, due)
        (None, None, true),
        (Some(now - hours(24)), None, true),
        (Some(now - hours(25)), None, true),
        (Some(now - hours(23)), None, false),
        (Some(now - minutes(1)), None, false),
        (Some(now), None, false),
        // A clock that ran ahead holds the refresh off for at most a day.
        (Some(now + hours(23)), None, false),
        (Some(now + hours(25)), None, true),
        (None, Some(now - minutes(10)), true),
        (None, Some(now - minutes(9)), false),
        (None, Some(now), false),
        (None, Some(now + minutes(9)), false),
        (None, Some(now + minutes(11)), true),
        (Some(now - hours(48)), Some(now - minutes(5)), false),
        (Some(now - hours(2)), Some(now - hours(2)), false),
        (Some(now - hours(30)), Some(now - hours(30)), true),
    ];
    for (checked_at, attempt_at, due) in cases {
        assert_eq!(
            refresh_due(now, checked_at, attempt_at),
            due,
            "checked_at={checked_at:?} attempt_at={attempt_at:?}"
        );
    }
}

#[test]
fn unreadable_stored_times_count_as_absent() {
    let store = store();
    store
        .set_metadata_value(CHECKED_AT_KEY, "yesterday")
        .expect("checked_at");
    store
        .set_metadata_value(ATTEMPT_AT_KEY, "")
        .expect("attempt_at");
    store.set_metadata_value(ETAG_KEY, "").expect("etag");

    assert_eq!(bookkeeping(&store), RefreshBookkeeping::default());
}

#[test]
fn a_refresh_that_is_off_fetches_nothing() {
    for state in [
        PricingRefreshState::OffBySetting,
        PricingRefreshState::OffByEnvironment,
    ] {
        let store = store();
        let outcome = refresh_pricing_with(
            &store,
            UNREACHABLE_URL,
            now(),
            true,
            state,
            Duration::from_secs(1),
        )
        .expect("refresh");

        assert_eq!(outcome, RefreshOutcome::Disabled(state));
        assert_eq!(bookkeeping(&store), RefreshBookkeeping::default());
    }
}

#[test]
fn a_refresh_is_not_due_within_a_day_or_within_the_attempt_lease() {
    let checked_recently = store();
    let checked_at = (now() - hours(23)).to_rfc3339();
    checked_recently
        .set_metadata_value(CHECKED_AT_KEY, &checked_at)
        .expect("checked_at");
    assert_eq!(
        refresh(&checked_recently, UNREACHABLE_URL, false),
        RefreshOutcome::NotDue
    );
    assert_eq!(
        checked_recently
            .metadata_value(ATTEMPT_AT_KEY)
            .expect("attempt_at"),
        None,
        "an attempt that is not due claims nothing"
    );

    let attempt_running = store();
    let attempt_at = (now() - minutes(5)).to_rfc3339();
    attempt_running
        .set_metadata_value(ATTEMPT_AT_KEY, &attempt_at)
        .expect("attempt_at");
    assert_eq!(
        refresh(&attempt_running, UNREACHABLE_URL, false),
        RefreshOutcome::NotDue
    );
    assert_eq!(
        attempt_running
            .metadata_value(ATTEMPT_AT_KEY)
            .expect("attempt_at"),
        Some(attempt_at)
    );
    assert_eq!(
        attempt_running
            .metadata_value(CHECKED_AT_KEY)
            .expect("checked_at"),
        None
    );
}

#[test]
fn a_forced_refresh_ignores_the_schedule_and_sends_the_stored_etag() {
    let store = store();
    store
        .set_metadata_value(CHECKED_AT_KEY, &(now() - hours(1)).to_rfc3339())
        .expect("checked_at");
    store
        .set_metadata_value(ATTEMPT_AT_KEY, &(now() - minutes(1)).to_rfc3339())
        .expect("attempt_at");
    store.set_metadata_value(ETAG_KEY, "\"v1\"").expect("etag");
    let server = TestServer::start(vec![Reply::status(304)]);

    assert_eq!(
        refresh(&store, &server.url, true),
        RefreshOutcome::NotModified
    );

    let seen = server.requests();
    assert_eq!(seen[0].if_none_match.as_deref(), Some("\"v1\""));
    assert_eq!(
        bookkeeping(&store),
        RefreshBookkeeping {
            checked_at: Some(now()),
            attempt_at: Some(now()),
            etag: Some("\"v1\"".to_string()),
        }
    );
}

#[test]
fn a_due_refresh_adopts_a_newer_valid_file_and_keeps_its_etag() {
    let store = store();
    let text = newer_pricing_text();
    let server = TestServer::start(vec![Reply::ok(text.clone(), Some("\"v2\""))]);

    let outcome = refresh(&store, &server.url, false);

    assert_eq!(
        outcome,
        RefreshOutcome::Adopted {
            version: embedded_version() + 1
        }
    );
    server.requests();
    assert_eq!(stored_file(&store), Some(text));
    assert!(matches!(
        store.stored_pricing().expect("stored pricing"),
        StoredPricing::Usable(table) if table.version() == embedded_version() + 1
    ));
    assert_eq!(
        bookkeeping(&store),
        RefreshBookkeeping {
            checked_at: Some(now()),
            attempt_at: Some(now()),
            etag: Some("\"v2\"".to_string()),
        }
    );
    // Adopting only stores the file; repricing is left to the next command.
    assert_eq!(
        store
            .applied_pricing_ruleset_version()
            .expect("applied version"),
        None
    );
}

#[test]
fn the_embedded_file_is_not_newer_and_its_etag_is_kept() {
    let store = store();
    store.set_metadata_value(ETAG_KEY, "\"old\"").expect("etag");
    let server = TestServer::start(vec![Reply::ok(
        embedded_pricing().source_text().to_string(),
        Some("\"v1\""),
    )]);

    let outcome = refresh(&store, &server.url, false);

    assert_eq!(
        outcome,
        RefreshOutcome::NotNewer {
            version: embedded_version(),
            newest: embedded_version(),
        }
    );
    server.requests();
    assert_eq!(stored_file(&store), None);
    assert_eq!(bookkeeping(&store).etag.as_deref(), Some("\"v1\""));
}

#[test]
fn a_file_without_an_etag_clears_the_stored_one() {
    let store = store();
    store.set_metadata_value(ETAG_KEY, "\"old\"").expect("etag");
    let server = TestServer::start(vec![Reply::ok(newer_pricing_text(), None)]);

    assert!(matches!(
        refresh(&store, &server.url, false),
        RefreshOutcome::Adopted { .. }
    ));
    server.requests();
    assert_eq!(bookkeeping(&store).etag, None);
}

#[test]
fn a_file_of_an_unknown_format_is_rejected_and_nothing_is_stored() {
    let store = store();
    let future = json!({
        "format": u64::from(PRICING_FORMAT) + 1,
        "version": embedded_version() + 5,
        "models": {}
    })
    .to_string();
    let server = TestServer::start(vec![Reply::ok(future, Some("\"future\""))]);

    let outcome = refresh(&store, &server.url, false);

    let RefreshOutcome::Rejected(reason) = outcome else {
        panic!("an unknown format must be rejected, got {outcome:?}");
    };
    assert!(reason.contains("upgrade StatsAI"), "{reason}");
    server.requests();
    assert_eq!(stored_file(&store), None);
    let bookkeeping = bookkeeping(&store);
    assert_eq!(bookkeeping.etag, None);
    assert_eq!(bookkeeping.checked_at, Some(now()));
}

#[test]
fn a_file_with_a_negative_rate_is_rejected_and_nothing_is_stored() {
    let store = store();
    let negative = pricing_text_at(embedded_version() + 1, |file| {
        file["models"]["gpt-5.5"]["prices"][0]["input"] = json!(-1.0);
    });
    let server = TestServer::start(vec![Reply::ok(negative, Some("\"negative\""))]);

    let outcome = refresh(&store, &server.url, false);

    let RefreshOutcome::Rejected(reason) = outcome else {
        panic!("a negative rate must be rejected, got {outcome:?}");
    };
    assert!(reason.starts_with("invalid pricing file"), "{reason}");
    server.requests();
    assert_eq!(stored_file(&store), None);
    assert_eq!(bookkeeping(&store).etag, None);
}

#[test]
fn a_failed_fetch_still_counts_as_the_days_check() {
    let store = store();
    store.set_metadata_value(ETAG_KEY, "\"v1\"").expect("etag");
    let server = TestServer::start(vec![Reply::status(500)]);

    let outcome = refresh(&store, &server.url, false);

    let RefreshOutcome::Failed(reason) = outcome else {
        panic!("a server error must fail the refresh, got {outcome:?}");
    };
    assert!(reason.contains("HTTP 500"), "{reason}");
    server.requests();
    assert_eq!(
        bookkeeping(&store),
        RefreshBookkeeping {
            checked_at: Some(now()),
            attempt_at: Some(now()),
            etag: Some("\"v1\"".to_string()),
        }
    );
    assert_eq!(stored_file(&store), None);
    assert!(!refresh_due(now() + hours(23), Some(now()), Some(now())));
}

#[test]
fn a_background_refresh_turned_off_by_the_environment_opens_nothing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("statsai.sqlite");

    let handle = spawn_background_refresh_with(
        &path,
        true,
        UNREACHABLE_URL.to_string(),
        Duration::from_secs(1),
    );

    assert!(handle.is_none());
    assert!(!path.exists());
}

#[test]
fn a_background_refresh_adopts_a_newer_file_on_its_own_connection() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("statsai.sqlite");
    drop(Store::open(&path).expect("create store"));
    let text = newer_pricing_text();
    let server = TestServer::start(vec![Reply::ok(text.clone(), Some("\"v2\""))]);

    spawn_background_refresh_with(&path, false, server.url.clone(), Duration::from_secs(5))
        .expect("background refresh started")
        .join()
        .expect("background refresh");

    server.requests();
    let store = Store::open(&path).expect("reopen");
    assert_eq!(stored_file(&store), Some(text));
    assert_eq!(bookkeeping(&store).etag.as_deref(), Some("\"v2\""));
}

#[test]
fn a_background_refresh_respects_the_stores_setting() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("statsai.sqlite");
    let store = Store::open(&path).expect("create store");
    store
        .set_pricing_refresh_setting(false)
        .expect("turn the refresh off");
    drop(store);

    spawn_background_refresh_with(
        &path,
        false,
        UNREACHABLE_URL.to_string(),
        Duration::from_secs(1),
    )
    .expect("background refresh started")
    .join()
    .expect("background refresh");

    let store = Store::open(&path).expect("reopen");
    assert_eq!(bookkeeping(&store), RefreshBookkeeping::default());
}

#[test]
fn the_daemon_logs_an_adopted_file_and_stays_quiet_when_nothing_changed() {
    let store = store();
    let server = TestServer::start(vec![Reply::ok(newer_pricing_text(), Some("\"v2\""))]);

    let line = daemon_refresh_once(&store, &server.url).expect("an adopted file is logged");
    assert!(
        line.contains(&format!("version {}", embedded_version() + 1)),
        "{line}"
    );
    server.requests();

    // Checked just now, so the next hourly check is not due.
    assert_eq!(daemon_refresh_once(&store, UNREACHABLE_URL), None);

    let off = Store::in_memory().expect("store");
    off.set_pricing_refresh_setting(false).expect("setting");
    assert_eq!(daemon_refresh_once(&off, UNREACHABLE_URL), None);
    assert_eq!(bookkeeping(&off), RefreshBookkeeping::default());
}
