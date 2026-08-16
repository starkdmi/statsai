//! Turns a recorded agent edit into counted lines.

use super::*;
use crate::hash_text;
use std::path::{Path, PathBuf};

#[must_use]
pub fn parse_unified_patch(context: &TraceEditContext<'_>, patch: &str) -> ParsedMutation {
    #[derive(Default)]
    struct Section {
        index: usize,
        path: Option<PathBuf>,
        creation: bool,
        deletion: bool,
        additions: Vec<String>,
        deletions: Vec<String>,
        saw_hunk: bool,
    }

    fn finish_section(
        context: &TraceEditContext<'_>,
        section: &mut Section,
        edits: &mut Vec<TraceEdit>,
        unsupported: &mut u64,
        observed: &mut u64,
    ) {
        let Some(path) = section.path.take() else {
            // A hunk body that precedes any file header belongs to no file.
            // Leaving it in place would carry its lines into the next section,
            // attributing them to the wrong file and counting that file's churn
            // twice, so it is dropped and reported as unmeasured instead.
            if section.saw_hunk {
                *unsupported = unsupported.saturating_add(1);
            }
            *section = Section::default();
            return;
        };
        // Some agents emit absolute paths in patch headers. Attribution and
        // classification both expect repository-relative paths.
        let path = repository_relative_path(context.repository_path, path);
        let Some(category) = classify_code_path(&path) else {
            *observed = observed.saturating_add(1);
            *section = Section::default();
            return;
        };
        if !section.saw_hunk || (section.deletion && section.deletions.is_empty()) {
            *unsupported = unsupported.saturating_add(1);
            *section = Section::default();
            return;
        }
        *observed = observed.saturating_add(1);
        let kind = if section.creation {
            MutationKind::FileCreation
        } else {
            MutationKind::UnifiedPatch
        };
        edits.push(build_trace_edit(
            context,
            path,
            TraceEditMutation {
                category,
                kind,
                section_index: section.index,
                additions: &section.additions,
                deletions: &section.deletions,
                lines_written: 0,
            },
        ));
        *section = Section::default();
    }

    let mut edits = Vec::new();
    let mut unsupported_sections = 0u64;
    let mut observed_sections = 0u64;
    let mut section = Section::default();
    let mut next_section_index = 0_usize;
    let mut in_hunk = false;
    let lines = logical_lines(patch);
    for (line_index, raw_line) in lines.iter().enumerate() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let next_line = lines
            .get(line_index + 1)
            .map(|value| value.strip_suffix('\r').unwrap_or(value));
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            finish_section(
                context,
                &mut section,
                &mut edits,
                &mut unsupported_sections,
                &mut observed_sections,
            );
            section.index = next_section_index;
            next_section_index = next_section_index.saturating_add(1);
            section.path = clean_patch_path(path, PatchPathStyle::Literal);
            section.creation = true;
            in_hunk = true;
            section.saw_hunk = true;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            finish_section(
                context,
                &mut section,
                &mut edits,
                &mut unsupported_sections,
                &mut observed_sections,
            );
            section.index = next_section_index;
            next_section_index = next_section_index.saturating_add(1);
            section.path = clean_patch_path(path, PatchPathStyle::Literal);
            in_hunk = false;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            finish_section(
                context,
                &mut section,
                &mut edits,
                &mut unsupported_sections,
                &mut observed_sections,
            );
            section.index = next_section_index;
            next_section_index = next_section_index.saturating_add(1);
            section.path = clean_patch_path(path, PatchPathStyle::Literal);
            section.deletion = true;
            in_hunk = true;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Move to: ") {
            section.path = clean_patch_path(path, PatchPathStyle::Literal);
            continue;
        }
        if let Some(paths) = line.strip_prefix("diff --git ") {
            finish_section(
                context,
                &mut section,
                &mut edits,
                &mut unsupported_sections,
                &mut observed_sections,
            );
            section.index = next_section_index;
            next_section_index = next_section_index.saturating_add(1);
            section.path = paths
                .split_whitespace()
                .nth(1)
                .and_then(|path| clean_patch_path(path, PatchPathStyle::GitDestination));
            in_hunk = false;
            continue;
        }
        // A `---`/`+++` pair is the only file header in a plain unified diff
        // that carries no `diff --git` line. Requiring the pair keeps a deleted
        // body line such as `-- text` from being mistaken for a header, and
        // ending the previous section here stops a second file's hunks from
        // being attributed to the first one.
        if line.starts_with("--- ") && next_line.is_some_and(|next| next.starts_with("+++ ")) {
            if section.saw_hunk {
                finish_section(
                    context,
                    &mut section,
                    &mut edits,
                    &mut unsupported_sections,
                    &mut observed_sections,
                );
                section.index = next_section_index;
                next_section_index = next_section_index.saturating_add(1);
            }
            in_hunk = false;
            if line == "--- /dev/null" {
                section.creation = true;
            } else if next_line == Some("+++ /dev/null") {
                // A deletion carries its path on the source side only.
                section.path =
                    clean_patch_path(line.trim_start_matches("--- "), PatchPathStyle::GitSource);
                section.deletion = true;
            }
            continue;
        }
        // Outside a hunk a lone `+++` still names the destination file; inside
        // one it is body text such as `+++ still a line`.
        if !in_hunk {
            if let Some(path) = line.strip_prefix("+++ ") {
                if path != "/dev/null" {
                    section.path = clean_patch_path(path, PatchPathStyle::GitDestination);
                }
                continue;
            }
        }
        if line.starts_with("@@") {
            in_hunk = true;
            section.saw_hunk = true;
            continue;
        }
        if line.starts_with("*** End Patch") || line.starts_with("*** End of File") {
            continue;
        }
        if in_hunk {
            if let Some(added) = line.strip_prefix('+') {
                section.additions.push(added.to_string());
            } else if let Some(deleted) = line.strip_prefix('-') {
                section.deletions.push(deleted.to_string());
                if section.deletion {
                    section.saw_hunk = true;
                }
            }
        }
    }
    finish_section(
        context,
        &mut section,
        &mut edits,
        &mut unsupported_sections,
        &mut observed_sections,
    );
    ParsedMutation {
        coverage: if unsupported_sections == 0 && observed_sections > 0 {
            CoverageStatus::Complete
        } else if observed_sections == 0 {
            CoverageStatus::Unavailable
        } else {
            CoverageStatus::Partial
        },
        edits,
        unsupported_sections,
    }
}

