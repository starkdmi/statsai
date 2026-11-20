//! Fetching the pricing file from `main` and keeping it in the store.
//!
//! Prices live in `crates/statsai-pricing/pricing.json`. Every binary embeds
//! the copy it was built with, and while the pricing refresh is on, clients
//! also fetch the copy on `main` at most once a day and offer it to the store
//! ([`Store::offer_pricing_file`]), which keeps it only when it is valid, of a
//! format this binary reads, and newer than every copy the process knows.
//! Nothing here reprices: the next command that reads price-derived data does
//! that with the newest copy it can read.
//!
//! The schedule is kept in the store's metadata, so every process sharing a
//! store shares it: [`CHECKED_AT_KEY`] records the last completed attempt and
//! [`ATTEMPT_AT_KEY`] the last started one, which keeps two processes from
//! fetching at once for [`ATTEMPT_LEASE`]. An attempt is claimed with a
//! compare-and-set of [`ATTEMPT_AT_KEY`], so of two processes that find a
//! refresh due at the same moment, only one fetches.
//!
//! `statsai scan` and `statsai sync` start a refresh that is due as a short,
//! detached `statsai pricing refresh --background` process
//! ([`start_background_refresh`]), so it finishes even when the command exits
//! first. The watch daemon runs its own refresher thread
//! ([`spawn_daemon_refresher`]). See `docs/pricing.md`.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use statsai_store::{
    refresh_disabled_by_env, PricingOffer, PricingRefreshState, Store, PRICING_FORMAT,
};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

/// Where clients fetch the pricing file: the copy on `main`.
pub const DEFAULT_PRICING_URL: &str =
    "https://raw.githubusercontent.com/starkdmi/statsai/main/crates/statsai-pricing/pricing.json";

/// Replaces [`DEFAULT_PRICING_URL`] when set and not empty, for testing.
pub const PRICING_URL_ENV: &str = "STATSAI_PRICING_URL";

/// RFC 3339 time of the last completed refresh attempt, whatever its result.
pub const CHECKED_AT_KEY: &str = "pricing.refresh.checked_at";

/// RFC 3339 time of the last started refresh attempt. Written before the
/// request, so a second process does not fetch while the first one is.
pub const ATTEMPT_AT_KEY: &str = "pricing.refresh.attempt_at";

/// The `ETag` of the last response whose file was stored or was not newer,
/// sent back as `If-None-Match`. Empty means none.
pub const ETAG_KEY: &str = "pricing.refresh.etag";

/// How long after a completed attempt the next one is due.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a started attempt keeps other processes from starting one.
pub const ATTEMPT_LEASE: Duration = Duration::from_secs(10 * 60);

/// The overall time limit of one request once the host name resolves:
/// connecting, sending and reading. The name lookup itself is bounded only by
/// the system resolver.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest pricing file a client reads, the same limit validation applies.
pub const MAX_PRICING_FILE_BYTES: u64 = 1024 * 1024;

/// How often the watch daemon checks whether a refresh is due.
pub const DAEMON_REFRESH_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The URL to fetch the pricing file from: `STATSAI_PRICING_URL` when it is
/// set and not empty, otherwise [`DEFAULT_PRICING_URL`].
#[must_use]
pub fn pricing_url() -> String {
    std::env::var(PRICING_URL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_PRICING_URL.to_string())
}

/// What one request for the pricing file returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// `304 Not Modified`: the file has not changed since the `ETag` sent.
    NotModified,
    /// `200 OK` with the file's text and the response's `ETag`, if any.
    Fetched { text: String, etag: Option<String> },
}

