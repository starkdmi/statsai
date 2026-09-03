use crate::state::{save, Paths, State};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use statsai_store::{clone_database_to, database_schema_version, DatabaseClone, Store, SyncState};
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
    // syncs there, so its `sync_state` has no row for it -- or a stale one from an
    // earlier era. Letting the clone overwrite carry that in tells the next sync it
    // is behind a server that is actually ahead, and the CLI responds by clearing
    // tracking and re-uploading the entire account. That is a full-history sync on
    // every refresh, which is how a routine `data refresh` came to cost six figures
    // of D1 row reads.
    let preserved = carried_sync_states(&paths.dev_store)?;
    let cloned = clone_database_to(&paths.prod_store, &paths.dev_store).with_context(|| {
        format!(
            "refresh isolated development database from {}",
            paths.prod_store.display()
        )
    })?;
    let restored = restore_sync_states(&paths.dev_store, &preserved)?;
    state.data.refreshed_at = Some(Utc::now());
    save(paths, state)?;
    Ok(RefreshOutcome {
        clone: cloned,
        restored_sync_states: restored,
    })
}

/// The cursors the outgoing clone was carrying, or none when there is no clone yet.
fn carried_sync_states(dev_store: &Path) -> Result<Vec<SyncState>> {
    if !dev_store
        .try_exists()
        .with_context(|| format!("check development store path {}", dev_store.display()))?
    {
        return Ok(Vec::new());
    }
    let store = Store::open(dev_store).with_context(|| {
        format!(
            "read sync cursors from development database {}",
            dev_store.display()
        )
    })?;
    store
        .list_sync_states()
        .context("list development sync cursors")
}

fn restore_sync_states(dev_store: &Path, states: &[SyncState]) -> Result<usize> {
    if states.is_empty() {
        return Ok(0);
    }
    let store = Store::open(dev_store).with_context(|| {
        format!(
            "reopen refreshed development database {}",
            dev_store.display()
        )
    })?;
    store
        .restore_sync_states(states)
        .context("restore development sync cursors")
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

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }

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

    fn cursor(path: &Path, target: &str) -> Option<SyncState> {
        Store::open(path)
            .expect("open store")
            .sync_state("http", target)
            .expect("read cursor")
    }

    #[test]
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

    #[test]
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
