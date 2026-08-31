use super::*;

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
