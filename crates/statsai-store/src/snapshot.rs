//! SQLite-consistent APFS cloning for development stores.

use super::enable_sqlite_defensive;
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
    if !has_local_metadata(&connection, path)? {
        return Ok(None);
    }
    let value = read_metadata_value(
        &connection,
        path,
        super::APPLIED_PRICING_RULESET_VERSION_KEY,
    )?;
    parse_applied_pricing_ruleset_value(value.as_deref())
}

/// `local_metadata` key holding the text of the newest pricing file the store
/// has adopted.
pub const STORED_PRICING_FILE_KEY: &str = "pricing.file";

/// `local_metadata` key holding the pricing refresh setting, `"on"` or `"off"`.
/// Absent means on.
pub const PRICING_REFRESH_SETTING_KEY: &str = "pricing.refresh";

/// The `format` and `version` of the pricing file stored in a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredPricingFileHeader {
    /// The file's structure version.
    pub format: u64,
    /// The file's price version.
    pub version: u64,
}

/// Everything about a database's pricing that decides which pricing version a
/// given binary would price it with, read without opening it as a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabasePricingState {
    /// The pricing version the store's estimates were last repriced with.
    pub applied_version: Option<u64>,
    /// The header of the stored pricing file, when one is stored and readable.
    pub stored_file: Option<StoredPricingFileHeader>,
    /// A pricing file is stored, but its `format` and `version` could not be
    /// read. Binaries ignore such a copy.
    pub stored_file_unreadable: bool,
    /// The store's pricing refresh setting. Only the value `"off"` turns it off.
    pub refresh_setting_on: bool,
}

/// Reads the applied pricing version, the stored pricing file's header and the
/// refresh setting without creating, migrating, or repricing the database.
///
/// `Ok(None)` means the path does not exist. A database without a
/// `local_metadata` table has no applied version, no stored file and the
/// refresh on. Only the stored file's integer `format` and `version` are read;
/// a stored file without them is reported as unreadable rather than failing.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be opened as a readable
/// SQLite database, or when the applied version is not a valid unsigned integer.
pub fn database_pricing_state(path: impl AsRef<Path>) -> Result<Option<DatabasePricingState>> {
    let path = path.as_ref();
    if !path
        .try_exists()
        .with_context(|| format!("check whether database {} exists", path.display()))?
    {
        return Ok(None);
    }

    let connection = open_read_only(path)?;
    if !has_local_metadata(&connection, path)? {
        return Ok(Some(DatabasePricingState {
            applied_version: None,
            stored_file: None,
            stored_file_unreadable: false,
            refresh_setting_on: true,
        }));
    }
    let applied = read_metadata_value(
        &connection,
        path,
        super::APPLIED_PRICING_RULESET_VERSION_KEY,
    )?;
    let stored_file = read_metadata_value(&connection, path, STORED_PRICING_FILE_KEY)?
        .map(|text| parse_stored_pricing_file_header(&text));
    let refresh = read_metadata_value(&connection, path, PRICING_REFRESH_SETTING_KEY)?;
    Ok(Some(DatabasePricingState {
        applied_version: parse_applied_pricing_ruleset_value(applied.as_deref())?,
        stored_file: stored_file.flatten(),
        stored_file_unreadable: stored_file.is_some_and(|header| header.is_none()),
        refresh_setting_on: pricing_refresh_setting_is_on(refresh.as_deref()),
    }))
}

/// Whether a `pricing.refresh` setting value leaves the refresh on: only `off`
/// turns it off, and an absent setting means on.
pub(crate) fn pricing_refresh_setting_is_on(value: Option<&str>) -> bool {
    value != Some("off")
}

/// Reads the integer `format` and `version` of a stored pricing file, ignoring
/// everything else in it. A file that is not a JSON object with both has none.
fn parse_stored_pricing_file_header(text: &str) -> Option<StoredPricingFileHeader> {
    let file: serde_json::Map<String, serde_json::Value> = serde_json::from_str(text).ok()?;
    Some(StoredPricingFileHeader {
        format: file.get("format")?.as_u64()?,
        version: file.get("version")?.as_u64()?,
    })
}

fn has_local_metadata(connection: &Connection, path: &Path) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'local_metadata')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .with_context(|| format!("inspect local metadata at {}", path.display()))
}

