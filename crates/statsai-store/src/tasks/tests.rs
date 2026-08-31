use super::*;
use chrono::TimeZone;
use statsai_core::{EventId, ProjectInfo, SourceId, TaskVerdict, TASK_SPAN_SCHEMA_VERSION};

fn test_span(
    title: &str,
    summary_preview: Option<&str>,
    branch_family: Option<&str>,
) -> SpanContext {
    test_span_with_title_source(title, summary_preview, branch_family, "test")
}

fn test_span_with_title_source(
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

fn test_span_with_options(
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

fn test_work_item(
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

fn test_git_project(branch_label: &str) -> ProjectInfo {
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

fn test_task_bucket_snapshot(
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

#[test]
fn derived_work_item_rounds_cost_after_aggregating_exact_micro_usd() {
    let started_at = Utc.with_ymd_and_hms(2026, 7, 25, 12, 0, 0).unwrap();
    let spans = (0..3)
        .map(|index| {
            let mut span = test_span_with_options(
                &format!("span-cost-{index}"),
                "codex",
                Some("session-cost"),
                "bucket-cost",
                started_at + chrono::Duration::minutes(index),
                "Implement exact task pricing",
                Some("Implement exact task pricing"),
            )
            .span;
            span.estimated_cost_usd = Some(0);
            span.estimated_cost_micro_usd = Some(2_250);
            span.event_count = 1;
            span.has_usage_evidence = true;
            span
        })
        .collect::<Vec<_>>();

    let (work_items, _) = derive_task_work_items(spans, &[]);

    assert_eq!(work_items.len(), 1);
    assert_eq!(work_items[0].estimated_cost_micro_usd, Some(6_750));
    assert_eq!(work_items[0].estimated_cost_usd, Some(1));
}

#[test]
fn chooses_branch_family_when_span_titles_are_only_generic() {
    let title = choose_work_item_title(&[test_span(
            "Review uncommitted changes",
            Some(">>> TRANSCRIPT DELTA START [288] tool exec_command result: Chunk ID: 84e62e Wall time: 1.0006 seconds Process running with session ID 32988 Original token count: 30 Output:"),
            Some("add project token tracking"),
        )]);
    assert_eq!(title, "Add project token tracking");
}

#[test]
fn replacing_task_bucket_snapshot_marks_existing_sync_state_dirty() {
    let store = Store::in_memory().expect("store");
    let initial_snapshot = test_task_bucket_snapshot(
        "bucket-a",
        "span-a",
        "Implement sync dirty tracking",
        Utc.with_ymd_and_hms(2026, 7, 6, 10, 0, 0).unwrap(),
    );
    store
        .replace_task_bucket_snapshot(&initial_snapshot)
        .expect("replace initial snapshot");
    store
        .record_task_bucket_snapshots_synced(
            "http",
            "https://example.invalid/api/sync/batches",
            "device-1",
            std::slice::from_ref(&initial_snapshot),
        )
        .expect("record synced snapshot");

    let clean_pending = store
        .pending_task_bucket_snapshots_for_sync(
            "http",
            "https://example.invalid/api/sync/batches",
            "device-1",
            false,
            None,
        )
        .expect("pending snapshots before replacement");
    assert!(clean_pending.is_empty());

    let updated_snapshot = test_task_bucket_snapshot(
        "bucket-a",
        "span-a",
        "Implement sync dirty tracking v2",
        Utc.with_ymd_and_hms(2026, 7, 6, 11, 0, 0).unwrap(),
    );
    store
        .replace_task_bucket_snapshot(&updated_snapshot)
        .expect("replace updated snapshot");

    let pending = store
        .pending_task_bucket_snapshots_for_sync(
            "http",
            "https://example.invalid/api/sync/batches",
            "device-1",
            false,
            None,
        )
        .expect("pending snapshots after replacement");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].project_bucket, "bucket-a");
    assert_eq!(pending[0].spans.len(), 1);
    assert_eq!(
        pending[0].spans[0].title,
        "Implement sync dirty tracking v2"
    );
}

#[test]
fn falls_back_to_unresolved_when_no_signal_survives() {
    let title = choose_work_item_title(&[test_span(
            "<environment_context> <cwd>/Users/example/workspace/project</cwd>",
            Some(">>> TRANSCRIPT DELTA START [288] tool exec_command result: Chunk ID: 84e62e Wall time: 1.0006 seconds Process running with session ID 32988 Original token count: 30 Output:"),
            None,
        )]);
    assert_eq!(title, "Unresolved work item");
    assert!(task_title_is_generic(Some(title.as_str())));
}

#[test]
fn prefers_cleaner_candidate_over_code_heavy_prompt_dump() {
    let title = choose_work_item_title(&[
            test_span(
                "Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1",
                Some("Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1"),
                None,
            ),
            test_span(
                "I have interesting data from asr 1k 125 steps with eval.",
                Some("I have interesting data from asr 1k 125 steps with eval."),
                None,
            ),
        ]);
    assert_eq!(
        title,
        "I have interesting data from asr 1k 125 steps with eval"
    );
}

#[test]
fn repeated_code_heavy_candidate_does_not_beat_cleaner_single_candidate() {
    let title = choose_work_item_title(&[
            test_span(
                "Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1",
                Some("Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1"),
                None,
            ),
            test_span(
                "Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1",
                Some("Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1"),
                None,
            ),
            test_span(
                "I have interesting data from asr 1k 125 steps with eval.",
                Some("I have interesting data from asr 1k 125 steps with eval."),
                None,
            ),
        ]);
    assert_eq!(
        title,
        "I have interesting data from asr 1k 125 steps with eval"
    );
}

#[test]
fn prefers_summary_preview_over_command_invocation_title() {
    let title = choose_work_item_title(&[test_span(
            "Command line invocation: /Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild -scheme SampleApp",
            Some("Investigate transition timing drift in SampleApp"),
            None,
        )]);
    assert_eq!(title, "Investigate transition timing drift in SampleApp");
}

#[test]
fn prefers_representative_summary_over_repeated_settings_banner() {
    let title = choose_work_item_title(&[
        test_span(
            "Build settings from command line",
            Some("Investigate native alignment drift"),
            None,
        ),
        test_span(
            "Build settings from command line",
            Some("Investigate native alignment drift"),
            None,
        ),
        test_span("Build settings from command line", None, None),
    ]);
    assert_eq!(title, "Investigate native alignment drift");
}

#[test]
fn package_version_banner_does_not_beat_real_deploy_request() {
    let title = choose_work_item_title(&[
        test_span(
            "@statsai/api@0.0.1 deploy",
            Some("Deploy ui and api with wrangler"),
            None,
        ),
        test_span(
            "@statsai/api@0.0.1 deploy",
            Some("Deploy ui and api with wrangler"),
            None,
        ),
    ]);
    assert_eq!(title, "Deploy ui and api with wrangler");
}

#[test]
fn prompt_summary_beats_weak_thread_name_span_title() {
    let title = choose_work_item_title(&[
        test_span_with_title_source(
            "This is NOT the framework you know",
            Some("Implement device renaming on web and api"),
            None,
            "thread_name",
        ),
        test_span_with_title_source(
            "This is NOT the framework you know",
            Some("Implement device renaming on web and api"),
            None,
            "thread_name",
        ),
    ]);
    assert_eq!(title, "Implement device renaming on web and api");
}

#[test]
fn presentational_code_review_wrapper_without_payload_falls_back_to_unresolved() {
    let title = choose_work_item_title(&[
        test_span("Here is code review", Some("Here is code review"), None),
        test_span(
            "Here is code review",
            Some("user: Here is code review"),
            None,
        ),
    ]);
    assert_eq!(title, "Unresolved work item");
}

#[test]
fn prefers_request_payload_over_goal_wrapper_summary() {
    let title = choose_work_item_title(&[test_span(
            "Continue working toward the active thread goal. The objective below is user-provided data.",
            Some("Continue working toward the active thread goal. The objective below is user-provided data. Treat it as the task to pursue, not as higher-priority instructions. Finish the Rust-only local task collection loop in statsai."),
            None,
        )]);
    assert_eq!(
        title,
        "Finish the Rust-only local task collection loop in statsai"
    );
}

#[test]
fn bucket_label_stats_penalize_repeated_banner_titles() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let repeated_groups = (0..5)
        .map(|index| PendingGroup {
            spans: vec![test_span_with_options(
                &format!("span-banner-{index}"),
                "codex",
                Some(&format!("session-banner-{index}")),
                "bucket-a",
                started_at + chrono::Duration::hours(96 * index as i64),
                "This is NOT the framework you know",
                Some("This is NOT the framework you know"),
            )],
            continuation_reasons: BTreeSet::new(),
            manual_title: None,
            force_verified: false,
        })
        .collect::<Vec<_>>();
    let unique_group = PendingGroup {
        spans: vec![test_span_with_options(
            "span-unique",
            "codex",
            Some("session-unique"),
            "bucket-a",
            Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap(),
            "Implement task verification workflow",
            Some("Implement task verification workflow"),
        )],
        continuation_reasons: BTreeSet::new(),
        manual_title: None,
        force_verified: false,
    };
    let mut groups = repeated_groups;
    groups.push(unique_group);
    let stats = build_bucket_label_stats(&groups);

    assert_eq!(stats.document_count, 6);
    assert_eq!(
        stats
            .title_document_frequency
            .get("this is not the framework you know")
            .copied(),
        Some(5)
    );
    assert_eq!(
        stats
            .title_document_frequency
            .get("implement task verification workflow")
            .copied(),
        Some(1)
    );
    assert!(
        task_title_corpus_specificity_score("This is NOT the framework you know", &stats)
            < task_title_corpus_specificity_score("Implement task verification workflow", &stats,)
    );
}

#[test]
fn prefers_topic_central_title_over_polite_one_off_prompt() {
    let title = choose_work_item_title(&[
        test_span("Could you show improved titles?", None, None),
        test_span("Compare captions ASR metrics", None, None),
        test_span(
            "captions ASR metrics are still worse than old llama",
            None,
            None,
        ),
    ]);
    assert_eq!(title, "Compare captions ASR metrics");
}

#[test]
fn live_like_qwen_metrics_cluster_avoids_prompt_style_title() {
    let title = choose_work_item_title(&[
        test_span("Could you show improved titles?", None, None),
        test_span(
            "What are results of ckpt 900? captions asr - vs other captions",
            None,
            None,
        ),
        test_span(
            "Maybe float16 instead of bfloat16 was the issue of new 4bit quants",
            None,
            None,
        ),
        test_span(
            "show me few ids from 900 repetitions, I'll check other rep pens",
            None,
            None,
        ),
    ]);
    assert_ne!(title, "show improved titles");
    assert!(
        title.contains("captions")
            || title.contains("ckpt")
            || title.contains("4bit")
            || title.contains("float16")
    );
}

#[test]
fn interruption_only_cluster_falls_back_to_unresolved() {
    let interruption = "The user interrupted the previous turn on purpose. Any running unified exec processes may still be running in the background. If any tools/commands were aborted, they may have partially executed.";
    let title = choose_work_item_title(&[
        test_span(interruption, Some(interruption), None),
        test_span(interruption, Some(interruption), None),
    ]);
    assert_eq!(title, "Unresolved work item");
}

#[test]
fn prefers_meaningful_candidate_over_tool_wrapper_title() {
    let title = choose_work_item_title(&[
            test_span(
                "I want to have ability to track tokens usage also by projects",
                Some("I want to have ability to track tokens usage also by projects"),
                None,
            ),
            test_span(
                "tool web_search call: {\"type\":\"open_page\",\"url\":\"https://base-ui.com/react/overview/releases/v1-4-0\"}",
                Some("tool web_search call: {\"type\":\"open_page\",\"url\":\"https://base-ui.com/react/overview/releases/v1-4-0\"}"),
                None,
            ),
        ]);
    assert_eq!(
        title,
        "I want to have ability to track tokens usage also by projects"
    );
}

#[test]
fn prefers_real_title_over_abstract_followups_and_tool_metadata() {
    let title = choose_work_item_title(&[
            test_span(
                "Improve, replace, or remove existing work as needed to satisfy the actual objective",
                Some("Improve, replace, or remove existing work as needed to satisfy the actual objective"),
                None,
            ),
            test_span(
                "\"tool_title\": \"Get Test List\"",
                Some("\"tool_title\": \"Get Test List\""),
                None,
            ),
            test_span("Fix CLI device login", Some("Fix CLI device login"), None),
        ]);
    assert_eq!(title, "Fix CLI device login");
}

#[test]
fn prefers_meaningful_candidate_over_single_cell_shell() {
    let title = choose_work_item_title(&[
        test_span("single cell, 8 only", Some("single cell, 8 only"), None),
        test_span(
            "I have interesting data from asr 1k 125 steps with eval",
            Some("I have interesting data from asr 1k 125 steps with eval"),
            None,
        ),
    ]);
    assert_eq!(
        title,
        "I have interesting data from asr 1k 125 steps with eval"
    );
}

#[test]
fn prefers_meaningful_candidate_over_url_dump_title() {
    let title = choose_work_item_title(&[
        test_span(
            "notebook https://proxy.example.invalid/session?token=eyJhbGciOiJIUzI1NiJ9",
            Some("notebook https://proxy.example.invalid/session?token=eyJhbGciOiJIUzI1NiJ9"),
            None,
        ),
        test_span(
            "Explore chapter-llama finetuning attempts",
            Some("Explore chapter-llama finetuning attempts"),
            None,
        ),
    ]);
    assert_eq!(title, "Explore chapter-llama finetuning attempts");
}

#[test]
fn prefers_meaningful_candidate_over_apply_patch_result_title() {
    let title = choose_work_item_title(&[
            test_span(
                "Success. Updated the following files: M /Users/example/workspace/project/ui/components/ui/sheet.tsx",
                Some("Success. Updated the following files: M /Users/example/workspace/project/ui/components/ui/sheet.tsx"),
                None,
            ),
            test_span(
                "Track tokens usage by project directory",
                Some("Track tokens usage by project directory"),
                None,
            ),
        ]);
    assert_eq!(title, "Track tokens usage by project directory");
}

#[test]
fn prefers_real_intent_over_repeated_metric_result_labels() {
    let title = choose_work_item_title(&[
            test_span(
                "we had Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34",
                Some(
                    "we had Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34",
                ),
                None,
            ),
            test_span(
                "coverage=1.000 (100/100) F1@0.5=67.10 F1@0.7=51.60 MAE=2.230",
                Some("coverage=1.000 (100/100) F1@0.5=67.10 F1@0.7=51.60 MAE=2.230"),
                None,
            ),
            test_span(
                "I want to choose the best adapters to average",
                Some("I want to choose the best adapters to average"),
                None,
            ),
        ]);
    assert_eq!(title, "I want to choose the best adapters to average");
}

#[test]
fn progress_output_cluster_falls_back_to_unresolved() {
    let title = choose_work_item_title(&[
        test_span(
            "[DEBUG] ChapterLlamaBoundaryFinder: Wrote stage1 transcript to /tmp/stage1.txt",
            Some("[DEBUG] ChapterLlamaBoundaryFinder: Wrote stage1 transcript to /tmp/stage1.txt"),
            None,
        ),
        test_span(
            "Generating train split: 10 examples [00:00, 674.63 examples/s]",
            Some("Generating train split: 10 examples [00:00, 674.63 examples/s]"),
            None,
        ),
    ]);
    assert_eq!(title, "Unresolved work item");
}

#[test]
fn metric_only_cluster_falls_back_to_unresolved() {
    let title = choose_work_item_title(&[
        test_span(
            "Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34",
            Some("Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34"),
            None,
        ),
        test_span(
            "coverage=1.000 (100/100) F1@0.5=67.10 F1@0.7=51.60 MAE=2.230",
            Some("coverage=1.000 (100/100) F1@0.5=67.10 F1@0.7=51.60 MAE=2.230"),
            None,
        ),
    ]);
    assert_eq!(title, "Unresolved work item");
}

#[test]
fn same_session_investigation_spans_stay_one_work_item() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Investigate rollout failure in task collection",
            Some("Investigate rollout failure in task collection"),
        ),
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(12),
            "Debug rollout failure in local task collection",
            Some("Debug rollout failure in local task collection"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 2);
    assert_eq!(work_items[0].span_count, 2);
}

