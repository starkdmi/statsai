//! `statsai scan` starts the pricing refresh as a detached
//! `statsai pricing refresh --background` process: the scan never waits for
//! it, it finishes after the scan has exited, it holds neither the scan lock
//! nor the scan's store connection, and nothing starts while the refresh is
//! off.
//!
//! Every `statsai` this file runs gets `STATSAI_PRICING_REFRESH` and
//! `STATSAI_PRICING_URL` set on it explicitly; the test process's own
//! environment is never read for the refresh policy, nor changed. The only
//! network is a server on 127.0.0.1.

use serde_json::{json, Value};
use statsai_store::{embedded_pricing, PricingTable, Store, STORED_PRICING_FILE_KEY};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tiny_http::{Header, Response, Server};

/// The longest the server holds its answer to the refresh. The test releases
/// it sooner, once it has checked everything it checks while the request is
/// pending. It stays well under the client's 10-second timeout.
const HOLD: Duration = Duration::from_secs(6);

/// How long the test waits for the background process to send its request.
const REQUEST_WAIT: Duration = Duration::from_secs(20);

/// How long the test waits for the background process to store the file.
const COMPLETION_WAIT: Duration = Duration::from_secs(30);

const CHECKED_AT_KEY: &str = "pricing.refresh.checked_at";
const ATTEMPT_AT_KEY: &str = "pricing.refresh.attempt_at";

/// A `statsai` with an empty home, its own device id and the refresh off;
/// [`refresh_on`] turns it on.
fn statsai(home: &Path, store: &Path) -> Command {
    let mut store_argument = std::ffi::OsString::from("--store=");
    store_argument.push(store);
    statsai_with_store_argument(home, store_argument)
}

/// [`statsai`] with its `--store` argument exactly as given.
fn statsai_with_store_argument(home: &Path, store_argument: std::ffi::OsString) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_statsai"));
    command
        .arg(store_argument)
        // Nothing to collect, and nothing outside the test directory to find.
        .env("HOME", home)
        .env("CODEX_HOME", home.join("codex"))
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("STATSAI_DEVICE_ID", "pricing-refresh-cli-test")
        .env("STATSAI_PRICING_REFRESH", "off")
        .env_remove("STATSAI_PRICING_URL");
    command
}

fn refresh_on<'a>(command: &'a mut Command, url: &str) -> &'a mut Command {
    command
        .env("STATSAI_PRICING_REFRESH", "on")
        .env("STATSAI_PRICING_URL", url)
}

/// The embedded pricing file with its `version` raised by one.
fn newer_pricing_text() -> (String, u64) {
    let embedded = embedded_pricing();
    let mut file: Value = serde_json::from_str(embedded.source_text()).expect("embedded json");
    let version = embedded.version() + 1;
    file["version"] = json!(version);
    (
        serde_json::to_string_pretty(&file).expect("serialize pricing file"),
        version,
    )
}

/// A server on 127.0.0.1 that answers each request with `body`, but only once
/// the test releases it or [`HOLD`] has passed since the request arrived.
struct HeldServer {
    url: String,
    server: Arc<Server>,
    requests: Receiver<Instant>,
    release: Sender<()>,
    answered: Arc<AtomicBool>,
    thread: thread::JoinHandle<usize>,
}

impl HeldServer {
    fn start(body: String) -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind test server"));
        let address = server.server_addr().to_ip().expect("ip address");
        let url = format!("http://{address}/crates/statsai-pricing/pricing.json");
        let (request_sender, requests) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel::<()>();
        let answered = Arc::new(AtomicBool::new(false));
        let thread = {
            let server = Arc::clone(&server);
            let answered = Arc::clone(&answered);
            thread::spawn(move || {
                let mut count = 0;
                // Ends when the test unblocks the server.
                while let Ok(request) = server.recv() {
                    let _ = request_sender.send(Instant::now());
                    let _ = release_receiver.recv_timeout(HOLD);
                    let response = Response::from_string(body.clone())
                        .with_header(Header::from_bytes("ETag", "\"newer\"").expect("etag header"));
                    answered.store(true, Ordering::SeqCst);
                    let _ = request.respond(response);
                    count += 1;
                }
                count
            })
        };
        Self {
            url,
            server,
            requests,
            release,
            answered,
            thread,
        }
    }

    fn has_answered(&self) -> bool {
        self.answered.load(Ordering::SeqCst)
    }

    /// Stops the server and returns how many requests it answered.
    fn stop(self) -> usize {
        drop(self.release);
        self.server.unblock();
        self.thread.join().expect("test server thread")
    }
}