#[must_use]
pub fn parse_structured_edit(
    context: &TraceEditContext<'_>,
    path: &Path,
    old_string: &str,
    new_string: &str,
    section_index: usize,
) -> ParsedMutation {
    let Some(category) = classify_code_path(path) else {
        return ParsedMutation {
            edits: Vec::new(),
            coverage: CoverageStatus::Complete,
            unsupported_sections: 0,
        };
    };
    let old_lines = owned_logical_lines(old_string);
    let new_lines = owned_logical_lines(new_string);
    let (additions, deletions) = changed_lines(&old_lines, &new_lines);
    ParsedMutation {
        edits: vec![build_trace_edit(
            context,
            path.to_path_buf(),
            TraceEditMutation {
                category,
                kind: MutationKind::StructuredEdit,
                section_index,
                additions: &additions,
                deletions: &deletions,
                lines_written: 0,
            },
        )],
        coverage: CoverageStatus::Complete,
        unsupported_sections: 0,
    }
}

#[must_use]
pub fn parse_full_file_write(
    context: &TraceEditContext<'_>,
    path: &Path,
    content: &str,
    creation_known: bool,
) -> ParsedMutation {
    let Some(category) = classify_code_path(path) else {
        return ParsedMutation {
            edits: Vec::new(),
            coverage: CoverageStatus::Complete,
            unsupported_sections: 0,
        };
    };
    let lines = owned_logical_lines(content);
    let (additions, lines_written, kind) = if creation_known {
        (lines.as_slice(), 0, MutationKind::FileCreation)
    } else {
        (&[][..], lines.len() as u64, MutationKind::FileWrite)
    };
    ParsedMutation {
        edits: vec![build_trace_edit(
            context,
            path.to_path_buf(),
            TraceEditMutation {
                category,
                kind,
                section_index: 0,
                additions,
                deletions: &[],
                lines_written,
            },
        )],
        coverage: CoverageStatus::Complete,
        unsupported_sections: 0,
    }
}

struct TraceEditMutation<'a> {
    category: CodeCategory,
    kind: MutationKind,
    section_index: usize,
    additions: &'a [String],
    deletions: &'a [String],
    lines_written: u64,
}

