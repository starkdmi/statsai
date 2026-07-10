use anyhow::{bail, Context, Result};
use chrono::Utc;
use keyring::{Entry, Error as KeyringError};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use getrandom::getrandom;

const DEFAULT_CLOUDFLARE_API_URL: &str = "https://api.statsai.dev";
const DEFAULT_CLOUDFLARE_WEB_URL: &str = "https://statsai.dev";
const AUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg_attr(test, allow(dead_code))]
fn keyring_backend_key(api_base_url: &str) -> String {
    backend_namespace_key(api_base_url)
}

#[cfg_attr(test, allow(dead_code))]
fn legacy_keyring_backend_key(api_base_url: &str) -> String {
    api_base_url.replace([':', '/', '.', ' '], "_")
}

fn legacy_refresh_keyring_account(api_base_url: &str) -> String {
    format!("cf-refresh-{}", legacy_keyring_backend_key(api_base_url))
}

fn legacy_access_keyring_account(api_base_url: &str) -> String {
    format!("cf-access-{}", legacy_keyring_backend_key(api_base_url))
}

#[cfg_attr(test, allow(dead_code))]
fn session_entry(api_base_url: &str) -> Result<Entry> {
    Entry::new(
        "statsai",
        &format!("cf-session-{}", keyring_backend_key(api_base_url)),
    )
    .context("failed to open keyring for auth session")
}

#[cfg_attr(test, allow(dead_code))]
fn legacy_session_entry(api_base_url: &str) -> Result<Entry> {
    Entry::new(
        "statsai",
        &format!("cf-session-{}", legacy_keyring_backend_key(api_base_url)),
    )
    .context("failed to open legacy keyring for auth session")
}

#[cfg_attr(test, allow(dead_code))]
fn legacy_refresh_entry(api_base_url: &str) -> Result<Entry> {
    Entry::new("statsai", &legacy_refresh_keyring_account(api_base_url))
        .context("failed to open legacy keyring refresh token")
}

#[cfg_attr(test, allow(dead_code))]
fn legacy_access_entry(api_base_url: &str) -> Result<Entry> {
    Entry::new("statsai", &legacy_access_keyring_account(api_base_url))
        .context("failed to open legacy keyring access token")
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
    api_base_url: Option<String>,
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
    let session: KeyringSession =
        serde_json::from_str(&secret).context("parse auth session stored in OS keyring")?;
    Ok(keyring_session_matches_backend(&session, api_base_url).then_some(session))
}

#[cfg_attr(test, allow(dead_code))]
fn load_legacy_session_from_keyring(api_base_url: &str) -> Result<Option<KeyringSession>> {
    let Some(secret) =
        load_secret_from_keyring(&legacy_session_entry(api_base_url)?, "legacy auth session")?
    else {
        return Ok(None);
    };
    serde_json::from_str(&secret)
        .context("parse legacy auth session stored in OS keyring")
        .map(Some)
}

