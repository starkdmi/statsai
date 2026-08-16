//! Fixtures shared by more than one stage's tests.

use super::*;
use crate::SourceId;
use chrono::{DateTime, Utc};
use std::fs;
use std::path::Path;
use std::process::Command;

pub(super) fn context<'a>(source: &'a SourceId) -> TraceEditContext<'a> {
    TraceEditContext {
        provider: "codex",
        source_id: source,
        cache_key: "archive.jsonl",
        conversation_id: "conversation",
        source_record_id: "record:1",
        occurred_at: Some(
            DateTime::parse_from_rfc3339("2026-08-01T10:00:00Z")
                .unwrap()
                .into(),
        ),
        project: None,
        repository_path: None,
    }
}

pub(super) fn run_test_git(path: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

pub(super) fn commit_test_file(
    path: &Path,
    relative_path: &str,
    contents: &str,
    message: &str,
    email: &str,
    committed_at: DateTime<Utc>,
) {
    fs::write(path.join(relative_path), contents).unwrap();
    run_test_git(path, &["add", relative_path]);
    let timestamp = committed_at.to_rfc3339();
    let status = Command::new("git")
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_AUTHOR_DATE", &timestamp)
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", email)
        .env("GIT_COMMITTER_DATE", timestamp)
        .args(["commit", "-qm", message])
        .status()
        .unwrap();
    assert!(status.success());
}
