use anyhow::{bail, Context, Result};
use chrono::Utc;
use getrandom::getrandom;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

mod keyring;
mod paths;
mod refresh;
#[cfg(test)]
mod tests;

pub(crate) use keyring::*;
pub(crate) use paths::*;
pub use refresh::get_or_refresh_token;
pub(crate) use refresh::*;

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

#[derive(Debug)]
pub(crate) enum DeviceSessionRequestError {
    InvalidDeviceId,
    Fatal(anyhow::Error),
}

pub(crate) type DeviceSessionRequestResult<T> = std::result::Result<T, DeviceSessionRequestError>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HeadlessLoginStart {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_at: u64,
    interval: u64,
}

pub fn login(no_open: bool, headless: bool, device_name: Option<String>) -> Result<()> {
    let api_base_url = cloudflare_api_url();
    validate_credential_transport_url(&api_base_url, "authentication API")?;
    let device_name = requested_device_name(device_name);
    let remembered_device_id = preferred_auth_device_id(&auth_base_dir(), &api_base_url);
    if headless {
        return headless_login(&api_base_url, &remembered_device_id, &device_name);
    }

    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|error| anyhow::anyhow!("Failed to bind loopback server: {}", error))?;
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr.port(),
        _ => bail!("Expected loopback IP address"),
    };
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let state = generate_random_string(32)?;
    let web_base_url = cloudflare_web_url();
    validate_credential_transport_url(&web_base_url, "authentication web app")?;
    let auth_url = format!(
        "{}/connect-device?redirect_uri={}&state={}",
        web_base_url.trim_end_matches('/'),
        percent_encoding::utf8_percent_encode(&redirect_uri, percent_encoding::NON_ALPHANUMERIC),
        state
    );

    if no_open {
        println!("Open this link in your browser to connect this device:");
    } else {
        println!("Opening your browser to connect this device...");
        println!("If the browser does not open automatically, please open this link:");
    }
    println!("\n{}\n", auth_url);
    if !no_open {
        let _ = open::that(&auth_url);
    }

    println!(
        "Waiting for device authorization callback on port {}...",
        port
    );
    let code = listen_for_callback(&server, &state)?;
    let credentials = with_device_id_retry(
        &remembered_device_id,
        "Cloudflare device exchange failed: the backend rejected both the remembered and fresh device identifiers.",
        |device_id| {
            exchange_cloudflare_device_code(
                api_base_url.as_str(),
                &code,
                &state,
                device_id,
                &device_name,
            )
        },
    )?;
    save_credentials(credentials)?;
    Ok(())
}

