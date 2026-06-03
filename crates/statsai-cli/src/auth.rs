use anyhow::{bail, Context, Result};
use chrono::Utc;
use keyring::{Entry, Error as KeyringError};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use getrandom::getrandom;

const DEFAULT_CLOUDFLARE_API_URL: &str = "https://api.statsai.dev";
const DEFAULT_CLOUDFLARE_WEB_URL: &str = "https://statsai.dev";

#[cfg_attr(test, allow(dead_code))]
fn keyring_backend_key(api_base_url: &str) -> String {
    api_base_url.replace([':', '/', '.', ' '], "_")
}

#[cfg_attr(test, allow(dead_code))]
fn session_entry(api_base_url: &str) -> Result<Entry> {
    Entry::new(
        "statsai-cli",
        &format!("cf-session-{}", keyring_backend_key(api_base_url)),
    )
    .context("failed to open keyring for auth session")
}

#[cfg_attr(test, allow(dead_code))]
fn legacy_token_entry(api_base_url: &str, kind: &str) -> Result<Entry> {
    Entry::new(
        "statsai-cli",
        &format!("cf-{}-{}", kind, keyring_backend_key(api_base_url)),
    )
    .with_context(|| format!("failed to open legacy keyring for {kind} token"))
}

#[cfg_attr(test, allow(dead_code))]
fn access_token_entry(api_base_url: &str) -> Result<Entry> {
    legacy_token_entry(api_base_url, "access").context("failed to open keyring for access token")
}

#[cfg_attr(test, allow(dead_code))]
fn refresh_token_entry(api_base_url: &str) -> Result<Entry> {
    legacy_token_entry(api_base_url, "refresh").context("failed to open keyring for refresh token")
}

#[cfg_attr(test, allow(dead_code))]
fn load_secret_from_keyring(entry: &Entry, label: &str) -> Result<Option<String>> {
    match entry.get_secret() {
        Ok(secret) => String::from_utf8(secret)
            .with_context(|| format!("{label} stored in OS keyring is not valid UTF-8"))
            .map(Some),
        Err(KeyringError::NoEntry) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {label} from OS keyring")),
    }
}

#[cfg_attr(test, allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KeyringSession {
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

#[cfg_attr(test, allow(dead_code))]
fn load_session_from_keyring(api_base_url: &str) -> Result<Option<KeyringSession>> {
    let Some(secret) = load_secret_from_keyring(&session_entry(api_base_url)?, "auth session")?
    else {
        return Ok(None);
    };
    serde_json::from_str::<KeyringSession>(&secret)
        .context("parse auth session stored in OS keyring")
        .map(Some)
}

#[cfg_attr(test, allow(dead_code))]
fn load_legacy_session_from_keyring(api_base_url: &str) -> Result<Option<KeyringSession>> {
    let refresh_token =
        load_secret_from_keyring(&refresh_token_entry(api_base_url)?, "refresh token")?;
    let access_token =
        load_secret_from_keyring(&access_token_entry(api_base_url)?, "access token")?;
    if refresh_token.is_none() && access_token.is_none() {
        return Ok(None);
    }
    Ok(Some(KeyringSession {
        refresh_token,
        access_token,
    }))
}

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

fn auth_base_dir() -> PathBuf {
    statsai_core::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".statsai")
}

fn legacy_auth_path(base: &Path) -> PathBuf {
    base.join("auth.json")
}

fn auth_path_for_api_base_url(base: &Path, api_base_url: &str) -> PathBuf {
    base.join(format!(
        "auth-{}.json",
        sanitize_backend_key(&normalize_base_url(api_base_url))
    ))
}

fn auth_device_id_path_for_api_base_url(base: &Path, api_base_url: &str) -> PathBuf {
    base.join(format!(
        "auth-device-{}",
        sanitize_backend_key(&normalize_base_url(api_base_url))
    ))
}

#[derive(Debug)]
enum DeviceSessionRequestError {
    InvalidDeviceId,
    Fatal(anyhow::Error),
}

