use crate::legacy_bundle::{
    is_menu_bar_identifier, is_translocated, launch_agent_points_into, legacy_bundle_candidates,
    login_item_update_script, parse_login_items, running_bundle, BUNDLE_IDENTIFIER,
    LIST_LOGIN_ITEMS_SCRIPT,
};
use objc2_app_kit::NSRunningApplication;
use objc2_foundation::{NSFileManager, NSString, NSURL};
use statsai_core::{home_dir, DAEMON_LAUNCH_AGENT_LABEL};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const QUIT_TIMEOUT: Duration = Duration::from_secs(5);
// Long enough for the user to answer the one-time Automation prompt.
const OSASCRIPT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub(super) struct LegacyMigration {
    /// The LaunchAgent still runs the CLI from a bundle that is now in the
    /// Trash, so it has to be reinstalled from this bundle.
    pub(super) launch_agent_needs_reinstall: bool,
}

/// Retires copies of the menu bar app installed under its old bundle name:
/// quits a running one, moves its login item to this bundle and moves it to
/// the Trash. Only a bundle whose identifier is the menu bar app's is touched,
/// and only once its login item, if any, points here.
/// Every failure is logged and never stops the launch.
pub(super) fn migrate_legacy_bundles() -> LegacyMigration {
    let mut migration = LegacyMigration::default();
    let Some(current) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok())
        .and_then(|exe| running_bundle(&exe))
    else {
        return migration;
    };
    if is_translocated(&current) {
        return migration;
    }

    for legacy in legacy_bundle_candidates(home_dir().as_deref()) {
        if !legacy.is_dir() || same_path(&legacy, &current) {
            continue;
        }
        match bundle_identifier(&legacy) {
            Ok(identifier) if is_menu_bar_identifier(&identifier) => {}
            Ok(_) => continue,
            Err(message) => {
                eprintln!("statsai menubar: skipped {}: {message}", legacy.display());
                continue;
            }
        }

        quit_running_copies(&legacy);
        // Before the move: a login item follows its bundle into the Trash, and
        // would open the old copy from there at every login. When it can't be
        // moved, the old copy stays where it is.
        if let Err(message) = move_login_items(&legacy, &current) {
            eprintln!(
                "statsai menubar: could not move the login item for {}, so it stays in \
                 place; quit it and move it out of Applications: {message}",
                legacy.display()
            );
            continue;
        }
        match move_to_trash(&legacy) {
            Ok(()) => {
                if launch_agent_runs_from(&legacy) {
                    migration.launch_agent_needs_reinstall = true;
                }
            }
            Err(message) => eprintln!(
                "statsai menubar: could not move {} to the Trash: {message}",
                legacy.display()
            ),
        }
    }
    migration
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn bundle_identifier(bundle: &Path) -> Result<String, String> {
    let info_plist = bundle.join("Contents").join("Info.plist");
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(&info_plist)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("run plutil: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "read CFBundleIdentifier from {}",
            info_plist.display()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// An old copy holds the instance lock, so this launch cannot start until it
/// exits. Ask it to quit, then send SIGTERM if it is still running.
fn quit_running_copies(legacy: &Path) {
    let identifier = NSString::from_str(BUNDLE_IDENTIFIER);
    let mut pids = Vec::new();
    for app in NSRunningApplication::runningApplicationsWithBundleIdentifier(&identifier).iter() {
        let Some(path) = app.bundleURL().and_then(|url| url.path()) else {
            continue;
        };
        let pid = app.processIdentifier();
        if pid > 0 && same_path(Path::new(&path.to_string()), legacy) {
            app.terminate();
            pids.push(pid);
        }
    }
    if pids.is_empty() || wait_for_exit(&pids, QUIT_TIMEOUT) {
        return;
    }
    for &pid in &pids {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    if !wait_for_exit(&pids, QUIT_TIMEOUT) {
        eprintln!(
            "statsai menubar: {} is still running after a quit request",
            legacy.display()
        );
    }
}

// NSRunningApplication's state only refreshes on the main run loop, which is
// not running yet, so poll the processes themselves.
fn wait_for_exit(pids: &[libc::pid_t], timeout: Duration) -> bool {
    let started = Instant::now();
    loop {
        if pids.iter().all(|&pid| unsafe { libc::kill(pid, 0) } != 0) {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn move_login_items(legacy: &Path, current: &Path) -> Result<(), String> {
    let items = parse_login_items(&run_osascript(LIST_LOGIN_ITEMS_SCRIPT)?);
    match login_item_update_script(&items, legacy, current) {
        Some(script) => run_osascript(&script).map(|_| ()),
        None => Ok(()),
    }
}

fn run_osascript(script: &str) -> Result<String, String> {
    let mut command = Command::new("/usr/bin/osascript");
    for line in script.lines() {
        command.arg("-e").arg(line);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("run osascript: {err}"))?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() >= OSASCRIPT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "osascript timed out after {} seconds",
                    OSASCRIPT_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait for osascript: {err}"));
            }
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|err| format!("read osascript output: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Moves the bundle to the user's Trash, where it can still be recovered.
fn move_to_trash(bundle: &Path) -> Result<(), String> {
    let path = bundle
        .to_str()
        .ok_or_else(|| "path is not valid UTF-8".to_string())?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|err| err.localizedDescription().to_string())
}

fn launch_agent_runs_from(bundle: &Path) -> bool {
    launch_agent_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|plist| launch_agent_points_into(&plist, bundle))
}

fn launch_agent_path() -> Option<PathBuf> {
    Some(
        home_dir()?
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{DAEMON_LAUNCH_AGENT_LABEL}.plist")),
    )
}