pub(crate) fn headless_login(
    api_base_url: &str,
    preferred_device_id: &str,
    device_name: &str,
) -> Result<()> {
    let credentials = with_device_id_retry(
        preferred_device_id,
        "Cloudflare headless login failed: the backend rejected both the remembered and fresh device identifiers.",
        |device_id| {
            let start = start_headless_device_login(api_base_url, device_id, device_name)?;
            println!("Open this URL on any trusted browser:");
            println!("\n{}\n", start.verification_uri);
            println!("Enter code: {}", start.user_code);
            if let Some(verification_uri_complete) = start.verification_uri_complete.as_deref() {
                println!("Direct approval link:");
                println!("\n{}\n", verification_uri_complete);
            }
            println!("Waiting for approval...");
            poll_headless_device_login(api_base_url, &start)
        },
    )?;
    save_credentials(credentials)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginSnapshot {
    pub logged_in: bool,
    pub device_id: Option<String>,
}

pub fn login_snapshot() -> Result<LoginSnapshot> {
    let api_base_url = cloudflare_api_url();
    let Some((_path, credentials)) = auth_record_from_file(&auth_base_dir(), &api_base_url)? else {
        return Ok(LoginSnapshot {
            logged_in: false,
            device_id: None,
        });
    };
    if !is_device_linked(&credentials) {
        return Ok(LoginSnapshot {
            logged_in: false,
            device_id: None,
        });
    }
    Ok(LoginSnapshot {
        logged_in: true,
        device_id: credentials.device_id.clone(),
    })
}

pub fn status() -> Result<()> {
    let api_base_url = cloudflare_api_url();
    let Some((_path, credentials)) = auth_record_for_backend(&auth_base_dir(), &api_base_url)?
    else {
        println!("Status: Not logged in");
        println!("API:    {api_base_url}");
        return Ok(());
    };
    if !has_cloudflare_session(&credentials) {
        println!("Status: Not logged in");
        println!("API:    {api_base_url}");
        println!(
            "Note:   Stored credentials are from a removed auth flow. Run `statsai auth login` again."
        );
        return Ok(());
    }

    let now = Utc::now().timestamp() as u64;
    println!("Status: Logged in");
    println!("Mode:   Cloudflare + Better Auth device token");
    if let Some(api_url) = credentials.api_base_url.as_deref() {
        println!("API:    {api_url}");
    }
    if let Some(device_id) = credentials.device_id.as_deref() {
        println!("Device: {device_id}");
    }
    if credentials.cloudflare_access_expires_at_secs > now {
        let mins_left = (credentials.cloudflare_access_expires_at_secs - now) / 60;
        println!("Expiry: Access token expires in {} minutes", mins_left);
    } else {
        println!("Expiry: Access token expired, will refresh on next sync");
    }
    Ok(())
}

pub fn logout() -> Result<()> {
    let api_base_url = cloudflare_api_url();
    if logout_backend(&auth_base_dir(), &api_base_url, |api_base_url| {
        delete_tokens_from_keyring_for_api_base_url(api_base_url)
    })? {
        println!("Successfully logged out.");
    } else {
        println!("Already logged out.");
    }
    Ok(())
}

pub(crate) fn logout_backend(
    base: &Path,
    api_base_url: &str,
    delete_keyring: impl FnOnce(&str) -> Result<()>,
) -> Result<bool> {
    let keyring_result = delete_keyring(api_base_url);
    let metadata_result = remove_auth_metadata_for_backend(base, api_base_url);

    match (keyring_result, metadata_result) {
        (Ok(()), Ok(removed)) => Ok(removed),
        (Err(error), Ok(_)) => Err(error).context("delete credentials from OS keyring"),
        (Ok(()), Err(error)) => Err(error),
        (Err(keyring_error), Err(metadata_error)) => {
            bail!("logout cleanup failed: keyring: {keyring_error:#}; metadata: {metadata_error:#}")
        }
    }
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

pub(crate) fn exchange_cloudflare_device_code(
    api_base_url: &str,
    code: &str,
    state: &str,
    device_id: &str,
    device_name: &str,
) -> DeviceSessionRequestResult<AuthCredentials> {
    validate_credential_transport_url(api_base_url, "authentication API")
        .map_err(DeviceSessionRequestError::Fatal)?;
    let url = format!(
        "{}/api/devices/exchange",
        api_base_url.trim_end_matches('/')
    );
    let response = ureq::post(&url)
        .timeout(AUTH_HTTP_TIMEOUT)
        .send_json(serde_json::json!({
            "code": code,
            "state": state,
            "deviceId": device_id,
            "deviceName": device_name,
            "platform": std::env::consts::OS,
            "collectorVersion": env!("CARGO_PKG_VERSION")
        }));
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            return Err(device_session_request_error(
                "Cloudflare device exchange failed",
                code,
                body,
            ));
        }
        Err(error) => {
            return Err(DeviceSessionRequestError::Fatal(anyhow::anyhow!(
                "Cloudflare device exchange failed: {}",
                error
            )));
        }
    };
    parse_device_session_response(
        api_base_url,
        response
            .into_json()
            .map_err(|error| DeviceSessionRequestError::Fatal(error.into()))?,
    )
}

pub(crate) fn start_headless_device_login(
    api_base_url: &str,
    device_id: &str,
    device_name: &str,
) -> DeviceSessionRequestResult<HeadlessLoginStart> {
    validate_credential_transport_url(api_base_url, "authentication API")
        .map_err(DeviceSessionRequestError::Fatal)?;
    let url = format!("{}/api/devices/start", api_base_url.trim_end_matches('/'));
    let response = ureq::post(&url)
        .timeout(AUTH_HTTP_TIMEOUT)
        .send_json(serde_json::json!({
            "deviceId": device_id,
            "deviceName": device_name,
            "platform": std::env::consts::OS,
            "collectorVersion": env!("CARGO_PKG_VERSION")
        }));
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            return Err(device_session_request_error(
                "Cloudflare headless login start failed",
                code,
                body,
            ));
        }
        Err(error) => {
            return Err(DeviceSessionRequestError::Fatal(anyhow::anyhow!(
                "Cloudflare headless login start failed: {}",
                error
            )));
        }
    };
    response
        .into_json()
        .context("parse headless login start response")
        .map_err(DeviceSessionRequestError::Fatal)
}

