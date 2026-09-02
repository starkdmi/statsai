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

/// What one bounded transcript scan established about a session's project.
#[derive(Default)]
pub(crate) struct ClaudeTranscriptScan {
    pub(crate) project_paths: Vec<String>,
    /// The scan reached the end of the file inside its window.
    read_to_end: bool,
    /// At least one record was skipped without being understood.
    unread_record: bool,
}

impl ClaudeTranscriptScan {
    /// Whether this file provably names no project, rather than merely not
    /// having named one yet.
    fn is_conclusively_empty(&self) -> bool {
        self.project_paths.is_empty() && self.read_to_end && !self.unread_record
    }
}

pub(crate) fn claude_project_paths_from_transcripts(project_store: &Path) -> Option<Vec<PathBuf>> {
    let transcripts = collect_jsonl_files(project_store).ok()?;
    if transcripts.is_empty() {
        return Some(Vec::new());
    }
    let mut paths_by_transcript = HashMap::<PathBuf, ClaudeTranscriptScan>::new();
    for transcript in transcripts {
        let file = File::open(&transcript).ok()?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut scan = ClaudeTranscriptScan::default();
        for index in 0..CLAUDE_PROJECT_METADATA_SCAN_LINES {
            match read_bounded_jsonl_line(&mut reader, &mut line, MAX_JSONL_RECORD_BYTES).ok()? {
                BoundedLineRead::Eof => {
                    scan.read_to_end = true;
                    break;
                }
                BoundedLineRead::Oversized => {
                    // A record too large to read could be the one naming the
                    // project, so this file is never conclusively empty.
                    scan.unread_record = true;
                    continue;
                }
                BoundedLineRead::Complete => {}
            }
            if index + 1 == CLAUDE_PROJECT_METADATA_SCAN_LINES {
                // The window closed before the file did: metadata may follow.
                scan.unread_record = true;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                scan.unread_record = true;
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
                scan.project_paths.push(project_path.to_string());
            }
        }
        paths_by_transcript.insert(transcript, scan);
    }

    let mut project_paths = HashMap::new();
    let mut dismissible = Vec::new();
    for (transcript, scan) in &paths_by_transcript {
        if !scan.project_paths.is_empty() {
            for project_path in &scan.project_paths {
                insert_claude_project_path(&mut project_paths, project_path);
            }
            continue;
        }
        // A subagent records no `cwd` of its own and inherits its parent's. Its
        // usage belongs to a session that has to be placed, so a missing or
        // unplaced parent still blocks.
        if let Some(parent_transcript) =
            claude_parent_transcript_for_subagent(project_store, transcript)
        {
            let parent_paths = paths_by_transcript
                .get(&parent_transcript)
                .map(|parent| &parent.project_paths)
                .filter(|parent_paths| !parent_paths.is_empty())?;
            for project_path in parent_paths {
                insert_claude_project_path(&mut project_paths, project_path);
            }
            continue;
        }
        dismissible.push(scan);
    }

    // An unresolved transcript blocks the store unless two things hold at once:
    // the file was read to its end with every record parsed and still named no
    // project, and a sibling did name one. The first means it is empty rather
    // than merely unread -- a record too large to read, a record that did not
    // parse, or a file still going when the scan window closed may name a project
    // later, and each of those still fails closed. The second means the store's
    // scope is established by other sessions and its settings are checked; a
    // session abandoned before its first message leaves exactly such a file,
    // holding only interface state with no messages and no usage to attribute,
    // and vetoing on it cost attribution for every project in the store and for
    // the whole source. A store nothing resolves in remains unidentifiable.
    if dismissible
        .iter()
        .any(|scan| !scan.is_conclusively_empty() || project_paths.is_empty())
    {
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
