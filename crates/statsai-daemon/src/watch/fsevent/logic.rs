//! Portable FSEvents decisions.
//!
//! The CoreFoundation watcher is macOS-only. These helpers are what that watcher
//! uses, and they are tested on every platform.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) const MUST_SCAN_SUBDIRS: u32 = 0x1;
pub(crate) const USER_DROPPED: u32 = 0x2;
pub(crate) const KERNEL_DROPPED: u32 = 0x4;
pub(crate) const IDS_WRAPPED: u32 = 0x8;
pub(crate) const HISTORY_DONE: u32 = 0x10;
pub(crate) const ROOT_CHANGED: u32 = 0x20;
pub(crate) const MOUNT: u32 = 0x40;
pub(crate) const UNMOUNT: u32 = 0x80;
pub(crate) const ITEM_CREATED: u32 = 0x100;
pub(crate) const ITEM_REMOVED: u32 = 0x200;
pub(crate) const INODE_META_MOD: u32 = 0x400;
pub(crate) const ITEM_RENAMED: u32 = 0x800;
pub(crate) const ITEM_MODIFIED: u32 = 0x1000;
pub(crate) const FINDER_INFO_MOD: u32 = 0x2000;
pub(crate) const ITEM_CHANGE_OWNER: u32 = 0x4000;
pub(crate) const ITEM_XATTR_MOD: u32 = 0x8000;
pub(crate) const IS_FILE: u32 = 0x10000;
pub(crate) const IS_DIR: u32 = 0x20000;
pub(crate) const IS_SYMLINK: u32 = 0x40000;
pub(crate) const OWN_EVENT: u32 = 0x80000;
pub(crate) const IS_HARDLINK: u32 = 0x100000;
pub(crate) const IS_LAST_HARDLINK: u32 = 0x200000;
pub(crate) const ITEM_CLONED: u32 = 0x400000;

const KNOWN_FLAGS: u32 = MUST_SCAN_SUBDIRS
    | USER_DROPPED
    | KERNEL_DROPPED
    | IDS_WRAPPED
    | HISTORY_DONE
    | ROOT_CHANGED
    | MOUNT
    | UNMOUNT
    | ITEM_CREATED
    | ITEM_REMOVED
    | INODE_META_MOD
    | ITEM_RENAMED
    | ITEM_MODIFIED
    | FINDER_INFO_MOD
    | ITEM_CHANGE_OWNER
    | ITEM_XATTR_MOD
    | IS_FILE
    | IS_DIR
    | IS_SYMLINK
    | OWN_EVENT
    | IS_HARDLINK
    | IS_LAST_HARDLINK
    | ITEM_CLONED;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedFlags {
    pub(crate) known: u32,
    pub(crate) unknown: bool,
    pub(crate) rescan: bool,
    pub(crate) ignore: bool,
    pub(crate) refresh_root: bool,
}

