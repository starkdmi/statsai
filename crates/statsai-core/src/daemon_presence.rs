//! Whether anything else could be writing to a StatsAI store right now.
//!
//! Three independent facts, deliberately kept together. The LaunchAgent may be
//! loaded before the daemon answers; a daemon started by hand answers without any
//! LaunchAgent; and `statsai daemon --api` accepts any loopback address, so a
//! daemon can hold the store open while neither of the first two says anything at
//! all. Only the last check -- who has the file open -- is independent of how the
//! daemon was started and which binary started it, which is what matters when the
//! daemon in the field is an older release.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DAEMON_LAUNCH_AGENT_LABEL: &str = "dev.statsai.daemon";
pub const DAEMON_LOOPBACK_ADDRESS: &str = "127.0.0.1:8765";

const PROBE_TIMEOUT: Duration = Duration::from_millis(400);
/// The status lsof exits with when it looked and found nothing.
const LSOF_NOTHING_FOUND: i32 = 1;
/// The status `launchctl print` exits with when the domain exists and holds no
/// such service -- the one non-zero result that means "not loaded". A domain it
/// could not reach exits 112 and a malformed target exits 64, and neither says
/// anything about the daemon.
const LAUNCHCTL_NO_SUCH_SERVICE: i32 = 113;

/// Whether the daemon's LaunchAgent is loaded, including the case where asking
/// did not work.
///
/// The third state is not pedantry. A LaunchAgent with `KeepAlive` has a restart
/// window in which the port is closed and the database is unopened, so during it
/// the only signal that the daemon is coming back is the LaunchAgent itself. A
/// probe that failed in that window and reported `NotLoaded` would be handing out
/// exactly the wrong answer at exactly the wrong moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchAgentState {
    Loaded,
    NotLoaded,
    /// The probe could not be run, so this says nothing either way.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonPresence {
    pub launch_agent: LaunchAgentState,
    pub reachable: bool,
}

impl DaemonPresence {
    /// True when anything could be writing to the store on this host, *or* when
    /// that could not be established. Callers guarding production have to treat
    /// both the same way; the fields say which it was.
    #[must_use]
    pub fn any(self) -> bool {
        !matches!(self.launch_agent, LaunchAgentState::NotLoaded) || self.reachable
    }
}

/// Probes both facts at the default label and loopback address.
#[must_use]
pub fn daemon_presence() -> DaemonPresence {
    DaemonPresence {
        launch_agent: launch_agent_state(),
        reachable: daemon_reachable_at(DAEMON_LOOPBACK_ADDRESS),
    }
}

#[must_use]
pub fn launch_agent_target(domain: &str) -> String {
    format!("{domain}/{DAEMON_LAUNCH_AGENT_LABEL}")
}

#[must_use]
pub fn gui_domain() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let uid = String::from_utf8(output.stdout).ok()?;
    let uid = uid.trim();
    (!uid.is_empty()).then(|| format!("gui/{uid}"))
}

/// The LaunchAgent's state in this user's GUI domain.
///
/// A domain that cannot be resolved is `Unknown` rather than `NotLoaded`: on macOS
/// every user has one, so failing to read it means `id` failed, not that there is
/// no LaunchAgent to find. Elsewhere there are no LaunchAgents at all, which is a
/// real `NotLoaded`.
#[must_use]
pub fn launch_agent_state() -> LaunchAgentState {
    if !cfg!(target_os = "macos") {
        return LaunchAgentState::NotLoaded;
    }
    match gui_domain() {
        Some(domain) => launch_agent_state_in(&domain),
        None => LaunchAgentState::Unknown,
    }
}

/// Reads the LaunchAgent's state out of `launchctl print`.
///
/// Only one failure means "not loaded", and it has its own exit status. Treating
/// every non-zero result that way would fold "the domain could not be reached"
/// (112) and "that is not a target" (64) into "the daemon is not running", which
/// is the reading this must never produce.
#[must_use]
pub fn launch_agent_state_in(domain: &str) -> LaunchAgentState {
    if !cfg!(target_os = "macos") {
        return LaunchAgentState::NotLoaded;
    }
    match std::process::Command::new("launchctl")
        .args(["print", launch_agent_target(domain).as_str()])
        .output()
    {
        Ok(output) => classify_launchctl(output.status),
        Err(_) => LaunchAgentState::Unknown,
    }
}

