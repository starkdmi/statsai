use super::*;

/// Thread names from `session_index.jsonl`, as the provider shows them. Task
/// titles clean them further with the prompt rules; session names do not.
pub(crate) fn load_codex_thread_names(root: &Path) -> HashMap<String, String> {
    let index_path = root.join("session_index.jsonl");
    let Ok(file) = File::open(&index_path) else {
        return HashMap::new();
    };
    let mut reader = BufReader::new(file);
    let mut names = HashMap::new();
    let mut line_bytes = Vec::new();
    while let Ok(line_status) =
        read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)
    {
        if line_status == BoundedLineRead::Eof {
            break;
        }
        if line_status == BoundedLineRead::Oversized {
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(session_id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(name) = statsai_core::provider_session_name(
            value.get("thread_name").and_then(Value::as_str),
            90,
        ) {
            names.insert(session_id.to_string(), name);
        }
    }
    names
}

pub(crate) fn codex_thread_titles_from_names(
    names: &HashMap<String, String>,
) -> HashMap<String, String> {
    names
        .iter()
        .filter_map(|(session_id, name)| {
            summarize_task_text(Some(name), 90).map(|title| (session_id.clone(), title))
        })
        .collect()
}

/// Codex never names its sub-agents. A spawned agent records its parent
/// thread, and a guardian review carries the parent's id as `session_id`, so
/// both borrow the parent's name, marked with the agent's nickname or kind.
pub(crate) fn codex_subagent_session_title(
    value: &Value,
    session_id: &str,
    thread_titles: &HashMap<String, String>,
) -> Option<String> {
    let payload = value.get("payload")?;
    let subagent = payload.pointer("/source/subagent")?;
    let (parent, label) = if let Some(spawn) = subagent.get("thread_spawn") {
        let label = ["agent_nickname", "agent_role"]
            .iter()
            .find_map(|key| spawn.get(*key).and_then(Value::as_str))
            .unwrap_or("sub-agent");
        (
            spawn.get("parent_thread_id").and_then(Value::as_str)?,
            label,
        )
    } else {
        let parent = payload
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|parent| *parent != session_id)?;
        let label = subagent
            .get("other")
            .and_then(Value::as_str)
            .unwrap_or("sub-agent");
        (parent, label)
    };
    let parent_title = thread_titles.get(parent)?;
    statsai_core::provider_session_name(Some(&format!("{parent_title} · {label}")), 90)
}

pub(crate) fn codex_project_context_from_value(
    value: &Value,
    cache: &mut ProjectContextCache,
) -> Option<ProjectInfo> {
    let payload = value.get("payload");
    let project_path = payload
        .and_then(|payload| payload.get("cwd"))
        .and_then(Value::as_str)
        .map(expand_home_path);
    let repository_url = payload
        .and_then(|payload| payload.get("git"))
        .and_then(|git| git.get("repository_url"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let branch = payload
        .and_then(|payload| payload.get("git"))
        .and_then(|git| git.get("branch"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    resolve_project_context_cached(project_path, repository_url, branch, cache)
}

pub(crate) fn codex_headless_usage_value(value: &Value) -> Option<&Value> {
    [
        value.get("usage"),
        value.pointer("/data/usage"),
        value.pointer("/result/usage"),
        value.pointer("/response/usage"),
        value.get("token_count"),
        value.pointer("/event_msg/token_count"),
    ]
    .into_iter()
    .flatten()
    .next()
}

pub(crate) fn session_raw_from_value(value: &Value) -> Option<String> {
    [
        value.get("session_id"),
        value.get("sessionId"),
        value.pointer("/message/sessionId"),
        value.pointer("/message/session_id"),
        value.pointer("/data/session_id"),
        value.pointer("/result/session_id"),
        value.pointer("/response/session_id"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .map(ToOwned::to_owned)
}
