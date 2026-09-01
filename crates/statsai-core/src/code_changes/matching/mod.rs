//! Decides, conservatively, which commit an agent edit became.

use super::*;
use crate::hash_text;
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

/// Distinct changed lines an edit must carry before it may be attributed at all.
///
/// Below this floor an ordered overlap says nothing about authorship, so the
/// match is withheld at every confidence level rather than only at High.
const MIN_ATTRIBUTION_DISTINCT_FINGERPRINTS: usize = 3;

#[must_use]
pub fn match_trace_edits_to_commits(
    trace_edits: &[TraceEdit],
    git_scans: &[GitScan],
) -> Vec<CodeChangeMatch> {
    // Grouping each scan's changed files by path once keeps matching linear in
    // the number of candidate commits per file instead of rescanning every
    // commit in every repository for every trace edit.
    let files_by_path = git_scans
        .iter()
        .map(|scan| {
            let mut index: BTreeMap<&Path, Vec<(&GitCommitChange, &GitFileChange)>> =
                BTreeMap::new();
            for commit in &scan.commits {
                // Merge diffs replay a parent commit's entire patch. Using them
                // for attribution would turn an otherwise exact match into an
                // ambiguity or double-attribute the same edit.
                if commit.parent_count > 1 {
                    continue;
                }
                for file in &commit.files {
                    index
                        .entry(file.relative_path.as_path())
                        .or_default()
                        .push((commit, file));
                }
            }
            index
        })
        .collect::<Vec<_>>();
    let scan_roots = canonical_scan_roots(git_scans);
    let mut canonical_paths = CanonicalPathCache::default();
    let mut matches = Vec::new();
    let mut matched_trace_ids = BTreeSet::new();
    for trace in trace_edits {
        let Some(trace_time) = trace.occurred_at else {
            continue;
        };
        // Without a repository path no scan can place the edit's file.
        let Some(repository_path) = trace.repository_path.as_deref() else {
            continue;
        };
        // An edit carrying too little distinct content cannot single out a
        // commit: ubiquitous lines such as a lone `}` or a blank line reach a
        // perfect overlap against whichever commit happens to touch the file,
        // which would credit an agent for otherwise human-written work. Such an
        // edit still counts as an applied agent edit and its commit still
        // counts as committed churn; only the attribution between them is
        // withheld.
        let distinct_fingerprints = trace
            .added_line_fingerprints
            .iter()
            .chain(&trace.deleted_line_fingerprints)
            .collect::<BTreeSet<_>>()
            .len();
        if distinct_fingerprints < MIN_ATTRIBUTION_DISTINCT_FINGERPRINTS {
            continue;
        }
        let canonical_repository_path = canonical_paths.resolve(repository_path).to_path_buf();
        let mut best: Option<(AttributionConfidence, f64, &GitCommitChange, &GitFileChange)> = None;
        let mut ambiguous = false;
        for ((scan, index), scan_root) in git_scans.iter().zip(&files_by_path).zip(&scan_roots) {
            let Some(trace_relative_path) =
                trace_path_in_scan(trace, scan, &canonical_repository_path, scan_root)
            else {
                continue;
            };
            let Some(candidates) = index.get(trace_relative_path.as_path()) else {
                continue;
            };
            for (commit, file) in candidates.iter().copied() {
                let Some(delay) = commit_delay_since_trace(commit, trace_time) else {
                    continue;
                };
                if delay.num_seconds() > 30 * 24 * 60 * 60 {
                    continue;
                }
                let added_overlap = ordered_overlap_ratio(
                    &trace.added_line_fingerprints,
                    &file.added_line_fingerprints,
                );
                let deleted_overlap = ordered_overlap_ratio(
                    &trace.deleted_line_fingerprints,
                    &file.deleted_line_fingerprints,
                );
                let overlap = weighted_overlap(
                    added_overlap,
                    trace.added_line_fingerprints.len(),
                    deleted_overlap,
                    trace.deleted_line_fingerprints.len(),
                );
                let confidence = if delay.num_seconds() <= 7 * 24 * 60 * 60
                    && added_overlap == 1.0
                    && deleted_overlap == 1.0
                {
                    Some(AttributionConfidence::High)
                } else if overlap >= 0.8 {
                    Some(AttributionConfidence::Medium)
                } else {
                    None
                };
                let Some(confidence) = confidence else {
                    continue;
                };
                let rank = |value: AttributionConfidence| match value {
                    AttributionConfidence::High => 2,
                    AttributionConfidence::Medium => 1,
                };
                let is_better = best.as_ref().is_none_or(|current| {
                    rank(confidence) > rank(current.0)
                        || (rank(confidence) == rank(current.0) && overlap > current.1)
                });
                let is_equally_strong = best.as_ref().is_some_and(|current| {
                    rank(confidence) == rank(current.0)
                        && overlap == current.1
                        && commit.deduplication_id != current.2.deduplication_id
                });
                if is_better {
                    best = Some((confidence, overlap, commit, file));
                    ambiguous = false;
                } else if is_equally_strong {
                    ambiguous = true;
                }
            }
        }
        if let Some((confidence, overlap, commit, file)) = best.filter(|_| !ambiguous) {
            if matched_trace_ids.insert(trace.trace_edit_id.clone()) {
                matches.push(CodeChangeMatch {
                    match_id: hash_text(&format!(
                        "code-match.v1:{}:{}:{}",
                        trace.trace_edit_id,
                        commit.deduplication_id,
                        file.relative_path.display()
                    )),
                    trace_edit_id: trace.trace_edit_id.clone(),
                    commit_deduplication_id: commit.deduplication_id.clone(),
                    commit_hash: commit.commit_hash.clone(),
                    repository_hash: commit.repository_hash.clone(),
                    relative_path: file.relative_path.clone(),
                    committed_at: commit.committed_at,
                    confidence,
                    ordered_line_overlap: overlap,
                    counts: trace.counts,
                });
            }
        }
    }
    matches
}

