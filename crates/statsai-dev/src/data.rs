use crate::state::{save, Paths, State};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use statsai_store::{clone_database_to, database_schema_version, DatabaseClone, Store};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct DataStatus {
    pub(crate) source_exists: bool,
    pub(crate) dev_exists: bool,
    pub(crate) refreshed_at: Option<DateTime<Utc>>,
    pub(crate) prod_schema: Option<i64>,
    pub(crate) dev_schema: Option<i64>,
    pub(crate) logical_size: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct RefreshOutcome {
    pub(crate) clone: DatabaseClone,
    pub(crate) restored_sync_states: usize,
}

pub(crate) fn refresh(paths: &Paths, state: &mut State) -> Result<RefreshOutcome> {
    if !paths
        .prod_store
        .try_exists()
        .with_context(|| format!("check production store path {}", paths.prod_store.display()))?
    {
        bail!(
            "production StatsAI database does not exist at {}",
            paths.prod_store.display()
        );
    }
    paths.ensure_cache_dirs()?;
    // The clone is the only place a dev-backend sync cursor exists: production never
    // syncs there, so its tracking tables have no rows for it -- or stale ones from
    // an earlier era. Letting the clone overwrite carry that in tells the next sync
    // it is behind a server that is actually ahead, and the CLI responds by clearing
    // tracking and re-uploading the entire account. That is a full-history sync on
    // every refresh, which is how a routine `data refresh` came to cost six figures
    // of D1 row reads.
    //
    // The outgoing clone is moved aside rather than overwritten, so it stays intact
    // until the new one has adopted its cursors. Publishing first would mean a
    // failure between the copy and the adoption -- a production schema this launcher
    // cannot open, say -- destroys the only copy of them, and retrying cannot help.
    let previous = stage_previous_clone(&paths.dev_store)?;
    let refreshed = (|| {
        let cloned = clone_database_to(&paths.prod_store, &paths.dev_store).with_context(|| {
            format!(
                "refresh isolated development database from {}",
                paths.prod_store.display()
            )
        })?;
        let adopted = match previous.as_deref() {
            Some(previous) => adopt_sync_tracking(&paths.dev_store, previous)?,
            None => 0,
        };
        Ok(RefreshOutcome {
            clone: cloned,
            restored_sync_states: adopted,
        })
    })();

    match refreshed {
        Ok(outcome) => {
            if let Some(previous) = previous.as_deref() {
                discard_staged_clone(previous);
            }
            state.data.refreshed_at = Some(Utc::now());
            save(paths, state)?;
            Ok(outcome)
        }
        Err(error) => {
            if let Some(previous) = previous.as_deref() {
                restore_staged_clone(previous, &paths.dev_store)?;
            }
            Err(error)
        }
    }
}

/// Moves the existing clone out of the way, returning where it went. `None` means
/// there was nothing to move.
fn stage_previous_clone(dev_store: &Path) -> Result<Option<PathBuf>> {
    if !dev_store
        .try_exists()
        .with_context(|| format!("check development store path {}", dev_store.display()))?
    {
        return Ok(None);
    }
    let staged = path_with_suffix(dev_store, ".previous");
    discard_staged_clone(&staged);
    // A partial move is worse than not moving at all: this fails before `refresh`
    // has anything to roll back, so whatever already moved would be stranded under
    // the staged suffix and a retry would find no complete clone at either path.
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (from, to) in database_files(dev_store)
        .into_iter()
        .zip(database_files(&staged))
    {
        match fs::rename(&from, &to) {
            Ok(()) => moved.push((to, from)),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                for (staged_path, live_path) in moved.into_iter().rev() {
                    let _ = fs::rename(staged_path, live_path);
                }
                return Err(error)
                    .with_context(|| format!("set aside development database {}", from.display()));
            }
        }
    }
    Ok(Some(staged))
}

fn restore_staged_clone(staged: &Path, dev_store: &Path) -> Result<()> {
    for (from, to) in database_files(staged)
        .into_iter()
        .zip(database_files(dev_store))
    {
        match fs::rename(&from, &to) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("restore development database {}", to.display()));
            }
        }
    }
    Ok(())
}

fn discard_staged_clone(staged: &Path) {
    for path in database_files(staged) {
        let _ = fs::remove_file(path);
    }
}

fn adopt_sync_tracking(dev_store: &Path, previous: &Path) -> Result<usize> {
    let store = Store::open(dev_store).with_context(|| {
        format!(
            "reopen refreshed development database {}",
            dev_store.display()
        )
    })?;
    // A clone that cannot be read carries nothing worth keeping, and refusing to
    // refresh because of it would break the one command that repairs a broken clone.
    match store.adopt_sync_tracking_from(previous) {
        Ok(adopted) => Ok(adopted),
        Err(error) => {
            eprintln!(
                "warning: could not carry sync cursors from the previous development database ({error:#}); the next sync may re-upload history"
            );
            Ok(0)
        }
    }
}

pub(crate) fn clean(paths: &Paths, state: &mut State) -> Result<usize> {
    let mut removed = 0;
    for path in database_files(&paths.dev_store) {
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("remove development database {}", path.display()));
            }
        }
    }
    state.data.refreshed_at = None;
    save(paths, state)?;
    if paths.data_dir.try_exists()? {
        fs::File::open(&paths.data_dir)?.sync_all()?;
    }
    Ok(removed)
}