fn read_metadata_value(connection: &Connection, path: &Path, key: &str) -> Result<Option<String>> {
    connection
        .query_row(
            "SELECT value FROM local_metadata WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .with_context(|| format!("read {key} at {}", path.display()))
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

/// Opens `path` so that nothing this connection does can change the file.
///
/// `SQLITE_OPEN_READONLY` is what the `mode=ro` URI parameter sets; passing the
/// flag avoids escaping the path into a URI. Without `SQLITE_OPEN_CREATE` a
/// missing file is an error rather than a new database.
pub(crate) fn open_read_only(path: &Path) -> Result<Connection> {
    open_read_only_unlabelled(path)
        .with_context(|| format!("open database read-only at {}", path.display()))
}

/// [`open_read_only`] without the context naming the path, for a caller whose
/// own error already names it. [`crate::ReadStore`] builds its connection on
/// this one.
pub(crate) fn open_read_only_unlabelled(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    enable_sqlite_defensive(&connection)?;
    Ok(connection)
}

#[cfg(target_os = "macos")]
fn open_read_write(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open database for snapshot at {}", path.display()))?;
    enable_sqlite_defensive(&connection)?;
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

    fn store_with_metadata(path: &Path, values: &[(&str, &str)]) {
        let store = crate::Store::open(path).expect("create store");
        for (key, value) in values {
            store
                .set_metadata_value(key, value)
                .expect("write metadata value");
        }
    }

    #[test]
    fn missing_database_has_no_pricing_state() {
        let directory = tempfile::tempdir().expect("temporary directory");
        assert_eq!(
            database_pricing_state(directory.path().join("missing.sqlite"))
                .expect("inspect missing database"),
            None
        );
    }

    #[test]
    fn database_without_metadata_has_default_pricing_state() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("legacy.sqlite");
        let connection = Connection::open(&path).expect("create legacy database");
        connection
            .execute("CREATE TABLE legacy (id INTEGER PRIMARY KEY)", [])
            .expect("create legacy table");
        drop(connection);

        assert_eq!(
            database_pricing_state(&path).expect("read legacy pricing"),
            Some(DatabasePricingState {
                applied_version: None,
                stored_file: None,
                stored_file_unreadable: false,
                refresh_setting_on: true,
            })
        );
    }

    #[test]
    fn pricing_state_reads_applied_version_stored_header_and_setting() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("statsai.sqlite");
        store_with_metadata(
            &path,
            &[
                (crate::APPLIED_PRICING_RULESET_VERSION_KEY, "8"),
                (
                    STORED_PRICING_FILE_KEY,
                    r#"{"format": 2, "version": 9, "catalog": "official:2027-01-01", "models": {}}"#,
                ),
                (PRICING_REFRESH_SETTING_KEY, "off"),
            ],
        );

        assert_eq!(
            database_pricing_state(&path).expect("read pricing state"),
            Some(DatabasePricingState {
                applied_version: Some(8),
                stored_file: Some(StoredPricingFileHeader {
                    format: 2,
                    version: 9,
                }),
                stored_file_unreadable: false,
                refresh_setting_on: false,
            })
        );
    }

    #[test]
    fn pricing_refresh_is_on_unless_the_setting_is_off() {
        let directory = tempfile::tempdir().expect("temporary directory");
        for (label, setting) in [
            ("absent", None),
            ("on", Some("on")),
            ("other", Some("OFF ")),
        ] {
            let path = directory.path().join(format!("{label}.sqlite"));
            let values: Vec<_> = setting
                .map(|value| (PRICING_REFRESH_SETTING_KEY, value))
                .into_iter()
                .collect();
            store_with_metadata(&path, &values);

            let state = database_pricing_state(&path)
                .expect("read pricing state")
                .expect("database exists");
            assert!(state.refresh_setting_on, "{label}");
            assert_eq!(state.applied_version, None, "{label}");
            assert_eq!(state.stored_file, None, "{label}");
            assert!(!state.stored_file_unreadable, "{label}");
        }
    }

    #[test]
    fn an_unparsable_stored_pricing_file_is_reported_not_fatal() {
        let directory = tempfile::tempdir().expect("temporary directory");
        for (label, text) in [
            ("not json", "not json"),
            ("empty", ""),
            ("missing version", r#"{"format": 1}"#),
            ("string format", r#"{"format": "1", "version": 7}"#),
            ("negative version", r#"{"format": 1, "version": -7}"#),
            ("not an object", "[1, 7]"),
        ] {
            let path = directory
                .path()
                .join(format!("{}.sqlite", label.replace(' ', "-")));
            store_with_metadata(
                &path,
                &[
                    (crate::APPLIED_PRICING_RULESET_VERSION_KEY, "7"),
                    (STORED_PRICING_FILE_KEY, text),
                ],
            );

            let state = database_pricing_state(&path)
                .unwrap_or_else(|error| panic!("{label}: {error:#}"))
                .expect("database exists");
            assert_eq!(state.applied_version, Some(7), "{label}");
            assert_eq!(state.stored_file, None, "{label}");
            assert!(state.stored_file_unreadable, "{label}");
        }
    }

    #[test]
    fn an_invalid_applied_pricing_version_is_an_error() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("statsai.sqlite");
        store_with_metadata(
            &path,
            &[(crate::APPLIED_PRICING_RULESET_VERSION_KEY, "seven")],
        );

        let error = database_pricing_state(&path).expect_err("invalid applied version");
        assert!(
            format!("{error:#}").contains("pricing.applied_ruleset_version"),
            "{error:#}"
        );
    }

    #[test]
    fn reading_pricing_state_does_not_create_or_migrate_the_database() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("empty.sqlite");
        std::fs::write(&path, b"").expect("create empty file");

        assert_eq!(
            database_pricing_state(&path)
                .expect("read empty database")
                .map(|state| state.applied_version),
            Some(None)
        );
        assert_eq!(
            database_schema_version(&path).expect("still unmigrated"),
            Some(0)
        );
    }
}
