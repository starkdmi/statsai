mod scan;
mod state;

pub(crate) use super::lock_store;
use scan::*;
use state::*;

use anyhow::{Context, Result};
use notify::{Event, EventKind};
use statsai_adapters::{default_adapters, ProviderAdapter};
use statsai_store::Store;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tiny_http::Server;

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
    let pending_changed_paths = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
    let callback_pending_paths = Arc::clone(&pending_changed_paths);

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            if matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                callback_pending_paths
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .extend(event.paths);
                let _ = watcher_signal_tx.try_send(());
            }
        }
    })
    .context("create file watcher")?;

    let background_store = {
        let store = super::lock_store(&store);
        store.reopen()
    };
    let (scan_signal_tx, scan_signal_rx) = mpsc::sync_channel(1);
    let worker_scan_signal_tx = scan_signal_tx.clone();
    let pending_scan_paths = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
    let worker_pending_scan_paths = Arc::clone(&pending_scan_paths);
    let worker_shared_store = Arc::clone(&store);
    let worker_verification_dependencies = Arc::clone(&verification_dependencies);
    let worker_device_id = device_id.to_string();
    let _scan_worker = std::thread::Builder::new()
        .name("statsai-watch-scan".to_string())
        .spawn(move || {
            let mut retry_delay = WATCH_SCAN_INITIAL_RETRY_DELAY;
            while scan_signal_rx.recv().is_ok() {
                let changed = worker_pending_scan_paths
                    .lock()
                    .map(|mut paths| {
                        std::mem::take(&mut *paths).into_iter().collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|error| {
                        std::mem::take(&mut *error.into_inner())
                            .into_iter()
                            .collect::<Vec<_>>()
                    });
                if changed.is_empty() {
                    continue;
                }
                let dependency_snapshot = worker_verification_dependencies
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let scan_succeeded = process_background_scan(
                    &worker_pending_scan_paths,
                    &worker_scan_signal_tx,
                    changed,
                    retry_delay,
                    |changed| match background_store.as_ref() {
                        Ok(store) => rescan_changed_sources(
                            store,
                            &worker_shared_store,
                            &worker_device_id,
                            changed,
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
                                changed,
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
        &mut watcher,
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
        match watcher_signal_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(()) => {
                let changed = pending_changed_paths
                    .lock()
                    .map(|mut paths| std::mem::take(&mut *paths).into_iter().collect::<Vec<_>>())
                    .unwrap_or_else(|error| {
                        std::mem::take(&mut *error.into_inner())
                            .into_iter()
                            .collect::<Vec<_>>()
                    });
                verification_dependency_cache.invalidate_changed(&watch_adapters, &changed);
                enqueue_background_scan(&pending_scan_paths, &scan_signal_tx, changed);
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
                        &mut watcher,
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
