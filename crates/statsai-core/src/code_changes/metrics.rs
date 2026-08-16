//! Aggregates edits, commits, and matches into publishable metrics.

use super::*;
use crate::hash_text;
use chrono::{DateTime, NaiveDate, Utc};
use std::collections::BTreeMap;

/// Metrics for one refresh together with the coverage they were built under.
///
/// Building can itself discover unmeasurable churn, so the effective trace
/// coverage is reported back rather than left at the caller's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeChangeMetricBuild {
    pub metrics: Vec<CodeChangeMetric>,
    pub trace_coverage: CoverageStatus,
}

pub fn build_code_change_metrics(
    now: DateTime<Utc>,
    device_id: &str,
    trace_edits: &[TraceEdit],
    git_scans: &[GitScan],
    matches: &[CodeChangeMatch],
    committed_metric_ids: &BTreeMap<String, String>,
    trace_coverage: CoverageStatus,
) -> Result<CodeChangeMetricBuild, CodeChangeMetricBuildError> {
    let git_coverage = git_scans
        .iter()
        .map(|scan| scan.coverage)
        .reduce(CoverageStatus::combine)
        .unwrap_or(CoverageStatus::Unavailable);
    let latest_reportable_day = max_reportable_day(now);
    let mut metrics = Vec::new();
    // Agent edits are the only unbounded input: one archive can hold hundreds
    // of thousands of them. They are published on the daily, project, and
    // repository dimensions the dashboard reads, so the metric count stays
    // proportional to observed days rather than to individual edits.
    let mut agent_edits =
        BTreeMap::<(NaiveDate, Option<String>, Option<String>), CodeLineCounts>::new();
    let scan_roots = canonical_scan_roots(git_scans);
    let mut canonical_paths = CanonicalPathCache::default();
    let mut skipped_edits = 0_u64;
    for edit in trace_edits {
        let Some(occurred_at) = edit.occurred_at else {
            continue;
        };
        let day = occurred_at.date_naive();
        if day > latest_reportable_day {
            skipped_edits = skipped_edits.saturating_add(1);
            continue;
        }
        let repository_hash = edit.repository_path.as_deref().and_then(|repository_path| {
            let canonical_repository_path = canonical_paths.resolve(repository_path).to_path_buf();
            repository_hash_for_trace(edit, git_scans, &scan_roots, &canonical_repository_path)
        });
        agent_edits
            .entry((day, edit.project_id.clone(), repository_hash))
            .or_default()
            .add(edit.counts);
    }
    // A clock-skewed edit is churn the archive recorded and this build declined
    // to publish. Leaving coverage untouched would let the surviving metrics
    // claim they describe the period completely.
    let trace_coverage = if skipped_edits > 0 {
        trace_coverage.combine(CoverageStatus::Partial)
    } else {
        trace_coverage
    };
    for ((day, project_id, repository_hash), counts) in agent_edits {
        metrics.push(CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: agent_edit_metric_id(device_id, day, &project_id, &repository_hash),
            device_id: device_id.to_string(),
            day,
            project_id,
            repository_hash,
            commit_hash: None,
            kind: CodeChangeMetricKind::AgentEdit,
            counts,
            attribution_confidence: None,
            trace_coverage,
            git_coverage,
        });
    }
    for scan in git_scans {
        for commit in &scan.commits {
            let metric_id = committed_metric_ids
                .get(&commit.deduplication_id)
                .ok_or(CodeChangeMetricBuildError::MissingCommittedMetricId)?;
            let mut counts = CodeLineCounts::default();
            for file in &commit.files {
                counts.add(file.counts);
            }
            metrics.push(CodeChangeMetric {
                schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
                metric_id: metric_id.clone(),
                device_id: device_id.to_string(),
                day: commit.committed_at.date_naive(),
                project_id: commit.project_id.clone(),
                repository_hash: Some(commit.repository_hash.clone()),
                commit_hash: Some(commit.commit_hash.clone()),
                kind: CodeChangeMetricKind::Committed,
                counts,
                attribution_confidence: None,
                trace_coverage,
                git_coverage,
            });
        }
    }
    // Indexed once: a match carries no project of its own, and scanning every
    // trace edit per match is quadratic on archives this size.
    let project_by_trace_edit = trace_edits
        .iter()
        .map(|edit| (edit.trace_edit_id.as_str(), edit.project_id.as_deref()))
        .collect::<BTreeMap<_, _>>();
    for matched in matches {
        if matched.committed_at.date_naive() > latest_reportable_day {
            continue;
        }
        metrics.push(CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: matched.match_id.clone(),
            device_id: device_id.to_string(),
            day: matched.committed_at.date_naive(),
            project_id: project_by_trace_edit
                .get(matched.trace_edit_id.as_str())
                .copied()
                .flatten()
                .map(ToOwned::to_owned),
            repository_hash: Some(matched.repository_hash.clone()),
            commit_hash: Some(matched.commit_hash.clone()),
            kind: CodeChangeMetricKind::TraceMatchedCommitted,
            counts: matched.counts,
            attribution_confidence: Some(matched.confidence),
            trace_coverage,
            git_coverage,
        });
    }
    metrics.sort_by(|left, right| left.metric_id.cmp(&right.metric_id));
    metrics.dedup_by(|left, right| left.metric_id == right.metric_id);
    Ok(CodeChangeMetricBuild {
        metrics,
        trace_coverage,
    })
}

