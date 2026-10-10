use anyhow::Result;
use clap::Parser;
use statsai::{default_device_id, default_store_path, snapshot, ScanLockGuard};
use statsai_store::Store;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;

/// How long `statsai scan` and `statsai sync` wait for another scanner, such
/// as the watch daemon, to finish before skipping this run.
pub(crate) const SCAN_LOCK_WAIT: Duration = Duration::from_secs(5);

pub(crate) fn run() -> Result<()> {
    let cli = Cli::parse();
    let store_path = cli.store.unwrap_or_else(default_store_path);
    let device_id = cli.device_id.unwrap_or_else(default_device_id);

    match cli.command {
        Command::Schema(command) => schema(command),
        Command::Store(command) => store_admin(command, &store_path),
        Command::Doctor => doctor(&store_path),
        Command::Auth(command) => auth(command),
        Command::Service(command) => service(command),
        Command::Snapshot(command) => snapshot::run(command, &store_path, &device_id),
        command => {
            // Checked first, so a bad combination is an error even when a
            // running scan turns this sync away.
            if let Command::Sync(sync) = &command {
                validate_sync_command(sync)?;
            }
            // For `scan` and `sync`, taken before the store opens, so this
            // command's own open does not overlap a running scan, and held
            // until the command returns. Nothing below takes it again.
            let _scan_lock = match acquire_command_scan_lock(&command, &store_path, SCAN_LOCK_WAIT)
            {
                CommandScanLock::NotNeeded => None,
                CommandScanLock::Held(guard) => Some(guard),
                CommandScanLock::Busy(lock_path) => {
                    return skip_for_running_scan(&command, &store_path, &lock_path);
                }
                CommandScanLock::Unavailable { path, error } => {
                    eprintln!(
                        "warning: could not use scan lock {}: {error:#}; continuing without it",
                        path.display()
                    );
                    None
                }
            };
            let store = if command_reprices_persisted_usage(&command) {
                statsai::open_operational_store(&store_path)?
            } else {
                Store::open(&store_path)?
            };
            match command {
                Command::Scan(command) => scan(command, &store, &device_id),
                Command::Report(command) => report(command, &store),
                Command::Sessions(command) => sessions(command, &store),
                Command::Source(command) => source(command, &store, &device_id),
                Command::Account(command) => account(command, &store),
                Command::Subscription(command) => subscription(command, &store),
                Command::Import(command) => import(command, &store, &device_id),
                Command::Export(command) => export(command, &store),
                Command::Task(command) => task(command, &store),
                Command::Conversation(command) => conversation(command, &store, &device_id),
                Command::Quota(command) => quota(command, &store, &device_id),
                Command::Activity(command) => activity(command, &store),
                Command::Privacy(command) => {
                    statsai::privacy_cli::run(command, &store, &store_path)
                }
                Command::Sync(command) => sync(command, &store, &device_id),
                Command::Daemon(command) => daemon(command, store, &device_id, &store_path),
                Command::Status => status(&store),
                Command::Schema(_)
                | Command::Store(_)
                | Command::Doctor
                | Command::Auth(_)
                | Command::Service(_)
                | Command::Snapshot(_) => {
                    unreachable!("handled before store open")
                }
            }
        }
    }
}

pub(crate) fn command_reprices_persisted_usage(command: &Command) -> bool {
    matches!(
        command,
        Command::Scan(_)
            | Command::Report(_)
            | Command::Sessions(_)
            | Command::Import(_)
            | Command::Export(_)
            | Command::Task(_)
            | Command::Sync(_)
            | Command::Daemon(_)
    )
}

/// Whether a command scans into the store and so must hold the scan lock.
///
/// `sync` refreshes git code-change scans and rebuilds daily summaries before
/// it sends, and `--reset-remote` clears the sync tracking a running sync
/// records, so it must not interleave with one. Only `--status` and
/// `--verify`, which read, go without it. The watch daemon is not listed: it
/// takes the lock for each pass instead of for its lifetime.
pub(crate) fn command_takes_scan_lock(command: &Command) -> bool {
    match command {
        Command::Scan(_) => true,
        Command::Sync(sync) => !(sync.status || sync.verify),
        _ => false,
    }
}

pub(crate) enum CommandScanLock {
    NotNeeded,
    Held(ScanLockGuard),
    /// Another process still held the lock at this path after the wait.
    Busy(PathBuf),
    /// The lock file could not be opened or locked. The lock is advisory, so
    /// the command runs without it rather than failing.
    Unavailable {
        path: PathBuf,
        error: anyhow::Error,
    },
}

/// Takes the scan lock beside `store_path` for a command that scans, waiting
/// up to `wait` for another scanner to finish.
pub(crate) fn acquire_command_scan_lock(
    command: &Command,
    store_path: &Path,
    wait: Duration,
) -> CommandScanLock {
    if !command_takes_scan_lock(command) {
        return CommandScanLock::NotNeeded;
    }
    let path = statsai::scan_lock_path(store_path);
    match statsai::acquire_scan_lock_with_timeout(&path, wait) {
        Ok(Some(guard)) => CommandScanLock::Held(guard),
        Ok(None) => CommandScanLock::Busy(path),
        Err(error) => CommandScanLock::Unavailable { path, error },
    }
}

/// What a command does instead of running while another scan holds the lock.
///
/// A `sync` still records preference flags such as `--exclude-projects`, as
/// the run it skips would have: they apply to every later sync, and an
/// opt-out must not be lost because a scan happened to be running. It collects
/// and sends nothing.
fn skip_for_running_scan(command: &Command, store_path: &Path, lock_path: &Path) -> Result<()> {
    let Command::Sync(sync) = command else {
        eprintln!(
            "Another statsai scan is running (lock: {}); skipping.",
            lock_path.display()
        );
        return Ok(());
    };
    // `--dry-run` and `--reset-remote` never record preferences.
    if !sync.dry_run && !sync.reset_remote && sync_command_sets_preferences(sync) {
        apply_sync_preference_overrides(&Store::open(store_path)?, sync)?;
    }
    eprintln!(
        "Another statsai scan is running (lock: {}); skipping this sync. Nothing was sent.",
        lock_path.display()
    );
    Ok(())
}