fn classify_launchctl(status: std::process::ExitStatus) -> LaunchAgentState {
    if status.success() {
        return LaunchAgentState::Loaded;
    }
    match status.code() {
        Some(LAUNCHCTL_NO_SUCH_SERVICE) => LaunchAgentState::NotLoaded,
        _ => LaunchAgentState::Unknown,
    }
}

/// For callers that only report or precheck, where "could not tell" and "not
/// loaded" lead to the same next step. Anything guarding production wants
/// [`launch_agent_state_in`] instead.
#[must_use]
pub fn launch_agent_loaded_in(domain: &str) -> bool {
    matches!(launch_agent_state_in(domain), LaunchAgentState::Loaded)
}

/// A connect probe rather than an HTTP request: an unauthenticated caller cannot
/// ask the daemon anything, but it can tell whether something holds the port.
#[must_use]
pub fn daemon_reachable_at(address: &str) -> bool {
    let Ok(address) = address.parse::<SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&address, PROBE_TIMEOUT).is_ok()
}

/// A process holding a store's files open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreOpener {
    pub pid: u32,
    pub command: String,
}

impl std::fmt::Display for StoreOpener {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "pid {} ({})", self.pid, self.command)
    }
}

/// Lists the other processes with `store` or its SQLite sidecars open.
///
/// This is the endpoint-independent writer check: a daemon on a non-default
/// `--api` address, a second CLI, or a daemon from an older release all appear
/// here, and none of them appear in the LaunchAgent or loopback probes. This
/// process is excluded; a caller that has the store open itself would otherwise
/// find only itself.
///
/// # Errors
///
/// Returns an error when `lsof` cannot be run, so that a caller guarding
/// production can refuse rather than read "no answer" as "no writers".
pub fn store_openers(store: &Path) -> std::io::Result<Vec<StoreOpener>> {
    let existing = existing_store_files(store);
    if existing.is_empty() {
        return Ok(Vec::new());
    }
    match probe_openers(&existing) {
        Ok(openers) => Ok(openers),
        Err(error) => {
            // A `-wal` that SQLite removed while this was running makes lsof fail on a
            // path that no longer exists, which is a race rather than an unanswerable
            // question. Retried once, and only when the set of files actually shrank.
            let remaining = existing_store_files(store);
            if remaining.len() < existing.len() && !remaining.is_empty() {
                return probe_openers(&remaining);
            }
            Err(error)
        }
    }
}

fn existing_store_files(store: &Path) -> Vec<PathBuf> {
    store_files(store)
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

/// Runs one lsof probe over `files`.
///
/// lsof reports "nothing has these files open" as exit status 1 with no output at
/// all, and reports its own failures -- an unreadable path, a permission problem --
/// as the same status *with* a message on stderr. Reading the status alone would
/// call every failure "no openers", which is the reading this guard must never
/// make, so the two are told apart by whether anything was said.
fn probe_openers(files: &[PathBuf]) -> std::io::Result<Vec<StoreOpener>> {
    // `-F pc` asks for machine-readable output: one `p<pid>` line, then a `c<command>`
    // line for each of that process's matching descriptors.
    //
    // Deliberately without `-w`. That flag suppresses warnings -- including the ones
    // lsof emits when it cannot inspect a path or a process -- and warnings are the
    // only thing separating "nothing has these files open" from "some of what I
    // looked at would not answer me". Suppressing them turns the second into the
    // first, silently, which is the one reading this guard must never produce. The
    // cost is that any diagnostic at all refuses; in practice a probe over explicit
    // paths that finds nothing says nothing.
    let output = std::process::Command::new("lsof")
        .args(["-F", "pc"])
        .arg("--")
        .args(files)
        .output()?;
    interpret_probe(
        output.status,
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
        files,
    )
}

/// Decides what one lsof run actually said.
///
/// Exit status 1 with no output is lsof's way of saying "nothing has these files
/// open". Its own failures share that status but say so on stderr -- as errors, or
/// as the warnings [`probe_openers`] is careful not to suppress -- and a run killed
/// by a signal says nothing at all. So silence only means "no openers" when it comes
/// with the exact status lsof uses for that answer. Every other outcome is "no
/// answer", which this guard must never read as "no writers".
fn interpret_probe(
    status: std::process::ExitStatus,
    stdout: &str,
    stderr: &str,
    files: &[PathBuf],
) -> std::io::Result<Vec<StoreOpener>> {
    let openers = parse_lsof_openers(stdout, std::process::id());
    if status.success() || !openers.is_empty() {
        return Ok(openers);
    }
    let complaint = stderr.lines().next().unwrap_or_default().trim();
    if complaint.is_empty() && status.code() == Some(LSOF_NOTHING_FOUND) {
        return Ok(Vec::new());
    }
    let complaint = if complaint.is_empty() {
        "no output"
    } else {
        complaint
    };
    Err(std::io::Error::other(format!(
        "lsof could not report who has {} open ({}): {complaint}",
        files
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        describe_status(status)
    )))
}

fn describe_status(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => "terminated by a signal".to_string(),
    }
}