#[test]
fn two_span_same_session_topic_shift_splits_without_distribution_stats() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Investigate SQLite migration failure in local task store",
            Some("Analyze sqlite migration failure and schema upgrade rollback behavior"),
        ),
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(18),
            "Design benchmark export dashboard for task review",
            Some("Plan benchmark export dashboard metrics and review workflow"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 2);
    assert_eq!(members.len(), 2);
    assert_eq!(work_items[0].span_count, 1);
    assert_eq!(work_items[1].span_count, 1);
}

#[test]
fn same_session_topic_shift_splits_on_cohesion_boundary() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Investigate SQLite migration failure in local task store",
            Some("Analyze sqlite migration failure and schema upgrade rollback behavior"),
        ),
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(18),
            "Design CLI task verification commands",
            Some("Plan accept reject split merge task verification commands"),
        ),
        test_span_with_options(
            "span-c",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(31),
            "Implement CLI task verification commands",
            Some("Implement accept reject split merge task verification output"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 2);
    assert_eq!(members.len(), 3);
    assert_eq!(work_items[0].span_count, 1);
    assert_eq!(work_items[1].span_count, 2);
}

#[test]
fn shared_issue_key_overrides_same_session_topic_boundary() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut span_a = test_span_with_options(
        "span-a",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at,
        "Stabilize menubar wake handling",
        Some("Fix tray wake handling and sleep resume edge cases"),
    );
    span_a.span.issue_keys = vec!["OPS-42".to_string()];
    let mut span_b = test_span_with_options(
        "span-b",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at + chrono::Duration::minutes(18),
        "Design benchmark JSON export gate",
        Some("Plan benchmark json export schema and gate metrics"),
    );
    span_b.span.issue_keys = vec!["OPS-42".to_string()];
    let mut span_c = test_span_with_options(
        "span-c",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at + chrono::Duration::minutes(30),
        "Implement benchmark JSON export gate",
        Some("Implement benchmark json export schema and gate metrics"),
    );
    span_c.span.issue_keys = vec!["OPS-42".to_string()];

    let (work_items, members, _) = build_work_items(vec![span_a, span_b, span_c], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 3);
    assert_eq!(work_items[0].span_count, 3);
}

