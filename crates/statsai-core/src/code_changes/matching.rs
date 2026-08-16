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

pub(super) fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::SourceId;
    use std::fs;
    use tempfile::TempDir;

    fn scan_with_file(
        repository_root: &str,
        commit_hash: &str,
        committed_at: &str,
        file: GitFileChange,
    ) -> GitScan {
        GitScan {
            repository_root: PathBuf::from(repository_root),
            repository_hash: "repository".to_string(),
            commits: vec![GitCommitChange {
                deduplication_id: format!("dedup-{commit_hash}"),
                repository_hash: "repository".to_string(),
                commit_hash: commit_hash.to_string(),
                committed_at: DateTime::parse_from_rfc3339(committed_at).unwrap().into(),
                authored_at: None,
                parent_count: 1,
                project_id: None,
                files: vec![file],
            }],
            coverage: CoverageStatus::Complete,
        }
    }

    /// Test view of repository attribution, resolving the canonical paths the
    /// production caller resolves once per refresh.
    fn trace_repository_hash(trace: &TraceEdit, scans: &[GitScan]) -> Option<String> {
        let scan_roots = canonical_scan_roots(scans);
        let canonical_repository_path = canonical_path(trace.repository_path.as_deref()?);
        repository_hash_for_trace(trace, scans, &scan_roots, &canonical_repository_path)
    }

    #[test]
    fn attribution_is_exact_high_partial_medium_and_chronology_bounded() {
        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\nnew-d\nnew-e\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/repo"));
        let exact_file = GitFileChange {
            relative_path: PathBuf::from("src/lib.rs"),
            category: CodeCategory::Source,
            counts: CodeLineCounts::classified(CodeCategory::Source, 6, 2),
            added_line_fingerprints: trace
                .added_line_fingerprints
                .iter()
                .cloned()
                .chain([hash_text("human")])
                .collect(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let exact_scan = scan_with_file("/repo", "exact", "2026-08-05T10:00:00Z", exact_file);
        let exact = match_trace_edits_to_commits(&[trace.clone()], &[exact_scan]);
        assert_eq!(exact[0].confidence, AttributionConfidence::High);

        let mut partial_file = GitFileChange {
            relative_path: PathBuf::from("src/lib.rs"),
            category: CodeCategory::Source,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints[..4].to_vec(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let partial_scan = scan_with_file(
            "/repo",
            "partial",
            "2026-08-12T10:00:00Z",
            partial_file.clone(),
        );
        let partial = match_trace_edits_to_commits(&[trace.clone()], &[partial_scan]);
        assert_eq!(partial[0].confidence, AttributionConfidence::Medium);

        partial_file.relative_path = PathBuf::from("src/other.rs");
        let other_file_scan =
            scan_with_file("/repo", "other-file", "2026-08-12T10:00:00Z", partial_file);
        assert!(match_trace_edits_to_commits(&[trace.clone()], &[other_file_scan]).is_empty());

        let delayed_file = GitFileChange {
            relative_path: PathBuf::from("src/lib.rs"),
            category: CodeCategory::Source,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let delayed_scan = scan_with_file("/repo", "delayed", "2026-09-02T10:00:01Z", delayed_file);
        assert!(match_trace_edits_to_commits(&[trace], &[delayed_scan]).is_empty());
    }

    #[test]
    fn equally_strong_repeated_boilerplate_is_left_unattributed() {
        let source = SourceId("source".to_string());
        // Distinct enough to clear the attribution floor, so this exercises the
        // ambiguity guard rather than being dropped before it.
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "repeated-a\nrepeated-b\nrepeated-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/repo"));
        let file = GitFileChange {
            relative_path: trace.relative_path.clone(),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let first = scan_with_file("/repo", "first", "2026-08-02T10:00:00Z", file.clone());
        let second = scan_with_file("/repo", "second", "2026-08-03T10:00:00Z", file.clone());
        assert!(
            match_trace_edits_to_commits(&[trace.clone()], &[first.clone(), second]).is_empty(),
            "two equally strong candidates leave the edit unattributed"
        );
        assert_eq!(
            match_trace_edits_to_commits(&[trace], &[first]).len(),
            1,
            "the same edit does match when only one candidate exists"
        );
    }

    #[test]
    fn edits_too_trivial_to_identify_a_commit_are_not_attributed_at_any_confidence() {
        let source = SourceId("source".to_string());
        // A lone `}` reaches a perfect overlap against whichever commit happens
        // to touch the file, so it identifies no author and is withheld rather
        // than merely demoted to medium confidence.
        for (old_string, new_string) in [("", "}"), ("", "\n"), ("old", "new")] {
            let mut trace = parse_structured_edit(
                &context(&source),
                Path::new("src/lib.rs"),
                old_string,
                new_string,
                0,
            )
            .edits
            .remove(0);
            trace.repository_path = Some(PathBuf::from("/repo"));
            let file = GitFileChange {
                relative_path: trace.relative_path.clone(),
                category: trace.category,
                counts: trace.counts,
                added_line_fingerprints: trace.added_line_fingerprints.clone(),
                deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
            };
            let scan = scan_with_file("/repo", "trivial", "2026-08-02T10:00:00Z", file);

            assert!(
                match_trace_edits_to_commits(&[trace], &[scan]).is_empty(),
                "{old_string:?} -> {new_string:?} carries too little distinct content to attribute"
            );
        }
    }

    #[test]
    fn merge_replay_does_not_make_the_original_commit_match_ambiguous() {
        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/repo"));
        let file = GitFileChange {
            relative_path: trace.relative_path.clone(),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let original = scan_with_file(
            "/repo",
            "feature-commit",
            "2026-08-02T10:00:00Z",
            file.clone(),
        );
        let mut merge = scan_with_file("/repo", "merge-commit", "2026-08-03T10:00:00Z", file);
        merge.commits[0].parent_count = 2;
        let matched = match_trace_edits_to_commits(&[trace], &[original, merge]);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].commit_hash, "feature-commit");
    }

    #[test]
    fn attribution_normalizes_nested_project_paths_to_the_repository_root() {
        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/repo/packages/app"));
        let file = GitFileChange {
            relative_path: PathBuf::from("packages/app/src/lib.rs"),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let scan = scan_with_file("/repo", "nested", "2026-08-02T10:00:00Z", file);
        let matched = match_trace_edits_to_commits(&[trace.clone()], std::slice::from_ref(&scan));
        assert_eq!(matched.len(), 1);
        assert_eq!(
            trace_repository_hash(&trace, &[scan]),
            Some("repository".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn attribution_matches_a_symlinked_project_to_the_physical_repository_root() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new().unwrap();
        let repository_root = directory.path().join("repository");
        fs::create_dir_all(&repository_root).unwrap();
        let linked_root = directory.path().join("linked-repository");
        symlink(&repository_root, &linked_root).unwrap();

        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(linked_root);
        let file = GitFileChange {
            relative_path: PathBuf::from("src/lib.rs"),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let scan = scan_with_file(
            &repository_root.to_string_lossy(),
            "symlinked",
            "2026-08-02T10:00:00Z",
            file,
        );

        let matched = match_trace_edits_to_commits(&[trace.clone()], std::slice::from_ref(&scan));

        assert_eq!(matched.len(), 1);
        assert_eq!(
            trace_repository_hash(&trace, &[scan]),
            Some("repository".to_string())
        );
    }

    #[test]
    fn trace_repository_hash_uses_deepest_matching_repository_root() {
        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/repo/nested"));
        let mut parent = scan_with_file(
            "/repo",
            "parent",
            "2026-08-02T10:00:00Z",
            GitFileChange {
                relative_path: PathBuf::from("nested/src/lib.rs"),
                category: trace.category,
                counts: trace.counts,
                added_line_fingerprints: trace.added_line_fingerprints.clone(),
                deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
            },
        );
        parent.repository_hash = "parent-repository".to_string();
        let mut nested = scan_with_file(
            "/repo/nested",
            "nested",
            "2026-08-02T10:00:00Z",
            GitFileChange {
                relative_path: PathBuf::from("src/lib.rs"),
                category: trace.category,
                counts: trace.counts,
                added_line_fingerprints: trace.added_line_fingerprints.clone(),
                deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
            },
        );
        nested.repository_hash = "nested-repository".to_string();

        assert_eq!(
            trace_repository_hash(&trace, &[parent, nested]),
            Some("nested-repository".to_string())
        );
    }

    #[test]
    fn rebased_commits_keep_their_attribution_through_the_author_date() {
        let source = SourceId("source".to_string());
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        // The edit is made a few hours before the work is first committed.
        trace.occurred_at = Some(
            DateTime::parse_from_rfc3339("2026-07-11T10:00:00Z")
                .unwrap()
                .into(),
        );
        trace.repository_path = Some(PathBuf::from("/repo"));
        let file = GitFileChange {
            relative_path: trace.relative_path.clone(),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        // The branch is rebased a month later, which resets the committer date
        // well past the matching window while the author date stays put.
        let mut scan = scan_with_file("/repo", "rebased", "2026-08-14T15:52:02Z", file);
        assert!(
            match_trace_edits_to_commits(&[trace.clone()], std::slice::from_ref(&scan)).is_empty(),
            "the committer date alone puts the commit outside the window"
        );

        scan.commits[0].authored_at = Some(
            DateTime::parse_from_rfc3339("2026-07-11T16:23:50Z")
                .unwrap()
                .into(),
        );
        let matched = match_trace_edits_to_commits(&[trace], std::slice::from_ref(&scan));

        assert_eq!(matched.len(), 1, "the author date rescues the attribution");
        assert_eq!(matched[0].confidence, AttributionConfidence::High);
    }

    #[test]
    fn edits_in_a_multi_repository_workspace_reach_the_nested_repository() {
        let source = SourceId("source".to_string());
        // The agent's project is a workspace folder; the Git repository is a
        // directory inside it, so the edit's path leads with that directory
        // while the commit records the path relative to the repository root.
        let mut trace = parse_structured_edit(
            &context(&source),
            Path::new("AudioToolSwift/Parity/adapters/chatterbox.py"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        trace.repository_path = Some(PathBuf::from("/workspace"));
        let file = GitFileChange {
            relative_path: PathBuf::from("Parity/adapters/chatterbox.py"),
            category: trace.category,
            counts: trace.counts,
            added_line_fingerprints: trace.added_line_fingerprints.clone(),
            deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
        };
        let scan = scan_with_file(
            "/workspace/AudioToolSwift",
            "nested",
            "2026-08-02T10:00:00Z",
            file,
        );

        let matched = match_trace_edits_to_commits(&[trace.clone()], std::slice::from_ref(&scan));

        assert_eq!(matched.len(), 1, "the nested repository owns this edit");
        assert_eq!(
            matched[0].relative_path,
            Path::new("Parity/adapters/chatterbox.py")
        );
        assert_eq!(
            trace_repository_hash(&trace, std::slice::from_ref(&scan)),
            Some("repository".to_string())
        );

        // A sibling repository in the same workspace must not claim it.
        let sibling = scan_with_file(
            "/workspace/ClearVoice",
            "sibling",
            "2026-08-02T10:00:00Z",
            GitFileChange {
                relative_path: PathBuf::from("Parity/adapters/chatterbox.py"),
                category: trace.category,
                counts: trace.counts,
                added_line_fingerprints: trace.added_line_fingerprints.clone(),
                deleted_line_fingerprints: trace.deleted_line_fingerprints.clone(),
            },
        );
        assert_eq!(
            trace_repository_hash(&trace, std::slice::from_ref(&sibling)),
            None
        );
    }

    #[test]
    fn edits_outside_the_repository_do_not_inherit_its_identity() {
        let source = SourceId("source".to_string());
        let file = GitFileChange {
            relative_path: PathBuf::from("src/lib.rs"),
            category: CodeCategory::Source,
            counts: CodeLineCounts::classified(CodeCategory::Source, 3, 0),
            added_line_fingerprints: Vec::new(),
            deleted_line_fingerprints: Vec::new(),
        };
        let scan = scan_with_file("/repo", "commit", "2026-08-02T10:00:00Z", file);

        for outside in ["/etc/elsewhere.rs", "../outside/lib.rs", "../../lib.rs"] {
            let mut trace = parse_structured_edit(
                &context(&source),
                Path::new(outside),
                "old-a\nold-b\n",
                "new-a\nnew-b\nnew-c\n",
                0,
            )
            .edits
            .remove(0);
            trace.repository_path = Some(PathBuf::from("/repo"));

            assert_eq!(
                trace_repository_hash(&trace, std::slice::from_ref(&scan)),
                None,
                "{outside} is not inside the scanned repository"
            );
            assert!(
                match_trace_edits_to_commits(&[trace], std::slice::from_ref(&scan)).is_empty(),
                "{outside} must not be attributed to this repository"
            );
        }

        // A path genuinely inside the repository is still placed.
        let mut inside = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "old-a\nold-b\n",
            "new-a\nnew-b\nnew-c\n",
            0,
        )
        .edits
        .remove(0);
        inside.repository_path = Some(PathBuf::from("/repo"));
        assert_eq!(
            trace_repository_hash(&inside, std::slice::from_ref(&scan)),
            Some("repository".to_string())
        );
    }
}
