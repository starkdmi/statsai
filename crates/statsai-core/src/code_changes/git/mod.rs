//! Reads what a local Git repository already contains.

use super::*;
use crate::{hash_text, SourceId};
use chrono::{DateTime, Duration, Utc};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GIT_PATCH_BATCH_SIZE: usize = 256;

#[cfg(test)]
thread_local! {
    static TEST_GIT_COMMAND_COUNT: Cell<usize> = const { Cell::new(0) };
}

/// Inspect commits already present in a local Git object database.
///
/// The implementation invokes only local, read-only Git commands and disables
/// prompts, optional locks, external diff drivers, and text conversion.
pub fn scan_local_git_repository(
    path: &Path,
    project_id: Option<&str>,
) -> Result<GitScan, GitScanError> {
    scan_local_git_repository_cached(path, project_id, &[], &BTreeSet::new())
}

/// Blinds a committer address into the form scans and the store compare on.
///
/// Only equality is ever asked of a committer identity, and an email address
/// identifies a person, so it is hashed exactly like the repository identity
/// and commit hashes it sits beside rather than kept in the clear.
pub fn committer_identity_hash(email: &str) -> String {
    hash_text(&format!(
        "git-committer.v1:{}",
        email.trim().to_ascii_lowercase()
    ))
}