#[test]
fn recurring_generic_review_shells_split_without_anchor() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Review uncommitted changes",
            Some("Review uncommitted changes"),
        ),
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-b"),
            "bucket-a",
            started_at + chrono::Duration::hours(96),
            "Review uncommitted changes",
            Some("Review uncommitted changes"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 2);
    assert_eq!(members.len(), 2);
}

#[test]
fn same_title_in_different_project_buckets_never_merges() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "repo:a|path:a|branch:a",
            started_at,
            "Implement task verification workflow",
            Some("Implement task verification workflow"),
        ),
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-a"),
            "repo:b|path:b|branch:b",
            started_at + chrono::Duration::minutes(10),
            "Implement task verification workflow",
            Some("Implement task verification workflow"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 2);
    assert_eq!(members.len(), 2);
}

#[test]
fn no_git_path_only_workspace_still_produces_work_item() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![test_span_with_options(
        "span-a",
        "codex",
        Some("session-a"),
        "repo:none|path:abc|branch:none",
        started_at,
        "Implement local task collection",
        Some("Implement local task collection"),
    )];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 1);
    assert_eq!(work_items[0].title, "Implement local task collection");
    assert!(work_items[0].no_git);
    assert_eq!(work_items[0].status, TaskStatus::NeedsReview);
}

