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
        committer_identities: BTreeSet::new(),
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