/// Fetches the pricing file at `url`.
///
/// Sends `If-None-Match` when `etag` is given. `timeout` bounds the whole
/// request after the host name resolves, connecting and reading included; the
/// name lookup is bounded only by the system resolver. The body is read up to
/// [`MAX_PRICING_FILE_BYTES`].
///
/// # Errors
///
/// A URL that is not `https`, except plain `http` to a loopback host (for
/// tests); a transport failure or timeout; any status other than 200 and 304;
/// a body over the limit or not UTF-8.
pub fn fetch_pricing_file(
    url: &str,
    etag: Option<&str>,
    timeout: Duration,
) -> Result<FetchOutcome> {
    let https = require_secure_pricing_url(url)?;
    let agent = ureq::AgentBuilder::new()
        .timeout(timeout)
        .https_only(https)
        .user_agent(&format!("statsai/{}", env!("CARGO_PKG_VERSION")))
        .build();
    let mut request = agent.get(url);
    if let Some(etag) = etag {
        request = request.set("If-None-Match", etag);
    }
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) => bail!("the server answered HTTP {code}"),
        Err(ureq::Error::Transport(error)) => bail!(describe_transport_error(&error)),
    };
    match response.status() {
        304 => Ok(FetchOutcome::NotModified),
        200 => {
            let etag = response
                .header("ETag")
                .map(str::trim)
                .filter(|etag| !etag.is_empty())
                .map(str::to_string);
            let mut body = Vec::new();
            response
                .into_reader()
                .take(MAX_PRICING_FILE_BYTES + 1)
                .read_to_end(&mut body)
                .context("read the pricing file")?;
            if body.len() as u64 > MAX_PRICING_FILE_BYTES {
                bail!("the pricing file is larger than {MAX_PRICING_FILE_BYTES} bytes");
            }
            let text = String::from_utf8(body).context("the pricing file is not UTF-8")?;
            Ok(FetchOutcome::Fetched { text, etag })
        }
        code => bail!("the server answered HTTP {code}"),
    }
}

/// A transport error without the URL, which every caller already names.
fn describe_transport_error(error: &ureq::Transport) -> String {
    let mut description = error.kind().to_string();
    if let Some(message) = error.message() {
        description.push_str(": ");
        description.push_str(message);
    }
    if let Some(source) = std::error::Error::source(error) {
        description.push_str(&format!(": {source}"));
    }
    description
}

/// Accepts an `https` URL, or a plain `http` one to a loopback host, and
/// returns whether it is `https`.
fn require_secure_pricing_url(url: &str) -> Result<bool> {
    let parsed = url::Url::parse(url).with_context(|| format!("invalid pricing URL {url}"))?;
    match parsed.scheme() {
        "https" => Ok(true),
        "http" if is_loopback_host(&parsed) => Ok(false),
        _ => bail!("refusing to fetch the pricing file from {url}: only https URLs are allowed"),
    }
}

fn is_loopback_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Whether a refresh is due at `now`: the last completed attempt
/// (`checked_at`) is absent or at least [`REFRESH_INTERVAL`] old, and the last
/// started one (`attempt_at`) is absent or at least [`ATTEMPT_LEASE`] old.
///
/// A time further in the future than its interval counts as absent, so a clock
/// that was once set wrong cannot hold the refresh off for longer than that.
#[must_use]
pub fn refresh_due(
    now: DateTime<Utc>,
    checked_at: Option<DateTime<Utc>>,
    attempt_at: Option<DateTime<Utc>>,
) -> bool {
    interval_elapsed(now, checked_at, REFRESH_INTERVAL)
        && interval_elapsed(now, attempt_at, ATTEMPT_LEASE)
}

fn interval_elapsed(now: DateTime<Utc>, at: Option<DateTime<Utc>>, interval: Duration) -> bool {
    let Some(at) = at else {
        return true;
    };
    let interval = TimeDelta::from_std(interval).unwrap_or(TimeDelta::MAX);
    match now.checked_add_signed(interval) {
        Some(latest) if at > latest => true,
        _ => now.signed_duration_since(at) >= interval,
    }
}

/// The refresh schedule and `ETag` kept in a store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshBookkeeping {
    /// The last completed attempt, if recorded and readable.
    pub checked_at: Option<DateTime<Utc>>,
    /// The last started attempt, if recorded and readable.
    pub attempt_at: Option<DateTime<Utc>>,
    /// The `ETag` sent with the next request, if one is stored.
    pub etag: Option<String>,
}

impl RefreshBookkeeping {
    /// Reads the bookkeeping from `store`. A time that does not parse counts
    /// as absent.
    pub fn read(store: &Store) -> Result<Self> {
        Ok(Self {
            checked_at: stored_time(store, CHECKED_AT_KEY)?,
            attempt_at: stored_time(store, ATTEMPT_AT_KEY)?,
            etag: store
                .metadata_value(ETAG_KEY)?
                .filter(|etag| !etag.trim().is_empty()),
        })
    }
}

fn stored_time(store: &Store, key: &str) -> Result<Option<DateTime<Utc>>> {
    Ok(store
        .metadata_value(key)?
        .as_deref()
        .and_then(parse_stored_time))
}

fn parse_stored_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// The refresh schedule of a store as one process read it: what it decides
/// whether a refresh is due from, and claims an attempt against.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RefreshSchedule {
    /// The last completed attempt, if recorded and readable.
    checked_at: Option<DateTime<Utc>>,
    /// [`ATTEMPT_AT_KEY`] exactly as stored, readable or not. A claim
    /// succeeds only while the store still holds this value.
    attempt_at: Option<String>,
}

