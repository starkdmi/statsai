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
            // Taken before the store opens, so the migrations and repricing an
            // open may run wait for a running scan as well, and held until the
            // command returns. Nothing below takes it again.
            let scan_lock = acquire_command_scan_lock(&command, &store_path, SCAN_LOCK_WAIT)?;
            let _scan_lock = match scan_lock {
                CommandScanLock::NotNeeded => None,
                CommandScanLock::Held(guard) => Some(guard),
                CommandScanLock::Busy(lock_path) => {
                    eprintln!(
                        "Another statsai scan is running (lock: {}); skipping.",
                        lock_path.display()
                    );
                    return Ok(());
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
/// it sends; only `--status`, `--verify`, and `--reset-remote` skip that. The
/// watch daemon is not listed: it takes the lock for each pass instead of for
/// its lifetime.
pub(crate) fn command_takes_scan_lock(command: &Command) -> bool {
    match command {
        Command::Scan(_) => true,
        Command::Sync(sync) => !(sync.status || sync.verify || sync.reset_remote),
        _ => false,
    }
}

pub(crate) enum CommandScanLock {
    NotNeeded,
    Held(ScanLockGuard),
    /// Another process still held the lock at this path after the wait.
    Busy(PathBuf),
}

/// Takes the scan lock beside `store_path` for a command that scans, waiting
/// up to `wait` for another scanner to finish.
pub(crate) fn acquire_command_scan_lock(
    command: &Command,
    store_path: &Path,
    wait: Duration,
) -> Result<CommandScanLock> {
    if !command_takes_scan_lock(command) {
        return Ok(CommandScanLock::NotNeeded);
    }
    let lock_path = statsai::scan_lock_path(store_path);
    let lock = statsai::acquire_scan_lock_with_timeout(&lock_path, wait)?;
    Ok(match lock {
        Some(guard) => CommandScanLock::Held(guard),
        None => CommandScanLock::Busy(lock_path),
    })
}
