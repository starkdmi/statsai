use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod keyring;
mod logout;
mod paths;
mod refresh;
mod session;
#[cfg(test)]
mod tests;

pub(crate) use keyring::*;
pub use logout::logout;
pub(crate) use paths::*;
pub use refresh::get_or_refresh_token;
pub(crate) use refresh::*;
pub use session::{login, login_snapshot, status, LoginSnapshot};

pub(crate) const DEFAULT_CLOUDFLARE_API_URL: &str = "https://api.statsai.dev";

pub(crate) const DEFAULT_CLOUDFLARE_WEB_URL: &str = "https://statsai.dev";

pub(crate) const AUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthCredentials {
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub api_base_url: Option<String>,
    #[serde(default)]
    pub cloudflare_refresh_token: Option<String>,
    #[serde(default)]
    pub cloudflare_refresh_expires_at_secs: u64,
    #[serde(default)]
    pub cloudflare_access_token: Option<String>,
    #[serde(default)]
    pub cloudflare_access_expires_at_secs: u64,
    #[serde(default)]
    pub device_id: Option<String>,
}

pub fn cloudflare_api_url() -> String {
    normalize_url(
        &std::env::var("STATSAI_API_URL")
            .unwrap_or_else(|_| DEFAULT_CLOUDFLARE_API_URL.to_string()),
        DEFAULT_CLOUDFLARE_API_URL,
    )
}

pub fn cloudflare_web_url() -> String {
    normalize_url(
        &std::env::var("STATSAI_WEB_URL")
            .unwrap_or_else(|_| DEFAULT_CLOUDFLARE_WEB_URL.to_string()),
        DEFAULT_CLOUDFLARE_WEB_URL,
    )
}

pub fn is_local_backend() -> bool {
    let api = cloudflare_api_url();
    url::Url::parse(&api)
        .ok()
        .is_some_and(|url| url_has_explicit_loopback_host(&url))
}

pub(crate) fn validate_credential_transport_url(value: &str, label: &str) -> Result<()> {
    let parsed = url::Url::parse(value).with_context(|| format!("parse {label} URL"))?;
    let secure = parsed.scheme() == "https"
        || (parsed.scheme() == "http" && url_has_explicit_loopback_host(&parsed));
    if !secure {
        bail!("{label} must use HTTPS or an explicit loopback IP address");
    }
    Ok(())
}

pub(crate) fn url_has_explicit_loopback_host(url: &url::Url) -> bool {
    url.host().is_some_and(|host| match host {
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
        url::Host::Domain(_) => false,
    })
}

pub(crate) fn remembered_auth_device_id_from_base(
    base: &Path,
    api_base_url: &str,
) -> Option<String> {
    let path = auth_device_id_path_for_api_base_url(base, api_base_url);
    let value = std::fs::read_to_string(path).ok()?;
    let value = value.trim();
    (!value.is_empty()).then_some(value.to_string())
}

pub(crate) fn preferred_auth_device_id(base: &Path, api_base_url: &str) -> String {
    preferred_auth_device_id_with_fallback(base, api_base_url, crate::default_device_id)
}

pub(crate) fn preferred_auth_device_id_with_fallback<F>(
    base: &Path,
    api_base_url: &str,
    fallback: F,
) -> String
where
    F: FnOnce() -> String,
{
    if let Some(device_id) = remembered_auth_device_id_from_base(base, api_base_url) {
        return device_id;
    }

    match auth_record_for_backend(base, api_base_url) {
        Ok(Some((_path, credentials))) => {
            if let Some(device_id) = credentials
                .device_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                remember_auth_device_id_in_base(base, api_base_url, device_id);
                return device_id.to_string();
            }
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!(
                "Warning: ignoring unreadable stored auth state while choosing a device ID: {error}"
            );
        }
    }

    fallback()
}

pub(crate) fn remember_auth_device_id(api_base_url: &str, device_id: &str) {
    remember_auth_device_id_in_base(&auth_base_dir(), api_base_url, device_id);
}

pub(crate) fn remember_auth_device_id_in_base(base: &Path, api_base_url: &str, device_id: &str) {
    let path = auth_device_id_path_for_api_base_url(base, api_base_url);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format!("{device_id}\n"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

pub(crate) fn append_collector_metadata(payload: &mut serde_json::Map<String, serde_json::Value>) {
    payload.insert(
        "platform".to_string(),
        serde_json::Value::String(std::env::consts::OS.to_string()),
    );
    payload.insert(
        "collectorVersion".to_string(),
        serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string()),
    );
}

pub(crate) fn token_refresh_request_payload(
    refresh_token: &str,
    rotation_id: &str,
) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "refreshToken".to_string(),
        serde_json::Value::String(refresh_token.to_string()),
    );
    payload.insert(
        "rotationId".to_string(),
        serde_json::Value::String(rotation_id.to_string()),
    );
    append_collector_metadata(&mut payload);
    serde_json::Value::Object(payload)
}