impl RefreshSchedule {
    /// Reads both times from `store` at one point in time.
    fn read(store: &Store) -> Result<Self> {
        store.with_read_snapshot(|store| {
            Ok(Self {
                checked_at: stored_time(store, CHECKED_AT_KEY)?,
                attempt_at: store.metadata_value(ATTEMPT_AT_KEY)?,
            })
        })
    }

    /// Whether a refresh is due at `now` ([`refresh_due`]). An unreadable
    /// attempt time counts as absent.
    fn is_due(&self, now: DateTime<Utc>) -> bool {
        refresh_due(
            now,
            self.checked_at,
            self.attempt_at.as_deref().and_then(parse_stored_time),
        )
    }

    /// Claims an attempt by writing `stamp` to [`ATTEMPT_AT_KEY`], only if
    /// it still holds the value this schedule read. Returns `false`, writing
    /// nothing, when another process claimed an attempt since.
    fn claim(&self, store: &Store, stamp: &str) -> Result<bool> {
        store.compare_and_set_metadata_value(ATTEMPT_AT_KEY, self.attempt_at.as_deref(), stamp)
    }
}

/// What one refresh did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The refresh is off, for the reason given. Nothing was fetched.
    Disabled(PricingRefreshState),
    /// The last check is less than a day old, another attempt started in the
    /// last ten minutes, or another process claimed this attempt first.
    /// Nothing was fetched.
    NotDue,
    /// The file has not changed since the last check.
    NotModified,
    /// The fetched file is now the store's pricing file. It applies the next
    /// time prices are read.
    Adopted { version: u64 },
    /// The fetched file is not newer than `newest`, the newest copy the
    /// process already has.
    NotNewer { version: u64, newest: u64 },
    /// The fetched file was refused: invalid, or of a format this binary
    /// cannot read. Nothing was stored.
    Rejected(String),
    /// The request failed: transport error, timeout, unexpected status, or a
    /// body over the limit. The attempt still counts as the day's check.
    Failed(String),
}

/// Refreshes the store's pricing file from `url` when the refresh is on and a
/// refresh is due at `now`, or regardless of the schedule when `force` is set.
///
/// Claims the attempt in [`ATTEMPT_AT_KEY`] before fetching, sends the stored
/// `ETag`, offers a fetched file to the store, and records [`CHECKED_AT_KEY`]
/// after every completed attempt, a failed one included. The response's `ETag`
/// is kept only when the file was stored or was not newer, so a refused file
/// is fetched again on the next check.
///
/// The claim is a compare-and-set of [`ATTEMPT_AT_KEY`] from the value this
/// call read when it found the refresh due
/// ([`Store::compare_and_set_metadata_value`]), so when several processes
/// find it due at once, one fetches and the others return
/// [`RefreshOutcome::NotDue`]. A forced refresh claims unconditionally.
///
/// # Errors
///
/// Only store errors. A failed request is [`RefreshOutcome::Failed`].
pub fn refresh_pricing_with(
    store: &Store,
    url: &str,
    now: DateTime<Utc>,
    force: bool,
    refresh: PricingRefreshState,
    timeout: Duration,
) -> Result<RefreshOutcome> {
    if !refresh.is_on() {
        return Ok(RefreshOutcome::Disabled(refresh));
    }
    let stamp = now.to_rfc3339();
    if force {
        store.set_metadata_value(ATTEMPT_AT_KEY, &stamp)?;
    } else {
        let schedule = RefreshSchedule::read(store)?;
        if !schedule.is_due(now) || !schedule.claim(store, &stamp)? {
            return Ok(RefreshOutcome::NotDue);
        }
    }
    let etag = RefreshBookkeeping::read(store)?.etag;
    let outcome = match fetch_pricing_file(url, etag.as_deref(), timeout) {
        Err(error) => RefreshOutcome::Failed(format!("{error:#}")),
        Ok(FetchOutcome::NotModified) => RefreshOutcome::NotModified,
        Ok(FetchOutcome::Fetched { text, etag }) => {
            let outcome = match store.offer_pricing_file(&text)? {
                PricingOffer::Stored { version } => RefreshOutcome::Adopted { version },
                PricingOffer::NotNewer { version, newest } => {
                    RefreshOutcome::NotNewer { version, newest }
                }
                PricingOffer::UnknownFormat { format, version } => RefreshOutcome::Rejected(
                    format!(
                        "pricing file version {version} has format {format}, which this StatsAI binary cannot read (it reads format {PRICING_FORMAT}); upgrade StatsAI to use it"
                    ),
                ),
                PricingOffer::Invalid(message) => {
                    RefreshOutcome::Rejected(format!("invalid pricing file: {message}"))
                }
            };
            if matches!(
                outcome,
                RefreshOutcome::Adopted { .. } | RefreshOutcome::NotNewer { .. }
            ) {
                store.set_metadata_value(ETAG_KEY, etag.as_deref().unwrap_or(""))?;
            }
            outcome
        }
    };
    store.set_metadata_value(CHECKED_AT_KEY, &stamp)?;
    Ok(outcome)
}