fn build_trace_edit(
    context: &TraceEditContext<'_>,
    relative_path: PathBuf,
    mutation: TraceEditMutation<'_>,
) -> TraceEdit {
    let added_line_fingerprints = mutation
        .additions
        .iter()
        .map(|line| hash_text(line))
        .collect::<Vec<_>>();
    let deleted_line_fingerprints = mutation
        .deletions
        .iter()
        .map(|line| hash_text(line))
        .collect::<Vec<_>>();
    let mut counts = CodeLineCounts::classified(
        mutation.category,
        mutation.additions.len() as u64,
        mutation.deletions.len() as u64,
    );
    counts.unclassified_lines_written = mutation.lines_written;
    let fingerprint = hash_text(&format!(
        "{}:{}:{}:{}:{}:{}:{:?}:{:?}",
        context.provider,
        context.source_id.0,
        context.conversation_id,
        context.source_record_id,
        relative_path.display(),
        mutation.section_index,
        added_line_fingerprints,
        deleted_line_fingerprints
    ));
    TraceEdit {
        schema_version: TRACE_EDIT_SCHEMA_VERSION.to_string(),
        trace_edit_id: format!("edit_{}", &fingerprint[..24]),
        provider: context.provider.to_string(),
        source_id: context.source_id.clone(),
        cache_key: context.cache_key.to_string(),
        conversation_id: context.conversation_id.to_string(),
        source_record_id: context.source_record_id.to_string(),
        occurred_at: context.occurred_at,
        project_id: context.project.map(|project| project.project_id.clone()),
        repository_path: context.repository_path.map(Path::to_path_buf),
        relative_path,
        category: mutation.category,
        mutation_kind: mutation.kind,
        counts,
        added_line_fingerprints,
        deleted_line_fingerprints,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchPathStyle {
    /// `apply_patch` headers carry the repository path verbatim.
    Literal,
    /// Git destination paths carry the `b/` prefix from `diff --git`/`+++`.
    GitDestination,
    /// Git source paths carry the `a/` prefix from `---`.
    GitSource,
}

fn clean_patch_path(value: &str, style: PatchPathStyle) -> Option<PathBuf> {
    let value = value.trim().trim_matches('"');
    if value.is_empty() || value == "/dev/null" {
        return None;
    }
    // Only Git adds the `b/` destination prefix. Stripping it unconditionally
    // would rewrite a real top-level `a/` or `b/` directory.
    let value = match style {
        PatchPathStyle::Literal => value,
        PatchPathStyle::GitDestination => value.strip_prefix("b/").unwrap_or(value),
        PatchPathStyle::GitSource => value.strip_prefix("a/").unwrap_or(value),
    };
    Some(PathBuf::from(value))
}

/// Rebases an edited file's path onto the repository that contains it.
///
/// Paths are only ever made shorter, never reinterpreted: an already-relative
/// path is returned untouched, because it is relative to the repository
/// already and stripping a prefix from it a second time would silently drop a
/// directory level and point the edit at the wrong file.
pub fn repository_relative_path(repository_path: Option<&Path>, path: PathBuf) -> PathBuf {
    if path.is_relative() {
        return path;
    }
    let relative = repository_path
        .and_then(|root| path.strip_prefix(root).ok())
        .map(Path::to_path_buf);
    relative.unwrap_or(path)
}

fn logical_lines(value: &str) -> Vec<&str> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split_terminator('\n').collect()
    }
}

fn owned_logical_lines(value: &str) -> Vec<String> {
    logical_lines(value)
        .into_iter()
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect()
}

