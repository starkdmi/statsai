use crate::state::{save, Paths, State};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use statsai_store::{clone_database_to, database_schema_version, DatabaseClone};
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

pub(crate) fn refresh(paths: &Paths, state: &mut State) -> Result<DatabaseClone> {
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
    let cloned = clone_database_to(&paths.prod_store, &paths.dev_store).with_context(|| {
        format!(
            "refresh isolated development database from {}",
            paths.prod_store.display()
        )
    })?;
    state.data.refreshed_at = Some(Utc::now());
    save(paths, state)?;
    Ok(cloned)
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
