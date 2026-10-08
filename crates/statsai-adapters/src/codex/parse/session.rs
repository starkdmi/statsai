use super::*;

pub(crate) const CODEX_SESSION_INDEX_FILE: &str = "session_index.jsonl";

/// Thread names from `session_index.jsonl`, as the provider shows them. Task
/// titles clean them further with the prompt rules; session names do not.
pub(crate) fn load_codex_thread_names(root: &Path) -> HashMap<String, String> {
    let index_path = root.join(CODEX_SESSION_INDEX_FILE);
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
/// The name is joined when the session is built, so a parent rename reaches
/// its sub-agents without re-reading their rollouts.
pub(crate) fn codex_subagent_session_name(
    value: &Value,
    session_id: &str,
) -> Option<statsai_core::SessionName> {
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
    Some(statsai_core::SessionName {
        local_session_id_hash: hash_text(session_id),
        title: statsai_core::provider_session_name(Some(label), 40)?,
        parent_local_session_id_hash: Some(hash_text(parent)),
    })
}

/// The directory, remote, and branch one Codex line records for its project.
#[derive(Debug, Clone, Default)]
pub(crate) struct CodexProjectInputs {
    project_path: Option<PathBuf>,
    repository_url: Option<String>,
    branch: Option<String>,
}

pub(crate) fn codex_project_inputs_from_value(value: &Value) -> CodexProjectInputs {
    let payload = value.get("payload");
    let git = payload.and_then(|payload| payload.get("git"));
    let git_text = |key: &str| {
        git.and_then(|git| git.get(key))
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(ToOwned::to_owned)
    };
    CodexProjectInputs {
        project_path: payload
            .and_then(|payload| payload.get("cwd"))
            .and_then(Value::as_str)
            .map(expand_home_path),
        repository_url: git_text("repository_url"),
        branch: git_text("branch"),
    }
}

pub(crate) fn codex_project_context_from_inputs(
    inputs: CodexProjectInputs,
    cache: &mut ProjectContextCache,
) -> Option<ProjectInfo> {
    resolve_project_context_cached(
        inputs.project_path,
        inputs.repository_url,
        inputs.branch,
        cache,
    )
}

pub(crate) fn codex_project_context_from_value(
    value: &Value,
    cache: &mut ProjectContextCache,
) -> Option<ProjectInfo> {
    codex_project_context_from_inputs(codex_project_inputs_from_value(value), cache)
}

/// `turn_context` repeats the session's cwd but never its `git` block, so a
/// turn in the directory `session_meta` named keeps the remote and branch it
/// recorded. A turn in another directory has no recorded branch.
pub(crate) fn codex_turn_context_project_from_value(
    value: &Value,
    session: &CodexProjectInputs,
    cache: &mut ProjectContextCache,
) -> Option<ProjectInfo> {
    let mut inputs = codex_project_inputs_from_value(value);
    if inputs.project_path.is_some() && inputs.project_path == session.project_path {
        inputs.repository_url = inputs
            .repository_url
            .or_else(|| session.repository_url.clone());
        inputs.branch = inputs.branch.or_else(|| session.branch.clone());
    }
    codex_project_context_from_inputs(inputs, cache)
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