pub(crate) fn decode_stream_flags(raw: u32) -> DecodedFlags {
    let unknown_bits = raw & !KNOWN_FLAGS;
    let known = raw & KNOWN_FLAGS;
    let rescan = unknown_bits != 0
        || known & (MUST_SCAN_SUBDIRS | USER_DROPPED | KERNEL_DROPPED | IDS_WRAPPED | ROOT_CHANGED)
            != 0;
    DecodedFlags {
        known,
        unknown: unknown_bits != 0,
        rescan,
        ignore: known & HISTORY_DONE != 0 && known & !HISTORY_DONE == 0 && unknown_bits == 0,
        refresh_root: known & ROOT_CHANGED != 0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistrationError {
    InteriorNul,
    MissingPath,
}

pub(crate) fn validate_registration_path(
    bytes: &[u8],
    exists: bool,
) -> Result<(), RegistrationError> {
    if bytes.contains(&0) {
        return Err(RegistrationError::InteriorNul);
    }
    if !exists {
        return Err(RegistrationError::MissingPath);
    }
    Ok(())
}

/// A null CoreFoundation reference must not be passed to `CFRelease`.
pub(crate) fn cf_ref_needs_release(is_null: bool) -> bool {
    !is_null
}

#[cfg(unix)]
pub(crate) fn path_from_event_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
pub(crate) fn path_from_event_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchedRoot {
    pub(crate) configured: PathBuf,
    pub(crate) canonical: PathBuf,
    pub(crate) recursive: bool,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RootMap {
    by_configured: HashMap<PathBuf, WatchedRoot>,
}

impl RootMap {
    pub(crate) fn insert(&mut self, configured: PathBuf, canonical: PathBuf, recursive: bool) {
        self.by_configured.insert(
            configured.clone(),
            WatchedRoot {
                configured,
                canonical,
                recursive,
            },
        );
    }

    pub(crate) fn unwatch(&mut self, configured: &Path) -> Option<Option<PathBuf>> {
        let removed = self.by_configured.remove(configured)?;
        let still_used = self
            .by_configured
            .values()
            .any(|root| root.canonical == removed.canonical);
        if still_used {
            Some(None)
        } else {
            Some(Some(removed.canonical))
        }
    }

    pub(crate) fn refresh_targets(&mut self, canonicalize: impl Fn(&Path) -> Option<PathBuf>) {
        for root in self.by_configured.values_mut() {
            if let Some(canonical) = canonicalize(&root.configured) {
                root.canonical = canonical;
            }
        }
    }

    pub(crate) fn canonical_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<_> = self
            .by_configured
            .values()
            .map(|root| root.canonical.clone())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    pub(crate) fn translate(&self, raw: &Path) -> TranslatedEvent {
        if raw.as_os_str().is_empty() {
            return TranslatedEvent {
                paths: Vec::new(),
                rescan: true,
            };
        }
        let mut paths = Vec::new();
        let mut matched = false;
        for root in self.by_configured.values() {
            if raw == root.canonical || raw.starts_with(&root.canonical) {
                matched = true;
                if root.recursive
                    || raw == root.canonical
                    || raw.parent() == Some(root.canonical.as_path())
                {
                    if let Ok(suffix) = raw.strip_prefix(&root.canonical) {
                        let mut configured = root.configured.clone();
                        if !suffix.as_os_str().is_empty() {
                            configured.push(suffix);
                        }
                        paths.push(configured);
                    }
                }
            }
        }
        if matched && paths.iter().all(|path| path != raw) {
            paths.push(raw.to_path_buf());
        }
        TranslatedEvent {
            rescan: false,
            paths,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranslatedEvent {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) rescan: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_flags_are_truncated_and_request_a_rescan() {
        let decoded = decode_stream_flags(ITEM_MODIFIED | 0x8000_0000);
        assert!(decoded.unknown);
        assert!(decoded.rescan);
        assert_eq!(decoded.known, ITEM_MODIFIED);
        assert!(!decoded.ignore);
    }

    #[test]
    fn dropped_and_wrapped_events_rescan_without_treating_history_as_data() {
        assert!(decode_stream_flags(USER_DROPPED).rescan);
        assert!(decode_stream_flags(KERNEL_DROPPED).rescan);
        assert!(decode_stream_flags(MUST_SCAN_SUBDIRS).rescan);
        assert!(decode_stream_flags(IDS_WRAPPED).rescan);
        assert!(decode_stream_flags(ROOT_CHANGED).refresh_root);
        let history = decode_stream_flags(HISTORY_DONE);
        assert!(history.ignore);
        assert!(!history.rescan);
    }

    #[cfg(unix)]
    #[test]
    fn event_bytes_keep_invalid_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let path = path_from_event_bytes(b"/tmp/caf\xffe");
        assert_eq!(path.as_os_str().as_bytes(), b"/tmp/caf\xffe");
    }

    #[test]
    fn null_core_foundation_values_are_not_released() {
        assert!(!cf_ref_needs_release(true));
        assert!(cf_ref_needs_release(false));
    }

    #[test]
    fn registration_rejects_interior_nul_and_missing_paths() {
        assert_eq!(validate_registration_path(b"/tmp/ok", true), Ok(()));
        assert_eq!(
            validate_registration_path(b"/tmp/\0hidden", true),
            Err(RegistrationError::InteriorNul)
        );
        assert_eq!(
            validate_registration_path(b"/tmp/gone", false),
            Err(RegistrationError::MissingPath)
        );
    }

    #[test]
    fn symlink_roots_translate_back_to_the_configured_path() {
        let mut roots = RootMap::default();
        roots.insert(
            PathBuf::from("/configured/link"),
            PathBuf::from("/real/target"),
            true,
        );
        let raw = path_from_event_bytes(b"/real/target/child");
        let translated = roots.translate(&raw);
        assert!(!translated.rescan);
        assert!(translated
            .paths
            .contains(&PathBuf::from("/configured/link/child")));
        assert!(translated.paths.contains(&raw));
    }

    #[test]
    fn unwatch_uses_the_configured_path_after_deletion() {
        let mut roots = RootMap::default();
        roots.insert(
            PathBuf::from("/configured/deleted"),
            PathBuf::from("/real/deleted"),
            true,
        );
        roots.insert(
            PathBuf::from("/configured/alias"),
            PathBuf::from("/real/deleted"),
            false,
        );
        assert_eq!(roots.unwatch(Path::new("/configured/deleted")), Some(None));
        assert_eq!(
            roots.unwatch(Path::new("/configured/alias")),
            Some(Some(PathBuf::from("/real/deleted")))
        );
        assert_eq!(roots.unwatch(Path::new("/configured/alias")), None);
    }

    #[test]
    fn refresh_keeps_the_stored_canonical_path_when_the_file_is_gone() {
        let mut roots = RootMap::default();
        roots.insert(
            PathBuf::from("/configured/link"),
            PathBuf::from("/real/old"),
            true,
        );
        roots.refresh_targets(|_| None);
        assert_eq!(roots.canonical_paths(), vec![PathBuf::from("/real/old")]);
        roots.refresh_targets(|configured| {
            (configured == Path::new("/configured/link")).then(|| PathBuf::from("/real/new"))
        });
        assert_eq!(roots.canonical_paths(), vec![PathBuf::from("/real/new")]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_root_on_disk_follows_retargets_and_keeps_deleted_mappings() {
        let directory = tempfile::tempdir().expect("tempdir");
        let old_target = directory.path().join("old");
        let new_target = directory.path().join("new");
        std::fs::create_dir(&old_target).expect("old target");
        std::fs::create_dir(&new_target).expect("new target");
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&old_target, &link).expect("symlink");
        let mut roots = RootMap::default();
        roots.insert(link.clone(), link.canonicalize().expect("canonical"), true);
        let child = link.canonicalize().expect("child root").join("child");
        let translated = roots.translate(&child);
        assert!(translated.paths.contains(&link.join("child")));
        std::fs::remove_file(&link).expect("remove link");
        std::os::unix::fs::symlink(&new_target, &link).expect("retarget");
        roots.refresh_targets(|configured| configured.canonicalize().ok());
        assert_eq!(
            roots.canonical_paths(),
            vec![new_target.canonicalize().expect("new canonical")]
        );
        std::fs::remove_file(&link).expect("delete link");
        let stored = roots.canonical_paths();
        roots.refresh_targets(|configured| configured.canonicalize().ok());
        assert_eq!(roots.canonical_paths(), stored);
        assert_eq!(roots.unwatch(&link), Some(Some(stored[0].clone())));
    }

    #[test]
    fn an_empty_event_path_requests_a_rescan() {
        let roots = RootMap::default();
        let translated = roots.translate(Path::new(""));
        assert!(translated.rescan);
        assert!(translated.paths.is_empty());
    }
}