/// How long after a trace edit the commit carrying it was written.
///
/// A rebase, amend, or squash rewrites the committer date, so a month of work
/// can suddenly claim to have been committed today. The author date survives
/// those rewrites, so the shorter non-negative gap of the two is the honest
/// one: without it, rebasing work older than the matching window destroys its
/// attribution permanently. `None` means the commit predates the edit and
/// cannot carry it.
fn commit_delay_since_trace(
    commit: &GitCommitChange,
    trace_time: DateTime<Utc>,
) -> Option<Duration> {
    [commit.authored_at, Some(commit.committed_at)]
        .into_iter()
        .flatten()
        .map(|written_at| written_at.signed_duration_since(trace_time))
        .filter(|delay| delay.num_seconds() >= 0)
        .min()
}

/// Locates a trace edit's file inside one scanned repository.
///
/// A project and a repository nest either way around. The project can sit
/// inside the repository, in which case the edit's path needs the project's
/// offset prepended. The repository can equally sit inside the project: an
/// agent working in a multi-repository workspace records the workspace as its
/// project, so the edit's path already leads with the repository's directory
/// and that leading component has to come off instead. Handling only the first
/// direction silently drops every edit made in such a workspace.
///
/// Symlinked project paths only agree with a scan root after canonicalization,
/// which costs a syscall per component. Both canonical forms are therefore
/// resolved once by the caller and passed in: this runs for every
/// (edit, repository) pair, and archives reach hundreds of thousands of edits.
pub(super) fn trace_path_in_scan(
    trace: &TraceEdit,
    scan: &GitScan,
    canonical_repository_path: &Path,
    canonical_scan_root: &Path,
) -> Option<PathBuf> {
    let repository_path = trace.repository_path.as_deref()?;
    if repository_path == scan.repository_root || canonical_repository_path == canonical_scan_root {
        return repository_contained_path(trace.relative_path.clone());
    }
    if let Some(project_offset) = repository_path
        .strip_prefix(&scan.repository_root)
        .ok()
        .or_else(|| {
            canonical_repository_path
                .strip_prefix(canonical_scan_root)
                .ok()
        })
    {
        return repository_contained_path(project_offset.join(&trace.relative_path));
    }
    let repository_offset = scan
        .repository_root
        .strip_prefix(repository_path)
        .ok()
        .or_else(|| {
            canonical_scan_root
                .strip_prefix(canonical_repository_path)
                .ok()
        })?;
    repository_contained_path(
        trace
            .relative_path
            .strip_prefix(repository_offset)
            .ok()?
            .to_path_buf(),
    )
}

