mod fsevent;
mod pending;
mod scan;
mod state;

pub(crate) use super::lock_store;
#[cfg(test)]
use pending::WatchNotice;
use pending::{note_notify_event, PendingWatch};
use scan::*;
use state::*;

use crate::http::Server;
use anyhow::{Context, Result};
use notify::{Event, Watcher};
use statsai_adapters::{default_adapters, ProviderAdapter};
use statsai_store::Store;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

const WATCH_SOURCE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const WATCH_SCAN_INITIAL_RETRY_DELAY: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_millis(250)
};
const WATCH_SCAN_MAX_RETRY_DELAY: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_secs(5)
};

pub fn watch_and_serve(
    addr: &str,
    store: Arc<Mutex<Store>>,
    device_id: &str,
    auth_token: &str,
) -> Result<()> {
    let bind_addr = super::resolve_loopback_addr(addr)?;
    let startup_executable = current_executable_stamp();

    let watch_adapters = default_adapters();
    let mut verification_dependency_cache = VerificationDependencyCache::default();
    let initial_configured_result = {
        let s = super::lock_store(&store);
        s.list_sources()
    };
    let initial_plan = match initial_configured_result {
        Ok(configured) => discover_watch_plan(
            &configured,
            &watch_adapters,
            &mut verification_dependency_cache,
        ),
        Err(error) => {
            eprintln!("daemon: initial watch source discovery failed: {error:#}");
            discover_watch_plan(&[], &watch_adapters, &mut verification_dependency_cache)
        }
    };
    let WatchPlan {
        paths: initial_sources,
        verification_dependencies: initial_verification_dependencies,
    } = initial_plan;
    let verification_dependencies = Arc::new(RwLock::new(initial_verification_dependencies));
    let (watcher_signal_tx, watcher_signal_rx) = mpsc::sync_channel(1);
    let pending_changed_paths = Arc::new(Mutex::new(PendingWatch::default()));
    let callback_pending_paths = Arc::clone(&pending_changed_paths);
    let callback_signal = watcher_signal_tx;

    #[cfg(target_os = "macos")]
    {
        let mut watcher = fsevent::FsEventWatcher::new(
            move |event| note_signaled_event(&callback_pending_paths, &callback_signal, event),
            notify::Config::default(),
        )
        .context("create FSEvents watcher")?;
        run_watch_loop(
            &mut watcher,
            |watcher| Ok(watcher.poll_restart()?),
            addr,
            store,
            device_id,
            auth_token,
            watch_adapters,
            verification_dependency_cache,
            verification_dependencies,
            initial_sources,
            watcher_signal_rx,
            pending_changed_paths,
            startup_executable,
            bind_addr,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut watcher = notify::recommended_watcher(move |event| {
            note_signaled_event(&callback_pending_paths, &callback_signal, event);
        })
        .context("create file watcher")?;
        run_watch_loop(
            &mut watcher,
            |_| Ok(()),
            addr,
            store,
            device_id,
            auth_token,
            watch_adapters,
            verification_dependency_cache,
            verification_dependencies,
            initial_sources,
            watcher_signal_rx,
            pending_changed_paths,
            startup_executable,
            bind_addr,
        )
    }
}

fn note_signaled_event(
    pending: &Arc<Mutex<PendingWatch>>,
    signal: &mpsc::SyncSender<()>,
    event: Result<Event, notify::Error>,
) {
    note_notify_event(
        &mut pending.lock().unwrap_or_else(|error| error.into_inner()),
        event,
    );
    let _ = signal.try_send(());
}

#[allow(clippy::too_many_arguments)]
fn run_watch_loop<W: Watcher>(
    watcher: &mut W,
    mut poll_watcher: impl FnMut(&mut W) -> Result<()>,
    _addr: &str,
    store: Arc<Mutex<Store>>,
    device_id: &str,
    auth_token: &str,
    watch_adapters: Vec<Box<dyn ProviderAdapter>>,
    mut verification_dependency_cache: VerificationDependencyCache,
    verification_dependencies: Arc<RwLock<VerificationDependencySnapshot>>,
    initial_sources: HashMap<PathBuf, WatchScope>,
    watcher_signal_rx: mpsc::Receiver<()>,
    pending_changed_paths: Arc<Mutex<PendingWatch>>,
    startup_executable: Option<ExecutableStamp>,
    bind_addr: std::net::SocketAddr,
) -> Result<()> {
    let background_store = {
        let store = super::lock_store(&store);
        store.reopen()
    };
    let (scan_signal_tx, scan_signal_rx) = mpsc::sync_channel(1);
    let worker_scan_signal_tx = scan_signal_tx.clone();
    let pending_scan_paths = Arc::new(Mutex::new(PendingWatch::default()));
    let worker_pending_scan_paths = Arc::clone(&pending_scan_paths);
    let worker_shared_store = Arc::clone(&store);
    let worker_verification_dependencies = Arc::clone(&verification_dependencies);
    let worker_device_id = device_id.to_string();
    let _scan_worker = std::thread::Builder::new()
        .name("statsai-watch-scan".to_string())
        .spawn(move || {
            let mut retry_delay = WATCH_SCAN_INITIAL_RETRY_DELAY;
            while scan_signal_rx.recv().is_ok() {
                let notice = worker_pending_scan_paths
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if notice.paths().is_empty() && !notice.rescan_all() {
                    continue;
                }
                let dependency_snapshot = worker_verification_dependencies
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let scan_succeeded = process_background_scan(
                    &worker_pending_scan_paths,
                    &worker_scan_signal_tx,
                    notice,
                    retry_delay,
                    |notice| match background_store.as_ref() {
                        Ok(store) => rescan_changed_sources(
                            store,
                            &worker_shared_store,
                            &worker_device_id,
                            notice.paths(),
                            notice.rescan_all(),
                            &dependency_snapshot,
                        ),
                        Err(error) => {
                            eprintln!(
                                "daemon: dedicated scan connection unavailable ({error:#}); using shared store"
                            );
                            let store = super::lock_store(&worker_shared_store);
                            let adapters: Vec<Box<dyn ProviderAdapter>> = default_adapters();
                            rescan_changed_sources_with_adapters_and_dependencies(
                                &store,
                                &worker_device_id,
                                notice.paths(),
                                notice.rescan_all(),
                                &adapters,
                                &dependency_snapshot,
                            )
                        }
                    },
                );
                retry_delay = if scan_succeeded {
                    WATCH_SCAN_INITIAL_RETRY_DELAY
                } else {
                    retry_delay
                        .saturating_mul(2)
                        .min(WATCH_SCAN_MAX_RETRY_DELAY)
                };
            }
        })
        .context("start background scan worker")?;

    let mut watched_sources = HashMap::new();
    let mut uncertain_watch_sources = HashSet::new();
    let initially_watched = reconcile_watch_sources(
        watcher,
        &mut watched_sources,
        &mut uncertain_watch_sources,
        initial_sources,
    );
    enqueue_background_scan(&pending_scan_paths, &scan_signal_tx, initially_watched);
    let mut last_watch_source_refresh = Instant::now();

    eprintln!("daemon: API listening on http://{bind_addr}");
    let server = Server::http(bind_addr)
        .map_err(|err| anyhow::anyhow!("start local API on {bind_addr}: {err}"))?;

    loop {
        if startup_executable
            .as_ref()
            .is_some_and(executable_was_replaced)
        {
            eprintln!("daemon: executable changed on disk; restarting");
            return Ok(());
        }
        poll_watcher(watcher)?;
        match watcher_signal_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(()) => {
                let notice = pending_changed_paths
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if notice.rescan_all() {
                    verification_dependency_cache.invalidate_all();
                } else {
                    verification_dependency_cache
                        .invalidate_changed(&watch_adapters, notice.paths());
                }
                enqueue_watch_notice(&pending_scan_paths, &scan_signal_tx, notice);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if last_watch_source_refresh.elapsed() >= WATCH_SOURCE_REFRESH_INTERVAL {
            let configured_result = {
                let s = super::lock_store(&store);
                s.list_sources()
            };
            last_watch_source_refresh = Instant::now();
            match configured_result {
                Ok(configured) => {
                    let desired_plan = discover_watch_plan(
                        &configured,
                        &watch_adapters,
                        &mut verification_dependency_cache,
                    );
                    *verification_dependencies
                        .write()
                        .unwrap_or_else(|error| error.into_inner()) =
                        desired_plan.verification_dependencies;
                    let newly_watched = reconcile_watch_sources(
                        watcher,
                        &mut watched_sources,
                        &mut uncertain_watch_sources,
                        desired_plan.paths,
                    );
                    if !newly_watched.is_empty() {
                        enqueue_background_scan(
                            &pending_scan_paths,
                            &scan_signal_tx,
                            newly_watched,
                        );
                    }
                }
                Err(error) => {
                    eprintln!("daemon: watch source discovery failed: {error:#}");
                }
            }
        }

        if let Ok(Some(request)) = server.try_recv() {
            if let Err(error) = super::handle_request(request, &store, auth_token) {
                eprintln!("daemon: request failed: {error:#}");
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecutableStamp {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

fn executable_stamp(path: &Path) -> Option<ExecutableStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(ExecutableStamp {
        path: path.to_path_buf(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn current_executable_stamp() -> Option<ExecutableStamp> {
    let path = std::env::current_exe().ok()?;
    executable_stamp(&path)
}

fn executable_was_replaced(startup: &ExecutableStamp) -> bool {
    executable_stamp(&startup.path).as_ref() != Some(startup)
}

#[cfg(test)]
mod tests;