fn store_files(store: &Path) -> [PathBuf; 4] {
    let with_suffix = |suffix: &str| {
        let mut value = store.as_os_str().to_os_string();
        value.push(suffix);
        PathBuf::from(value)
    };
    [
        store.to_path_buf(),
        with_suffix("-wal"),
        with_suffix("-shm"),
        with_suffix("-journal"),
    ]
}

fn parse_lsof_openers(output: &str, exclude_pid: u32) -> Vec<StoreOpener> {
    let mut openers: Vec<StoreOpener> = Vec::new();
    let mut pid = None;
    for line in output.lines() {
        let Some((field, value)) = line.split_at_checked(1) else {
            continue;
        };
        match field {
            "p" => pid = value.trim().parse::<u32>().ok(),
            "c" => {
                let Some(pid) = pid else { continue };
                if pid == exclude_pid || openers.iter().any(|opener| opener.pid == pid) {
                    continue;
                }
                openers.push(StoreOpener {
                    pid,
                    command: value.trim().to_string(),
                });
            }
            _ => {}
        }
    }
    openers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openers_are_read_per_process_and_exclude_this_one() {
        let output = "p321\ncstatsai-daemon\np777\ncstatsai\ncstatsai\np999\ncstatsai\n";
        assert_eq!(
            parse_lsof_openers(output, 777),
            vec![
                StoreOpener {
                    pid: 321,
                    command: "statsai-daemon".to_string()
                },
                StoreOpener {
                    pid: 999,
                    command: "statsai".to_string()
                }
            ]
        );
    }

    #[test]
    fn no_output_means_no_openers() {
        assert!(parse_lsof_openers("", 1).is_empty());
        // A command line without a preceding pid line describes nothing.
        assert!(parse_lsof_openers("cstatsai\n", 1).is_empty());
    }

    #[test]
    fn a_store_nobody_has_open_lists_no_openers() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = directory.path().join("statsai.sqlite");
        std::fs::write(
            &store,
            b"not a database, but a file lsof can be asked about",
        )
        .expect("write store");

        assert_eq!(store_openers(&store).expect("probe openers"), Vec::new());
    }

    #[test]
    #[cfg(unix)]
    fn only_a_missing_service_reads_as_not_loaded() {
        use std::os::unix::process::ExitStatusExt;

        let exited = |code: i32| std::process::ExitStatus::from_raw(code << 8);

        assert_eq!(classify_launchctl(exited(0)), LaunchAgentState::Loaded);
        assert_eq!(
            classify_launchctl(exited(LAUNCHCTL_NO_SUCH_SERVICE)),
            LaunchAgentState::NotLoaded
        );
        // 112 is "no such domain" and 64 is a usage error: both are launchctl failing
        // to look, not launchctl reporting that nothing is loaded.
        assert_eq!(classify_launchctl(exited(112)), LaunchAgentState::Unknown);
        assert_eq!(classify_launchctl(exited(64)), LaunchAgentState::Unknown);
        assert_eq!(
            classify_launchctl(std::process::ExitStatus::from_raw(9)),
            LaunchAgentState::Unknown
        );
    }

    /// The statuses above are what this machine's launchctl actually returns, so
    /// the constant is pinned to observed behaviour rather than to documentation.
    #[test]
    #[cfg(target_os = "macos")]
    fn launchctl_reports_a_missing_service_with_the_expected_status() {
        let Some(domain) = gui_domain() else {
            return;
        };
        let status = std::process::Command::new("launchctl")
            .args([
                "print",
                &format!("{domain}/dev.statsai.daemon.absent.probe-only"),
            ])
            .output()
            .expect("run launchctl")
            .status;

        assert_eq!(status.code(), Some(LAUNCHCTL_NO_SUCH_SERVICE));
        assert_eq!(classify_launchctl(status), LaunchAgentState::NotLoaded);
    }

    #[test]
    #[cfg(unix)]
    fn silence_only_means_no_openers_at_the_status_lsof_uses_for_it() {
        use std::os::unix::process::ExitStatusExt;

        let files = [PathBuf::from("/tmp/statsai.sqlite")];
        let exited = |code: i32| std::process::ExitStatus::from_raw(code << 8);

        assert_eq!(
            interpret_probe(exited(LSOF_NOTHING_FOUND), "", "", &files).expect("nothing found"),
            Vec::new()
        );
        // Killed before it could look: no output, and no answer either. Reading this
        // as "nothing has the store open" is what would let a live daemon through.
        let signalled = interpret_probe(std::process::ExitStatus::from_raw(9), "", "", &files)
            .expect_err("a signalled probe answers nothing");
        assert!(signalled.to_string().contains("no output"), "{signalled}");
        // An exit code lsof does not use for "nothing found" is equally unreadable.
        assert!(interpret_probe(exited(2), "", "", &files).is_err());
        // Whatever the status, openers that were reported are still openers.
        assert_eq!(
            interpret_probe(exited(1), "p42\ncstatsai\n", "", &files)
                .expect("reported openers count")
                .len(),
            1
        );
    }

    /// lsof answers "nobody has this open" and "I could not tell you" with the same
    /// exit status, and reading the second as the first is what would let a live
    /// daemon through.
    #[test]
    fn a_probe_that_could_not_answer_is_an_error_not_an_empty_list() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing = directory.path().join("vanished.sqlite");

        let error = probe_openers(std::slice::from_ref(&missing))
            .expect_err("lsof must report its failure");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        let message = error.to_string();
        assert!(message.contains("could not report who has"), "{message}");
        assert!(message.contains("vanished.sqlite"), "{message}");
    }

    /// The check exists for a daemon that no port or LaunchAgent probe can see, so
    /// it is worth proving against a process actually holding the file rather than
    /// only against parsed output.
    #[test]
    #[cfg(unix)]
    fn a_process_holding_the_store_open_is_found() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = directory.path().join("statsai.sqlite");
        std::fs::write(&store, b"held open by another process").expect("write store");

        let mut holder = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("exec 3< '{}'; sleep 10", store.display()))
            .spawn()
            .expect("spawn a process holding the store open");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let found = loop {
            let openers = store_openers(&store).expect("probe openers");
            if openers.iter().any(|opener| opener.pid == holder.id()) {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let _ = holder.kill();
        let _ = holder.wait();

        assert!(found, "a process holding the store open went undetected");
    }

    #[test]
    fn launch_agent_target_combines_domain_and_label() {
        assert_eq!(launch_agent_target("gui/501"), "gui/501/dev.statsai.daemon");
    }

    #[test]
    fn any_signal_alone_counts_as_present_including_not_knowing() {
        let absent = DaemonPresence {
            launch_agent: LaunchAgentState::NotLoaded,
            reachable: false,
        };
        assert!(!absent.any());
        assert!(DaemonPresence {
            launch_agent: LaunchAgentState::Loaded,
            ..absent
        }
        .any());
        assert!(DaemonPresence {
            reachable: true,
            ..absent
        }
        .any());
        // A KeepAlive daemon between restarts holds neither the port nor the
        // database, so an unreadable LaunchAgent is the only thing left to go on.
        assert!(DaemonPresence {
            launch_agent: LaunchAgentState::Unknown,
            ..absent
        }
        .any());
    }

    #[test]
    fn an_unparsable_address_is_not_reachable() {
        assert!(!daemon_reachable_at("not-an-address"));
    }
}
