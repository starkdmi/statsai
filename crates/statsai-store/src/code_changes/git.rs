use super::*;

pub(crate) fn upsert_git_commit(
    conn: &rusqlite::Connection,
    commit: &GitCommitChange,
) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO code_git_commits
          (deduplication_id, repository_hash, commit_hash, committed_at, project_id, payload)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(deduplication_id) DO UPDATE SET
          committed_at = excluded.committed_at,
          project_id = excluded.project_id,
          payload = excluded.payload
        "#,
        params![
            &commit.deduplication_id,
            &commit.repository_hash,
            &commit.commit_hash,
            commit.committed_at.to_rfc3339(),
            &commit.project_id,
            serde_json::to_string(commit)?,
        ],
    )?;
    Ok(())
}

pub(crate) fn coverage_name(coverage: CoverageStatus) -> &'static str {
    match coverage {
        CoverageStatus::Complete => "complete",
        CoverageStatus::Partial => "partial",
        CoverageStatus::Unavailable => "unavailable",
    }
}

pub(crate) fn parse_coverage(value: &str) -> CoverageStatus {
    match value {
        "complete" => CoverageStatus::Complete,
        "partial" => CoverageStatus::Partial,
        _ => CoverageStatus::Unavailable,
    }
}

pub(crate) fn confidence_name(confidence: AttributionConfidence) -> &'static str {
    match confidence {
        AttributionConfidence::High => "high",
        AttributionConfidence::Medium => "medium",
    }
}

pub(crate) fn metric_kind_name(kind: CodeChangeMetricKind) -> &'static str {
    match kind {
        CodeChangeMetricKind::AgentEdit => "agent_edit",
        CodeChangeMetricKind::Committed => "committed",
        CodeChangeMetricKind::TraceMatchedCommitted => "trace_matched_committed",
    }
}