pub(crate) fn default_device_name() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "device".to_string());
    format!("{} ({})", host, std::env::consts::OS)
}

pub(crate) fn load_credentials_from_file(path: &Path) -> Result<AuthCredentials> {
    let file = std::fs::File::open(path)?;
    serde_json::from_reader(file).context("parse stored auth credentials")
}

pub(crate) fn auth_metadata_exists(path: &Path) -> Result<bool> {
    path.try_exists()
        .with_context(|| format!("inspect auth metadata {}", path.display()))
}

pub(crate) fn auth_record_candidate_exists(base: &Path, api_base_url: &str) -> Result<bool> {
    let api_base_url = normalize_url(api_base_url, DEFAULT_CLOUDFLARE_API_URL);
    let scoped_path = auth_path_for_api_base_url(base, &api_base_url);
    if auth_metadata_exists(&scoped_path)? {
        let credentials = load_credentials_from_file(&scoped_path)?;
        return Ok(credentials_match_backend(&credentials, &api_base_url));
    }

    let old_scoped_path = legacy_scoped_auth_path(base, &api_base_url);
    if old_scoped_path != scoped_path && auth_metadata_exists(&old_scoped_path)? {
        let credentials = load_credentials_from_file(&old_scoped_path)?;
        if credentials_match_backend(&credentials, &api_base_url) {
            return Ok(true);
        }
    }

    let legacy_path = legacy_auth_path(base);
    if legacy_path == scoped_path || !auth_metadata_exists(&legacy_path)? {
        return Ok(false);
    }

    let credentials = load_credentials_from_file(&legacy_path)?;
    Ok(credentials_match_backend(&credentials, &api_base_url)
        || (!has_cloudflare_session(&credentials)
            && api_base_url
                == normalize_url(DEFAULT_CLOUDFLARE_API_URL, DEFAULT_CLOUDFLARE_API_URL)))
}

pub(crate) fn auth_record_from_file(
    base: &Path,
    api_base_url: &str,
) -> Result<Option<(PathBuf, AuthCredentials)>> {
    auth_record_from_file_with_loader(base, api_base_url, load_credentials)
}

pub(crate) fn auth_record_from_file_with_loader(
    base: &Path,
    api_base_url: &str,
    credential_loader: impl Fn(&Path) -> Result<AuthCredentials>,
) -> Result<Option<(PathBuf, AuthCredentials)>> {
    let api_base_url = normalize_url(api_base_url, DEFAULT_CLOUDFLARE_API_URL);
    let path = auth_path_for_api_base_url(base, &api_base_url);
    if path.exists() {
        let credentials = load_credentials_from_file(&path)?;
        return Ok(
            credentials_match_backend(&credentials, &api_base_url).then_some((path, credentials))
        );
    }

    let old_scoped_path = legacy_scoped_auth_path(base, &api_base_url);
    if old_scoped_path.exists() && old_scoped_path != path {
        let stored_credentials = load_credentials_from_file(&old_scoped_path)?;
        if credentials_match_backend(&stored_credentials, &api_base_url) {
            let credentials = credential_loader(&old_scoped_path)?;
            if credentials_match_backend(&credentials, &api_base_url)
                && has_cloudflare_session(&credentials)
            {
                write_credentials(&path, &credentials)?;
                let _ = std::fs::remove_file(old_scoped_path);
                return Ok(Some((path, credentials)));
            }
        }
    }

    let legacy_path = legacy_auth_path(base);
    if !legacy_path.exists() || legacy_path == path {
        return Ok(None);
    }

    let credentials = credential_loader(&legacy_path)?;
    if has_cloudflare_session(&credentials)
        && credentials_match_backend(&credentials, &api_base_url)
    {
        write_credentials(&path, &credentials)?;
        let mut sanitized = credentials.clone();
        sanitized.cloudflare_refresh_token = None;
        sanitized.cloudflare_access_token = None;
        let _ = write_credentials(&legacy_path, &sanitized);
        return Ok(Some((path, credentials)));
    }

    if !has_cloudflare_session(&credentials)
        && api_base_url == normalize_url(DEFAULT_CLOUDFLARE_API_URL, DEFAULT_CLOUDFLARE_API_URL)
    {
        return Ok(Some((legacy_path, credentials)));
    }

    Ok(None)
}

pub(crate) fn load_credentials(path: &Path) -> Result<AuthCredentials> {
    let mut credentials = load_credentials_from_file(path)?;
    hydrate_credentials_from_keyring(&mut credentials)?;
    Ok(credentials)
}