/// Opaque, device-scoped identifier for one day of aggregated agent edits.
///
/// The dimensions are hashed rather than concatenated so a metric ID never
/// leaks a project or repository identity, and the record separator keeps a
/// value that contains the delimiter from colliding with a different split.
fn agent_edit_metric_id(
    device_id: &str,
    day: NaiveDate,
    project_id: &Option<String>,
    repository_hash: &Option<String>,
) -> String {
    hash_text(&format!(
        "agent-edit-day.v1\u{1e}{device_id}\u{1e}{day}\u{1e}{}\u{1e}{}",
        project_id.as_deref().unwrap_or_default(),
        repository_hash.as_deref().unwrap_or_default()
    ))
}
#[must_use]
pub fn aggregate_code_change_metrics(
    metrics: &[CodeChangeMetric],
) -> BTreeMap<(NaiveDate, CodeChangeMetricKind), CodeLineCounts> {
    let mut totals = BTreeMap::new();
    for metric in metrics {
        totals
            .entry((metric.day, metric.kind))
            .or_insert_with(CodeLineCounts::default)
            .add(metric.counts);
    }
    totals
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::SourceId;
    use chrono::Duration;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn clock_skewed_commits_are_left_unmeasured_instead_of_poisoning_the_batch() {
        let temp = TempDir::new().unwrap();
        run_test_git(temp.path(), &["init", "-q"]);
        run_test_git(temp.path(), &["config", "user.email", "test@example.com"]);
        run_test_git(temp.path(), &["config", "user.name", "Test"]);
        commit_test_file(
            temp.path(),
            "now.rs",
            "now\n",
            "current",
            "test@example.com",
            Utc::now(),
        );
        commit_test_file(
            temp.path(),
            "skewed.rs",
            "skewed\n",
            "clock skewed",
            "test@example.com",
            Utc::now() + Duration::days(30),
        );

        let scan = scan_local_git_repository(temp.path(), None).unwrap();

        assert_eq!(scan.commits.len(), 1);
        assert!(scan
            .commits
            .iter()
            .flat_map(|commit| &commit.files)
            .all(|file| file.relative_path != Path::new("skewed.rs")));
        assert_eq!(scan.coverage, CoverageStatus::Partial);
    }

    #[test]
    fn agent_edits_are_published_per_day_project_and_repository() {
        let source = SourceId("source".to_string());
        let edit_at = |timestamp: &str, path: &str| {
            let mut context = context(&source);
            context.occurred_at = Some(DateTime::parse_from_rfc3339(timestamp).unwrap().into());
            parse_full_file_write(&context, Path::new(path), "one\ntwo\n", true)
                .edits
                .remove(0)
        };
        let edits = vec![
            edit_at("2026-08-01T10:00:00Z", "src/first.rs"),
            edit_at("2026-08-01T18:00:00Z", "src/second.rs"),
            edit_at("2026-08-02T10:00:00Z", "src/third.rs"),
        ];

        let metrics = build_code_change_metrics(
            DateTime::parse_from_rfc3339("2026-08-03T00:00:00Z")
                .unwrap()
                .into(),
            "device",
            &edits,
            &[],
            &[],
            &BTreeMap::new(),
            CoverageStatus::Complete,
        )
        .unwrap();

        let agent_edits = metrics
            .metrics
            .iter()
            .filter(|metric| metric.kind == CodeChangeMetricKind::AgentEdit)
            .collect::<Vec<_>>();
        assert_eq!(agent_edits.len(), 2, "one metric per observed day");
        let first_day = agent_edits
            .iter()
            .find(|metric| metric.day == NaiveDate::from_ymd_opt(2026, 8, 1).unwrap())
            .expect("first day");
        assert_eq!(first_day.counts.source_additions, 4);
        assert!(agent_edits
            .iter()
            .all(|metric| metric.metric_id.len() == 64));
    }

    #[test]
    fn future_dated_agent_edits_are_not_published() {
        let source = SourceId("source".to_string());
        let mut context = context(&source);
        context.occurred_at = Some(
            DateTime::parse_from_rfc3339("2027-01-01T00:00:00Z")
                .unwrap()
                .into(),
        );
        let edits = parse_full_file_write(&context, Path::new("src/lib.rs"), "one\n", true).edits;

        let metrics = build_code_change_metrics(
            DateTime::parse_from_rfc3339("2026-08-03T00:00:00Z")
                .unwrap()
                .into(),
            "device",
            &edits,
            &[],
            &[],
            &BTreeMap::new(),
            CoverageStatus::Complete,
        )
        .unwrap();

        assert!(metrics.metrics.is_empty());
        assert_eq!(
            metrics.trace_coverage,
            CoverageStatus::Partial,
            "an edit the build declined to publish is unmeasured churn"
        );
    }

    #[test]
    fn a_skipped_future_edit_downgrades_coverage_for_the_edits_that_are_published() {
        let source = SourceId("source".to_string());
        let edit_at = |timestamp: &str, path: &str| {
            let mut context = context(&source);
            context.occurred_at = Some(DateTime::parse_from_rfc3339(timestamp).unwrap().into());
            parse_full_file_write(&context, Path::new(path), "one\n", true)
                .edits
                .remove(0)
        };
        let edits = vec![
            edit_at("2026-08-01T10:00:00Z", "src/current.rs"),
            edit_at("2027-01-01T10:00:00Z", "src/skewed.rs"),
        ];

        let build = build_code_change_metrics(
            DateTime::parse_from_rfc3339("2026-08-03T00:00:00Z")
                .unwrap()
                .into(),
            "device",
            &edits,
            &[],
            &[],
            &BTreeMap::new(),
            CoverageStatus::Complete,
        )
        .unwrap();

        assert_eq!(build.metrics.len(), 1);
        assert_eq!(build.trace_coverage, CoverageStatus::Partial);
        assert!(
            build
                .metrics
                .iter()
                .all(|metric| metric.trace_coverage == CoverageStatus::Partial),
            "the surviving metrics must not claim they describe the period completely"
        );
    }
}
