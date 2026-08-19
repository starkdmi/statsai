//! SQLite-consistent APFS cloning for development stores.

use anyhow::{bail, Context, Result};
#[cfg(target_os = "macos")]
use rusqlite::TransactionBehavior;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
#[cfg(target_os = "macos")]
use std::fs;
#[cfg(target_os = "macos")]
use std::fs::File;
#[cfg(target_os = "macos")]
use std::io::ErrorKind;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
const SNAPSHOT_BUSY_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
const CHECKPOINT_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Metadata about a completed database clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseClone {
    /// Logical byte length of the cloned database file.
    pub logical_size: u64,
    /// StatsAI schema version recorded in the clone.
    pub schema_version: i64,
}

/// Reads the StatsAI schema version without creating or migrating the database.
///
/// `Ok(None)` means that `path` does not exist. A database without a
/// `schema_migrations` table has version zero.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be opened as a readable
/// SQLite database.
pub fn database_schema_version(path: impl AsRef<Path>) -> Result<Option<i64>> {
    let path = path.as_ref();
    if !path
        .try_exists()
        .with_context(|| format!("check whether database {} exists", path.display()))?
    {
        return Ok(None);
    }

    let connection = open_read_only(path)?;
    let has_migrations_table = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .with_context(|| format!("inspect database schema at {}", path.display()))?;
    if !has_migrations_table {
        return Ok(Some(0));
    }

    connection
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .map(|version| Some(version.unwrap_or(0)))
        .with_context(|| format!("read database schema version at {}", path.display()))
}

/// Reads the last applied pricing ruleset without creating, migrating, or
/// repricing the database.
///
/// `Ok(None)` means the path does not exist, the metadata table is missing, or
/// no applied ruleset has been recorded yet.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be opened as a readable
/// SQLite database, or when the stored value is not a valid unsigned integer.
pub fn database_applied_pricing_ruleset_version(path: impl AsRef<Path>) -> Result<Option<u64>> {
    let path = path.as_ref();
    if !path
        .try_exists()
        .with_context(|| format!("check whether database {} exists", path.display()))?
    {
        return Ok(None);
    }

    let connection = open_read_only(path)?;
    let has_metadata_table = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'local_metadata')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .with_context(|| format!("inspect local metadata at {}", path.display()))?;
    if !has_metadata_table {
        return Ok(None);
    }

    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM local_metadata WHERE key = ?1",
            [super::APPLIED_PRICING_RULESET_VERSION_KEY],
            |row| row.get(0),
        )
        .optional()
        .with_context(|| format!("read applied pricing ruleset version at {}", path.display()))?;
    parse_applied_pricing_ruleset_value(value.as_deref())
}

pub(crate) fn parse_applied_pricing_ruleset_value(value: Option<&str>) -> Result<Option<u64>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    value
        .parse::<u64>()
        .map(Some)
        .with_context(|| format!("invalid pricing.applied_ruleset_version value {value:?}"))
}