/// Inspect recent, locally attributable commits while reusing parsed commits.
///
/// Commit hashes cover their trees and parent relationships, so a cached patch
/// remains valid even when mutable repository identity metadata changes. The
/// scan is bounded to the current HEAD and local branches, the committer
/// identities this repository is known by, and the longest rolling dashboard
/// window.
///
/// `known_identities` are the blinded addresses earlier scans of this repository
/// ran under. They are matched in addition to the address configured now,
/// because reconfiguring `user.email` leaves the existing commits exactly as they
/// were.
///
/// Deciding which remembered identities belong to this repository is the
/// caller's job, not this scan's. A repository answers to two names that change
/// independently — its identity hash changes when an origin remote is added, its
/// root changes when the worktree moves — and only the caller holding the stored
/// scans can look up both. Selecting by either one alone loses the identities in
/// the case the other covers, and the scan would then delete in-window commits
/// made under an earlier address.
pub fn scan_local_git_repository_cached(
    path: &Path,
    project_id: Option<&str>,
    cached_commits: &[GitCommitChange],
    known_identities: &BTreeSet<String>,
) -> Result<GitScan, GitScanError> {
    let root = resolve_git_repository_root(path)?;
    let repository_hash = repository_identity_hash(&root)?;
    let configured_email = run_git_allow_missing(&root, &["config", "--get", "user.email"])?;
    let configured_email = configured_email.trim();
    let mut committer_identities = known_identities.clone();
    if !configured_email.is_empty() {
        committer_identities.insert(committer_identity_hash(configured_email));
    }
    // Commits are attributed by committer email, so with no identity ever known
    // for this repository the scan cannot tell whose work it holds. That is an
    // unanswerable question rather than an answer of "no commits": reporting an
    // empty success would let a caller replace commits it measured while an
    // identity was still known, so it is raised like any other unusable scan.
    // Once one identity is remembered, a temporarily missing `user.email` no
    // longer costs any coverage.
    if committer_identities.is_empty() {
        return Err(GitScanError::UnknownCommitterIdentity(root));
    }
    let now = Utc::now();
    let observation_start = (now - Duration::days(GIT_COMMIT_OBSERVATION_DAYS)).to_rfc3339();
    let latest_reportable_day = max_reportable_day(now);
    let since = format!("--since={observation_start}");
    let log = run_git(
        &root,
        &[
            "log",
            // A repository whose first commit has not landed yet has an unborn
            // HEAD, which `git log` otherwise rejects as an unknown revision,
            // failing the whole scan. Ignoring the missing revision leaves a
            // detached HEAD still covered.
            "--ignore-missing",
            "HEAD",
            "--branches",
            &since,
            "--format=%H%x09%cI%x09%aI%x09%P%x09%ce",
            "--no-show-signature",
        ],
    )?;
    let cached_by_hash = cached_commits
        .iter()
        .map(|commit| (commit.commit_hash.as_str(), commit))
        .collect::<BTreeMap<_, _>>();
    let mut metadata = Vec::new();
    let mut future_dated_commits = 0_u64;
    for line in log.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.splitn(5, '\t');
        let Some(commit_hash) = fields.next() else {
            continue;
        };
        let timestamp = fields.next().unwrap_or_default();
        let author_timestamp = fields.next().unwrap_or_default();
        let parents = fields
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>();
        let committer_email = fields.next().unwrap_or_default();
        if !committer_identities.contains(&committer_identity_hash(committer_email)) {
            continue;
        }
        // A merge diff replays every line the merged branch already contributed
        // through its own commits, so counting merges would double the churn of
        // every merged branch. Conflict resolutions carried only by the merge
        // commit are deliberately left uncounted rather than inflating totals.
        if parents.len() > 1 {
            continue;
        }
        let committed_at = DateTime::parse_from_rfc3339(timestamp)
            .map_err(|_| GitScanError::InvalidTimestamp {
                value: timestamp.to_string(),
            })?
            .with_timezone(&Utc);
        // A committer date is whatever the authoring clock said. One that is
        // implausibly far ahead would be rejected by the sync target and take
        // the whole batch down with it, so it is reported as unmeasured churn
        // through the scan's coverage instead.
        if committed_at.date_naive() > latest_reportable_day {
            future_dated_commits = future_dated_commits.saturating_add(1);
            continue;
        }
        // A rewritten history keeps the original author date, so it is the
        // honest anchor for when the work was written.
        let authored_at = DateTime::parse_from_rfc3339(author_timestamp)
            .ok()
            .map(|value| value.with_timezone(&Utc));
        metadata.push((
            commit_hash.to_string(),
            committed_at,
            authored_at,
            parents.len(),
        ));
    }
    let uncached_hashes = metadata
        .iter()
        .filter(|(commit_hash, _, _, _)| !cached_by_hash.contains_key(commit_hash.as_str()))
        .map(|(commit_hash, _, _, _)| commit_hash.as_str())
        .collect::<Vec<_>>();
    let mut patches_by_hash = git_patches_for_commits(&root, &uncached_hashes)?;
    let mut commits = Vec::with_capacity(metadata.len());
    for (commit_hash, committed_at, authored_at, parent_count) in metadata {
        if let Some(cached) = cached_by_hash.get(commit_hash.as_str()) {
            let mut commit = (*cached).clone();
            commit.deduplication_id =
                hash_text(&format!("git-commit.v1:{repository_hash}:{commit_hash}"));
            commit.repository_hash.clone_from(&repository_hash);
            commit.committed_at = committed_at;
            commit.authored_at = authored_at;
            commit.parent_count = parent_count;
            commit.project_id = project_id.map(ToOwned::to_owned);
            commits.push(commit);
            continue;
        }
        let files = patches_by_hash
            .remove(&commit_hash)
            .ok_or_else(|| GitScanError::Command {
                command: "git log --patch".to_string(),
                message: format!("batched patch output omitted commit {commit_hash}"),
            })?;
        commits.push(GitCommitChange {
            deduplication_id: hash_text(&format!("git-commit.v1:{repository_hash}:{commit_hash}")),
            repository_hash: repository_hash.clone(),
            commit_hash,
            committed_at,
            authored_at,
            parent_count,
            project_id: project_id.map(ToOwned::to_owned),
            files,
        });
    }
    Ok(GitScan {
        repository_root: root,
        repository_hash,
        commits,
        committer_identities,
        coverage: if future_dated_commits > 0 {
            CoverageStatus::Partial
        } else {
            CoverageStatus::Complete
        },
    })
}

/// Resolves the Git root that owns `path` with a single read-only command.
///
/// Several projects commonly share one repository. Resolving roots first lets
/// callers inspect each repository once instead of once per project path.
pub fn resolve_git_repository_root(path: &Path) -> Result<PathBuf, GitScanError> {
    let root_output = run_git(path, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root_output.trim());
    if root.as_os_str().is_empty() {
        return Err(GitScanError::NotRepository(path.to_path_buf()));
    }
    Ok(root)
}

fn git_patches_for_commits(
    root: &Path,
    commit_hashes: &[&str],
) -> Result<BTreeMap<String, Vec<GitFileChange>>, GitScanError> {
    if commit_hashes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut patches = BTreeMap::new();
    for commit_batch in commit_hashes.chunks(GIT_PATCH_BATCH_SIZE) {
        let mut args = vec![
            "log",
            "--no-walk=unsorted",
            "--format=%x1e%H%x1f",
            "--no-show-signature",
            "--patch",
            "--root",
            "--diff-merges=first-parent",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--unified=0",
        ];
        args.extend_from_slice(commit_batch);
        let output = run_git(root, &args)?;
        for record in output.split('\u{1e}').skip(1) {
            let Some((commit_hash, patch)) = record.split_once('\u{1f}') else {
                continue;
            };
            patches.insert(commit_hash.trim().to_string(), git_files_from_patch(patch));
        }
    }
    Ok(patches)
}