/// Accepts a repository-relative path only while it stays inside the repository.
///
/// A tool call can name a file the project does not contain, either absolutely
/// or by escaping with `..`, and reconstruction keeps that path verbatim when it
/// cannot be made relative. Such an edit is real but belongs to no scanned
/// repository, so it must not inherit this one's identity. Note that `join`
/// with an absolute path discards the prefix, so this also covers the nested
/// project case.
fn repository_contained_path(path: PathBuf) -> Option<PathBuf> {
    let mut depth = 0_usize;
    for component in path.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => depth = depth.checked_sub(1)?,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(path)
}

/// Canonical repository roots, positionally aligned with `git_scans`.
pub(super) fn canonical_scan_roots(git_scans: &[GitScan]) -> Vec<PathBuf> {
    git_scans
        .iter()
        .map(|scan| canonical_path(&scan.repository_root))
        .collect()
}

/// Resolves symlinks in `path` as far as the filesystem still allows.
///
/// A recorded path routinely no longer exists: a deleted worktree, a scratch
/// directory that has been cleaned up. `canonicalize` then fails outright and
/// leaves the logical path it was recorded as, which cannot be prefix-matched
/// against a stored Git root: Git reports `rev-parse --show-toplevel` as the
/// physical path, so a project recorded as `/tmp/project` faces a root stored as
/// `/private/tmp/project` and the two share no prefix. Resolving the longest
/// surviving ancestor and re-appending the rest keeps them comparable, so a
/// repository whose directory is gone is still recognised as one already
/// measured rather than one never seen.
///
/// A path that still exists resolves exactly as before.
pub fn canonical_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let mut trailing = Vec::new();
    let mut current = path;
    while let Some(parent) = current.parent() {
        let Some(name) = current.file_name() else {
            break;
        };
        trailing.push(name);
        if let Ok(canonical) = parent.canonicalize() {
            let mut resolved = canonical;
            resolved.extend(trailing.iter().rev());
            return resolved;
        }
        current = parent;
    }
    path.to_path_buf()
}

/// Memoizes canonical paths, since trace edits share a handful of project roots.
#[derive(Default)]
pub(super) struct CanonicalPathCache(BTreeMap<PathBuf, PathBuf>);

impl CanonicalPathCache {
    pub(super) fn resolve(&mut self, path: &Path) -> &Path {
        if !self.0.contains_key(path) {
            self.0.insert(path.to_path_buf(), canonical_path(path));
        }
        &self.0[path]
    }
}

fn ordered_overlap_ratio(needle: &[String], haystack: &[String]) -> f64 {
    if needle.is_empty() {
        return 1.0;
    }
    let mut matched = 0usize;
    for value in haystack {
        if needle.get(matched) == Some(value) {
            matched += 1;
            if matched == needle.len() {
                break;
            }
        }
    }
    matched as f64 / needle.len() as f64
}

fn weighted_overlap(added: f64, added_count: usize, deleted: f64, deleted_count: usize) -> f64 {
    let total = added_count.saturating_add(deleted_count);
    if total == 0 {
        0.0
    } else {
        (added * added_count as f64 + deleted * deleted_count as f64) / total as f64
    }
}

pub(super) fn repository_hash_for_trace(
    edit: &TraceEdit,
    scans: &[GitScan],
    scan_roots: &[PathBuf],
    canonical_repository_path: &Path,
) -> Option<String> {
    scans
        .iter()
        .zip(scan_roots)
        .filter(|(scan, scan_root)| {
            trace_path_in_scan(edit, scan, canonical_repository_path, scan_root).is_some()
        })
        .max_by_key(|(scan, _)| scan.repository_root.components().count())
        .map(|(scan, _)| scan.repository_hash.clone())
}

#[cfg(test)]
mod tests;
