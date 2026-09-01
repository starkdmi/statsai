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

mod overrides;
mod projects;

pub(crate) use overrides::*;
pub(crate) use projects::*;

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