type DeviceSessionRequestResult<T> = std::result::Result<T, DeviceSessionRequestError>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadlessLoginStart {
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

fn headless_login(api_base_url: &str, preferred_device_id: &str, device_name: &str) -> Result<()> {
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
    if let Some((path, credentials)) =
        auth_record_for_backend(&auth_base_dir(), &cloudflare_api_url())?
    {
        std::fs::remove_file(&path)?;
        delete_tokens_from_keyring(&credentials);
        println!("Successfully logged out.");
    } else {
        println!("Already logged out.");
    }
    Ok(())
}

pub fn get_or_refresh_token() -> Result<Option<String>> {
    let api_base_url = cloudflare_api_url();
    let Some((path, mut credentials)) = auth_record_for_backend(&auth_base_dir(), &api_base_url)?
    else {
        return Ok(None);
    };
    ensure_cloudflare_session(&path, &credentials)?;

    let now = Utc::now().timestamp() as u64;
    if credentials.cloudflare_access_expires_at_secs > now + 300 {
        if let Some(token) = credentials.cloudflare_access_token.clone() {
            if !token.trim().is_empty() {
                return Ok(Some(token));
            }
        }
    }

    let refresh_token = credentials
        .cloudflare_refresh_token
        .clone()
        .filter(|token| !token.trim().is_empty())
        .context("Cloudflare refresh token missing; run `statsai auth login`")?;
    let url = format!("{}/api/devices/token", api_base_url.trim_end_matches('/'));
    let response = ureq::post(&url).send_json(serde_json::json!({
        "refreshToken": refresh_token
    }));
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            if code == 400 || code == 401 {
                let _ = std::fs::remove_file(&path);
                delete_tokens_from_keyring(&credentials);
                bail!("Cloudflare device session expired. Please run 'statsai auth login' again.");
            }
            bail!("Cloudflare token refresh failed (HTTP {}): {}", code, body);
        }
        Err(error) => bail!("Cloudflare token refresh failed: {}", error),
    };
    let json: serde_json::Value = response.into_json()?;
    let access_token = json["accessToken"]
        .as_str()
        .context("Missing accessToken from token refresh")?
        .to_string();
    let access_expires_at = json["accessExpiresAt"]
        .as_u64()
        .context("Missing accessExpiresAt from token refresh")?;
    let next_refresh_token = json["refreshToken"]
        .as_str()
        .map(ToOwned::to_owned)
        .context("Missing refreshToken from token refresh")?;
    let refresh_expires_at = json["refreshExpiresAt"].as_u64().unwrap_or(0);

    credentials.backend = Some("cloudflare".to_string());
    credentials.api_base_url = Some(api_base_url.clone());
    credentials.cloudflare_refresh_token = Some(next_refresh_token);
    credentials.cloudflare_refresh_expires_at_secs = refresh_expires_at;
    credentials.cloudflare_access_token = Some(access_token.clone());
    credentials.cloudflare_access_expires_at_secs = access_expires_at;
    if let Some(device_id) = json["deviceId"].as_str() {
        credentials.device_id = Some(device_id.to_string());
        remember_auth_device_id(&api_base_url, device_id);
    }
    write_credentials(&path, &credentials)?;
    Ok(Some(access_token))
}

pub fn cloudflare_api_url() -> String {
    normalize_url(
        &std::env::var("STATSAI_API_URL")
            .unwrap_or_else(|_| DEFAULT_CLOUDFLARE_API_URL.to_string()),
        DEFAULT_CLOUDFLARE_API_URL,
    )
}

fn cloudflare_web_url() -> String {
    normalize_url(
        &std::env::var("STATSAI_WEB_URL")
            .unwrap_or_else(|_| DEFAULT_CLOUDFLARE_WEB_URL.to_string()),
        DEFAULT_CLOUDFLARE_WEB_URL,
    )
}