/// Runs `command`, which must succeed, and returns its output, its process id
/// and when it exited.
fn run_to_exit(command: &mut Command) -> (Output, u32, Instant) {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run statsai");
    let pid = child.id();
    // Reads both pipes until they close, so it returns only once every
    // process holding them has exited or closed them.
    let output = child.wait_with_output().expect("statsai output");
    let exited = Instant::now();
    assert!(
        output.status.success(),
        "statsai failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (output, pid, exited)
}

fn metadata(store: &Store, key: &str) -> Option<String> {
    store.metadata_value(key).expect("metadata value")
}

#[test]
fn the_refresh_a_scan_starts_finishes_after_the_scan_exits_without_its_lock() {
    let directory = tempfile::tempdir().expect("tempdir");
    let home = directory.path().join("home");
    std::fs::create_dir(&home).expect("home");
    let store_path = directory.path().join("statsai.sqlite");
    let (text, version) = newer_pricing_text();
    let server = HeldServer::start(text);

    let started = Instant::now();
    let (_, scan_pid, exited) =
        run_to_exit(refresh_on(&mut statsai(&home, &store_path), &server.url).arg("scan"));

    // The scan, its output pipes included, finished while the server still
    // held its answer: it waited neither for the refresh nor for the process
    // that runs it.
    assert!(
        !server.has_answered(),
        "the scan exited only after the pricing request was answered"
    );
    assert!(
        exited.duration_since(started) < HOLD,
        "the scan took {:?}",
        exited.duration_since(started)
    );
    // The scan released its lock, and the process it started did not keep
    // it: another scan can start at once.
    let lock = statsai::scan_lock_path(&store_path);
    let guard = statsai::try_acquire_scan_lock(&lock)
        .expect("open the scan lock")
        .expect("the scan lock is free as soon as the scan exits");
    drop(guard);

    let requested = server
        .requests
        .recv_timeout(REQUEST_WAIT)
        .expect("the background refresh sent its request");
    assert!(!server.has_answered());
    #[cfg(target_os = "linux")]
    linux::assert_detached_without_inherited_files(&store_path, &lock, scan_pid);
    #[cfg(not(target_os = "linux"))]
    let _ = scan_pid;
    server.release.send(()).expect("release the answer");

    // The background process stores the file after the scan has exited.
    let store = Store::open(&store_path).expect("open store");
    let deadline = requested + COMPLETION_WAIT;
    while metadata(&store, CHECKED_AT_KEY).is_none() {
        assert!(
            Instant::now() < deadline,
            "the background refresh did not finish within {COMPLETION_WAIT:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let stored = metadata(&store, STORED_PRICING_FILE_KEY).expect("a stored pricing file");
    assert_eq!(
        PricingTable::header(&stored).expect("header").version,
        version
    );
    assert!(metadata(&store, ATTEMPT_AT_KEY).is_some());
    assert_eq!(
        metadata(&store, "pricing.refresh.etag").as_deref(),
        Some("\"newer\"")
    );
    drop(store);
    assert_eq!(server.stop(), 1, "exactly one request");

    let (output, _, _) = run_to_exit(
        refresh_on(
            &mut statsai(&home, &store_path),
            "http://127.0.0.1:1/pricing.json",
        )
        .args(["pricing", "status", "--json"]),
    );
    let status: Value = serde_json::from_slice(&output.stdout).expect("status json");
    assert_eq!(status["stored"]["state"], "usable", "{status:#}");
    assert_eq!(status["stored"]["version"], version, "{status:#}");
    assert_eq!(status["prices_with"]["version"], version, "{status:#}");
    assert_eq!(status["prices_with"]["source"], "stored", "{status:#}");
}

#[test]
fn the_background_refresh_reaches_a_relative_store_path_that_starts_with_a_dash() {
    let directory = tempfile::tempdir().expect("tempdir");
    let home = directory.path().join("home");
    std::fs::create_dir(&home).expect("home");
    let (text, version) = newer_pricing_text();
    let server = HeldServer::start(text);
    // Answer at once.
    server.release.send(()).expect("release the answer");

    // Relative to the working directory, which the background process
    // inherits; passed on as `--store=<path>`, so it is not read as an option.
    let mut command = statsai_with_store_argument(&home, "--store=-store/statsai.sqlite".into());
    command.current_dir(directory.path());
    run_to_exit(refresh_on(&mut command, &server.url).arg("scan"));

    let requested = server
        .requests
        .recv_timeout(REQUEST_WAIT)
        .expect("the background refresh sent its request");
    let store =
        Store::open(&directory.path().join("-store").join("statsai.sqlite")).expect("open store");
    let deadline = requested + COMPLETION_WAIT;
    while metadata(&store, CHECKED_AT_KEY).is_none() {
        assert!(
            Instant::now() < deadline,
            "the background refresh did not finish within {COMPLETION_WAIT:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let stored = metadata(&store, STORED_PRICING_FILE_KEY).expect("a stored pricing file");
    assert_eq!(
        PricingTable::header(&stored).expect("header").version,
        version
    );
    drop(store);
    assert_eq!(server.stop(), 1, "exactly one request");
}

#[test]
fn a_scan_with_the_refresh_off_starts_no_refresh() {
    let directory = tempfile::tempdir().expect("tempdir");
    let home = directory.path().join("home");
    std::fs::create_dir(&home).expect("home");
    let store_path = directory.path().join("statsai.sqlite");
    let (text, _) = newer_pricing_text();
    let server = HeldServer::start(text);

    // The URL is set, so only the variable keeps the refresh from running.
    let (_, _, exited) = run_to_exit(
        statsai(&home, &store_path)
            .env("STATSAI_PRICING_URL", &server.url)
            .arg("scan"),
    );

    let waited = server
        .requests
        .recv_timeout(Duration::from_secs(3).saturating_sub(exited.elapsed()));
    assert_eq!(waited, Err(RecvTimeoutError::Timeout), "a request was sent");
    assert_eq!(server.stop(), 0);
    let store = Store::open(&store_path).expect("open store");
    assert_eq!(metadata(&store, ATTEMPT_AT_KEY), None);
    assert_eq!(metadata(&store, CHECKED_AT_KEY), None);
}

#[test]
fn the_background_refresh_prints_nothing_and_never_fails() {
    let directory = tempfile::tempdir().expect("tempdir");
    let home = directory.path().join("home");
    std::fs::create_dir(&home).expect("home");
    let store_path = directory.path().join("statsai.sqlite");
    drop(Store::open(&store_path).expect("create store"));
    let not_a_directory = directory.path().join("file");
    std::fs::write(&not_a_directory, b"").expect("file");

    for (store, expect_checked) in [
        // Nothing listens there: the request fails, and the failed attempt
        // counts as the day's check.
        (store_path.clone(), true),
        // The store cannot even be opened.
        (not_a_directory.join("statsai.sqlite"), false),
    ] {
        let (output, _, _) = run_to_exit(
            refresh_on(
                &mut statsai(&home, &store),
                "http://127.0.0.1:1/pricing.json",
            )
            .args(["pricing", "refresh", "--background"]),
        );
        assert!(output.stdout.is_empty(), "{}", store.display());
        assert!(
            output.stderr.is_empty(),
            "{}: {}",
            store.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        if expect_checked {
            let store = Store::open(&store).expect("open store");
            assert!(metadata(&store, CHECKED_AT_KEY).is_some());
            assert_eq!(metadata(&store, STORED_PRICING_FILE_KEY), None);
            assert_eq!(
                store
                    .applied_pricing_ruleset_version()
                    .expect("applied version"),
                None,
                "the background refresh never reprices"
            );
        }
    }
}

/// What `/proc` shows about the background refresh process on Linux.
#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// The ids of the running `statsai ... pricing refresh --background`
    /// processes for the store at `store`.
    fn background_refresh_processes(store: &Path) -> Vec<u32> {
        let mut store_argument = b"--store=".to_vec();
        store_argument.extend_from_slice(store.as_os_str().as_encoded_bytes());
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
                let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
                let args = cmdline.split(|byte| *byte == 0).collect::<Vec<_>>();
                (args.contains(&&b"--background"[..]) && args.contains(&store_argument.as_slice()))
                    .then_some(pid)
            })
            .collect()
    }

    fn open_files(pid: u32) -> Vec<(String, PathBuf)> {
        std::fs::read_dir(format!("/proc/{pid}/fd"))
            .expect("list open files")
            .flatten()
            .filter_map(|entry| {
                let target = std::fs::read_link(entry.path()).ok()?;
                Some((entry.file_name().to_string_lossy().into_owned(), target))
            })
            .collect()
    }

    /// The parent process id and the process group id in `/proc/<pid>/stat`.
    fn parent_and_group(pid: u32) -> (u32, u32) {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
        // After the command name, which is in parentheses and may hold spaces:
        // state, parent id, process group id.
        let fields = stat[stat.rfind(')').expect("command name") + 1..]
            .split_whitespace()
            .collect::<Vec<_>>();
        (
            fields[1].parse().expect("parent id"),
            fields[2].parse().expect("process group id"),
        )
    }

    /// While its request is pending, the background process runs on after
    /// the scan (`scan_pid`) exited, in its own process group, with its
    /// standard streams on `/dev/null`, without the scan lock at `lock`, and
    /// with only its own connection to the store at `store`.
    pub(super) fn assert_detached_without_inherited_files(
        store: &Path,
        lock: &Path,
        scan_pid: u32,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let pid = loop {
            match background_refresh_processes(store).as_slice() {
                [pid] => break *pid,
                [] if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                found => panic!("expected one background refresh process, found {found:?}"),
            }
        };

        let (parent, group) = parent_and_group(pid);
        assert_ne!(parent, scan_pid, "it outlives the scan that started it");
        assert_eq!(group, pid, "it leads its own process group");

        let files = open_files(pid);
        let target = |fd: &str| {
            files
                .iter()
                .find(|(number, _)| number == fd)
                .map(|(_, target)| target.clone())
        };
        for fd in ["0", "1", "2"] {
            assert_eq!(
                target(fd).as_deref(),
                Some(Path::new("/dev/null")),
                "fd {fd}: {files:?}"
            );
        }
        let lock = std::fs::canonicalize(lock).expect("lock path");
        assert!(
            files.iter().all(|(_, target)| *target != lock),
            "it holds the scan lock: {files:?}"
        );
        // Its own connection opens the database once. The scan's connection,
        // inherited, would be a second descriptor.
        let database = std::fs::canonicalize(store).expect("store path");
        let database_files = files
            .iter()
            .filter(|(_, target)| *target == database)
            .count();
        assert!(
            database_files <= 1,
            "it holds the store {database_files} times: {files:?}"
        );
    }
}
