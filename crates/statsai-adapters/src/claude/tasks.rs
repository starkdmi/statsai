use super::{ClaudeSessionProjectMetadata, ClaudeTaskEntry};
use crate::{file_modified_timestamp, resolve_project_context, timestamp_from_scalar};
use chrono::Utc;
use serde_json::Value;
use statsai_core::{
    canonical_display, expand_home_path, summarize_task_text, task_title_from_prompt,
    task_title_is_generic,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub(crate) fn load_claude_session_projects(
    projects_root: &Path,
) -> HashMap<String, ClaudeSessionProjectMetadata> {
    let mut projects = HashMap::new();
    if !projects_root.exists() {
        return projects;
    }

    for entry in WalkDir::new(projects_root).follow_links(false) {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_file() || entry.file_name() != "sessions-index.json" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(project_store_root) = entry.path().parent() {
            let original_path = value
                .get("originalPath")
                .and_then(Value::as_str)
                .map(expand_home_path)
                .or_else(|| {
                    value
                        .get("entries")
                        .and_then(Value::as_array)
                        .and_then(|entries| entries.first())
                        .and_then(|item| item.get("projectPath"))
                        .and_then(Value::as_str)
                        .map(expand_home_path)
                });
            if let Some(project_path) = original_path {
                projects.insert(
                    canonical_display(project_store_root),
                    ClaudeSessionProjectMetadata {
                        project_path: Some(project_path),
                        git_branch: None,
                    },
                );
            }
        }
        let Some(entries) = value.get("entries").and_then(Value::as_array) else {
            continue;
        };
        for item in entries {
            let Some(full_path) = item.get("fullPath").and_then(Value::as_str) else {
                continue;
            };
            let metadata = ClaudeSessionProjectMetadata {
                project_path: item
                    .get("projectPath")
                    .and_then(Value::as_str)
                    .map(expand_home_path),
                git_branch: item
                    .get("gitBranch")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            };
            let full_path = Path::new(full_path);
            projects.insert(canonical_display(full_path), metadata.clone());
            if full_path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                projects.insert(canonical_display(&full_path.with_extension("")), metadata);
            }
        }
    }

    projects
}

pub(crate) fn load_claude_task_entries(projects_root: &Path) -> Vec<ClaudeTaskEntry> {
    let mut entries_out = Vec::new();
    if !projects_root.exists() {
        return entries_out;
    }

    for entry in WalkDir::new(projects_root).follow_links(false) {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_file() || entry.file_name() != "sessions-index.json" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let original_path = value
            .get("originalPath")
            .and_then(Value::as_str)
            .map(expand_home_path);
        let Some(entries) = value.get("entries").and_then(Value::as_array) else {
            continue;
        };
        for item in entries {
            let Some(session_id) = item.get("sessionId").and_then(Value::as_str) else {
                continue;
            };
            let summary = item
                .get("summary")
                .and_then(Value::as_str)
                .and_then(|value| summarize_task_text(Some(value), 220));
            let first_prompt = item
                .get("firstPrompt")
                .and_then(Value::as_str)
                .and_then(|value| summarize_task_text(Some(value), 220));
            let title = if summary
                .as_deref()
                .is_some_and(|value| !task_title_is_generic(Some(value)))
            {
                summary
                    .as_deref()
                    .and_then(|value| summarize_task_text(Some(value), 90))
            } else {
                task_title_from_prompt(first_prompt.as_deref()).or_else(|| {
                    summary
                        .as_deref()
                        .and_then(|value| summarize_task_text(Some(value), 90))
                })
            };
            let title_source = if summary
                .as_deref()
                .is_some_and(|value| !task_title_is_generic(Some(value)))
            {
                "summary"
            } else if first_prompt.is_some() {
                "first_prompt"
            } else {
                "summary"
            };
            let project = item
                .get("projectPath")
                .and_then(Value::as_str)
                .map(expand_home_path)
                .or_else(|| original_path.clone())
                .and_then(|project_path| {
                    resolve_project_context(
                        Some(project_path),
                        None,
                        item.get("gitBranch")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                    )
                });
            let started_at = item
                .get("created")
                .and_then(timestamp_from_scalar)
                .or_else(|| file_modified_timestamp(entry.path()))
                .unwrap_or_else(Utc::now);
            let ended_at = item
                .get("modified")
                .and_then(timestamp_from_scalar)
                .or_else(|| item.get("created").and_then(timestamp_from_scalar))
                .or_else(|| file_modified_timestamp(entry.path()))
                .unwrap_or(started_at);
            let summary_preview = first_prompt
                .clone()
                .filter(|prompt| title.as_deref() != Some(prompt.as_str()))
                .or(summary.clone());
            entries_out.push(ClaudeTaskEntry {
                session_id: session_id.to_string(),
                title,
                title_source,
                summary_preview,
                project,
                started_at,
                ended_at,
                source_path: item
                    .get("fullPath")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
            });
        }
    }

    entries_out
}