#[cfg_attr(test, allow(dead_code))]
fn load_legacy_split_session_from_keyring(api_base_url: &str) -> Result<Option<KeyringSession>> {
    let refresh_token = load_secret_from_keyring(
        &legacy_refresh_entry(api_base_url)?,
        "legacy auth refresh token",
    )?
    .filter(|token| !token.trim().is_empty());
    let access_token = load_secret_from_keyring(
        &legacy_access_entry(api_base_url)?,
        "legacy auth access token",
    )?
    .filter(|token| !token.trim().is_empty());
    if refresh_token.is_none() && access_token.is_none() {
        return Ok(None);
    }
    Ok(Some(KeyringSession {
        api_base_url: None,
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
    base.join(format!("auth-{}.json", backend_namespace_key(api_base_url)))
}

fn auth_device_id_path_for_api_base_url(base: &Path, api_base_url: &str) -> PathBuf {
    base.join(format!(
        "auth-device-{}",
        backend_namespace_key(api_base_url)
    ))
}

fn legacy_scoped_auth_path(base: &Path, api_base_url: &str) -> PathBuf {
    base.join(format!(
        "auth-{}.json",
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
    if let Some((path, _credentials)) = auth_record_for_backend(&auth_base_dir(), &api_base_url)? {
        std::fs::remove_file(&path)?;
        delete_tokens_from_keyring_for_api_base_url(&api_base_url);
        println!("Successfully logged out.");
    } else {
        delete_tokens_from_keyring_for_api_base_url(&api_base_url);
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

    let access_token = refresh_cloudflare_access_token(&path, &mut credentials, &api_base_url)?;
    Ok(Some(access_token))
}

fn refresh_cloudflare_access_token(
    path: &Path,
    credentials: &mut AuthCredentials,
    api_base_url: &str,
) -> Result<String> {
    let refresh_token = credentials
        .cloudflare_refresh_token
        .clone()
        .filter(|token| !token.trim().is_empty())
        .context("Cloudflare refresh token missing; run `statsai auth login`")?;
    let url = format!("{}/api/devices/token", api_base_url.trim_end_matches('/'));
    let response = ureq::post(&url)
        .timeout(AUTH_HTTP_TIMEOUT)
        .send_json(token_refresh_request_payload(&refresh_token));
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            if code == 400 || code == 401 {
                let _ = std::fs::remove_file(path);
                delete_tokens_from_keyring(credentials);
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
    credentials.api_base_url = Some(api_base_url.to_string());
    credentials.cloudflare_refresh_token = Some(next_refresh_token);
    credentials.cloudflare_refresh_expires_at_secs = refresh_expires_at;
    credentials.cloudflare_access_token = Some(access_token.clone());
    credentials.cloudflare_access_expires_at_secs = access_expires_at;
    if let Some(device_id) = json["deviceId"].as_str() {
        credentials.device_id = Some(device_id.to_string());
        remember_auth_device_id(api_base_url, device_id);
    }
    write_credentials(path, credentials)?;
    Ok(access_token)
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
    api.contains("127.0.0.1") || api.contains("localhost")
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

fn start_headless_device_login(
    api_base_url: &str,
    device_id: &str,
    device_name: &str,
) -> DeviceSessionRequestResult<HeadlessLoginStart> {
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
    let fresh_device_id = crate::generate_device_id();
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
    preferred_auth_device_id_with_fallback(base, api_base_url, crate::default_device_id)
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

fn append_collector_metadata(payload: &mut serde_json::Map<String, serde_json::Value>) {
    payload.insert(
        "platform".to_string(),
        serde_json::Value::String(std::env::consts::OS.to_string()),
    );
    payload.insert(
        "collectorVersion".to_string(),
        serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string()),
    );
}

fn token_refresh_request_payload(refresh_token: &str) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "refreshToken".to_string(),
        serde_json::Value::String(refresh_token.to_string()),
    );
    append_collector_metadata(&mut payload);
    serde_json::Value::Object(payload)
}

fn default_device_name() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "device".to_string());
    format!("{} ({})", host, std::env::consts::OS)
}

fn load_credentials_from_file(path: &Path) -> Result<AuthCredentials> {
    let file = std::fs::File::open(path)?;
    serde_json::from_reader(file).context("parse stored auth credentials")
}

fn auth_record_from_file(
    base: &Path,
    api_base_url: &str,
) -> Result<Option<(PathBuf, AuthCredentials)>> {
    auth_record_from_file_with_loader(base, api_base_url, load_credentials)
}

fn auth_record_from_file_with_loader(
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

fn load_credentials(path: &Path) -> Result<AuthCredentials> {
    let mut credentials = load_credentials_from_file(path)?;
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

fn has_cloudflare_session(credentials: &AuthCredentials) -> bool {
    credentials
        .cloudflare_refresh_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty())
}

fn is_device_linked(credentials: &AuthCredentials) -> bool {
    credentials
        .device_id
        .as_deref()
        .is_some_and(|device_id| !device_id.trim().is_empty())
}

fn credentials_match_backend(credentials: &AuthCredentials, api_base_url: &str) -> bool {
    credentials.api_base_url.as_deref().is_some_and(|stored| {
        normalize_url(stored, DEFAULT_CLOUDFLARE_API_URL)
            == normalize_url(api_base_url, DEFAULT_CLOUDFLARE_API_URL)
    })
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
            .clone()
            .unwrap_or_else(|| DEFAULT_CLOUDFLARE_API_URL.to_string());
        let current_session = load_session_from_keyring(&api_base)?;
        if current_session.is_some() {
            hydrate_credentials_from_sessions(credentials, &api_base, current_session, None, None);
        } else if credentials_match_backend(credentials, &api_base) {
            let legacy_session = load_legacy_session_from_keyring(&api_base)?;
            let legacy_split_session = load_legacy_split_session_from_keyring(&api_base)?;
            if hydrate_credentials_from_sessions(
                credentials,
                &api_base,
                None,
                legacy_session,
                legacy_split_session,
            ) {
                let migrated = KeyringSession {
                    api_base_url: Some(normalize_base_url(&api_base)),
                    refresh_token: credentials.cloudflare_refresh_token.clone(),
                    access_token: credentials.cloudflare_access_token.clone(),
                };
                store_session_in_keyring(&api_base, &migrated)?;
                delete_legacy_tokens_from_keyring(&api_base);
            }
        }
    }
    #[cfg(test)]
    {
        let _ = credentials;
    }
    Ok(())
}

fn hydrate_credentials_from_sessions(
    credentials: &mut AuthCredentials,
    api_base_url: &str,
    current_session: Option<KeyringSession>,
    legacy_session: Option<KeyringSession>,
    legacy_split_session: Option<KeyringSession>,
) -> bool {
    let (session, migrated) = if let Some(session) =
        current_session.filter(|session| keyring_session_matches_backend(session, api_base_url))
    {
        (Some(session), false)
    } else if credentials_match_backend(credentials, api_base_url) {
        (
            merge_legacy_keyring_sessions(legacy_session, legacy_split_session),
            true,
        )
    } else {
        (None, false)
    };
    let Some(session) = session else {
        return false;
    };
    if credentials.cloudflare_refresh_token.is_none() {
        credentials.cloudflare_refresh_token = session.refresh_token;
    }
    if credentials.cloudflare_access_token.is_none() {
        credentials.cloudflare_access_token = session.access_token;
    }
    migrated
}

fn merge_legacy_keyring_sessions(
    legacy_session: Option<KeyringSession>,
    legacy_split_session: Option<KeyringSession>,
) -> Option<KeyringSession> {
    let mut merged = legacy_session.unwrap_or_default();
    if let Some(split) = legacy_split_session {
        if merged.refresh_token.is_none() {
            merged.refresh_token = split.refresh_token;
        }
        if merged.access_token.is_none() {
            merged.access_token = split.access_token;
        }
    }
    (merged.refresh_token.is_some() || merged.access_token.is_some()).then_some(merged)
}

fn write_tokens_to_keyring(credentials: &AuthCredentials) -> Result<()> {
    #[cfg(not(test))]
    {
        let api_base = credentials
            .api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_CLOUDFLARE_API_URL);
        let session = KeyringSession {
            api_base_url: Some(normalize_base_url(api_base)),
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
        delete_tokens_from_keyring_for_api_base_url(api_base);
    }
    #[cfg(test)]
    {
        let _ = credentials;
    }
}

fn delete_tokens_from_keyring_for_api_base_url(api_base_url: &str) {
    #[cfg(not(test))]
    {
        if let Ok(entry) = session_entry(api_base_url) {
            let _ = entry.delete_credential();
        }
        delete_legacy_tokens_from_keyring(api_base_url);
    }
    #[cfg(test)]
    {
        let _ = api_base_url;
    }
}

#[cfg_attr(test, allow(dead_code))]
fn delete_legacy_tokens_from_keyring(api_base_url: &str) {
    for entry in [
        legacy_session_entry(api_base_url),
        legacy_refresh_entry(api_base_url),
        legacy_access_entry(api_base_url),
    ]
    .into_iter()
    .flatten()
    {
        let _ = entry.delete_credential();
    }
}

#[cfg_attr(test, allow(dead_code))]
fn store_session_in_keyring(api_base_url: &str, session: &KeyringSession) -> Result<()> {
    let mut session = session.clone();
    session.api_base_url = Some(normalize_base_url(api_base_url));
    let payload =
        serde_json::to_string(&session).context("serialize auth session for OS keyring")?;
    session_entry(api_base_url)?
        .set_secret(payload.as_bytes())
        .context("store auth session in OS keyring")?;
    Ok(())
}

fn keyring_session_matches_backend(session: &KeyringSession, api_base_url: &str) -> bool {
    session
        .api_base_url
        .as_deref()
        .is_some_and(|stored| normalize_base_url(stored) == normalize_base_url(api_base_url))
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

fn backend_namespace_key(api_base_url: &str) -> String {
    let normalized = normalize_base_url(api_base_url);
    hex::encode(Sha256::digest(normalized.as_bytes()))
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
        assert_eq!(
            local
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::len),
            Some("auth-.json".len() + 64)
        );
        assert_eq!(
            hosted
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::len),
            Some("auth-.json".len() + 64)
        );
    }

    #[test]
    fn colliding_legacy_backend_names_have_distinct_auth_namespaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dotted = auth_path_for_api_base_url(dir.path(), "https://api.statsai.dev");
        let dashed = auth_path_for_api_base_url(dir.path(), "https://api-statsai.dev");

        assert_eq!(
            legacy_scoped_auth_path(dir.path(), "https://api.statsai.dev"),
            legacy_scoped_auth_path(dir.path(), "https://api-statsai.dev")
        );
        assert_ne!(dotted, dashed);
        assert_ne!(
            keyring_backend_key("https://api.statsai.dev"),
            keyring_backend_key("https://api-statsai.dev")
        );
    }

    #[test]
    fn scoped_auth_file_with_mismatched_backend_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let requested_backend = "https://api.statsai.dev";
        let path = auth_path_for_api_base_url(dir.path(), requested_backend);
        let credentials = AuthCredentials {
            backend: Some("cloudflare".to_string()),
            api_base_url: Some("https://attacker.invalid".to_string()),
            cloudflare_refresh_token: Some("must-not-load".to_string()),
            cloudflare_refresh_expires_at_secs: 0,
            cloudflare_access_token: None,
            cloudflare_access_expires_at_secs: 0,
            device_id: Some("device-1".to_string()),
        };
        write_credentials(&path, &credentials).expect("write mismatched credentials");

        assert!(auth_record_from_file(dir.path(), requested_backend)
            .expect("read auth record")
            .is_none());
    }

    #[test]
    fn scoped_auth_file_without_embedded_backend_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let requested_backend = "https://api.statsai.dev";
        let path = auth_path_for_api_base_url(dir.path(), requested_backend);
        let credentials = AuthCredentials {
            backend: Some("cloudflare".to_string()),
            api_base_url: None,
            cloudflare_refresh_token: Some("must-not-load".to_string()),
            cloudflare_refresh_expires_at_secs: 0,
            cloudflare_access_token: None,
            cloudflare_access_expires_at_secs: 0,
            device_id: Some("device-1".to_string()),
        };
        write_credentials(&path, &credentials).expect("write credentials");

        assert!(auth_record_from_file(dir.path(), requested_backend)
            .expect("read auth record")
            .is_none());
    }

    #[test]
    fn colliding_legacy_auth_file_is_not_migrated_for_another_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let requested_backend = "https://api.statsai.dev";
        let colliding_backend = "https://api-statsai.dev";
        let legacy_path = legacy_scoped_auth_path(dir.path(), colliding_backend);
        let credentials = AuthCredentials {
            backend: Some("cloudflare".to_string()),
            api_base_url: Some(colliding_backend.to_string()),
            cloudflare_refresh_token: Some("must-not-migrate".to_string()),
            cloudflare_refresh_expires_at_secs: 0,
            cloudflare_access_token: None,
            cloudflare_access_expires_at_secs: 0,
            device_id: Some("device-1".to_string()),
        };
        write_credentials(&legacy_path, &credentials).expect("write legacy credentials");

        assert!(auth_record_from_file(dir.path(), requested_backend)
            .expect("read auth record")
            .is_none());
        assert!(!auth_path_for_api_base_url(dir.path(), requested_backend).exists());
    }

    #[test]
    fn legacy_scoped_auth_record_hydrates_before_session_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let api_base_url = "https://api.statsai.dev";
        let legacy_path = legacy_scoped_auth_path(dir.path(), api_base_url);
        let redacted = AuthCredentials {
            backend: Some("cloudflare".to_string()),
            api_base_url: Some(api_base_url.to_string()),
            cloudflare_refresh_token: None,
            cloudflare_refresh_expires_at_secs: 0,
            cloudflare_access_token: None,
            cloudflare_access_expires_at_secs: 0,
            device_id: Some("device-1".to_string()),
        };
        write_credentials(&legacy_path, &redacted).expect("write legacy credentials");

        let record = auth_record_from_file_with_loader(dir.path(), api_base_url, |path| {
            let mut hydrated = load_credentials_from_file(path)?;
            hydrated.cloudflare_refresh_token = Some("legacy-refresh".to_string());
            Ok(hydrated)
        })
        .expect("migrate legacy auth record");

        let (path, credentials) = record.expect("hydrated auth record");
        assert_eq!(path, auth_path_for_api_base_url(dir.path(), api_base_url));
        assert_eq!(
            credentials.cloudflare_refresh_token.as_deref(),
            Some("legacy-refresh")
        );
        assert!(!legacy_path.exists());
    }

    #[test]
    fn keyring_session_requires_matching_embedded_backend() {
        let session = KeyringSession {
            api_base_url: Some("https://api.statsai.dev".to_string()),
            refresh_token: Some("refresh-token".to_string()),
            access_token: None,
        };

        assert!(keyring_session_matches_backend(
            &session,
            "https://api.statsai.dev/"
        ));
        assert!(!keyring_session_matches_backend(
            &session,
            "https://api-statsai.dev"
        ));
        assert!(!keyring_session_matches_backend(
            &KeyringSession {
                api_base_url: None,
                ..session
            },
            "https://api.statsai.dev"
        ));
    }

    #[test]
    fn legacy_keyring_session_hydrates_validated_upgrade_credentials() {
        let mut credentials = AuthCredentials {
            backend: Some("cloudflare".to_string()),
            api_base_url: Some("https://api.statsai.dev".to_string()),
            cloudflare_refresh_token: None,
            cloudflare_refresh_expires_at_secs: 0,
            cloudflare_access_token: None,
            cloudflare_access_expires_at_secs: 0,
            device_id: Some("device-1".to_string()),
        };
        let legacy_session = KeyringSession {
            api_base_url: None,
            refresh_token: Some("legacy-refresh".to_string()),
            access_token: Some("legacy-access".to_string()),
        };

        let migrated = hydrate_credentials_from_sessions(
            &mut credentials,
            "https://api.statsai.dev",
            None,
            Some(legacy_session),
            None,
        );

        assert!(migrated);
        assert_eq!(
            credentials.cloudflare_refresh_token.as_deref(),
            Some("legacy-refresh")
        );
        assert_eq!(
            credentials.cloudflare_access_token.as_deref(),
            Some("legacy-access")
        );
    }

    #[test]
    fn split_legacy_keyring_tokens_hydrate_validated_upgrade_credentials() {
        let mut credentials = AuthCredentials {
            backend: Some("cloudflare".to_string()),
            api_base_url: Some("https://api.statsai.dev".to_string()),
            cloudflare_refresh_token: None,
            cloudflare_refresh_expires_at_secs: 0,
            cloudflare_access_token: None,
            cloudflare_access_expires_at_secs: 0,
            device_id: Some("device-1".to_string()),
        };
        let split_session = KeyringSession {
            api_base_url: None,
            refresh_token: Some("split-refresh".to_string()),
            access_token: Some("split-access".to_string()),
        };

        let migrated = hydrate_credentials_from_sessions(
            &mut credentials,
            "https://api.statsai.dev",
            None,
            None,
            Some(split_session),
        );

        assert!(migrated);
        assert_eq!(
            credentials.cloudflare_refresh_token.as_deref(),
            Some("split-refresh")
        );
        assert_eq!(
            credentials.cloudflare_access_token.as_deref(),
            Some("split-access")
        );
        assert_eq!(
            legacy_refresh_keyring_account("https://api.statsai.dev"),
            "cf-refresh-https___api_statsai_dev"
        );
        assert_eq!(
            legacy_access_keyring_account("https://api.statsai.dev"),
            "cf-access-https___api_statsai_dev"
        );
        assert_eq!(
            legacy_refresh_keyring_account("https://api.statsai.dev/"),
            "cf-refresh-https___api_statsai_dev_"
        );
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
    fn token_refresh_sends_current_collector_metadata() {
        let json = token_refresh_request_payload("refresh-token");
        assert_eq!(json["refreshToken"].as_str(), Some("refresh-token"));
        assert_eq!(
            json["collectorVersion"].as_str(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(json["platform"].as_str(), Some(std::env::consts::OS));
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