fn exchange_cloudflare_device_code(
    api_base_url: &str,
    code: &str,
    state: &str,
    device_id: &str,
    device_name: &str,
) -> DeviceSessionRequestResult<AuthCredentials> {
    let url = format!(
        "{}/api/devices/exchange",
        api_base_url.trim_end_matches('/')
    );
    let response = ureq::post(&url).send_json(serde_json::json!({
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

fn start_headless_device_login(
    api_base_url: &str,
    device_id: &str,
    device_name: &str,
) -> DeviceSessionRequestResult<HeadlessLoginStart> {
    let url = format!("{}/api/devices/start", api_base_url.trim_end_matches('/'));
    let response = ureq::post(&url).send_json(serde_json::json!({
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

fn poll_headless_device_login(
    api_base_url: &str,
    start: &HeadlessLoginStart,
) -> DeviceSessionRequestResult<AuthCredentials> {
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
        let response = ureq::post(&url).send_json(serde_json::json!({
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

fn parse_device_session_response(
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

fn requested_device_name(device_name: Option<String>) -> String {
    device_name
        .and_then(|name| {
            let name = name.trim().to_string();
            (!name.is_empty()).then_some(name)
        })
        .unwrap_or_else(default_device_name)
}

fn login_device_id_candidates(preferred_device_id: &str) -> Vec<String> {
    let mut candidates = vec![preferred_device_id.to_string()];
    let fresh_device_id = super::generate_device_id();
    if fresh_device_id != preferred_device_id {
        candidates.push(fresh_device_id);
    }
    candidates
}

fn with_device_id_retry<T, F>(
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

fn remembered_auth_device_id_from_base(base: &Path, api_base_url: &str) -> Option<String> {
    let path = auth_device_id_path_for_api_base_url(base, api_base_url);
    let value = std::fs::read_to_string(path).ok()?;
    let value = value.trim();
    (!value.is_empty()).then_some(value.to_string())
}

fn preferred_auth_device_id(base: &Path, api_base_url: &str) -> String {
    preferred_auth_device_id_with_fallback(base, api_base_url, super::default_device_id)
}

fn preferred_auth_device_id_with_fallback<F>(base: &Path, api_base_url: &str, fallback: F) -> String
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

fn remember_auth_device_id(api_base_url: &str, device_id: &str) {
    remember_auth_device_id_in_base(&auth_base_dir(), api_base_url, device_id);
}

fn remember_auth_device_id_in_base(base: &Path, api_base_url: &str, device_id: &str) {
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

fn device_session_request_error(
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

fn response_error_code(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|json| json["error"].as_str().map(str::to_owned))
}

fn default_device_name() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "device".to_string());
    format!("{} ({})", host, std::env::consts::OS)
}

fn load_credentials(path: &Path) -> Result<AuthCredentials> {
    let file = std::fs::File::open(path)?;
    let mut credentials: AuthCredentials =
        serde_json::from_reader(file).context("parse stored auth credentials")?;
    hydrate_credentials_from_keyring(&mut credentials)?;
    Ok(credentials)
}

fn auth_record_for_backend(
    base: &Path,
    api_base_url: &str,
) -> Result<Option<(PathBuf, AuthCredentials)>> {
    let api_base_url = normalize_url(api_base_url, DEFAULT_CLOUDFLARE_API_URL);
    let path = auth_path_for_api_base_url(base, &api_base_url);
    if path.exists() {
        let credentials = load_credentials(&path)?;
        return Ok(Some((path, credentials)));
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
        // sanitize legacy file so tokens are no longer plaintext on disk
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

fn has_cloudflare_session(credentials: &AuthCredentials) -> bool {
    credentials
        .cloudflare_refresh_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty())
}

fn credentials_match_backend(credentials: &AuthCredentials, api_base_url: &str) -> bool {
    normalize_url(
        credentials
            .api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_CLOUDFLARE_API_URL),
        DEFAULT_CLOUDFLARE_API_URL,
    ) == normalize_url(api_base_url, DEFAULT_CLOUDFLARE_API_URL)
}

fn ensure_cloudflare_session(path: &Path, credentials: &AuthCredentials) -> Result<()> {
    if has_cloudflare_session(credentials) {
        return Ok(());
    }

    let _ = std::fs::remove_file(path);
    delete_tokens_from_keyring(credentials);
    bail!("Stored credentials use a removed auth flow. Please run `statsai auth login` again.")
}

fn save_credentials(credentials: AuthCredentials) -> Result<()> {
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

fn write_credentials(path: &Path, credentials: &AuthCredentials) -> Result<()> {
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
    let file = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(file, &data_to_write)?;
    restrict_file_permissions(path)?;
    Ok(())
}

fn hydrate_credentials_from_keyring(credentials: &mut AuthCredentials) -> Result<()> {
    #[cfg(not(test))]
    {
        let api_base = credentials
            .api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_CLOUDFLARE_API_URL);
        if let Some(session) = load_session_from_keyring(api_base)? {
            if credentials.cloudflare_refresh_token.is_none() {
                credentials.cloudflare_refresh_token = session.refresh_token;
            }
            if credentials.cloudflare_access_token.is_none() {
                credentials.cloudflare_access_token = session.access_token;
            }
        } else if let Some(session) = load_legacy_session_from_keyring(api_base)? {
            if credentials.cloudflare_refresh_token.is_none() {
                credentials.cloudflare_refresh_token = session.refresh_token.clone();
            }
            if credentials.cloudflare_access_token.is_none() {
                credentials.cloudflare_access_token = session.access_token.clone();
            }
            let _ = store_session_in_keyring(api_base, &session);
            delete_legacy_tokens_from_keyring(api_base);
        }
    }
    #[cfg(test)]
    {
        let _ = credentials;
    }
    Ok(())
}

fn write_tokens_to_keyring(credentials: &AuthCredentials) -> Result<()> {
    #[cfg(not(test))]
    {
        let api_base = credentials
            .api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_CLOUDFLARE_API_URL);
        let session = KeyringSession {
            refresh_token: credentials
                .cloudflare_refresh_token
                .clone()
                .filter(|token| !token.trim().is_empty()),
            access_token: credentials
                .cloudflare_access_token
                .clone()
                .filter(|token| !token.trim().is_empty()),
        };
        if session.refresh_token.is_some() || session.access_token.is_some() {
            store_session_in_keyring(api_base, &session)?;
            delete_legacy_tokens_from_keyring(api_base);
        }
    }
    #[cfg(test)]
    {
        let _ = credentials;
    }
    Ok(())
}

fn delete_tokens_from_keyring(credentials: &AuthCredentials) {
    #[cfg(not(test))]
    {
        let api_base = credentials
            .api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_CLOUDFLARE_API_URL);
        if let Ok(entry) = session_entry(api_base) {
            let _ = entry.delete_credential();
        }
        delete_legacy_tokens_from_keyring(api_base);
    }
    #[cfg(test)]
    {
        let _ = credentials;
    }
}

#[cfg_attr(test, allow(dead_code))]
fn delete_legacy_tokens_from_keyring(api_base_url: &str) {
    if let Ok(entry) = refresh_token_entry(api_base_url) {
        let _ = entry.delete_credential();
    }
    if let Ok(entry) = access_token_entry(api_base_url) {
        let _ = entry.delete_credential();
    }
}

#[cfg_attr(test, allow(dead_code))]
fn store_session_in_keyring(api_base_url: &str, session: &KeyringSession) -> Result<()> {
    let payload =
        serde_json::to_string(session).context("serialize auth session for OS keyring")?;
    session_entry(api_base_url)?
        .set_secret(payload.as_bytes())
        .context("store auth session in OS keyring")?;
    Ok(())
}

fn generate_random_string(len: usize) -> Result<String> {
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

fn listen_for_callback(server: &tiny_http::Server, expected_state: &str) -> Result<String> {
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

fn percent_decode(value: &str) -> Result<String> {
    percent_decode_str(value)
        .decode_utf8()
        .context("decode loopback callback query parameter")
        .map(|value| value.into_owned())
}

fn normalize_base_url(value: &str) -> String {
    normalize_url(value, DEFAULT_CLOUDFLARE_API_URL)
}

fn normalize_url(value: &str, default_value: &str) -> String {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() {
        default_value.to_string()
    } else {
        value.to_string()
    }
}

fn sanitize_backend_key(value: &str) -> String {
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

fn restrict_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

mod open {
    use std::process::Command;

    pub fn that(url: &str) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            Command::new("open").arg(url).status()?;
        }
        #[cfg(target_os = "windows")]
        {
            Command::new("cmd").args(["/C", "start", url]).status()?;
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Command::new("xdg-open").arg(url).status()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_session_requires_cloudflare_refresh_token() {
        assert!(!has_cloudflare_session(&AuthCredentials {
            backend: Some("legacy".to_string()),
            api_base_url: None,
            cloudflare_refresh_token: None,
            cloudflare_refresh_expires_at_secs: 0,
            cloudflare_access_token: None,
            cloudflare_access_expires_at_secs: 0,
            device_id: None,
        }));
        assert!(has_cloudflare_session(&AuthCredentials {
            backend: Some("cloudflare".to_string()),
            api_base_url: Some("http://127.0.0.1:8787".to_string()),
            cloudflare_refresh_token: Some("refresh-token".to_string()),
            cloudflare_refresh_expires_at_secs: 0,
            cloudflare_access_token: None,
            cloudflare_access_expires_at_secs: 0,
            device_id: Some("device-1".to_string()),
        }));
    }

    #[test]
    fn auth_path_is_scoped_by_api_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let local = auth_path_for_api_base_url(dir.path(), "http://127.0.0.1:8787");
        let hosted = auth_path_for_api_base_url(dir.path(), "https://api.example.com");

        assert_ne!(local, hosted);
        assert!(local.ends_with("auth-http___127_0_0_1_8787.json"));
        assert!(hosted.ends_with("auth-https___api_example_com.json"));
    }

    #[test]
    fn legacy_local_cloudflare_session_migrates_to_backend_scoped_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy_path = legacy_auth_path(dir.path());
        let credentials = AuthCredentials {
            backend: Some("cloudflare".to_string()),
            api_base_url: Some("http://127.0.0.1:8787".to_string()),
            cloudflare_refresh_token: Some("refresh-token".to_string()),
            cloudflare_refresh_expires_at_secs: 0,
            cloudflare_access_token: Some("access-token".to_string()),
            cloudflare_access_expires_at_secs: 123,
            device_id: Some("device-1".to_string()),
        };
        write_credentials(&legacy_path, &credentials).expect("write legacy creds");

        let record =
            auth_record_for_backend(dir.path(), "http://127.0.0.1:8787").expect("auth record");
        let Some((path, loaded)) = record else {
            panic!("expected migrated auth record");
        };

        assert_eq!(
            path,
            auth_path_for_api_base_url(dir.path(), "http://127.0.0.1:8787")
        );
        assert!(path.exists());
        assert_eq!(loaded.device_id.as_deref(), Some("device-1"));
    }

    #[test]
    fn local_session_does_not_bleed_into_other_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy_path = legacy_auth_path(dir.path());
        let credentials = AuthCredentials {
            backend: Some("cloudflare".to_string()),
            api_base_url: Some("http://127.0.0.1:8787".to_string()),
            cloudflare_refresh_token: Some("refresh-token".to_string()),
            cloudflare_refresh_expires_at_secs: 0,
            cloudflare_access_token: Some("access-token".to_string()),
            cloudflare_access_expires_at_secs: 123,
            device_id: Some("device-1".to_string()),
        };
        write_credentials(&legacy_path, &credentials).expect("write legacy creds");

        let record =
            auth_record_for_backend(dir.path(), "https://api.example.com").expect("auth record");
        assert!(record.is_none());
    }

    #[test]
    fn preferred_auth_device_id_reuses_stored_backend_device_id_when_sidecar_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let api_base_url = "https://api.example.com";
        let auth_path = auth_path_for_api_base_url(dir.path(), api_base_url);
        let credentials = AuthCredentials {
            backend: Some("cloudflare".to_string()),
            api_base_url: Some(api_base_url.to_string()),
            cloudflare_refresh_token: Some("refresh-token".to_string()),
            cloudflare_refresh_expires_at_secs: 0,
            cloudflare_access_token: Some("access-token".to_string()),
            cloudflare_access_expires_at_secs: 123,
            device_id: Some("device-1".to_string()),
        };
        write_credentials(&auth_path, &credentials).expect("write auth credentials");

        let preferred = preferred_auth_device_id(dir.path(), api_base_url);

        assert_eq!(preferred, "device-1");
        let sidecar = std::fs::read_to_string(auth_device_id_path_for_api_base_url(
            dir.path(),
            api_base_url,
        ))
        .expect("sidecar device id");
        assert_eq!(sidecar.trim(), "device-1");
    }

    #[test]
    fn preferred_auth_device_id_falls_back_when_auth_record_is_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let api_base_url = "https://api.example.com";
        let auth_path = auth_path_for_api_base_url(dir.path(), api_base_url);
        std::fs::write(&auth_path, "{not-json").expect("write corrupt auth file");

        let preferred = preferred_auth_device_id_with_fallback(dir.path(), api_base_url, || {
            "fallback-device".to_string()
        });

        assert_eq!(preferred, "fallback-device");
    }

    #[test]
    fn with_device_id_retry_retries_after_invalid_device_id() {
        let mut seen_device_ids = Vec::new();
        let result = with_device_id_retry("remembered-device", "retry exhausted", |device_id| {
            seen_device_ids.push(device_id.to_string());
            if seen_device_ids.len() == 1 {
                Err(DeviceSessionRequestError::InvalidDeviceId)
            } else {
                Ok("ok")
            }
        })
        .expect("retry succeeds");

        assert_eq!(result, "ok");
        assert_eq!(seen_device_ids.len(), 2);
        assert_eq!(seen_device_ids[0], "remembered-device");
        assert_ne!(seen_device_ids[1], "remembered-device");
    }

    #[test]
    fn with_device_id_retry_propagates_fatal_errors() {
        let error = with_device_id_retry("remembered-device", "retry exhausted", |_device_id| {
            Err::<(), _>(DeviceSessionRequestError::Fatal(anyhow::anyhow!(
                "fatal problem"
            )))
        })
        .expect_err("fatal error should propagate");

        assert!(error.to_string().contains("fatal problem"));
    }
}