/// Derives the stable identity of the repository rooted at `root`.
///
/// The identity is its origin remote, or its root commits when it has none, so it
/// survives the worktree moving and changes only when the repository is re-keyed
/// by gaining a remote. Callers that hold stored scans need it to recognise a
/// repository whose location changed.
pub fn repository_identity_hash(root: &Path) -> Result<String, GitScanError> {
    let remote = run_git_allow_missing(root, &["config", "--get", "remote.origin.url"])?;
    let identity = if remote.trim().is_empty() {
        let roots = run_git(root, &["rev-list", "--max-parents=0", "--all"])?;
        let mut root_hashes = roots.lines().collect::<Vec<_>>();
        root_hashes.sort_unstable();
        if root_hashes.is_empty() {
            // A repository with neither an origin nor a single commit has no
            // shared identity to derive yet, and every such repository would
            // otherwise hash to the same empty root list and merge unrelated
            // work. The local path keeps them distinct until the first commit
            // supplies a stable root; the scan then re-keys the repository, and
            // superseded-hash handling retires the placeholder row.
            format!("path:{}", root.display())
        } else {
            format!("roots:{}", root_hashes.join(","))
        }
    } else {
        let remote = normalize_git_remote(&remote).unwrap_or_else(|| remote.trim().to_string());
        format!("remote:{remote}")
    };
    Ok(hash_text(&format!("repository.v1:{identity}")))
}

/// Converts common Git remote transports into a stable lowercase host/path identity.
#[must_use]
pub fn normalize_git_remote(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let host_and_path = if let Some(rest) = trimmed.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        format!("{host}/{path}")
    } else if let Some((_, rest)) = trimmed.split_once("://") {
        let rest = rest.trim_start_matches('/');
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next().unwrap_or(authority);
        format!("{host}/{path}")
    } else {
        trimmed.to_string()
    };

    let mut parts = host_and_path
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }
    if let Some(last) = parts.last_mut() {
        if let Some(stripped) = last.strip_suffix(".git") {
            *last = stripped.to_string();
        }
    }
    Some(parts.join("/"))
}

fn run_git(path: &Path, args: &[&str]) -> Result<String, GitScanError> {
    let output = git_command(path, args)
        .output()
        .map_err(|error| GitScanError::Command {
            command: format!("git {}", args.join(" ")),
            message: error.to_string(),
        })?;
    output_text(args, output, false)
}

fn run_git_allow_missing(path: &Path, args: &[&str]) -> Result<String, GitScanError> {
    let output = git_command(path, args)
        .output()
        .map_err(|error| GitScanError::Command {
            command: format!("git {}", args.join(" ")),
            message: error.to_string(),
        })?;
    output_text(args, output, true)
}

fn git_command(path: &Path, args: &[&str]) -> Command {
    #[cfg(test)]
    TEST_GIT_COMMAND_COUNT.set(TEST_GIT_COMMAND_COUNT.get().saturating_add(1));
    let mut command = Command::new("git");
    command
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .args(args);
    command
}

fn output_text(args: &[&str], output: Output, allow_failure: bool) -> Result<String, GitScanError> {
    if !output.status.success() && !allow_failure {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if message.contains("not a git repository") {
            GitScanError::NotRepository(PathBuf::from("."))
        } else {
            GitScanError::Command {
                command: format!("git {}", args.join(" ")),
                message,
            }
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_files_from_patch(patch: &str) -> Vec<GitFileChange> {
    let context = TraceEditContext {
        provider: "git",
        source_id: &SourceId("git".to_string()),
        cache_key: "git",
        conversation_id: "git",
        source_record_id: "git",
        occurred_at: None,
        project: None,
        repository_path: None,
    };
    parse_unified_patch(&context, patch)
        .edits
        .into_iter()
        .map(|edit| GitFileChange {
            relative_path: edit.relative_path,
            category: edit.category,
            counts: edit.counts,
            added_line_fingerprints: edit.added_line_fingerprints,
            deleted_line_fingerprints: edit.deleted_line_fingerprints,
        })
        .collect()
}

#[cfg(test)]
mod tests;