/// Replaces `destination` with a SQLite-consistent APFS copy-on-write clone.
///
/// The source is opened without running StatsAI migrations. In WAL mode, this
/// function acquires SQLite's single-writer lock and completes a passive
/// checkpoint from a second connection before cloning the database file. That
/// leaves the main database current and immutable for the short duration of the
/// APFS clone operation. Other writers resume as soon as the clone is created.
///
/// The clone is staged beside the destination, checked through SQLite, and
/// published with an atomic rename. There is deliberately no byte-copy fallback:
/// callers either receive an APFS clone or an actionable error.
///
/// # Errors
///
/// Returns an error if the source is missing, busy for longer than 30 seconds,
/// not a valid SQLite database, on a different volume from the destination, or
/// if the filesystem does not support APFS cloning.
pub fn clone_database_to(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<DatabaseClone> {
    clone_database_to_impl(source.as_ref(), destination.as_ref())
}

#[cfg(not(target_os = "macos"))]
fn clone_database_to_impl(source: &Path, destination: &Path) -> Result<DatabaseClone> {
    let _ = (source, destination);
    bail!("APFS database cloning is supported only on macOS")
}

#[cfg(target_os = "macos")]
fn clone_database_to_impl(source: &Path, destination: &Path) -> Result<DatabaseClone> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    validate_clone_paths(source, destination)?;
    let parent = destination_parent(destination);
    create_private_parent(parent)?;

    // Open this descriptor before SQLite takes any locks. fclonefileat can then
    // clone from it without opening and closing the database behind SQLite's
    // back, which is important for POSIX advisory-lock correctness.
    let source_file =
        File::open(source).with_context(|| format!("open source database {}", source.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".statsai-clone-")
        .tempdir_in(parent)
        .with_context(|| format!("create clone staging directory in {}", parent.display()))?;
    restrict_dir_permissions(staging.path())?;
    let staged_name = CString::new("database.sqlite").expect("static filename has no NUL");
    let staged_path = staging.path().join("database.sqlite");
    let staging_dir = File::open(staging.path())
        .with_context(|| format!("open clone staging directory {}", staging.path().display()))?;

    let mut lock_connection = open_read_write(source)?;
    let checkpoint_connection = open_read_write(source)?;
    let transaction = lock_connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .with_context(|| {
            format!(
                "acquire SQLite writer lock for consistent clone of {}",
                source.display()
            )
        })?;
    let journal_mode: String = transaction
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .with_context(|| format!("read journal mode for {}", source.display()))?;

    if journal_mode.eq_ignore_ascii_case("wal") {
        checkpoint_wal_while_writer_is_held(&checkpoint_connection, source)?;
    }

    // SAFETY: both descriptors are live for the call, staged_name is a valid
    // NUL-terminated relative filename, and the destination does not exist.
    let clone_result = unsafe {
        libc::fclonefileat(
            source_file.as_raw_fd(),
            staging_dir.as_raw_fd(),
            staged_name.as_ptr(),
            0,
        )
    };
    if clone_result != 0 {
        let error = std::io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| code == libc::EXDEV || code == libc::ENOTSUP)
        {
            bail!(
                "cannot create APFS clone from {} to {}: source and destination must be on the same APFS volume ({error})",
                source.display(),
                destination.display()
            );
        }
        return Err(error).with_context(|| {
            format!(
                "create APFS clone from {} to {}",
                source.display(),
                destination.display()
            )
        });
    }

    // No database pages were changed by this transaction. Releasing it here
    // minimizes the time normal production writers are paused.
    transaction
        .rollback()
        .context("release SQLite writer lock after database clone")?;
    drop(checkpoint_connection);
    drop(lock_connection);
    drop(source_file);

    restrict_file_permissions(&staged_path)?;
    File::open(&staged_path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("sync cloned database {}", staged_path.display()))?;
    let schema_version = database_schema_version(&staged_path)?
        .ok_or_else(|| anyhow::anyhow!("staged database clone disappeared"))?;
    remove_sqlite_sidecars(&staged_path)?;
    let logical_size = fs::metadata(&staged_path)
        .with_context(|| format!("read cloned database metadata {}", staged_path.display()))?
        .len();

    publish_clone(&staged_path, destination, parent)?;
    restrict_file_permissions(destination)?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync database clone directory {}", parent.display()))?;

    Ok(DatabaseClone {
        logical_size,
        schema_version,
    })
}

#[cfg(target_os = "macos")]
fn publish_clone(staged: &Path, destination: &Path, parent: &Path) -> Result<()> {
    let sidecars = DestinationSidecarBackup::capture(destination, parent)?;
    if let Err(error) = fs::rename(staged, destination) {
        let publish_error = anyhow::Error::new(error).context(format!(
            "atomically replace database clone {} with {}",
            staged.display(),
            destination.display()
        ));
        if let Err(restore_error) = sidecars.restore() {
            bail!(
                "{publish_error:#}; additionally failed to restore destination SQLite sidecars: {restore_error:#}"
            );
        }
        return Err(publish_error);
    }

    sidecars.discard().with_context(|| {
        format!(
            "database clone was published to {}, but displaced SQLite sidecars could not be removed",
            destination.display()
        )
    })
}

#[cfg(target_os = "macos")]
struct DestinationSidecarBackup {
    directory: tempfile::TempDir,
    moved: Vec<(PathBuf, PathBuf)>,
}

#[cfg(target_os = "macos")]
impl DestinationSidecarBackup {
    fn capture(destination: &Path, parent: &Path) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix(".statsai-sidecars-")
            .tempdir_in(parent)
            .with_context(|| {
                format!(
                    "create destination sidecar backup directory in {}",
                    parent.display()
                )
            })?;
        restrict_dir_permissions(directory.path())?;
        let mut backup = Self {
            directory,
            moved: Vec::new(),
        };

