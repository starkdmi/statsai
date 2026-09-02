use super::*;

pub(crate) fn claude_project_paths_from_session_indexes(
    projects_root: &Path,
) -> Option<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(projects_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let mut project_paths = HashMap::new();

    for entry in entries {
        let entry = entry.ok()?;
        if !entry.metadata().ok()?.is_dir() {
            continue;
        }
        let project_store = entry.path();
        let index_path = project_store.join("sessions-index.json");
        let file = match File::open(index_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                for project_path in claude_project_paths_from_transcripts(&project_store)? {
                    insert_claude_project_path(
                        &mut project_paths,
                        project_path.to_string_lossy().as_ref(),
                    );
                }
                continue;
            }
            Err(_) => return None,
        };
        let index: Value = serde_json::from_reader(BufReader::new(file)).ok()?;
        let indexed_entries = index.get("entries").and_then(Value::as_array);
        let store_project_path = index
            .get("originalPath")
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .or_else(|| {
                indexed_entries.and_then(|entries| {
                    entries.iter().find_map(|item| {
                        item.get("projectPath")
                            .and_then(Value::as_str)
                            .filter(|path| !path.trim().is_empty())
                    })
                })
            })?;
        insert_claude_project_path(&mut project_paths, store_project_path);

        for project_path in indexed_entries
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("projectPath").and_then(Value::as_str))
            .filter(|path| !path.trim().is_empty())
        {
            insert_claude_project_path(&mut project_paths, project_path);
        }
    }

    Some(project_paths.into_values().collect())
}

pub(crate) const CLAUDE_PROJECT_METADATA_SCAN_LINES: usize = 64;

pub(crate) fn claude_project_paths_from_transcripts(project_store: &Path) -> Option<Vec<PathBuf>> {
    let transcripts = collect_jsonl_files(project_store).ok()?;
    if transcripts.is_empty() {
        return Some(Vec::new());
    }
    let mut paths_by_transcript = HashMap::<PathBuf, Vec<String>>::new();
    for transcript in transcripts {
        let file = File::open(&transcript).ok()?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut transcript_project_paths = Vec::new();
        for _ in 0..CLAUDE_PROJECT_METADATA_SCAN_LINES {
            match read_bounded_jsonl_line(&mut reader, &mut line, MAX_JSONL_RECORD_BYTES).ok()? {
                BoundedLineRead::Eof => break,
                BoundedLineRead::Oversized => continue,
                BoundedLineRead::Complete => {}
            }
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if let Some(project_path) = value
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|path| !path.trim().is_empty())
                .or_else(|| {
                    value
                        .get("projectPath")
                        .and_then(Value::as_str)
                        .filter(|path| !path.trim().is_empty())
                })
            {
                transcript_project_paths.push(project_path.to_string());
            }
        }
        paths_by_transcript.insert(transcript, transcript_project_paths);
    }

    // Fail closed on the *store*, not on each transcript. Claude Code names a
    // project store after the working directory of the sessions inside it, so a
    // transcript that never records one is covered by whatever its siblings
    // resolve to; it cannot introduce a project scope they do not already name.
    // Sessions that end before their first message leave a stub holding only
    // interface state -- `last-prompt`, `mode`, `permission-mode` -- with no
    // `cwd`, no messages, and no usage to attribute. Vetoing on that stub blocked
    // attribution for every project in the store, and with it the whole source.
    // A store that resolves to nothing at all is still unidentifiable and still
    // blocks, which is the case this rule exists for.
    let mut project_paths = HashMap::new();
    let mut unresolved_transcript = false;
    for (transcript, transcript_project_paths) in &paths_by_transcript {
        let resolved_paths = if transcript_project_paths.is_empty() {
            claude_parent_transcript_for_subagent(project_store, transcript)
                .and_then(|parent_transcript| paths_by_transcript.get(&parent_transcript))
                .filter(|parent_paths| !parent_paths.is_empty())
        } else {
            Some(transcript_project_paths)
        };
        let Some(resolved_paths) = resolved_paths else {
            unresolved_transcript = true;
            continue;
        };
        for project_path in resolved_paths {
            insert_claude_project_path(&mut project_paths, project_path);
        }
    }
    if project_paths.is_empty() && unresolved_transcript {
        return None;
    }

    Some(project_paths.into_values().collect())
}

pub(crate) fn claude_parent_transcript_for_subagent(
    project_store: &Path,
    transcript: &Path,
) -> Option<PathBuf> {
    let relative = transcript.strip_prefix(project_store).ok()?;
    let mut components = relative.components();
    let session_id = components.next()?;
    if components.next()?.as_os_str() != "subagents" {
        return None;
    }
    Some(
        project_store
            .join(session_id.as_os_str())
            .with_extension("jsonl"),
    )
}

pub(crate) fn insert_claude_project_path(
    project_paths: &mut HashMap<String, PathBuf>,
    value: &str,
) {
    let path = expand_home_path(value.trim());
    project_paths
        .entry(canonical_display(&path))
        .or_insert(path);
}

pub(crate) fn claude_project_settings_roots(project_path: &Path) -> Vec<PathBuf> {
    let Some(repository_root) = project_path
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
    else {
        return vec![project_path.join(".claude")];
    };

    let mut roots = Vec::new();
    for ancestor in project_path.ancestors() {
        roots.push(ancestor.join(".claude"));
        if ancestor == repository_root {
            break;
        }
    }
    roots.reverse();
    roots
}

pub(crate) fn claude_settings_paths(root: &Path) -> [PathBuf; 2] {
    [root.join("settings.json"), root.join("settings.local.json")]
}