pub(crate) fn inspect(paths: &Paths, state: &State) -> Result<DataStatus> {
    let prod_schema = database_schema_version(&paths.prod_store)?;
    let dev_schema = database_schema_version(&paths.dev_store)?;
    let source_exists = prod_schema.is_some();
    let dev_exists = dev_schema.is_some();
    let metadata = match fs::metadata(&paths.dev_store) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "read development database metadata {}",
                    paths.dev_store.display()
                )
            });
        }
    };
    let refreshed_at = state.data.refreshed_at.or_else(|| {
        metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .map(DateTime::<Utc>::from)
    });
    Ok(DataStatus {
        source_exists,
        dev_exists,
        refreshed_at,
        prod_schema,
        dev_schema,
        logical_size: metadata.map(|metadata| metadata.len()),
    })
}

pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn database_files(database: &Path) -> [PathBuf; 4] {
    [
        database.to_path_buf(),
        path_with_suffix(database, "-wal"),
        path_with_suffix(database, "-shm"),
        path_with_suffix(database, "-journal"),
    ]
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use statsai_store::SyncState;

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[cfg(target_os = "macos")]
    fn record_cursor(path: &Path, target: &str, at: DateTime<Utc>, batch: &str) {
        let store = Store::open(path).expect("open store");
        store
            .restore_sync_states(&[SyncState {
                sink: "http".to_string(),
                target: target.to_string(),
                last_success_at: at,
                last_batch_id: batch.to_string(),
                last_event_started_at: None,
                last_event_id: None,
                last_summary_observed_at: None,
                last_summary_id: None,
                last_task_verification_updated_at: None,
                last_task_verification_id: None,
                failure_count: 0,
                pending_resume_batch_id: None,
            }])
            .expect("record cursor");
    }

    #[cfg(target_os = "macos")]
    fn cursor(path: &Path, target: &str) -> Option<SyncState> {
        Store::open(path)
            .expect("open store")
            .sync_state("http", target)
            .expect("read cursor")
    }

    // `clone_database_to` is APFS-only, so the refresh path cannot run elsewhere.
    #[test]
    #[cfg(target_os = "macos")]
    fn refresh_keeps_the_dev_cursor_production_never_had() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let dev_target = "https://dev-api.example.test/api/sync/batches";
        let now = Utc::now();

        // Production has never synced to the dev backend, so only the clone carries
        // that cursor. Losing it makes the next sync a full-history upload.
        drop(Store::open(&paths.prod_store).expect("create production store"));
        drop(Store::open(&paths.dev_store).expect("create development store"));
        record_cursor(&paths.dev_store, dev_target, now, "batch-dev-latest");

        let mut state = State::default();
        let outcome = refresh(&paths, &mut state).expect("refresh development data");

        assert_eq!(outcome.restored_sync_states, 1);
        let kept = cursor(&paths.dev_store, dev_target).expect("dev cursor survives refresh");
        assert_eq!(kept.last_batch_id, "batch-dev-latest");
    }

    // `clone_database_to` is APFS-only, so the refresh path cannot run elsewhere.
    #[test]
    #[cfg(target_os = "macos")]
    fn refresh_does_not_rewind_a_cursor_to_a_stale_production_one() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let dev_target = "https://dev-api.example.test/api/sync/batches";
        let prod_target = "https://api.example.test/api/sync/batches";
        let now = Utc::now();
        let stale = now - chrono::Duration::hours(12);

        // Production holds an old dev cursor from an earlier era, and a *fresher*
        // production cursor. The first must not overwrite the clone's newer one; the
        // second must win, because rewinding it would be equally wrong.
        drop(Store::open(&paths.prod_store).expect("create production store"));
        record_cursor(&paths.prod_store, dev_target, stale, "batch-dev-stale");
        record_cursor(&paths.prod_store, prod_target, now, "batch-prod-fresh");
        drop(Store::open(&paths.dev_store).expect("create development store"));
        record_cursor(&paths.dev_store, dev_target, now, "batch-dev-latest");
        record_cursor(&paths.dev_store, prod_target, stale, "batch-prod-stale");

        let mut state = State::default();
        refresh(&paths, &mut state).expect("refresh development data");

        assert_eq!(
            cursor(&paths.dev_store, dev_target)
                .expect("dev cursor")
                .last_batch_id,
            "batch-dev-latest"
        );
        assert_eq!(
            cursor(&paths.dev_store, prod_target)
                .expect("prod cursor")
                .last_batch_id,
            "batch-prod-fresh"
        );
    }

    #[test]
    fn a_failed_staging_leaves_the_clone_whole() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        fs::write(&paths.dev_store, b"database").expect("write database");
        let wal = path_with_suffix(&paths.dev_store, "-wal");
        fs::write(&wal, b"wal").expect("write wal");
        // A directory cannot be replaced by a rename, so the second move fails after
        // the first has already succeeded.
        fs::create_dir(path_with_suffix(
            &path_with_suffix(&paths.dev_store, ".previous"),
            "-wal",
        ))
        .expect("block the staged wal path");

        let error = stage_previous_clone(&paths.dev_store)
            .expect_err("staging must fail when a file cannot be moved");
        assert!(error.to_string().contains("set aside"));

        assert_eq!(
            fs::read(&paths.dev_store).expect("database stays at the live path"),
            b"database"
        );
        assert_eq!(fs::read(&wal).expect("wal stays at the live path"), b"wal");
    }

    #[test]
    fn clean_does_not_touch_production_database() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        fs::write(&paths.prod_store, b"production").expect("write production marker");
        fs::write(&paths.dev_store, b"development").expect("write development marker");
        let mut state = State::default();

        assert_eq!(
            clean(&paths, &mut state).expect("clean development data"),
            1
        );
        assert_eq!(
            fs::read(&paths.prod_store).expect("read production marker"),
            b"production"
        );
        assert!(!paths.dev_store.exists());
    }
}
