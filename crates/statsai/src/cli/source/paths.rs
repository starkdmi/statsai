use super::*;

pub(crate) fn normalize_configured_source_path(provider: &str, path: &Path) -> Result<PathBuf> {
    let mut path = expand_cli_path(path)?;
    if provider_matches(provider, "claude_code")
        && path.file_name().is_some_and(|name| name == "projects")
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    if provider_matches(provider, "codex")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "sessions" | "archived_sessions"))
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    if provider_matches(provider, "opencode")
        && path.file_name().is_some_and(|name| name == "opencode.db")
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    if provider_matches(provider, "grok_build")
        && path.file_name().is_some_and(|name| name == "sessions")
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
}

pub(crate) fn expand_cli_path(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir().context("HOME is not set");
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return Ok(home_dir().context("HOME is not set")?.join(rest));
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("read current directory")?
        .join(path))
}

pub(crate) fn path_label_from_hashless_source(source: &SourceLocation) -> Option<String> {
    let home = home_dir()?;
    match (source.provider.as_str(), source.location_origin.clone()) {
        ("claude_code", LocationOrigin::Default) if source.path_hash.is_some() => {
            let a = home.join(".config/claude/projects");
            let b = home.join(".claude/projects");
            let hash = source.path_hash.as_ref()?;
            for path in [a, b] {
                if statsai_core::path_hash(&path) == *hash {
                    return Some(path.to_string_lossy().to_string());
                }
            }
            None
        }
        ("codex", LocationOrigin::Default) if source.path_hash.is_some() => {
            let root = home.join(".codex");
            let hash = source.path_hash.as_ref()?;
            if statsai_core::path_hash(&root) == *hash {
                return Some(root.to_string_lossy().to_string());
            }
            None
        }
        _ => None,
    }
}

pub(crate) fn sources_refer_to_same_location(
    left: &SourceLocation,
    right: &SourceLocation,
) -> bool {
    if left.source_kind != right.source_kind || !provider_matches(&left.provider, &right.provider) {
        return false;
    }
    if left.source_id == right.source_id
        || left
            .path_hash
            .as_deref()
            .zip(right.path_hash.as_deref())
            .is_some_and(|(left, right)| left == right)
    {
        return true;
    }
    match (comparable_source_path(left), comparable_source_path(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

pub(crate) fn dedupe_overlapping_sources(sources: Vec<SourceLocation>) -> Vec<SourceLocation> {
    sources
        .iter()
        .enumerate()
        .filter_map(|(index, source)| {
            let Some(source_path) = comparable_source_path(source) else {
                return Some(source.clone());
            };
            let shadowed = sources.iter().enumerate().any(|(other_index, other)| {
                if index == other_index || !provider_matches(&source.provider, &other.provider) {
                    return false;
                }
                let Some(other_path) = comparable_source_path(other) else {
                    return false;
                };
                if !provider_shadowing_covers_nested_source(source, &source_path, &other_path) {
                    return false;
                }
                other_path != source_path
                    && source_path.starts_with(&other_path)
                    && source_preference_rank(other) >= source_preference_rank(source)
            });
            (!shadowed).then(|| source.clone())
        })
        .collect()
}

pub(crate) fn comparable_source_path(source: &SourceLocation) -> Option<PathBuf> {
    let path = PathBuf::from(source.path_label.as_deref()?);
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

pub(crate) fn provider_shadowing_covers_nested_source(
    source: &SourceLocation,
    source_path: &Path,
    other_path: &Path,
) -> bool {
    match canonical_provider_name(&source.provider) {
        Some("claude_code") => true,
        Some("codex") => codex_source_path_is_covered_by_parent(other_path, source_path),
        _ => false,
    }
}

pub(crate) fn codex_source_path_is_covered_by_parent(
    parent_path: &Path,
    child_path: &Path,
) -> bool {
    if parent_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "sessions" | "archived_sessions"))
    {
        return child_path.starts_with(parent_path);
    }

    child_path.starts_with(parent_path.join("sessions"))
        || child_path.starts_with(parent_path.join("archived_sessions"))
}

pub(crate) fn source_preference_rank(source: &SourceLocation) -> u8 {
    match source.location_origin {
        LocationOrigin::Configured | LocationOrigin::Env => 3,
        LocationOrigin::Discovered => 2,
        LocationOrigin::Default => 1,
    }
}

pub(crate) fn provider_matches(left: &str, right: &str) -> bool {
    match (
        canonical_provider_name(left),
        canonical_provider_name(right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => left == right || left.replace('-', "_") == right || left.replace('_', "-") == right,
    }
}

pub(crate) fn canonical_provider(provider: &str) -> Result<String> {
    canonical_provider_name(provider)
        .map(str::to_string)
        .with_context(|| format!("unsupported provider {provider}"))
}

pub(crate) fn canonical_provider_name(provider: &str) -> Option<&'static str> {
    adapter_for_provider(provider).map(|adapter| adapter.provider())
}

pub(crate) fn persist_source_after_preview(store: &Store, source: &SourceLocation) -> Result<()> {
    store.upsert_source(source)
}

pub(crate) fn location_origin_label(origin: &LocationOrigin) -> &'static str {
    match origin {
        LocationOrigin::Default => "default",
        LocationOrigin::Configured => "configured",
        LocationOrigin::Env => "env",
        LocationOrigin::Discovered => "discovered",
    }
}

pub(crate) fn preview_path_label(source: &SourceLocation) -> String {
    source
        .path_label
        .as_deref()
        .map(abbreviate_home)
        .unwrap_or_else(|| "unknown".to_string())
}
