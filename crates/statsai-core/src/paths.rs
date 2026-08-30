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
