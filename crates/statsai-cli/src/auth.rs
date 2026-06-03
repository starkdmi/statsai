use anyhow::{bail, Context, Result};
use chrono::Utc;
use keyring::{Entry, Error as KeyringError};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use getrandom::getrandom;

const DEFAULT_CLOUDFLARE_API_URL: &str = "http://127.0.0.1:8787";
const DEFAULT_CLOUDFLARE_WEB_URL: &str = "http://127.0.0.1:3000";

#[cfg_attr(test, allow(dead_code))]
fn keyring_username_for_token(api_base_url: &str, kind: &str) -> String {
    let safe = api_base_url.replace([':', '/', '.', ' '], "_");
    format!("cf-{}-{}", kind, safe)
}

#[cfg_attr(test, allow(dead_code))]
fn refresh_token_entry(api_base_url: &str) -> Result<Entry> {
    Entry::new(
        "statsai-cli",
        &keyring_username_for_token(api_base_url, "refresh"),
    )
    .context("failed to open keyring for refresh token")
}

#[cfg_attr(test, allow(dead_code))]
fn access_token_entry(api_base_url: &str) -> Result<Entry> {
    Entry::new(
        "statsai-cli",
        &keyring_username_for_token(api_base_url, "access"),
    )
    .context("failed to open keyring for access token")
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

pub fn login() -> Result<()> {
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|error| anyhow::anyhow!("Failed to bind loopback server: {}", error))?;
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr.port(),
        _ => bail!("Expected loopback IP address"),
    };
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let state = generate_random_string(32)?;
    let api_base_url = cloudflare_api_url();
    let web_base_url = cloudflare_web_url();
    let auth_url = format!(
        "{}/connect-device?redirect_uri={}&state={}",
        web_base_url.trim_end_matches('/'),
        percent_encoding::utf8_percent_encode(&redirect_uri, percent_encoding::NON_ALPHANUMERIC),
        state
    );

    println!("Opening your browser to connect this device...");
    println!("If the browser does not open automatically, please open this link:");
    println!("\n{}\n", auth_url);
    let _ = open::that(&auth_url);

    println!(
        "Waiting for device authorization callback on port {}...",
        port
    );
    let code = listen_for_callback(&server, &state)?;
    let credentials = exchange_cloudflare_device_code(&api_base_url, &code, &state)?;
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
    credentials.api_base_url = Some(api_base_url);
    credentials.cloudflare_refresh_token = Some(next_refresh_token);
    credentials.cloudflare_refresh_expires_at_secs = refresh_expires_at;
    credentials.cloudflare_access_token = Some(access_token.clone());
    credentials.cloudflare_access_expires_at_secs = access_expires_at;
    if let Some(device_id) = json["deviceId"].as_str() {
        credentials.device_id = Some(device_id.to_string());
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
) -> Result<AuthCredentials> {
    let url = format!(
        "{}/api/devices/exchange",
        api_base_url.trim_end_matches('/')
    );
    let response = ureq::post(&url).send_json(serde_json::json!({
        "code": code,
        "state": state,
        "deviceId": super::default_device_id(),
        "deviceName": default_device_name(),
        "platform": std::env::consts::OS,
        "collectorVersion": env!("CARGO_PKG_VERSION")
    }));
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            bail!(
                "Cloudflare device exchange failed (HTTP {}): {}",
                code,
                body
            );
        }
        Err(error) => bail!("Cloudflare device exchange failed: {}", error),
    };
    let json: serde_json::Value = response.into_json()?;
    let refresh_token = json["refreshToken"]
        .as_str()
        .context("Missing refreshToken from device exchange")?
        .to_string();
    let access_token = json["accessToken"]
        .as_str()
        .context("Missing accessToken from device exchange")?
        .to_string();
    let access_expires_at = json["accessExpiresAt"]
        .as_u64()
        .context("Missing accessExpiresAt from device exchange")?;
    let refresh_expires_at = json["refreshExpiresAt"].as_u64().unwrap_or(0);
    let device_id = json["deviceId"]
        .as_str()
        .context("Missing deviceId from device exchange")?
        .to_string();

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
        if credentials.cloudflare_refresh_token.is_none() {
            if let Some(secret) =
                load_secret_from_keyring(&refresh_token_entry(api_base)?, "refresh token")?
            {
                credentials.cloudflare_refresh_token = Some(secret);
            }
        }
        if credentials.cloudflare_access_token.is_none() {
            if let Some(secret) =
                load_secret_from_keyring(&access_token_entry(api_base)?, "access token")?
            {
                credentials.cloudflare_access_token = Some(secret);
            }
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
        if let Some(token) = &credentials.cloudflare_refresh_token {
            refresh_token_entry(api_base)?
                .set_secret(token.as_bytes())
                .context("store refresh token in OS keyring")?;
        }
        if let Some(token) = &credentials.cloudflare_access_token {
            access_token_entry(api_base)?
                .set_secret(token.as_bytes())
                .context("store access token in OS keyring")?;
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
        if let Ok(entry) = refresh_token_entry(api_base) {
            let _ = entry.delete_credential();
        }
        if let Ok(entry) = access_token_entry(api_base) {
            let _ = entry.delete_credential();
        }
    }
    #[cfg(test)]
    {
        let _ = credentials;
    }
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
}