#[test]
fn cross_provider_same_session_can_merge_but_stays_reviewable() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let contexts = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Implement benchmark reporting",
            Some("Implement benchmark reporting"),
        ),
        test_span_with_options(
            "span-b",
            "opencode",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(8),
            "Implement benchmark reporting",
            Some("Implement benchmark reporting"),
        ),
    ];

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 2);
    assert!(work_items[0].cross_provider);
    assert_eq!(work_items[0].status, TaskStatus::NeedsReview);
}

#[test]
fn git_anchored_work_item_with_event_evidence_stays_auto_high() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut context = test_span_with_options(
        "span-a",
        "claude_code",
        Some("session-a"),
        "bucket-a",
        started_at,
        "SwiftUI Paywall Sheet Race Condition Fix",
        Some("SwiftUI Paywall Sheet Race Condition Fix"),
    );
    context.span.project = Some(test_git_project("main"));
    context.span.linked_event_ids = vec![EventId("event-a".to_string())];
    context.span.usage = UsageCounts {
        input_tokens: Some(100),
        output_tokens: Some(20),
        ..UsageCounts::default()
    };

    let (work_items, members, _) = build_work_items(vec![context], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 1);
    assert_eq!(work_items[0].status, TaskStatus::Auto);
    assert_eq!(work_items[0].confidence, Confidence::High);
    assert_eq!(work_items[0].event_count, 1);
    assert_eq!(work_items[0].total_tokens, 120);
    assert!(!work_items[0].no_git);
    assert!(work_items[0].review_reasons.is_empty());
}

