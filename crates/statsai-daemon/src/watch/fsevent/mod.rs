//! Patched FSEvents backend.
//!
//! The portable decisions live in `logic`. The CoreFoundation watcher is selected
//! explicitly on macOS; stable notify 8.2.0 still ships the unpatched backend.

#[cfg(any(target_os = "macos", test))]
mod logic;

#[cfg(target_os = "macos")]
mod darwin;

#[cfg(target_os = "macos")]
pub(super) use darwin::FsEventWatcher;
