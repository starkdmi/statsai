//! The advisory lock that keeps two scanners off one store at a time.
//!
//! `statsai scan`, `statsai sync` (except `--status`, `--verify`, and
//! `--reset-remote --dry-run`), and each pass of `statsai daemon --watch` take
//! this lock first. Other commands that
//! write collected data, such as `statsai import`, `statsai conversation
//! collect`, and `statsai source remove --delete-data`, do not take it yet, so
//! they can still run alongside a scan; bringing them under it is a follow-up.
//! It lives in a file beside the database rather than in SQLite, so a process
//! can learn that a scan is running without opening the store.
//!
//! The lock is the operating system's whole-file lock: `flock` on Unix and
//! `LockFileEx` on Windows, through [`File::try_lock`]. Both belong to the open
//! file rather than to the process, so two opens conflict even inside one
//! process, and the lock is released when the guard's file is closed, including
//! when the holder crashes. It is advisory: it keeps scanners apart and stops
//! nothing else from reading or writing the store.

use super::restrict_dir_permissions;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Name of the lock file, kept in the same directory as the database.
pub const SCAN_LOCK_FILE_NAME: &str = "scan.lock";

/// How often a waiting caller tries the lock again.
const SCAN_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The scan lock for the store at `store_path`: `scan.lock` in the same
/// directory, so `~/.statsai/statsai.sqlite` is guarded by
/// `~/.statsai/scan.lock`.
///
/// Symlinks are resolved, so a link to the store, or a path through a linked
/// directory, finds the same lock as the store's own path. When the store
/// does not exist yet, its directory is resolved instead, and when that does
/// not exist either, the path is used as written. Hard links to the store are
/// not detected: each directory that holds one has its own lock.
#[must_use]
pub fn scan_lock_path(store_path: &Path) -> PathBuf {
    let parent = store_path.parent().unwrap_or_else(|| Path::new(""));
    let directory = match std::fs::canonicalize(store_path) {
        Ok(store) => store
            .parent()
            .map_or_else(|| parent.to_path_buf(), Path::to_path_buf),
        Err(_) => std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf()),
    };
    directory.join(SCAN_LOCK_FILE_NAME)
}

/// Proof that this process holds the scan lock. Dropping it releases the lock.
///
/// The lock file is left in place on release. Deleting it would let a waiter
/// that already opened the old file lock it while a newcomer creates and locks
/// a new one, and both would believe they hold the lock.
#[derive(Debug)]
#[must_use = "the scan lock is released as soon as the guard is dropped"]
pub struct ScanLockGuard {
    file: File,
    path: PathBuf,
}

impl ScanLockGuard {
    /// The lock file this guard holds.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScanLockGuard {
    fn drop(&mut self) {
        // Closing the file releases the lock as well; unlocking first only
        // makes the release independent of when the descriptor is closed.
        let _ = self.file.unlock();
    }
}

/// Takes the scan lock at `lock_path` if no one holds it.
///
/// Returns `Ok(None)` when another holder has it. Creates the lock file, and
/// its directory with owner-only permissions, when they do not exist yet.
///
/// A process holds the lock through one guard: taking it again while a guard
/// is alive reports it as held, so take it once at the outermost level.
///
/// # Errors
///
/// Returns an error if the lock file cannot be created or opened, or the
/// operating system cannot lock it.
pub fn try_acquire_scan_lock(lock_path: &Path) -> Result<Option<ScanLockGuard>> {
    acquire_scan_lock_with_timeout(lock_path, Duration::ZERO)
}

/// Takes the scan lock at `lock_path`, waiting up to `timeout` for the current
/// holder to release it.
///
/// Returns `Ok(None)` when the lock is still held once `timeout` has passed.
/// A zero `timeout` tries once, like [`try_acquire_scan_lock`].
///
/// # Errors
///
/// Returns an error if the lock file cannot be created or opened, or the
/// operating system cannot lock it.
pub fn acquire_scan_lock_with_timeout(
    lock_path: &Path,
    timeout: Duration,
) -> Result<Option<ScanLockGuard>> {
    let file = open_lock_file(lock_path)?;
    // A timeout too large to represent waits indefinitely.
    let deadline = Instant::now().checked_add(timeout);
    loop {
        match file.try_lock() {
            Ok(()) => {
                return Ok(Some(ScanLockGuard {
                    file,
                    path: lock_path.to_path_buf(),
                }));
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("lock scan lock {}", lock_path.display()));
            }
        }
        let now = Instant::now();
        let pause = match deadline {
            Some(deadline) if now >= deadline => return Ok(None),
            Some(deadline) => SCAN_LOCK_POLL_INTERVAL.min(deadline - now),
            None => SCAN_LOCK_POLL_INTERVAL,
        };
        std::thread::sleep(pause);
    }
}

fn open_lock_file(lock_path: &Path) -> Result<File> {
    if let Some(parent) = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
            restrict_dir_permissions(parent)?;
        }
    }
    let mut options = OpenOptions::new();
    // Never truncated: holders and waiters share the one file.
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(lock_path)
        .with_context(|| format!("open scan lock {}", lock_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_lives_beside_the_store() {
        assert_eq!(
            scan_lock_path(Path::new("/home/me/.statsai/statsai.sqlite")),
            PathBuf::from("/home/me/.statsai/scan.lock")
        );
        assert_eq!(
            scan_lock_path(Path::new("statsai.sqlite")),
            PathBuf::from("scan.lock")
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_to_the_store_shares_the_store_s_lock() {
        let directory = tempfile::tempdir().expect("tempdir");
        let real_directory = directory.path().join("real");
        let link_directory = directory.path().join("link");
        std::fs::create_dir(&real_directory).expect("real directory");
        std::fs::create_dir(&link_directory).expect("link directory");
        let store = real_directory.join("statsai.sqlite");
        std::fs::write(&store, b"").expect("store file");
        let linked_store = link_directory.join("statsai.sqlite");
        std::os::unix::fs::symlink(&store, &linked_store).expect("symlink");

        let lock = scan_lock_path(&store);
        assert_eq!(scan_lock_path(&linked_store), lock);
        assert_eq!(
            lock,
            std::fs::canonicalize(&real_directory)
                .expect("canonical directory")
                .join(SCAN_LOCK_FILE_NAME)
        );
        let _held = try_acquire_scan_lock(&lock)
            .expect("acquire")
            .expect("uncontended lock");
        assert!(try_acquire_scan_lock(&scan_lock_path(&linked_store))
            .expect("try through the link")
            .is_none());
    }

    #[test]
    #[cfg(unix)]
    fn a_new_lock_directory_is_private_and_the_file_survives_release() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let lock_path = directory.path().join(".statsai").join(SCAN_LOCK_FILE_NAME);

        let guard = try_acquire_scan_lock(&lock_path)
            .expect("acquire")
            .expect("uncontended lock");
        assert_eq!(guard.path(), lock_path);
        drop(guard);

        let mode = |path: &Path| {
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(lock_path.parent().expect("parent")), 0o700);
        assert_eq!(mode(&lock_path), 0o600);
        assert!(lock_path.exists(), "release must not delete the lock file");
    }
}