pub(crate) fn auth_record_for_backend(
    base: &Path,
    api_base_url: &str,
) -> Result<Option<(PathBuf, AuthCredentials)>> {
    let api_base_url = normalize_url(api_base_url, DEFAULT_CLOUDFLARE_API_URL);
    let path = auth_path_for_api_base_url(base, &api_base_url);
    if path.exists() {
        let credentials = load_credentials_from_file(&path)?;
        if !credentials_match_backend(&credentials, &api_base_url) {
            return Ok(None);
        }
        let credentials = load_credentials(&path)?;
        return Ok(Some((path, credentials)));
    }

    let old_scoped_path = legacy_scoped_auth_path(base, &api_base_url);
    if old_scoped_path.exists() && old_scoped_path != path {
        let credentials = load_credentials_from_file(&old_scoped_path)?;
        if credentials_match_backend(&credentials, &api_base_url) {
            let mut credentials = credentials;
            hydrate_credentials_from_keyring(&mut credentials)?;
            if !has_cloudflare_session(&credentials) {
                return Ok(None);
            }
            write_credentials(&path, &credentials)?;
            let _ = std::fs::remove_file(old_scoped_path);
            return Ok(Some((path, credentials)));
        }
    }

    let legacy_path = legacy_auth_path(base);
    if !legacy_path.exists() || legacy_path == path {
        return Ok(None);
    }

    let credentials = load_credentials(&legacy_path)?;
    if has_cloudflare_session(&credentials)
        && credentials_match_backend(&credentials, &api_base_url)
    {
        write_credentials(&path, &credentials)?;
        let mut sanitized = credentials.clone();
        sanitized.cloudflare_refresh_token = None;
        sanitized.cloudflare_access_token = None;
        let _ = write_credentials(&legacy_path, &sanitized);
        return Ok(Some((path, credentials)));
    }

    if !has_cloudflare_session(&credentials)
        && api_base_url == normalize_url(DEFAULT_CLOUDFLARE_API_URL, DEFAULT_CLOUDFLARE_API_URL)
    {
        return Ok(Some((legacy_path, credentials)));
    }

    Ok(None)
}

pub(crate) fn has_cloudflare_session(credentials: &AuthCredentials) -> bool {
    credentials
        .cloudflare_refresh_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty())
}

pub(crate) fn is_device_linked(credentials: &AuthCredentials) -> bool {
    credentials
        .device_id
        .as_deref()
        .is_some_and(|device_id| !device_id.trim().is_empty())
}

pub(crate) fn credentials_match_backend(credentials: &AuthCredentials, api_base_url: &str) -> bool {
    credentials.api_base_url.as_deref().is_some_and(|stored| {
        normalize_url(stored, DEFAULT_CLOUDFLARE_API_URL)
            == normalize_url(api_base_url, DEFAULT_CLOUDFLARE_API_URL)
    })
}

pub(crate) fn ensure_cloudflare_session(path: &Path, credentials: &AuthCredentials) -> Result<()> {
    if has_cloudflare_session(credentials) {
        return Ok(());
    }

    let _ = std::fs::remove_file(path);
    delete_tokens_from_keyring(credentials);
    bail!("Stored credentials use a removed auth flow. Please run `statsai auth login` again.")
}

pub(crate) fn save_credentials(credentials: AuthCredentials) -> Result<()> {
    let label = credentials
        .device_id
        .clone()
        .unwrap_or_else(|| "Cloudflare device".to_string());
    let path = auth_path_for_api_base_url(
        &auth_base_dir(),
        credentials
            .api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_CLOUDFLARE_API_URL),
    );
    write_credentials(&path, &credentials)?;
    println!("\nSuccess! Logged in as: {}", label);
    Ok(())
}

pub(crate) fn write_credentials(path: &Path, credentials: &AuthCredentials) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_tokens_to_keyring(credentials)?;
    let data_to_write = if cfg!(test) {
        credentials.clone()
    } else {
        let mut redacted = credentials.clone();
        redacted.cloudflare_refresh_token = None;
        redacted.cloudflare_access_token = None;
        redacted
    };
    write_auth_metadata_atomically(path, &data_to_write)?;
    Ok(())
}

pub(crate) fn write_auth_metadata_atomically(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create auth directory {}", parent.display()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".statsai-auth-")
        .tempfile_in(parent)
        .with_context(|| format!("create temporary auth metadata in {}", parent.display()))?;
    restrict_file_permissions(temp.path())?;
    serde_json::to_writer_pretty(temp.as_file_mut(), value)
        .with_context(|| format!("serialize auth metadata {}", path.display()))?;
    temp.as_file_mut().flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace auth metadata {}", path.display()))?;
    restrict_file_permissions(path)?;

    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) fn normalize_base_url(value: &str) -> String {
    normalize_url(value, DEFAULT_CLOUDFLARE_API_URL)
}

pub(crate) fn normalize_url(value: &str, default_value: &str) -> String {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() {
        default_value.to_string()
    } else {
        value.to_string()
    }
}

pub(crate) fn sanitize_backend_key(value: &str) -> String {
    let mut key = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            key.push(ch.to_ascii_lowercase());
        } else {
            key.push('_');
        }
    }
    if key.is_empty() {
        "default".to_string()
    } else {
        key
    }
}

pub(crate) fn backend_namespace_key(api_base_url: &str) -> String {
    let normalized = normalize_base_url(api_base_url);
    hex::encode(Sha256::digest(normalized.as_bytes()))
}

pub(crate) fn restrict_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