#[test]
fn repeated_banner_titles_with_real_usage_do_not_merge() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut contexts = Vec::new();
    for index in 0..5 {
        let timestamp = started_at + chrono::Duration::hours(96 * index as i64);
        let mut context = test_span_with_options(
            &format!("span-banner-{index}"),
            "codex",
            Some(&format!("session-banner-{index}")),
            "bucket-a",
            timestamp,
            "This is NOT the framework you know",
            Some("This is NOT the framework you know"),
        );
        context.span.project = Some(test_git_project("main"));
        context.span.linked_event_ids = vec![EventId(format!("event-banner-{index}"))];
        context.span.event_count = 1;
        context.span.has_usage_evidence = true;
        context.span.total_messages = 8;
        context.span.user_messages = 3;
        context.span.assistant_messages = 3;
        context.span.usage = UsageCounts {
            input_tokens: Some(100),
            output_tokens: Some(20),
            ..UsageCounts::default()
        };
        contexts.push(context);
    }

    let (work_items, members, _) = build_work_items(contexts, &[]);
    assert_eq!(work_items.len(), 5);
    assert_eq!(members.len(), 5);
    assert!(work_items.iter().all(|item| item.span_count == 1));
    assert!(work_items
        .iter()
        .all(|item| item.title == "This is NOT the framework you know"));
    assert!(work_items.iter().all(|item| item
        .review_reasons
        .contains(&"low_specificity_title".to_string())));
}

#[test]
fn git_anchored_work_item_without_event_evidence_needs_review_low() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut context = test_span_with_options(
        "span-a",
        "claude_code",
        Some("session-a"),
        "bucket-a",
        started_at,
        "SwiftUI Paywall Sheet Race Condition Fix",
        Some("SwiftUI Paywall Sheet Race Condition Fix"),
    );
    context.span.project = Some(test_git_project("main"));

    let (work_items, members, _) = build_work_items(vec![context], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 1);
    assert_eq!(work_items[0].status, TaskStatus::NeedsReview);
    assert_eq!(work_items[0].confidence, Confidence::Low);
    assert_eq!(work_items[0].event_count, 0);
    assert_eq!(work_items[0].total_tokens, 0);
    assert!(!work_items[0].no_git);
    assert!(work_items[0]
        .review_reasons
        .contains(&"no_usage_evidence".to_string()));
}