        for (index, original) in sqlite_sidecars(destination).into_iter().enumerate() {
            let staged = backup.directory.path().join(format!("sidecar-{index}"));
            match fs::rename(&original, &staged) {
                Ok(()) => backup.moved.push((original, staged)),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    let capture_error = anyhow::Error::new(error).context(format!(
                        "preserve SQLite sidecar {} before database replacement",
                        original.display()
                    ));
                    if let Err(restore_error) = backup.restore() {
                        bail!(
                            "{capture_error:#}; additionally failed to restore already preserved SQLite sidecars: {restore_error:#}"
                        );
                    }
                    return Err(capture_error);
                }
            }
        }

        Ok(backup)
    }

    fn restore(self) -> Result<()> {
        let Self { directory, moved } = self;
        let mut errors = Vec::new();
        for (original, staged) in moved.iter().rev() {
            if let Err(error) = fs::rename(staged, original) {
                errors.push(format!(
                    "restore {} from {}: {error}",
                    original.display(),
                    staged.display()
                ));
            }
        }

        if errors.is_empty() {
            directory
                .close()
                .context("remove empty sidecar backup directory")?;
            return Ok(());
        }

        let recovery_path = directory.keep();
        bail!(
            "some SQLite sidecars could not be restored; recovery files remain at {}: {}",
            recovery_path.display(),
            errors.join("; ")
        )
    }

    fn discard(self) -> Result<()> {
        self.directory
            .close()
            .context("remove backed-up SQLite sidecars")
    }
}

#[cfg(target_os = "macos")]
fn checkpoint_wal_while_writer_is_held(connection: &Connection, source: &Path) -> Result<()> {
    let deadline = Instant::now() + SNAPSHOT_BUSY_TIMEOUT;
    loop {
        let (busy, log_frames, checkpointed_frames) = connection
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .with_context(|| format!("checkpoint WAL for {}", source.display()))?;
        if busy == 0 && log_frames == checkpointed_frames {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for readers to release {} so its WAL could be checkpointed ({checkpointed_frames}/{log_frames} frames checkpointed)",
                source.display()
            );
        }
        std::thread::sleep(CHECKPOINT_RETRY_DELAY);
    }
}

fn open_read_only(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open database read-only at {}", path.display()))
}

#[cfg(target_os = "macos")]
fn open_read_write(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open database for snapshot at {}", path.display()))?;
    connection
        .busy_timeout(SNAPSHOT_BUSY_TIMEOUT)
        .with_context(|| format!("configure snapshot timeout for {}", path.display()))?;
    Ok(connection)
}

#[cfg(target_os = "macos")]
fn validate_clone_paths(source: &Path, destination: &Path) -> Result<()> {
    let source_metadata = fs::metadata(source)
        .with_context(|| format!("read source database metadata {}", source.display()))?;
    if !source_metadata.is_file() {
        bail!("source database is not a file: {}", source.display());
    }
    if source == destination {
        bail!("source and destination database paths must differ");
    }
    if let Ok(destination_metadata) = fs::metadata(destination) {
        use std::os::unix::fs::MetadataExt;
        if source_metadata.dev() == destination_metadata.dev()
            && source_metadata.ino() == destination_metadata.ino()
        {
            bail!("source and destination database paths refer to the same file");
        }
        if !destination_metadata.is_file() {
            bail!(
                "destination database is not a file: {}",
                destination.display()
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn destination_parent(destination: &Path) -> &Path {
    destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(target_os = "macos")]
fn create_private_parent(parent: &Path) -> Result<()> {
    if parent.try_exists()? {
        return Ok(());
    }
    fs::create_dir_all(parent)
        .with_context(|| format!("create database clone directory {}", parent.display()))?;
    restrict_dir_permissions(parent)
}

#[cfg(target_os = "macos")]
fn remove_sqlite_sidecars(database: &Path) -> Result<()> {
    for sidecar in sqlite_sidecars(database) {
        match fs::remove_file(&sidecar) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("remove SQLite sidecar {}", sidecar.display()));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn sqlite_sidecars(database: &Path) -> [PathBuf; 3] {
    [
        path_with_suffix(database, "-wal"),
        path_with_suffix(database, "-shm"),
        path_with_suffix(database, "-journal"),
    ]
}

#[cfg(target_os = "macos")]
fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(target_os = "macos")]
fn restrict_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restrict directory permissions for {}", path.display()))
}

#[cfg(target_os = "macos")]
fn restrict_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict database permissions for {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_database_has_no_schema_version() {
        let directory = tempfile::tempdir().expect("temporary directory");
        assert_eq!(
            database_schema_version(directory.path().join("missing.sqlite"))
                .expect("inspect missing database"),
            None
        );
    }

    #[test]
    fn legacy_database_without_migrations_has_schema_zero() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("legacy.sqlite");
        let connection = Connection::open(&path).expect("create legacy database");
        connection
            .execute("CREATE TABLE legacy (id INTEGER PRIMARY KEY)", [])
            .expect("create legacy table");
        drop(connection);

        assert_eq!(
            database_schema_version(&path).expect("read legacy schema"),
            Some(0)
        );
    }
}
