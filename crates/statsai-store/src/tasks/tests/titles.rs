use super::support::*;
use super::*;

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