#[test]
fn session_control_item_without_event_evidence_is_rejected_meta() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut context = test_span_with_options(
        "span-a",
        "claude_code",
        Some("session-a"),
        "bucket-a",
        started_at,
        "Clearing Conversation History",
        Some("Clearing Conversation History"),
    );
    context.span.project = Some(test_git_project("main"));

    let (work_items, members, _) = build_work_items(vec![context], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 1);
    assert_eq!(work_items[0].status, TaskStatus::RejectedMeta);
    assert_eq!(work_items[0].confidence, Confidence::Low);
    assert_eq!(work_items[0].title, "Clearing Conversation History");
    assert!(work_items[0]
        .review_reasons
        .contains(&"no_usage_evidence".to_string()));
}

#[test]
fn low_volume_generic_exchange_is_rejected_meta() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut context = test_span_with_options(
        "span-a",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at,
        "Morning Greetings",
        Some("Say hi, nothing else"),
    );
    context.span.linked_event_ids = vec![EventId("event-a".to_string())];
    context.span.event_count = 1;
    context.span.has_usage_evidence = true;
    context.span.total_messages = 2;
    context.span.user_messages = 1;
    context.span.assistant_messages = 1;

    let (work_items, members, _) = build_work_items(vec![context], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 1);
    assert_eq!(work_items[0].status, TaskStatus::RejectedMeta);
    assert_eq!(work_items[0].confidence, Confidence::Low);
    assert!(work_items[0]
        .review_reasons
        .contains(&"low_signal_exchange".to_string()));
}

#[test]
fn repeated_low_volume_generic_shells_are_rejected_meta() {
    let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let mut morning = test_span_with_options(
        "span-a",
        "codex",
        Some("quota-session"),
        "bucket-a",
        started_at,
        "Morning Greetings",
        Some("Say hi, nothing else"),
    );
    morning.span.linked_event_ids = vec![EventId("event-a".to_string())];
    morning.span.event_count = 1;
    morning.span.has_usage_evidence = true;
    morning.span.total_messages = 2;
    morning.span.user_messages = 1;
    morning.span.assistant_messages = 1;

    let mut lunch = test_span_with_options(
        "span-b",
        "codex",
        Some("quota-session"),
        "bucket-a",
        started_at + chrono::Duration::hours(4),
        "Lunch Greetings",
        Some("Say hi, nothing else"),
    );
    lunch.span.linked_event_ids = vec![EventId("event-b".to_string())];
    lunch.span.event_count = 1;
    lunch.span.has_usage_evidence = true;
    lunch.span.total_messages = 2;
    lunch.span.user_messages = 1;
    lunch.span.assistant_messages = 1;

    let mut evening = test_span_with_options(
        "span-c",
        "codex",
        Some("quota-session"),
        "bucket-a",
        started_at + chrono::Duration::hours(8),
        "Evening Greetings",
        Some("Say hi, nothing else"),
    );
    evening.span.linked_event_ids = vec![EventId("event-c".to_string())];
    evening.span.event_count = 1;
    evening.span.has_usage_evidence = true;
    evening.span.total_messages = 2;
    evening.span.user_messages = 1;
    evening.span.assistant_messages = 1;

    let (work_items, members, _) = build_work_items(vec![morning, lunch, evening], &[]);
    assert_eq!(work_items.len(), 1);
    assert_eq!(members.len(), 3);
    assert_eq!(work_items[0].status, TaskStatus::RejectedMeta);
    assert_eq!(work_items[0].confidence, Confidence::Low);
    assert!(work_items[0]
        .review_reasons
        .contains(&"low_signal_exchange".to_string()));
}