/// [`refresh_pricing_with`] from [`pricing_url`], with the store's refresh
/// state ([`Store::pricing_refresh_state`]) and [`FETCH_TIMEOUT`].
pub fn refresh_pricing(store: &Store, now: DateTime<Utc>, force: bool) -> Result<RefreshOutcome> {
    refresh_pricing_with(
        store,
        &pricing_url(),
        now,
        force,
        store.pricing_refresh_state()?,
        FETCH_TIMEOUT,
    )
}

/// The arguments, after `--store=<path>`, of the process
/// [`start_background_refresh`] starts: the hidden
/// `statsai pricing refresh --background`, which does what
/// [`run_background_refresh`] does.
pub const BACKGROUND_REFRESH_ARGS: [&str; 3] = ["pricing", "refresh", "--background"];

/// Whether `statsai scan` or `statsai sync` starts a background refresh for
/// `store` at `now`: neither the environment (`disabled_by_env`) nor the
/// store's setting turns the refresh off, and a refresh is due
/// ([`refresh_due`]).
///
/// Only reads. The process it leads to claims the attempt, so a refresh that
/// never starts holds no other process off.
///
/// # Errors
///
/// Only store errors.
pub fn background_refresh_wanted(
    store: &Store,
    disabled_by_env: bool,
    now: DateTime<Utc>,
) -> Result<bool> {
    if disabled_by_env || !store.pricing_refresh_setting()? {
        return Ok(false);
    }
    Ok(RefreshSchedule::read(store)?.is_due(now))
}

/// Starts a due refresh for `statsai scan` and `statsai sync`, and returns
/// whether it started one.
///
/// `store` is the command's open connection to the store at `store_path`. It
/// is only read ([`background_refresh_wanted`]), so nothing starts when
/// `STATSAI_PRICING_REFRESH` or the store's setting turns the refresh off, or
/// no refresh is due. Otherwise it runs this binary again as
/// `statsai --store=<store_path> pricing refresh --background`, a short
/// process that claims the attempt, fetches, offers the file to the store and
/// exits, printing nothing.
///
/// The command never waits for it, and it finishes even when the command
/// exits first. It inherits the environment and the working directory. Its
/// standard input, output and error are null, so a caller that reads the
/// command's output until it closes, such as the menu bar or desktop app, is
/// not held open by it. On Unix it runs in its own process group, so a Ctrl-C
/// in the terminal, which signals the foreground group, does not stop it; on
/// Windows it is detached from the console, in its own process group, and has
/// no window. It inherits no open file: Rust opens files and SQLite opens the
/// store close-on-exec, so it holds neither the command's scan lock nor its
/// store connection. Failing to start it is silent.
pub fn start_background_refresh(store: &Store, store_path: &Path) -> bool {
    if !matches!(
        background_refresh_wanted(store, refresh_disabled_by_env(), Utc::now()),
        Ok(true)
    ) {
        return false;
    }
    let Ok(program) = std::env::current_exe() else {
        return false;
    };
    spawn_background_refresh_process(&program, store_path).is_ok()
}

