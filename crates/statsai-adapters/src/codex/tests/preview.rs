use super::*;

#[test]
fn codex_task_title_extracts_user_request_from_transcript_delta_prompt() {
    let (title, title_source, is_meta) = codex_task_title(
        None,
        Some(
            ">>> TRANSCRIPT DELTA START [167] user: Code review Found one actionable issue: \
             ::code-comment{title=\"[P2] Concurrent filter changes can overwrite each \
             other\" body=\"Each update derives from the last rendered searchParams\"}",
        ),
    );

    assert_eq!(title, "Code review");
    assert_eq!(title_source, "user_prompt");
    assert!(!is_meta);
}

#[test]
fn codex_task_title_rejects_tool_result_transcript_delta_prompt() {
    let (title, title_source, is_meta) = codex_task_title(
        None,
        Some(
            ">>> TRANSCRIPT DELTA START [288] tool exec_command result: Chunk ID: 84e62e \
             Wall time: 1.0006 seconds Process running with session ID 32988 Original \
             token count: 30 Output:",
        ),
    );

    assert_eq!(title, "Codex task");
    assert_eq!(title_source, "default");
    assert!(is_meta);
}

#[test]
fn codex_task_title_rejects_metric_report_prompt_without_intent() {
    let (title, title_source, is_meta) = codex_task_title(
        None,
        Some("Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34"),
    );

    assert_eq!(title, "Codex task");
    assert_eq!(title_source, "default");
    assert!(is_meta);
}

#[test]
fn codex_task_title_skips_instructional_preamble_and_keeps_request() {
    let (title, title_source, is_meta) = codex_task_title(
        None,
        Some(
            "This is NOT the Next.js you know. This version may differ from your training \
             data. Read the relevant guide before writing code. I need device renaming on \
             web and api.",
        ),
    );

    assert_eq!(title, "I need device renaming on web and api");
    assert_eq!(title_source, "user_prompt");
    assert!(!is_meta);
}

#[test]
fn codex_task_title_prefers_prompt_over_weak_thread_name_banner() {
    let (title, title_source, is_meta) = codex_task_title(
        Some("This is NOT the framework you know"),
        Some(
            "# This is NOT the framework you know\n\
             Read the relevant guide before writing code.\n\
             I need device renaming on web and api.",
        ),
    );

    assert_eq!(title, "I need device renaming on web and api");
    assert_eq!(title_source, "user_prompt");
    assert!(!is_meta);
}

#[test]
fn choose_best_task_preview_ignores_generic_wrapper_fallback() {
    let previews = vec![CodexPromptPreview {
        text: "Code review guidelines".to_string(),
        source: CodexPromptPreviewSource::ResponseItemUser,
    }];

    assert_eq!(choose_best_task_preview(&previews), None);
}

#[test]
fn codex_user_message_preview_skips_wrapped_response_item_user_content() {
    let value = serde_json::json!({
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [
                {
                    "type": "input_text",
                    "text": "<environment_context>\n<cwd>/tmp/example</cwd>\n</environment_context>"
                }
            ]
        }
    });

    let preview = codex_user_message_preview(&value).expect("candidate");
    assert_eq!(preview.source, CodexPromptPreviewSource::ResponseItemUser);
    assert!(materialize_codex_task_previews(&[preview]).is_empty());
}

#[test]
fn codex_json_string_prefix_decodes_unicode_without_losing_boundaries() {
    let line = r#"{"timestamp":"2026-06-03T09:36:25.000Z","type":"event_msg","payload":{"type":"user_message","message":"A\u2019éB"}}"#;

    assert_eq!(
        codex_event_user_message_preview_from_line(line, CODEX_TASK_PREVIEW_RAW_BYTES).as_deref(),
        Some("A’éB")
    );
}

#[test]
fn codex_message_content_preview_text_truncates_large_first_part() {
    let large = "é".repeat(70_000);
    let value = serde_json::json!([{
        "type": "input_text",
        "text": large,
    }]);

    let preview = codex_message_content_preview_text(Some(&value), CODEX_TASK_PREVIEW_RAW_BYTES)
        .expect("preview");
    let expected_source = "é".repeat(70_000);
    assert!(preview.len() <= CODEX_TASK_PREVIEW_RAW_BYTES);
    assert_eq!(
        preview,
        codex_prefix_at_char_boundary(expected_source.as_str(), CODEX_TASK_PREVIEW_RAW_BYTES)
    );
}
