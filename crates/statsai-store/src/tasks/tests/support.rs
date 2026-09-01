use super::*;
pub(super) use chrono::TimeZone;
pub(super) use statsai_core::{
    EventId, ProjectInfo, SourceId, TaskVerdict, TASK_SPAN_SCHEMA_VERSION,
};

pub(super) fn test_span(
    title: &str,
    summary_preview: Option<&str>,
    branch_family: Option<&str>,
) -> SpanContext {
    test_span_with_title_source(title, summary_preview, branch_family, "test")
}

pub(super) fn test_span_with_title_source(
    title: &str,
    summary_preview: Option<&str>,
    branch_family: Option<&str>,
    title_source: &str,
) -> SpanContext {
    SpanContext::from(TaskSpan {
        schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
        span_id: TaskSpanId("span_test".to_string()),
        provider: "codex".to_string(),
        source_id: SourceId("source_test".to_string()),
        span_kind: "codex_task".to_string(),
        source_record_id: None,
        source_file_path_hash: None,
        summary_id: None,
        session_id: Some("session".to_string()),
        thread_id: None,
        title: title.to_string(),
        normalized_title: normalize_task_title(title),
        title_source: Some(title_source.to_string()),
        summary_preview: summary_preview.map(ToOwned::to_owned),
        todo_excerpt: None,
        issue_keys: Vec::new(),
        branch_family: branch_family.map(ToOwned::to_owned),
        project_bucket: "bucket".to_string(),
        project: None,
        git: None,
        usage: UsageCounts::default(),
        estimated_cost_usd: None,
        estimated_cost_micro_usd: None,
        event_count: 0,
        has_usage_evidence: false,
        total_messages: 0,
        user_messages: 0,
        assistant_messages: 0,
        developer_messages: 0,
        linked_event_ids: Vec::new(),
        confidence: Confidence::Medium,
        is_meta: task_title_is_generic(Some(title)),
        started_at: Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap(),
        ended_at: Some(Utc.with_ymd_and_hms(2026, 6, 30, 12, 5, 0).unwrap()),
        duration_seconds: Some(300),
    })
}

pub(super) fn test_span_with_options(
    span_id: &str,
    provider: &str,
    session_id: Option<&str>,
    project_bucket: &str,
    started_at: DateTime<Utc>,
    title: &str,
    summary_preview: Option<&str>,
) -> SpanContext {
    SpanContext::from(TaskSpan {
        schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
        span_id: TaskSpanId(span_id.to_string()),
        provider: provider.to_string(),
        source_id: SourceId(format!("source_{provider}")),
        span_kind: "task".to_string(),
        source_record_id: None,
        source_file_path_hash: None,
        summary_id: None,
        session_id: session_id.map(ToOwned::to_owned),
        thread_id: None,
        title: title.to_string(),
        normalized_title: normalize_task_title(title),
        title_source: Some("test".to_string()),
        summary_preview: summary_preview.map(ToOwned::to_owned),
        todo_excerpt: None,
        issue_keys: Vec::new(),
        branch_family: None,
        project_bucket: project_bucket.to_string(),
        project: None,
        git: None,
        usage: UsageCounts::default(),
        estimated_cost_usd: None,
        estimated_cost_micro_usd: None,
        event_count: 0,
        has_usage_evidence: false,
        total_messages: 0,
        user_messages: 0,
        assistant_messages: 0,
        developer_messages: 0,
        linked_event_ids: Vec::new(),
        confidence: Confidence::Medium,
        is_meta: task_title_is_generic(Some(title)),
        started_at,
        ended_at: Some(started_at + chrono::Duration::minutes(5)),
        duration_seconds: Some(300),
    })
}

pub(super) fn test_work_item(
    work_item_id: &str,
    anchor_span_id: &str,
    status: TaskStatus,
    confidence: Confidence,
    total_tokens: u64,
    ended_at: DateTime<Utc>,
) -> WorkItem {
    WorkItem {
        schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
        work_item_id: WorkItemId(work_item_id.to_string()),
        anchor_span_id: TaskSpanId(anchor_span_id.to_string()),
        tail_span_id: TaskSpanId(anchor_span_id.to_string()),
        project_bucket: "bucket".to_string(),
        title: format!("Title {work_item_id}"),
        normalized_title: format!("title {work_item_id}"),
        status,
        confidence,
        started_at: ended_at - chrono::Duration::minutes(5),
        ended_at,
        duration_seconds: Some(300),
        span_count: 1,
        event_count: 1,
        total_input_tokens: 0,
        total_cache_creation_tokens: 0,
        total_cache_read_tokens: 0,
        total_output_tokens: 0,
        total_reasoning_tokens: 0,
        total_tokens,
        estimated_cost_usd: None,
        estimated_cost_micro_usd: None,
        providers: vec!["codex".to_string()],
        issue_keys: Vec::new(),
        repo_label: None,
        branch_labels: Vec::new(),
        path_label: None,
        summary_preview: None,
        todo_excerpt: None,
        no_git: true,
        cross_provider: false,
        continuation_reasons: Vec::new(),
        review_reasons: Vec::new(),
    }
}

pub(super) fn test_git_project(branch_label: &str) -> ProjectInfo {
    ProjectInfo {
        project_id: "project-test".to_string(),
        project_label: Some("project-test".to_string()),
        repo_remote_hash: Some("repo-test".to_string()),
        repo_label: Some("owner/project-test".to_string()),
        branch_hash: Some(format!("branch-{branch_label}")),
        branch_label: Some(branch_label.to_string()),
        path_hash: Some("path-test".to_string()),
        path_label: Some("/tmp/project-test".to_string()),
    }
}

pub(super) fn test_task_bucket_snapshot(
    project_bucket: &str,
    span_id: &str,
    title: &str,
    started_at: DateTime<Utc>,
) -> TaskBucketSnapshot {
    let span = test_span_with_options(
        span_id,
        "codex",
        Some("session-a"),
        project_bucket,
        started_at,
        title,
        Some(title),
    )
    .span;
    let spans = vec![span];
    let (work_items, members) = derive_task_work_items(spans.clone(), &[]);
    TaskBucketSnapshot {
        project_bucket: project_bucket.to_string(),
        generated_at: started_at + chrono::Duration::minutes(1),
        applied_verification_cursor: None,
        work_items,
        members,
        spans,
    }
}
