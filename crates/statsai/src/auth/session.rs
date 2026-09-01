use super::*;
use chrono::Utc;
use getrandom::getrandom;
use percent_encoding::percent_decode_str;
use std::thread::sleep;
use std::time::Duration;

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