pub(crate) fn poll_headless_device_login(
    api_base_url: &str,
    start: &HeadlessLoginStart,
) -> DeviceSessionRequestResult<AuthCredentials> {
    validate_credential_transport_url(api_base_url, "authentication API")
        .map_err(DeviceSessionRequestError::Fatal)?;
    let url = format!("{}/api/devices/poll", api_base_url.trim_end_matches('/'));
    let mut interval = start.interval.max(1);

    loop {
        let now = Utc::now().timestamp() as u64;
        if now >= start.expires_at {
            return Err(DeviceSessionRequestError::Fatal(anyhow::anyhow!(
                "Headless login expired before approval. Please run `statsai auth login --headless` again."
            )));
        }

        sleep(Duration::from_secs(interval));
        let response = ureq::post(&url)
            .timeout(AUTH_HTTP_TIMEOUT)
            .send_json(serde_json::json!({
                "deviceCode": start.device_code
            }));
        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(code, response)) => {
                let body = response.into_string().unwrap_or_default();
                let error = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|json| json["error"].as_str().map(ToOwned::to_owned))
                    .unwrap_or_default();
                match error.as_str() {
                    "authorization_pending" => continue,
                    "slow_down" => {
                        interval = interval.saturating_add(5).max(1);
                        continue;
                    }
                    "expired_token" => {
                        return Err(DeviceSessionRequestError::Fatal(anyhow::anyhow!(
                            "Headless login expired. Please run `statsai auth login --headless` again."
                        )));
                    }
                    "access_denied" => {
                        return Err(DeviceSessionRequestError::Fatal(anyhow::anyhow!(
                            "Headless login was denied."
                        )));
                    }
                    _ => {
                        return Err(device_session_request_error(
                            "Cloudflare headless login polling failed",
                            code,
                            body,
                        ));
                    }
                }
            }
            Err(error) => {
                return Err(DeviceSessionRequestError::Fatal(anyhow::anyhow!(
                    "Cloudflare headless login polling failed: {}",
                    error
                )));
            }
        };
        return parse_device_session_response(
            api_base_url,
            response
                .into_json()
                .map_err(|error| DeviceSessionRequestError::Fatal(error.into()))?,
        );
    }
}

pub(crate) fn parse_device_session_response(
    api_base_url: &str,
    json: serde_json::Value,
) -> DeviceSessionRequestResult<AuthCredentials> {
    let refresh_token = json["refreshToken"]
        .as_str()
        .context("Missing refreshToken from device login")
        .map_err(DeviceSessionRequestError::Fatal)?
        .to_string();
    let access_token = json["accessToken"]
        .as_str()
        .context("Missing accessToken from device login")
        .map_err(DeviceSessionRequestError::Fatal)?
        .to_string();
    let access_expires_at = json["accessExpiresAt"]
        .as_u64()
        .context("Missing accessExpiresAt from device login")
        .map_err(DeviceSessionRequestError::Fatal)?;
    let refresh_expires_at = json["refreshExpiresAt"].as_u64().unwrap_or(0);
    let device_id = json["deviceId"]
        .as_str()
        .context("Missing deviceId from device login")
        .map_err(DeviceSessionRequestError::Fatal)?
        .to_string();

    remember_auth_device_id(api_base_url, &device_id);

    Ok(AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some(api_base_url.to_string()),
        cloudflare_refresh_token: Some(refresh_token),
        cloudflare_refresh_expires_at_secs: refresh_expires_at,
        cloudflare_access_token: Some(access_token),
        cloudflare_access_expires_at_secs: access_expires_at,
        device_id: Some(device_id),
    })
}

pub(crate) fn requested_device_name(device_name: Option<String>) -> String {
    device_name
        .and_then(|name| {
            let name = name.trim().to_string();
            (!name.is_empty()).then_some(name)
        })
        .unwrap_or_else(default_device_name)
}

pub(crate) fn login_device_id_candidates(preferred_device_id: &str) -> Vec<String> {
    let mut candidates = vec![preferred_device_id.to_string()];
    let fresh_device_id = crate::generate_device_id();
    if fresh_device_id != preferred_device_id {
        candidates.push(fresh_device_id);
    }
    candidates
}