fn changed_lines(old: &[String], new: &[String]) -> (Vec<String>, Vec<String>) {
    if old.len().saturating_mul(new.len()) > 4_000_000 {
        let prefix = old
            .iter()
            .zip(new)
            .take_while(|(left, right)| left == right)
            .count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(left, right)| left == right)
            .count();
        return (
            new[prefix..new.len().saturating_sub(suffix)].to_vec(),
            old[prefix..old.len().saturating_sub(suffix)].to_vec(),
        );
    }
    let width = new.len() + 1;
    let mut lcs = vec![0u32; (old.len() + 1).saturating_mul(width)];
    for old_index in (0..old.len()).rev() {
        for new_index in (0..new.len()).rev() {
            let index = old_index * width + new_index;
            lcs[index] = if old[old_index] == new[new_index] {
                1 + lcs[(old_index + 1) * width + new_index + 1]
            } else {
                lcs[(old_index + 1) * width + new_index].max(lcs[old_index * width + new_index + 1])
            };
        }
    }
    let (mut old_index, mut new_index) = (0usize, 0usize);
    let mut additions = Vec::new();
    let mut deletions = Vec::new();
    while old_index < old.len() && new_index < new.len() {
        if old[old_index] == new[new_index] {
            old_index += 1;
            new_index += 1;
        } else if lcs[(old_index + 1) * width + new_index] >= lcs[old_index * width + new_index + 1]
        {
            deletions.push(old[old_index].clone());
            old_index += 1;
        } else {
            additions.push(new[new_index].clone());
            new_index += 1;
        }
    }
    deletions.extend_from_slice(&old[old_index..]);
    additions.extend_from_slice(&new[new_index..]);
    (additions, deletions)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::SourceId;

    #[test]
    fn an_already_relative_edit_path_is_never_rebased_a_second_time() {
        // Providers record a project path as a display label, which is not
        // guaranteed to be absolute. When both it and the edited path are
        // relative, stripping the prefix would drop a directory level and
        // attribute the edit to a file that was never touched.
        assert_eq!(
            repository_relative_path(
                Some(Path::new("crates/statsai-core")),
                PathBuf::from("crates/statsai-core/src/lib.rs")
            ),
            PathBuf::from("crates/statsai-core/src/lib.rs")
        );
        // An absolute path is still rebased onto the repository holding it.
        assert_eq!(
            repository_relative_path(Some(Path::new("/repo")), PathBuf::from("/repo/src/lib.rs")),
            PathBuf::from("src/lib.rs")
        );
        // One that lies outside the repository keeps its own path.
        assert_eq!(
            repository_relative_path(
                Some(Path::new("/repo")),
                PathBuf::from("/elsewhere/src/lib.rs")
            ),
            PathBuf::from("/elsewhere/src/lib.rs")
        );
    }

    #[test]
    fn parses_apply_patch_and_separates_test_code() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "*** Begin Patch\r\n*** Update File: src/lib.rs\r\n@@\r\n-old\r\n+new\r\n*** Add File: tests/new_test.rs\r\n+one\r\n+two\r\n*** End Patch\r\n",
        );
        assert_eq!(parsed.coverage, CoverageStatus::Complete);
        assert_eq!(parsed.edits.len(), 2);
        assert_eq!(parsed.edits[0].counts.source_additions, 1);
        assert_eq!(parsed.edits[0].counts.source_deletions, 1);
        assert_eq!(parsed.edits[1].counts.test_additions, 2);
    }

    #[test]
    fn repeated_identical_patch_sections_have_distinct_stable_trace_ids() {
        let source = SourceId("source".to_string());
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch\n";

        let first = parse_unified_patch(&context(&source), patch);
        let repeated = parse_unified_patch(&context(&source), patch);

        assert_eq!(first.edits.len(), 2);
        assert_ne!(first.edits[0].trace_edit_id, first.edits[1].trace_edit_id);
        assert_eq!(
            first
                .edits
                .iter()
                .map(|edit| edit.trace_edit_id.as_str())
                .collect::<Vec<_>>(),
            repeated
                .edits
                .iter()
                .map(|edit| edit.trace_edit_id.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn apply_patch_move_uses_the_destination_path() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "*** Begin Patch\n*** Update File: src/old.rs\n*** Move to: src/new.rs\n@@\n-old\n+new\n*** End Patch\n",
        );
        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/new.rs"));
    }

    #[test]
    fn apply_patch_delete_without_body_is_unavailable_instead_of_zero_lines() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "*** Begin Patch\n*** Delete File: src/old.rs\n*** End Patch\n",
        );

        assert!(parsed.edits.is_empty());
        assert_eq!(parsed.unsupported_sections, 1);
        assert_eq!(parsed.coverage, CoverageStatus::Unavailable);
    }

    #[test]
    fn non_code_patch_is_ignored_with_complete_coverage() {
        let source = SourceId("source".to_string());
        for patch in [
            "*** Begin Patch\n*** Update File: README.md\n@@\n-old\n+new\n*** End Patch\n",
            "*** Begin Patch\n*** Delete File: README.md\n*** End Patch\n",
        ] {
            let parsed = parse_unified_patch(&context(&source), patch);
            assert!(parsed.edits.is_empty());
            assert_eq!(parsed.unsupported_sections, 0);
            assert_eq!(parsed.coverage, CoverageStatus::Complete);
        }
    }

    #[test]
    fn apply_patch_keeps_literal_paths_that_look_like_git_prefixes() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "*** Begin Patch\n*** Update File: a/lib.rs\n@@\n-old\n+new\n*** End Patch\n",
        );

        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].relative_path, Path::new("a/lib.rs"));
    }

    #[test]
    fn git_patch_body_lines_are_not_mistaken_for_file_headers() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,0 +1,2 @@\n+++ still a body line\n+--- /dev/null\n",
        );

        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/lib.rs"));
        assert_eq!(parsed.edits[0].counts.source_additions, 2);
        assert_eq!(parsed.edits[0].mutation_kind, MutationKind::UnifiedPatch);
    }

    #[test]
    fn plain_unified_diffs_separate_files_without_git_headers() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "--- a/src/first.rs\n+++ b/src/first.rs\n@@ -1,0 +1,1 @@\n+one\n--- a/src/second.rs\n+++ b/src/second.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n",
        );

        assert_eq!(parsed.edits.len(), 2);
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/first.rs"));
        assert_eq!(parsed.edits[0].counts.source_additions, 1);
        assert_eq!(parsed.edits[0].counts.source_deletions, 0);
        assert_eq!(parsed.edits[1].relative_path, Path::new("src/second.rs"));
        assert_eq!(parsed.edits[1].counts.source_additions, 1);
        assert_eq!(parsed.edits[1].counts.source_deletions, 1);
    }

    #[test]
    fn plain_unified_diff_deletions_keep_the_source_side_path() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "--- a/src/gone.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n",
        );

        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/gone.rs"));
        assert_eq!(parsed.edits[0].counts.source_deletions, 2);
    }

    #[test]
    fn absolute_patch_paths_are_normalized_to_the_repository_root() {
        let source = SourceId("source".to_string());
        let repository = PathBuf::from("/repo");
        let mut context = context(&source);
        context.repository_path = Some(&repository);

        let parsed = parse_unified_patch(
            &context,
            "*** Begin Patch\n*** Update File: /repo/src/lib.rs\n@@\n-old\n+new\n*** End Patch\n",
        );

        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/lib.rs"));
    }

    #[test]
    fn structured_edit_uses_line_diff_and_handles_missing_final_newline() {
        let source = SourceId("source".to_string());
        let parsed = parse_structured_edit(
            &context(&source),
            Path::new("src/lib.rs"),
            "same\nold",
            "same\nnew",
            0,
        );
        assert_eq!(parsed.edits[0].counts.source_additions, 1);
        assert_eq!(parsed.edits[0].counts.source_deletions, 1);
    }

    #[test]
    fn overwrite_reports_lines_written_without_inventing_a_diff() {
        let source = SourceId("source".to_string());
        let parsed = parse_full_file_write(
            &context(&source),
            Path::new("src/lib.rs"),
            "one\ntwo\n",
            false,
        );
        assert_eq!(parsed.edits[0].counts.additions(), 0);
        assert_eq!(parsed.edits[0].counts.unclassified_lines_written, 2);
    }

    #[test]
    fn a_hunk_before_any_file_header_is_not_charged_to_the_next_file() {
        let source = SourceId("source".to_string());
        let parsed = parse_unified_patch(
            &context(&source),
            "@@ -1 +1 @@\n-old\n+new\n\
             diff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -1 +1 @@\n-x\n+y\n",
        );

        assert_eq!(parsed.edits.len(), 1, "only the headed section is measured");
        assert_eq!(parsed.edits[0].relative_path, Path::new("src/b.rs"));
        assert_eq!(parsed.edits[0].counts.source_additions, 1);
        assert_eq!(parsed.edits[0].counts.source_deletions, 1);
        assert_eq!(parsed.unsupported_sections, 1);
        assert_eq!(parsed.coverage, CoverageStatus::Partial);
    }
}
