use crate::{
    collect_jsonl_files, file_modified_at, parse_timestamp_value, read_bounded_jsonl_line,
    BoundedLineRead, MAX_JSONL_RECORD_BYTES,
};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;
use statsai_core::{
    canonical_display, expand_home_path, home_dir, LocationOrigin, SourceIdentityInference,
    VerifiedSourceObservation, VerifiedSourceState,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use url::Url;

pub(crate) const CLAUDE_SETTINGS_AUTH_OVERRIDE_KEYS: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
];

#[derive(Deserialize)]
pub(crate) struct ClaudeProfile {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<ClaudeOauthAccount>,
}

#[derive(Deserialize)]
pub(crate) struct ClaudeOauthAccount {
    #[serde(rename = "accountUuid")]
    account_uuid: Option<String>,
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
    #[serde(rename = "profileFetchedAt")]
    profile_fetched_at: Option<Value>,
}

pub(crate) fn claude_auth_snapshot(
    root: &Path,
    location_origin: &LocationOrigin,
) -> VerifiedSourceObservation {
    let managed_settings_root = claude_managed_settings_root();
    claude_auth_snapshot_with_probe_context(root, location_origin, managed_settings_root.as_deref())
}

pub(crate) fn claude_auth_dependency_paths(
    root: &Path,
    location_origin: &LocationOrigin,
) -> Vec<PathBuf> {
    let default_root = home_dir().map(|home| home.join(".claude"));
    let settings_root = if matches!(location_origin, LocationOrigin::Default) {
        default_root.as_deref().unwrap_or(root)
    } else {
        root
    };
    let mut paths = claude_profile_dependency_paths(root, location_origin, default_root.as_deref());
    paths.extend(claude_settings_paths(settings_root));
    if let Some(managed_settings_root) = claude_managed_settings_root() {
        paths.push(managed_settings_root);
    }
    if let Some(project_paths) = claude_project_paths_from_session_indexes(&root.join("projects")) {
        for project_path in project_paths {
            for project_settings_root in claude_project_settings_roots(&project_path) {
                if project_settings_root.is_dir() {
                    paths.push(project_settings_root);
                } else {
                    paths.extend(claude_settings_paths(&project_settings_root));
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

pub(crate) fn claude_profile_dependency_paths(
    root: &Path,
    location_origin: &LocationOrigin,
    default_root: Option<&Path>,
) -> Vec<PathBuf> {
    let nested_profile = root.join(".claude.json");
    let sibling_profile = root.parent().map(|parent| parent.join(".claude.json"));
    if matches!(location_origin, LocationOrigin::Default) {
        return vec![default_root
            .and_then(Path::parent)
            .map(|parent| parent.join(".claude.json"))
            .unwrap_or(nested_profile)];
    }
    if matches!(location_origin, LocationOrigin::Env) {
        return vec![nested_profile];
    }
    if default_root == Some(root) {
        return vec![sibling_profile.unwrap_or(nested_profile)];
    }
    if root.file_name().is_none_or(|name| name != ".claude") {
        return vec![nested_profile];
    }
    match sibling_profile {
        Some(sibling_profile) => vec![nested_profile, sibling_profile],
        None => vec![nested_profile],
    }
}

pub(crate) fn claude_verification_dependency_topology_changed(
    root: &Path,
    changed: &[PathBuf],
) -> bool {
    let projects_root = root.join("projects");
    changed.iter().any(|path| {
        if path == &projects_root {
            return true;
        }
        let Ok(relative) = path.strip_prefix(&projects_root) else {
            return false;
        };
        let mut components = relative.components();
        let Some(project_store_name) = components.next() else {
            return true;
        };
        let project_store = projects_root.join(project_store_name.as_os_str());
        let Some(child) = components.next() else {
            return true;
        };
        if components.next().is_none() && child.as_os_str() == "sessions-index.json" {
            return true;
        }
        path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            && !project_store.join("sessions-index.json").is_file()
    })
}

pub(crate) fn claude_auth_snapshot_with_probe_context(
    root: &Path,
    location_origin: &LocationOrigin,
    managed_settings_root: Option<&Path>,
) -> VerifiedSourceObservation {
    // Keep Claude identity discovery file-only: never invoke the provider CLI or contact a
    // local/remote service. Suppress automatic assignment whenever durable settings select
    // another credential or cannot be read conclusively.
    let default_root = home_dir().map(|home| home.join(".claude"));
    let settings_root = if matches!(location_origin, LocationOrigin::Default) {
        default_root.as_deref().unwrap_or(root)
    } else {
        root
    };
    let mut settings_block = None;
    if let Some(managed_settings_root) = managed_settings_root {
        settings_block = match claude_managed_settings_auth_override_in(managed_settings_root) {
            Some(ClaudeAuthOverrideProbe::Clear) => settings_block,
            Some(ClaudeAuthOverrideProbe::Blocked(block)) => {
                Some(merge_claude_auth_blocks(settings_block, block))
            }
            None => return claude_attribution_blocked(None),
        };
    }
    settings_block = match claude_source_settings_auth_override(root, settings_root) {
        Some(ClaudeAuthOverrideProbe::Clear) => settings_block,
        Some(ClaudeAuthOverrideProbe::Blocked(block)) => {
            Some(merge_claude_auth_blocks(settings_block, block))
        }
        None => return claude_attribution_blocked(None),
    };
    if let Some(block) = settings_block {
        return claude_attribution_blocked(block.blocked_since);
    }
    let settings_modified_at =
        claude_settings_modified_at(root, settings_root, managed_settings_root);
    claude_cached_profile_observation(
        root,
        location_origin,
        default_root.as_deref(),
        settings_modified_at,
    )
}

pub(crate) fn claude_attribution_blocked(
    blocked_since: Option<DateTime<Utc>>,
) -> VerifiedSourceObservation {
    VerifiedSourceObservation::AttributionBlocked { blocked_since }
}

pub(crate) fn claude_cached_profile_observation(
    root: &Path,
    location_origin: &LocationOrigin,
    default_root: Option<&Path>,
    settings_modified_at: Option<DateTime<Utc>>,
) -> VerifiedSourceObservation {
    let profile_path = match claude_profile_resolution(root, location_origin, default_root) {
        ClaudeProfileResolution::Path(path) => path,
        ClaudeProfileResolution::Missing => return VerifiedSourceObservation::Unavailable,
        ClaudeProfileResolution::Ambiguous => return claude_attribution_blocked(None),
    };
    claude_profile_snapshot(&profile_path)
        .map(Box::new)
        .map(|identity| VerifiedSourceObservation::Inferred {
            identity,
            basis: SourceIdentityInference::CachedLocalProfile,
            settings_modified_at,
        })
        .unwrap_or(VerifiedSourceObservation::Unavailable)
}

pub(crate) fn claude_settings_modified_at(
    root: &Path,
    settings_root: &Path,
    managed_settings_root: Option<&Path>,
) -> Option<DateTime<Utc>> {
    let mut paths = claude_settings_paths(settings_root).to_vec();
    if let Some(managed_root) = managed_settings_root {
        paths.push(managed_root.join("managed-settings.json"));
        let drop_ins = managed_root.join("managed-settings.d");
        paths.push(drop_ins.clone());
        if let Ok(entries) = std::fs::read_dir(drop_ins) {
            paths.extend(entries.filter_map(Result::ok).map(|entry| entry.path()));
        }
    }
    if let Some(project_paths) = claude_project_paths_from_session_indexes(&root.join("projects")) {
        for project_path in project_paths {
            for settings_root in claude_project_settings_roots(&project_path) {
                paths.push(settings_root.clone());
                paths.extend(claude_settings_paths(&settings_root));
            }
        }
    }
    paths
        .into_iter()
        .filter_map(|path| file_modified_at(&path))
        .max()
}

pub(crate) fn claude_profile_snapshot(profile_path: &Path) -> Option<VerifiedSourceState> {
    let file = File::open(profile_path).ok()?;
    let profile: ClaudeProfile = serde_json::from_reader(BufReader::new(file)).ok()?;
    let oauth_account = profile.oauth_account?;

    let provider_user_id = normalized_optional_string(oauth_account.account_uuid.as_deref());
    let email = normalized_optional_string(oauth_account.email_address.as_deref())
        .map(|email| email.to_ascii_lowercase());
    if provider_user_id.is_none() && email.is_none() {
        return None;
    }

    let profile_fetched_at = oauth_account
        .profile_fetched_at
        .as_ref()
        .and_then(claude_profile_timestamp);
    let verified_at = profile_fetched_at.or_else(|| file_modified_at(profile_path));

    Some(VerifiedSourceState {
        provider_user_id,
        email,
        account_label: None,
        plan_name: None,
        authenticated_at: verified_at,
        verified_at,
        subscription: None,
    })
}

#[cfg(test)]
pub(crate) fn claude_settings_have_auth_override(root: &Path) -> Option<bool> {
    let paths = claude_settings_paths(root);
    claude_auth_override_state_from_files(&paths, false).map(|state| state.has_auth_override())
}

pub(crate) fn claude_managed_settings_root() -> Option<PathBuf> {
    match std::env::consts::OS {
        "macos" => Some(PathBuf::from("/Library/Application Support/ClaudeCode")),
        "linux" => Some(PathBuf::from("/etc/claude-code")),
        "windows" => Some(PathBuf::from(r"C:\Program Files\ClaudeCode")),
        _ => None,
    }
}

pub(crate) fn claude_managed_settings_auth_override_in(
    root: &Path,
) -> Option<ClaudeAuthOverrideProbe> {
    let drop_ins = root.join("managed-settings.d");
    let entries = match std::fs::read_dir(drop_ins) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return claude_auth_override_state_from_files(
                &[root.join("managed-settings.json")],
                true,
            )
            .map(|state| state.probe());
        }
        Err(_) => return None,
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return None,
        };
        let file_name = entry.file_name();
        if file_name.to_string_lossy().starts_with('.')
            || entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("json")
        {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => return None,
        };
        if metadata.is_file() {
            paths.push(entry.path());
        }
    }
    paths.sort();
    paths.insert(0, root.join("managed-settings.json"));
    claude_auth_override_state_from_files(&paths, true).map(|state| state.probe())
}

#[cfg(test)]
pub(crate) fn claude_managed_settings_have_auth_override_in(root: &Path) -> Option<bool> {
    claude_managed_settings_auth_override_in(root)
        .map(|probe| matches!(probe, ClaudeAuthOverrideProbe::Blocked(_)))
}

pub(crate) fn claude_source_settings_auth_override(
    root: &Path,
    settings_root: &Path,
) -> Option<ClaudeAuthOverrideProbe> {
    let base_paths = claude_settings_paths(settings_root);
    let base_state = claude_auth_override_state_from_files(&base_paths, false)?;
    let projects_root = root.join("projects");
    let project_paths = claude_project_paths_from_session_indexes(&projects_root)?;
    if project_paths.is_empty() {
        return Some(base_state.probe());
    }
    let mut block = None;
    for project_path in &project_paths {
        let mut effective_state = base_state.clone();
        for project_settings_root in claude_project_settings_roots(project_path) {
            let project_settings = claude_settings_paths(&project_settings_root);
            claude_apply_auth_override_settings_files(
                &mut effective_state,
                &project_settings,
                false,
            )?;
        }
        if effective_state.has_auth_override() {
            block = Some(merge_claude_auth_blocks(
                block,
                effective_state.auth_block(),
            ));
        }
    }
    Some(match block {
        Some(block) => ClaudeAuthOverrideProbe::Blocked(block),
        None => ClaudeAuthOverrideProbe::Clear,
    })
}

#[cfg(test)]
pub(crate) fn claude_source_settings_have_auth_override(
    root: &Path,
    settings_root: &Path,
) -> Option<bool> {
    claude_source_settings_auth_override(root, settings_root)
        .map(|probe| matches!(probe, ClaudeAuthOverrideProbe::Blocked(_)))
}

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

    let mut project_paths = HashMap::new();
    for (transcript, transcript_project_paths) in &paths_by_transcript {
        let resolved_paths = if transcript_project_paths.is_empty() {
            let parent_transcript =
                claude_parent_transcript_for_subagent(project_store, transcript)?;
            paths_by_transcript.get(&parent_transcript)?
        } else {
            transcript_project_paths
        };
        if resolved_paths.is_empty() {
            return None;
        }
        for project_path in resolved_paths {
            insert_claude_project_path(&mut project_paths, project_path);
        }
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

#[derive(Clone, Copy, Default)]
pub(crate) struct ClaudeAuthOverrideValue {
    pub(crate) enabled: bool,
    pub(crate) evidence_at: Option<DateTime<Utc>>,
}

impl ClaudeAuthOverrideValue {
    pub(crate) fn apply(&mut self, value: &Value, evidence_at: Option<DateTime<Utc>>) {
        self.apply_enabled(json_value_enables_auth_override(Some(value)), evidence_at);
    }

    pub(crate) fn apply_enabled(&mut self, enabled: bool, evidence_at: Option<DateTime<Utc>>) {
        self.evidence_at = match (self.enabled, enabled, self.evidence_at) {
            (true, true, Some(current)) => {
                Some(evidence_at.map_or(current, |incoming| current.min(incoming)))
            }
            (true, true, None) => None,
            (false, true, _) => evidence_at,
            (_, false, _) => None,
        };
        self.enabled = enabled;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeAuthBlock {
    pub(crate) blocked_since: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaudeAuthOverrideProbe {
    Clear,
    Blocked(ClaudeAuthBlock),
}

pub(crate) fn merge_claude_auth_blocks(
    current: Option<ClaudeAuthBlock>,
    incoming: ClaudeAuthBlock,
) -> ClaudeAuthBlock {
    let Some(current) = current else {
        return incoming;
    };
    ClaudeAuthBlock {
        blocked_since: match (current.blocked_since, incoming.blocked_since) {
            (Some(current), Some(incoming)) => Some(current.min(incoming)),
            (None, _) | (_, None) => None,
        },
    }
}

#[derive(Clone, Default)]
pub(crate) struct ClaudeAuthOverrideState {
    pub(crate) api_key_helper: ClaudeAuthOverrideValue,
    pub(crate) policy_helper: ClaudeAuthOverrideValue,
    pub(crate) environment: [ClaudeAuthOverrideValue; CLAUDE_SETTINGS_AUTH_OVERRIDE_KEYS.len()],
}

impl ClaudeAuthOverrideState {
    pub(crate) fn apply(
        &mut self,
        settings: &Value,
        include_policy_helper: bool,
        evidence_at: Option<DateTime<Utc>>,
    ) {
        if let Some(value) = settings.get("apiKeyHelper") {
            self.api_key_helper.apply(value, evidence_at);
        }
        if include_policy_helper {
            if let Some(value) = settings.get("policyHelper") {
                self.policy_helper.apply(value, evidence_at);
            }
        }
        let Some(environment) = settings.get("env").and_then(Value::as_object) else {
            return;
        };
        for (index, name) in CLAUDE_SETTINGS_AUTH_OVERRIDE_KEYS.iter().enumerate() {
            if let Some(value) = environment.get(*name) {
                self.environment[index].apply_enabled(
                    claude_setting_enables_auth_override(name, value),
                    evidence_at,
                );
            }
        }
    }

    pub(crate) fn has_auth_override(&self) -> bool {
        self.auth_override_values().any(|value| value.enabled)
    }

    pub(crate) fn auth_block(&self) -> ClaudeAuthBlock {
        let mut blocked_since = None;
        for value in self.auth_override_values().filter(|value| value.enabled) {
            let Some(evidence_at) = value.evidence_at else {
                return ClaudeAuthBlock {
                    blocked_since: None,
                };
            };
            blocked_since = Some(blocked_since.map_or(evidence_at, |current: DateTime<Utc>| {
                current.min(evidence_at)
            }));
        }
        ClaudeAuthBlock { blocked_since }
    }

    pub(crate) fn probe(&self) -> ClaudeAuthOverrideProbe {
        if self.has_auth_override() {
            ClaudeAuthOverrideProbe::Blocked(self.auth_block())
        } else {
            ClaudeAuthOverrideProbe::Clear
        }
    }

    pub(crate) fn auth_override_values(&self) -> impl Iterator<Item = &ClaudeAuthOverrideValue> {
        std::iter::once(&self.api_key_helper)
            .chain(std::iter::once(&self.policy_helper))
            .chain(self.environment.iter())
    }
}

pub(crate) fn claude_auth_override_state_from_files(
    paths: &[PathBuf],
    include_policy_helper: bool,
) -> Option<ClaudeAuthOverrideState> {
    let mut state = ClaudeAuthOverrideState::default();
    claude_apply_auth_override_settings_files(&mut state, paths, include_policy_helper)?;
    Some(state)
}

pub(crate) fn claude_apply_auth_override_settings_files(
    state: &mut ClaudeAuthOverrideState,
    paths: &[PathBuf],
    include_policy_helper: bool,
) -> Option<()> {
    for path in paths {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let evidence_at = file
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .map(DateTime::<Utc>::from);
        let settings: Value = serde_json::from_reader(BufReader::new(file)).ok()?;
        state.apply(&settings, include_policy_helper, evidence_at);
    }
    Some(())
}

#[cfg(test)]
pub(crate) fn claude_settings_value_has_auth_override(settings: &Value) -> bool {
    let mut state = ClaudeAuthOverrideState::default();
    state.apply(settings, false, None);
    state.has_auth_override()
}

pub(crate) fn json_value_enables_auth_override(value: Option<&Value>) -> bool {
    match value {
        Some(Value::String(value)) => !value.trim().is_empty(),
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_i64().is_none_or(|value| value != 0),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
        Some(Value::Null) | None => false,
    }
}

pub(crate) fn claude_setting_enables_auth_override(name: &str, value: &Value) -> bool {
    if name == "ANTHROPIC_BASE_URL" {
        return claude_base_url_is_non_default(value);
    }
    if claude_setting_is_provider_selector(name) {
        return claude_provider_selector_is_enabled(value);
    }
    json_value_enables_auth_override(Some(value))
}

pub(crate) fn claude_setting_is_provider_selector(name: &str) -> bool {
    matches!(
        name,
        "CLAUDE_CODE_USE_BEDROCK"
            | "CLAUDE_CODE_USE_VERTEX"
            | "CLAUDE_CODE_USE_FOUNDRY"
            | "CLAUDE_CODE_USE_MANTLE"
            | "CLAUDE_CODE_USE_ANTHROPIC_AWS"
    )
}

pub(crate) fn claude_provider_selector_is_enabled(value: &Value) -> bool {
    match value {
        Value::String(value) => matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true"),
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_i64() == Some(1),
        Value::Array(_) | Value::Object(_) | Value::Null => false,
    }
}

pub(crate) fn claude_base_url_is_non_default(value: &Value) -> bool {
    let Some(value) = value.as_str() else {
        return json_value_enables_auth_override(Some(value));
    };
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let Ok(url) = Url::parse(value) else {
        return true;
    };
    url.scheme() != "https"
        || url.host_str() != Some("api.anthropic.com")
        || url.port_or_known_default() != Some(443)
        || !matches!(url.path(), "" | "/")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
}

pub(crate) fn normalized_optional_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) enum ClaudeProfileResolution {
    Path(PathBuf),
    Missing,
    Ambiguous,
}

pub(crate) fn claude_profile_resolution(
    root: &Path,
    location_origin: &LocationOrigin,
    default_root: Option<&Path>,
) -> ClaudeProfileResolution {
    let nested_profile = root.join(".claude.json");
    // Auto-discovered histories share Claude's standard home profile, including an XDG
    // history root. Only an environment-origin source proves CLAUDE_CONFIG_DIR layout.
    if matches!(location_origin, LocationOrigin::Default) {
        return ClaudeProfileResolution::Path(
            default_root
                .and_then(Path::parent)
                .map(|parent| parent.join(".claude.json"))
                .unwrap_or(nested_profile),
        );
    }
    if matches!(location_origin, LocationOrigin::Env) {
        return ClaudeProfileResolution::Path(nested_profile);
    }
    if default_root == Some(root) {
        return ClaudeProfileResolution::Path(
            root.parent()
                .map(|parent| parent.join(".claude.json"))
                .unwrap_or(nested_profile),
        );
    }
    if root.file_name().is_none_or(|name| name != ".claude") {
        return if nested_profile.is_file() {
            ClaudeProfileResolution::Path(nested_profile)
        } else {
            ClaudeProfileResolution::Missing
        };
    }

    let Some(parent) = root.parent() else {
        return ClaudeProfileResolution::Missing;
    };
    let sibling_profile = parent.join(".claude.json");
    match (nested_profile.is_file(), sibling_profile.is_file()) {
        (true, false) => ClaudeProfileResolution::Path(nested_profile),
        (false, true) => ClaudeProfileResolution::Path(sibling_profile),
        (true, true) => ClaudeProfileResolution::Ambiguous,
        (false, false) => ClaudeProfileResolution::Missing,
    }
}

pub(crate) fn claude_profile_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .and_then(|milliseconds| Utc.timestamp_millis_opt(milliseconds).single()),
        _ => parse_timestamp_value(value),
    }
}