pub(crate) fn with_device_id_retry<T, F>(
    preferred_device_id: &str,
    exhausted_message: &str,
    mut action: F,
) -> Result<T>
where
    F: FnMut(&str) -> DeviceSessionRequestResult<T>,
{
    for (index, device_id) in login_device_id_candidates(preferred_device_id)
        .iter()
        .enumerate()
    {
        match action(device_id) {
            Ok(value) => return Ok(value),
            Err(DeviceSessionRequestError::InvalidDeviceId) if index == 0 => {
                eprintln!(
                    "The previous device identifier is already linked to another account. Restarting login with a fresh device ID..."
                );
            }
            Err(DeviceSessionRequestError::InvalidDeviceId) => {
                return Err(anyhow::anyhow!(exhausted_message.to_string()));
            }
            Err(DeviceSessionRequestError::Fatal(error)) => return Err(error),
        }
    }

    Err(anyhow::anyhow!(exhausted_message.to_string()))
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

pub(crate) fn device_session_request_error(
    context: &str,
    code: u16,
    body: String,
) -> DeviceSessionRequestError {
    if code == 409 && response_error_code(&body).as_deref() == Some("invalid_device_id") {
        DeviceSessionRequestError::InvalidDeviceId
    } else {
        DeviceSessionRequestError::Fatal(anyhow::anyhow!("{context} (HTTP {code}): {body}"))
    }
}

pub(crate) fn response_error_code(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|json| json["error"].as_str().map(str::to_owned))
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

pub(crate) fn generate_random_string(len: usize) -> Result<String> {
    let mut buf = vec![0u8; len];
    getrandom(&mut buf)
        .context("failed to obtain cryptographically secure random bytes for auth state")?;
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let s = buf
        .iter()
        .map(|byte| CHARS[(*byte as usize) % CHARS.len()] as char)
        .collect();
    Ok(s)
}

pub(crate) fn listen_for_callback(
    server: &tiny_http::Server,
    expected_state: &str,
) -> Result<String> {
    for request in server.incoming_requests() {
        let url = request.url().to_string();
        let mut code = None;
        let mut state = None;

        if let Some(query_idx) = url.find('?') {
            let query = &url[query_idx + 1..];
            for param in query.split('&') {
                if let Some((key, value)) = param.split_once('=') {
                    if key == "code" {
                        code = Some(percent_decode(value)?);
                    } else if key == "state" {
                        state = Some(percent_decode(value)?);
                    }
                }
            }
        }

        if let (Some(code), Some(state)) = (code, state) {
            if state == expected_state {
                let response = tiny_http::Response::from_string(
                    "<html>\
                     <head><style>body { font-family: sans-serif; text-align: center; padding-top: 50px; background-color: #f7f9fa; color: #1c1e21; }</style></head>\
                     <body>\
                       <h1>Device linked</h1>\
                       <p>You can now close this browser tab and return to your terminal.</p>\
                     </body>\
                     </html>",
                )
                .with_header(tiny_http::Header::from_bytes("content-type", "text/html").unwrap());
                let _ = request.respond(response);
                return Ok(code);
            }
        }

        let response =
            tiny_http::Response::from_string("Waiting for a valid device authorization...")
                .with_status_code(tiny_http::StatusCode(400));
        let _ = request.respond(response);
    }
    bail!("Server shut down without receiving device authorization code")
}

pub(crate) fn percent_decode(value: &str) -> Result<String> {
    percent_decode_str(value)
        .decode_utf8()
        .context("decode loopback callback query parameter")
        .map(|value| value.into_owned())
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

mod open {
    use std::io;
    #[cfg(not(target_os = "windows"))]
    use std::process::Command;

    pub fn that(url: &str) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            Command::new("open").arg(url).status()?;
        }
        #[cfg(target_os = "windows")]
        {
            use std::iter;
            use std::ptr;
            use windows_sys::Win32::UI::Shell::ShellExecuteW;
            use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

            if url.contains('\0') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "browser URL contains a null character",
                ));
            }
            let wide_url = url.encode_utf16().chain(iter::once(0)).collect::<Vec<_>>();
            // SAFETY: `wide_url` is null-terminated and remains alive for the call. All other
            // pointer arguments are optional according to the ShellExecuteW contract.
            let result = unsafe {
                ShellExecuteW(
                    ptr::null_mut(),
                    ptr::null(),
                    wide_url.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    SW_SHOWNORMAL,
                )
            };
            if result as isize <= 32 {
                return Err(io::Error::other(format!(
                    "failed to open browser (ShellExecuteW error {result:?})"
                )));
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Command::new("xdg-open").arg(url).status()?;
        }
        Ok(())
    }
}