#[test]
fn work_items_are_ordered_for_review_queue() {
    let store = Store::in_memory().expect("store");
    let ended_base = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
    let work_items = vec![
        test_work_item(
            "verified-high",
            "span-verified",
            TaskStatus::Verified,
            Confidence::High,
            999,
            ended_base + chrono::Duration::hours(5),
        ),
        test_work_item(
            "auto-low",
            "span-auto",
            TaskStatus::Auto,
            Confidence::Low,
            9999,
            ended_base + chrono::Duration::hours(4),
        ),
        test_work_item(
            "needs-medium",
            "span-medium",
            TaskStatus::NeedsReview,
            Confidence::Medium,
            100,
            ended_base + chrono::Duration::hours(3),
        ),
        test_work_item(
            "needs-low-earlier",
            "span-low-earlier",
            TaskStatus::NeedsReview,
            Confidence::Low,
            500,
            ended_base + chrono::Duration::hours(1),
        ),
        test_work_item(
            "needs-low-later",
            "span-low-later",
            TaskStatus::NeedsReview,
            Confidence::Low,
            500,
            ended_base + chrono::Duration::hours(2),
        ),
    ];
    let members = work_items
        .iter()
        .map(|item| WorkItemMember {
            work_item_id: item.work_item_id.clone(),
            span_id: item.anchor_span_id.clone(),
            ordinal: 0,
        })
        .collect::<Vec<_>>();

    store
        .insert_work_items_in_tx(&work_items, &members)
        .expect("insert work items");

    let ordered = store.work_items().expect("ordered work items");
    let ids = ordered
        .iter()
        .map(|item| item.work_item_id.0.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            "needs-low-later",
            "needs-low-earlier",
            "needs-medium",
            "auto-low",
            "verified-high",
        ]
    );
}

#[test]
fn resolve_task_verifications_keeps_latest_status_and_rename_per_anchor() {
    let created_at = Utc.with_ymd_and_hms(2026, 7, 1, 10, 0, 0).unwrap();
    let anchor_span_id = TaskSpanId("span-anchor".to_string());
    let work_item_id = WorkItemId("work-anchor".to_string());
    let reject = TaskVerification {
        schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
        verification_id: task_verification_id("reject", "status:span-anchor"),
        action_key: "status:span-anchor".to_string(),
        action: TaskVerificationAction::Reject {
            work_item_id: work_item_id.clone(),
            anchor_span_id: anchor_span_id.clone(),
            reason: TaskVerdict::Meta,
        },
        created_at,
        updated_at: created_at,
    };
    let rename = TaskVerification {
        schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
        verification_id: task_verification_id("rename", "rename:span-anchor"),
        action_key: "rename:span-anchor".to_string(),
        action: TaskVerificationAction::Rename {
            work_item_id,
            anchor_span_id,
            title: "Verified renamed task".to_string(),
        },
        created_at,
        updated_at: created_at + chrono::Duration::minutes(5),
    };

    let resolved = resolve_task_verifications(vec![reject, rename]);
    assert_eq!(resolved.len(), 2);
    assert!(matches!(
        resolved[0].action,
        TaskVerificationAction::Reject { .. }
    ));
    assert!(matches!(
        resolved[1].action,
        TaskVerificationAction::Rename { .. }
    ));
}

#[test]
fn merge_task_verification_canonicalizes_legacy_anchor_keys_before_insert() {
    let store = Store::in_memory().expect("store");
    let created_at = Utc.with_ymd_and_hms(2026, 7, 1, 10, 0, 0).unwrap();
    let anchor_span_id = TaskSpanId("span-anchor".to_string());
    let work_item_id = WorkItemId("work-anchor".to_string());
    let legacy_rename = TaskVerification {
        schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
        verification_id: TaskVerificationId("legacy-rename".to_string()),
        action_key: "anchor:span-anchor".to_string(),
        action: TaskVerificationAction::Rename {
            work_item_id: work_item_id.clone(),
            anchor_span_id: anchor_span_id.clone(),
            title: "Legacy rename".to_string(),
        },
        created_at,
        updated_at: created_at,
    };
    let payload = serde_json::to_string(&legacy_rename).expect("legacy payload");
    store
        .conn
        .execute(
            r#"
                INSERT INTO task_verifications (
                  verification_id, action_kind, action_key, updated_at, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5)
                "#,
            rusqlite::params![
                &legacy_rename.verification_id.0,
                legacy_rename.action.action_kind(),
                &legacy_rename.action_key,
                legacy_rename.updated_at.to_rfc3339(),
                &payload,
            ],
        )
        .expect("insert legacy rename");

    let legacy_reject = TaskVerification {
        schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
        verification_id: TaskVerificationId("legacy-reject".to_string()),
        action_key: "anchor:span-anchor".to_string(),
        action: TaskVerificationAction::Reject {
            work_item_id,
            anchor_span_id,
            reason: TaskVerdict::Meta,
        },
        created_at: created_at + chrono::Duration::minutes(1),
        updated_at: created_at + chrono::Duration::minutes(1),
    };

    assert!(store
        .merge_task_verification(&legacy_reject)
        .expect("merge legacy reject"));

    let stored = store.task_verifications().expect("task verifications");
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().any(|verification| {
        matches!(verification.action, TaskVerificationAction::Rename { .. })
            && verification.action_key == "anchor:span-anchor"
    }));
    assert!(stored.iter().any(|verification| {
        matches!(verification.action, TaskVerificationAction::Reject { .. })
            && verification.action_key == "status:span-anchor"
    }));
}

