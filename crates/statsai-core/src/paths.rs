use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[must_use]
pub fn hash_text(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)
}

#[must_use]
pub fn path_hash(path: &Path) -> String {
    let canonical = canonical_display(path);
    hash_text(&canonical)
}

#[must_use]
pub fn canonical_display(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| expand_home(path))
        .to_string_lossy()
        .to_string()
}

/// Display-friendly path normalization.
/// Expands `~` for home but does NOT perform filesystem canonicalization
/// (to avoid symlink/mount identity changes for labels).
#[must_use]
pub fn display_path(path: &Path) -> String {
    expand_home(path).to_string_lossy().to_string()
}

fn expand_home(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(stripped) = text.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}

#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Rewrites a path label that names `home` or a path inside it as `~` or
/// `~/rest`, for labels that leave the device. Only the label text changes:
/// path hashes and other identities are computed from the full path and are
/// not affected.
///
/// Matching is on whole path components, so `/Users/alice2` is not inside
/// `/Users/alice`. A Windows-style home (drive letter or backslash) matches
/// case-insensitively, accepts either separator, and keeps the label's own
/// separator after `~`. A home that is a filesystem or drive root never
/// matches, and labels outside home come back unchanged.
#[must_use]
pub fn collapse_home_path_label(label: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    let windows = home.contains('\\') || has_windows_drive_prefix(&home);
    let is_separator = |ch: char| ch == '/' || (windows && ch == '\\');
    let home = home.trim_end_matches(is_separator);
    let is_root = home.is_empty() || (has_windows_drive_prefix(home) && home.len() == 2);
    if is_root {
        return label.to_string();
    }
    let Some(prefix) = label.get(..home.len()) else {
        return label.to_string();
    };
    let same_home = if windows {
        prefix.bytes().zip(home.bytes()).all(|(left, right)| {
            left.eq_ignore_ascii_case(&right)
                || (matches!(left, b'/' | b'\\') && matches!(right, b'/' | b'\\'))
        })
    } else {
        prefix == home
    };
    if !same_home {
        return label.to_string();
    }
    let rest = &label[home.len()..];
    if rest.chars().all(is_separator) {
        return "~".to_string();
    }
    if rest.starts_with(is_separator) {
        return format!("~{rest}");
    }
    label.to_string()
}

fn has_windows_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[must_use]
pub fn expand_home_path(value: &str) -> PathBuf {
    if value == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(value)
}