/// Spawns `program --store=<store_path> pricing refresh --background`,
/// detached as [`start_background_refresh`] describes, and does not wait for
/// it.
fn spawn_background_refresh_process(program: &Path, store_path: &Path) -> std::io::Result<()> {
    // `--store=<path>` in one argument, so a path that starts with `-` is not
    // read as another option.
    let mut store_argument = std::ffi::OsString::from("--store=");
    store_argument.push(store_path);
    let mut command = Command::new(program);
    command
        .arg(store_argument)
        .args(BACKGROUND_REFRESH_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_from_terminal(&mut command);
    // Dropping the handle neither waits for the process nor stops it.
    command.spawn().map(drop)
}

/// Its own process group: a Ctrl-C in the terminal signals only the
/// foreground group, which the command that started it belongs to.
#[cfg(unix)]
fn detach_from_terminal(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// No console shared with the command, its own Ctrl-C group, and no window.
#[cfg(windows)]
fn detach_from_terminal(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // The values of `windows_sys::Win32::System::Threading`'s constants,
    // whose feature this crate does not enable.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(not(any(unix, windows)))]
fn detach_from_terminal(_command: &mut Command) {}

/// What `statsai pricing refresh --background` does: one refresh of the store
/// at `store_path` on the daily schedule, on its own connection, with the
/// environment's refresh state, [`pricing_url`] and [`FETCH_TIMEOUT`].
///
/// See [`run_background_refresh_with`].
///
/// # Errors
///
/// Only store errors, which the caller ignores.
pub fn run_background_refresh(store_path: &Path) -> Result<RefreshOutcome> {
    run_background_refresh_with(
        store_path,
        refresh_disabled_by_env(),
        &pricing_url(),
        FETCH_TIMEOUT,
    )
}

/// [`run_background_refresh`] with the environment's answer, the URL and the
/// timeout given.
///
/// Opens nothing when `disabled_by_env` is set. Otherwise it opens the store,
/// reads its setting, and runs [`refresh_pricing_with`] without `force`, so
/// the daily schedule and the attempt claim hold. It prints nothing and never
/// reprices: the next command that reads prices applies a file it stored.
///
/// # Errors
///
/// Only store errors.
pub fn run_background_refresh_with(
    store_path: &Path,
    disabled_by_env: bool,
    url: &str,
    timeout: Duration,
) -> Result<RefreshOutcome> {
    if disabled_by_env {
        return Ok(RefreshOutcome::Disabled(
            PricingRefreshState::OffByEnvironment,
        ));
    }
    let store = Store::open(store_path)?;
    let refresh = PricingRefreshState::resolve(false, store.pricing_refresh_setting()?);
    refresh_pricing_with(&store, url, Utc::now(), false, refresh, timeout)
}

/// Starts the watch daemon's refresher: a detached thread that checks every
/// [`DAEMON_REFRESH_CHECK_INTERVAL`] whether a refresh is due and runs it on
/// its own connection to the store at `store_path`.
///
/// Returns `None` when `STATSAI_PRICING_REFRESH` turns the refresh off. The
/// thread never ends on its own and does not keep the process alive. The
/// store's setting is read on every check, so turning it off takes effect
/// without a restart. A file it stores reprices the store on the daemon's next
/// scan pass.
pub fn spawn_daemon_refresher(store_path: &Path) -> Option<JoinHandle<()>> {
    if refresh_disabled_by_env() {
        return None;
    }
    let store_path = store_path.to_path_buf();
    std::thread::Builder::new()
        .name("statsai-pricing-refresher".to_string())
        .spawn(move || {
            let url = pricing_url();
            let mut store = None;
            loop {
                if store.is_none() {
                    store = Store::open(&store_path)
                        .map_err(|error| {
                            eprintln!("daemon: pricing refresh: cannot open the store: {error:#}");
                        })
                        .ok();
                }
                if let Some(connection) = store.as_ref() {
                    if let Some(line) = daemon_refresh_once(connection, &url) {
                        eprintln!("{line}");
                    }
                }
                std::thread::sleep(DAEMON_REFRESH_CHECK_INTERVAL);
            }
        })
        .ok()
}

/// One check of the daemon's refresher, and the line to log about it, if any.
fn daemon_refresh_once(store: &Store, url: &str) -> Option<String> {
    let refresh = match store.pricing_refresh_setting() {
        Ok(setting_on) => PricingRefreshState::resolve(false, setting_on),
        Err(error) => return Some(format!("daemon: pricing refresh: {error:#}")),
    };
    match refresh_pricing_with(store, url, Utc::now(), false, refresh, FETCH_TIMEOUT) {
        Ok(RefreshOutcome::Adopted { version }) => Some(format!(
            "daemon: pricing: stored pricing file version {version} from GitHub; it applies at the next scan pass"
        )),
        Ok(RefreshOutcome::Rejected(reason)) => Some(format!(
            "daemon: pricing: ignored the pricing file from {url}: {reason}"
        )),
        Ok(RefreshOutcome::Failed(reason)) => Some(format!(
            "daemon: pricing: could not fetch the pricing file from {url}: {reason}"
        )),
        Ok(
            RefreshOutcome::Disabled(_)
            | RefreshOutcome::NotDue
            | RefreshOutcome::NotModified
            | RefreshOutcome::NotNewer { .. },
        ) => None,
        Err(error) => Some(format!("daemon: pricing refresh: {error:#}")),
    }
}

#[cfg(test)]
mod tests;