#[test]
fn manual_split_preservation_uses_explicit_right_boundary() {
    let started_at = Utc.with_ymd_and_hms(2026, 7, 2, 10, 0, 0).unwrap();
    let span_a = test_span_with_options(
        "span-a",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at,
        "Implement task benchmark reporting",
        Some("Implement task benchmark reporting"),
    );
    let span_x = test_span_with_options(
        "span-x",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at + chrono::Duration::minutes(1),
        "Implement task benchmark reporting",
        Some("Implement task benchmark reporting"),
    );
    let span_b = test_span_with_options(
        "span-b",
        "codex",
        Some("session-a"),
        "bucket-a",
        started_at + chrono::Duration::minutes(2),
        "Implement task benchmark reporting",
        Some("Implement task benchmark reporting"),
    );
    let predicted_assignments = HashMap::from([
        ("span-a".to_string(), "work-left".to_string()),
        ("span-x".to_string(), "work-right".to_string()),
        ("span-b".to_string(), "work-left".to_string()),
    ]);
    let verification = TaskVerification {
        schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
        verification_id: task_verification_id("split", "split:span-a:span-b"),
        action_key: "split:span-a:span-b".to_string(),
        action: TaskVerificationAction::Split {
            after_span_id: TaskSpanId("span-a".to_string()),
            before_span_id: Some(TaskSpanId("span-b".to_string())),
            left_title: None,
            right_title: None,
        },
        created_at: started_at,
        updated_at: started_at,
    };

    assert!(!manual_constraints_preserved(
        &predicted_assignments,
        &[span_a.span, span_x.span, span_b.span],
        &[verification],
    ));
}

#[test]
fn localized_rebuild_deletes_layouts_reached_by_merged_ranges() {
    let store = Store::in_memory().expect("store");
    let started_at = Utc.with_ymd_and_hms(2026, 7, 2, 11, 0, 0).unwrap();
    let bucket = "bucket-a".to_string();
    let spans = vec![
        test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            &bucket,
            started_at,
            "Alpha payments cleanup",
            Some("Alpha payments cleanup"),
        )
        .span,
        test_span_with_options(
            "span-b",
            "codex",
            Some("session-b"),
            &bucket,
            started_at + chrono::Duration::minutes(10),
            "Vector search benchmark",
            Some("Vector search benchmark"),
        )
        .span,
        test_span_with_options(
            "span-c",
            "codex",
            Some("session-c"),
            &bucket,
            started_at + chrono::Duration::minutes(20),
            "Kernel tuning audit",
            Some("Kernel tuning audit"),
        )
        .span,
        test_span_with_options(
            "span-d",
            "codex",
            Some("session-d"),
            &bucket,
            started_at + chrono::Duration::minutes(30),
            "Latency regression report",
            Some("Latency regression report"),
        )
        .span,
        test_span_with_options(
            "span-e",
            "codex",
            Some("session-e"),
            &bucket,
            started_at + chrono::Duration::minutes(40),
            "Schema export polish",
            Some("Schema export polish"),
        )
        .span,
    ];
    store.upsert_task_spans(&spans).expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild without merge");

    let initial = store.work_items().expect("initial work items");
    assert_eq!(initial.len(), 5);
    let left = initial
        .iter()
        .find(|item| item.anchor_span_id.0 == "span-a")
        .expect("left work item");
    let right = initial
        .iter()
        .find(|item| item.anchor_span_id.0 == "span-e")
        .expect("right work item");
    store
        .upsert_task_verification(TaskVerificationAction::Merge {
            left_work_item_id: left.work_item_id.clone(),
            right_work_item_id: right.work_item_id.clone(),
            left_anchor_span_id: TaskSpanId("span-a".to_string()),
            right_anchor_span_id: TaskSpanId("span-e".to_string()),
            title: Some("Merged endpoint work".to_string()),
        })
        .expect("merge verification");
    store
        .rebuild_all_task_work_items()
        .expect("rebuild merged layouts");

    let merged = store.work_items().expect("merged work items");
    assert_eq!(merged.len(), 4);

    let report = store
        .rebuild_task_work_items_for_changes_report(
            &BTreeSet::from([bucket.clone()]),
            &BTreeSet::from(["span-a".to_string()]),
            &[],
        )
        .expect("localized rebuild after endpoint merge");
    assert_eq!(report.work_items_deleted, 4);
    assert_eq!(report.work_items_rebuilt, 4);
    assert_eq!(report.touched_span_count, 5);

    let after = store
        .work_items()
        .expect("work items after localized rebuild");
    assert_eq!(after.len(), 4);
    let members = store.work_item_members_map().expect("member map");
    assert_eq!(members.len(), 5);
    assert_eq!(members.values().cloned().collect::<HashSet<_>>().len(), 4);
    assert!(members.contains_key("span-d"));
}
