use anyhow::{bail, Context, Result};
use chrono::Utc;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const DEFAULT_FIREBASE_API_KEY: &str = "AIzaSyBiWB8m1Oq8tgLOxYslL67i77itmCvn4-4";
const DEFAULT_AUTH_URL: &str = "https://ai-stats-fire.web.app/login/";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthCredentials {
    pub firebase_refresh_token: String,
    pub firebase_id_token: String,
    pub expires_at_secs: u64,
    pub email: String,
    pub uid: String,
    #[serde(default)]
    pub firebase_api_key: Option<String>,
}

fn auth_path() -> PathBuf {
    ai_stats_core::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ai-stats")
        .join("auth.json")
}

pub fn login(client_id_arg: Option<String>) -> Result<()> {
    if let Some(client_id) = client_id_arg.or_else(|| std::env::var("GOOGLE_CLIENT_ID").ok()) {
        return login_with_google_oauth_client(client_id);
    }

    login_with_hosted_firebase()
}

fn login_with_hosted_firebase() -> Result<()> {
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| anyhow::anyhow!("Failed to bind loopback server: {}", e))?;
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr.port(),
        _ => bail!("Expected loopback IP address"),
    };
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let state = generate_random_string(32);
    let auth_base_url =
        std::env::var("AI_STATS_AUTH_URL").unwrap_or_else(|_| DEFAULT_AUTH_URL.to_string());
    let auth_url = format!(
        "{}?redirect_uri={}&state={}",
        auth_base_url.trim_end_matches('/'),
        percent_encoding::utf8_percent_encode(&redirect_uri, percent_encoding::NON_ALPHANUMERIC),
        state
    );

    println!("Opening your browser to authenticate with Firebase...");
    println!("If the browser does not open automatically, please open this link:");
    println!("\n{}\n", auth_url);
    let _ = open::that(&auth_url);

    println!("Waiting for authentication callback on port {}...", port);
    let callback = listen_for_firebase_callback(&server, &state)?;
    save_credentials(callback)?;
    Ok(())
}

fn login_with_google_oauth_client(client_id: String) -> Result<()> {
    let firebase_api_key = firebase_api_key()?;

    // 1. Start local loopback server
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| anyhow::anyhow!("Failed to bind loopback server: {}", e))?;
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr.port(),
        _ => bail!("Expected loopback IP address"),
    };
    let redirect_uri = format!("http://127.0.0.1:{}/callback", port);

    // Generate random state and verifier
    let state = generate_random_string(16);
    let code_verifier = generate_random_string(64);

    // Create code challenge (base64url encoded SHA256 of code_verifier)
    let code_challenge = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(code_verifier.as_bytes());
        let hash = hasher.finalize();
        base64url_encode(&hash)
    };

    // 2. Build Google Auth URL
    let auth_url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?\
         client_id={}&\
         redirect_uri={}&\
         response_type=code&\
         scope=openid%20email%20profile&\
         state={}&\
         code_challenge={}&\
         code_challenge_method=S256",
        client_id,
        percent_encoding::utf8_percent_encode(&redirect_uri, percent_encoding::NON_ALPHANUMERIC),
        state,
        code_challenge
    );

    println!("Opening your browser to authenticate with Google...");
    println!("If the browser does not open automatically, please open this link:");
    println!("\n{}\n", auth_url);

    // Open browser (OS specific)
    let _ = open::that(&auth_url);

    // 3. Listen for OAuth callback
    println!("Waiting for authentication callback on port {}...", port);
    let code = listen_for_callback(&server, &state)?;

    // 4. Exchange code for Google ID token
    println!("Exchanging authorization code for Google tokens...");
    let token_res = ureq::post("https://oauth2.googleapis.com/token").send_form(&[
        ("client_id", &client_id),
        ("code", &code),
        ("code_verifier", &code_verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", &redirect_uri),
    ]);

    let token_res = match token_res {
        Ok(res) => res,
        Err(e) => {
            if let ureq::Error::Status(code, response) = e {
                let error_text = response.into_string().unwrap_or_default();
                bail!(
                    "Google token exchange failed (HTTP {}): {}",
                    code,
                    error_text
                );
            } else {
                bail!("Google token exchange failed: {}", e);
            }
        }
    };

    let token_json: serde_json::Value = token_res.into_json()?;
    let google_id_token = token_json["id_token"]
        .as_str()
        .context("Missing id_token in Google response")?;

    // 5. Authenticate with Firebase using Google ID Token
    println!("Authenticating with Firebase...");
    let firebase_url = format!(
        "https://identitytoolkit.googleapis.com/v1/accounts:signInWithIdp?key={}",
        firebase_api_key
    );

    let firebase_res = ureq::post(&firebase_url).send_json(serde_json::json!({
        "postBody": format!("id_token={}&providerId=google.com", google_id_token),
        "requestUri": "http://localhost",
        "returnIdpCredential": true,
        "returnSecureToken": true
    }));

    let firebase_res = match firebase_res {
        Ok(res) => res,
        Err(e) => {
            if let ureq::Error::Status(code, response) = e {
                let error_text = response.into_string().unwrap_or_default();
                bail!("Firebase Auth failed (HTTP {}): {}", code, error_text);
            } else {
                bail!("Firebase Auth failed: {}", e);
            }
        }
    };

    let fb_json: serde_json::Value = firebase_res.into_json()?;
    let firebase_id_token = fb_json["idToken"]
        .as_str()
        .context("Missing idToken from Firebase response")?
        .to_string();
    let firebase_refresh_token = fb_json["refreshToken"]
        .as_str()
        .context("Missing refreshToken from Firebase response")?
        .to_string();
    let email = fb_json["email"].as_str().unwrap_or("").to_string();
    let uid = fb_json["localId"]
        .as_str()
        .context("Missing localId (uid) from Firebase response")?
        .to_string();
    let expires_in: u64 = fb_json["expiresIn"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600);

    let expires_at_secs = Utc::now().timestamp() as u64 + expires_in;

    let creds = AuthCredentials {
        firebase_refresh_token,
        firebase_id_token,
        expires_at_secs,
        email: email.clone(),
        uid,
        firebase_api_key: Some(firebase_api_key),
    };

    save_credentials(creds)?;
    Ok(())
}

pub fn status() -> Result<()> {
    let path = auth_path();
    if !path.exists() {
        println!("Status: Not logged in");
        return Ok(());
    }

    let file = std::fs::File::open(&path)?;
    let creds: AuthCredentials = serde_json::from_reader(file)?;

    let now = Utc::now().timestamp() as u64;
    if creds.expires_at_secs > now {
        let mins_left = (creds.expires_at_secs - now) / 60;
        println!("Status: Logged in");
        println!("Email:  {}", creds.email);
        println!("UID:    {}", creds.uid);
        println!(
            "Expiry: Token expires in {} minutes (will auto-refresh on sync)",
            mins_left
        );
    } else {
        println!("Status: Logged in (session expired, will refresh on next sync)");
        println!("Email:  {}", creds.email);
    }
    Ok(())
}

pub fn logout() -> Result<()> {
    let path = auth_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("Successfully logged out.");
    } else {
        println!("Already logged out.");
    }
    Ok(())
}

pub fn get_or_refresh_token() -> Result<Option<String>> {
    let path = auth_path();
    if !path.exists() {
        return Ok(None);
    }
    let file = std::fs::File::open(&path)?;
    let mut creds: AuthCredentials = serde_json::from_reader(file)?;

    let now = Utc::now().timestamp() as u64;
    // If token is valid for another 5 minutes, return it
    if creds.expires_at_secs > now + 300 {
        return Ok(Some(creds.firebase_id_token));
    }

    // Otherwise, perform token refresh
    let firebase_api_key = creds
        .firebase_api_key
        .clone()
        .map(Ok)
        .unwrap_or_else(firebase_api_key)?;
    let refresh_url = format!(
        "https://securetoken.googleapis.com/v1/token?key={}",
        firebase_api_key
    );

    let refresh_res = ureq::post(&refresh_url).send_form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &creds.firebase_refresh_token),
    ]);

    let refresh_res = match refresh_res {
        Ok(res) => res,
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            if should_clear_cached_credentials(code, &body) {
                let _ = std::fs::remove_file(&path);
                bail!(
                    "Firebase session expired. Please run 'ai-stats auth login' to sign in again."
                );
            }
            bail!(
                "Firebase token refresh failed (HTTP {}); keeping cached credentials intact: {}",
                code,
                body
            );
        }
        Err(error) => {
            bail!(
                "Firebase token refresh failed; keeping cached credentials intact: {}",
                error
            );
        }
    };

    let json: serde_json::Value = refresh_res.into_json()?;
    let new_id_token = json["id_token"]
        .as_str()
        .context("Missing id_token in refresh response")?
        .to_string();
    let new_refresh_token = json["refresh_token"]
        .as_str()
        .context("Missing refresh_token in refresh response")?
        .to_string();
    let expires_in: u64 = json["expires_in"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600);

    creds.firebase_id_token = new_id_token.clone();
    creds.firebase_refresh_token = new_refresh_token;
    creds.expires_at_secs = Utc::now().timestamp() as u64 + expires_in;

    // Save credentials
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(&path)?;
    serde_json::to_writer_pretty(file, &creds)?;
    restrict_file_permissions(&path)?;

    Ok(Some(new_id_token))
}

pub fn user_id() -> Result<Option<String>> {
    let path = auth_path();
    if !path.exists() {
        return Ok(None);
    }
    let file = std::fs::File::open(&path)?;
    let creds: AuthCredentials = serde_json::from_reader(file)?;
    Ok(Some(creds.uid))
}

pub fn user_id_from_token(token: &str) -> Result<Option<String>> {
    let Some(payload) = jwt_payload_json(token)? else {
        return Ok(None);
    };
    Ok(payload
        .get("user_id")
        .or_else(|| payload.get("sub"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned))
}

fn firebase_api_key() -> Result<String> {
    Ok(std::env::var("FIREBASE_API_KEY").unwrap_or_else(|_| DEFAULT_FIREBASE_API_KEY.to_string()))
}

fn should_clear_cached_credentials(status_code: u16, body: &str) -> bool {
    if status_code != 400 && status_code != 401 {
        return false;
    }
    let lowered = body.to_ascii_lowercase();
    [
        "invalid_grant",
        "invalid refresh token",
        "invalid_refresh_token",
        "token_expired",
        "user_disabled",
        "user_not_found",
        "refresh token is invalid",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn jwt_payload_json(token: &str) -> Result<Option<serde_json::Value>> {
    let mut parts = token.split('.');
    let _header = parts.next();
    let Some(payload) = parts.next() else {
        return Ok(None);
    };
    let bytes = match base64url_decode(payload) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let payload: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    Ok(Some(payload))
}

fn save_credentials(creds: AuthCredentials) -> Result<()> {
    let email = creds.email.clone();
    let path = auth_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(&path)?;
    serde_json::to_writer_pretty(file, &creds)?;
    restrict_file_permissions(&path)?;
    println!("\nSuccess! Logged in as: {}", email);
    Ok(())
}

fn rand_bytes(buf: &mut [u8]) {
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = file.read_exact(buf);
    } else {
        // Fallback LCG
        let ticks = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut t = ticks;
        for byte in buf.iter_mut() {
            t = t.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (t >> 32) as u8;
        }
    }
}

fn generate_random_string(len: usize) -> String {
    let mut buf = vec![0u8; len];
    rand_bytes(&mut buf);
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    buf.iter()
        .map(|&b| CHARS[(b as usize) % CHARS.len()] as char)
        .collect()
}

fn base64url_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut s = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };

        let val = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);

        s.push(CHARS[((val >> 18) & 63) as usize] as char);
        if i + 1 < data.len() {
            s.push(CHARS[((val >> 12) & 63) as usize] as char);
        } else {
            break;
        }
        if i + 2 < data.len() {
            s.push(CHARS[((val >> 6) & 63) as usize] as char);
            s.push(CHARS[(val & 63) as usize] as char);
        } else {
            s.push(CHARS[((val >> 6) & 63) as usize] as char);
            break;
        }
        i += 3;
    }
    s
}

fn base64url_decode(value: &str) -> Result<Vec<u8>> {
    fn decode_char(byte: u8) -> Result<u8> {
        match byte {
            b'A'..=b'Z' => Ok(byte - b'A'),
            b'a'..=b'z' => Ok(byte - b'a' + 26),
            b'0'..=b'9' => Ok(byte - b'0' + 52),
            b'-' => Ok(62),
            b'_' => Ok(63),
            _ => bail!("invalid base64url character"),
        }
    }

    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.len() % 4 == 1 {
        bail!("invalid base64url length");
    }

    let mut output = Vec::with_capacity((bytes.len() * 3) / 4);
    let mut index = 0usize;
    while index < bytes.len() {
        let remaining = bytes.len() - index;
        let take = remaining.min(4);
        let mut quartet = [0u8; 4];
        for offset in 0..take {
            quartet[offset] = decode_char(bytes[index + offset])?;
        }

        let value = ((quartet[0] as u32) << 18)
            | ((quartet[1] as u32) << 12)
            | ((quartet[2] as u32) << 6)
            | quartet[3] as u32;

        output.push(((value >> 16) & 0xff) as u8);
        if take >= 3 {
            output.push(((value >> 8) & 0xff) as u8);
        }
        if take == 4 {
            output.push((value & 0xff) as u8);
        }
        index += take;
    }

    Ok(output)
}

fn listen_for_callback(server: &tiny_http::Server, expected_state: &str) -> Result<String> {
    for request in server.incoming_requests() {
        let url = request.url().to_string();
        let mut code = None;
        let mut req_state = None;

        if let Some(query_idx) = url.find('?') {
            let query = &url[query_idx + 1..];
            for param in query.split('&') {
                if let Some((k, v)) = param.split_once('=') {
                    if k == "code" {
                        code = Some(percent_decode(v)?);
                    } else if k == "state" {
                        req_state = Some(percent_decode(v)?);
                    }
                }
            }
        }

        if let (Some(c), Some(s)) = (code, req_state) {
            if s == expected_state {
                // Return success response to browser
                let response = tiny_http::Response::from_string(
                    "<html>\
                     <head><style>body { font-family: sans-serif; text-align: center; padding-top: 50px; background-color: #f7f9fa; color: #1c1e21; }</style></head>\
                     <body>\
                       <h1>Authentication Successful!</h1>\
                       <p>You can now close this browser tab and return to your terminal.</p>\
                     </body>\
                     </html>"
                )
                .with_header(tiny_http::Header::from_bytes("content-type", "text/html").unwrap());
                let _ = request.respond(response);
                return Ok(c);
            }
        }

        let response = tiny_http::Response::from_string("Waiting for valid authentication...")
            .with_status_code(tiny_http::StatusCode(400));
        let _ = request.respond(response);
    }
    bail!("Server shut down without receiving authorization code")
}

fn listen_for_firebase_callback(
    server: &tiny_http::Server,
    expected_state: &str,
) -> Result<AuthCredentials> {
    for mut request in server.incoming_requests() {
        if request.method() == &tiny_http::Method::Options {
            let response = tiny_http::Response::empty(tiny_http::StatusCode(204))
                .with_header(cors_header("access-control-allow-origin", "*")?)
                .with_header(cors_header(
                    "access-control-allow-methods",
                    "POST, OPTIONS",
                )?)
                .with_header(cors_header("access-control-allow-headers", "content-type")?);
            let _ = request.respond(response);
            continue;
        }

        if request.method() != &tiny_http::Method::Post {
            let response = tiny_http::Response::from_string("expected POST callback")
                .with_status_code(tiny_http::StatusCode(405));
            let _ = request.respond(response);
            continue;
        }

        let mut body = String::new();
        request.as_reader().read_to_string(&mut body)?;
        let form = parse_form_body(&body)?;
        let state = form.get("state").map(String::as_str).unwrap_or_default();
        if state != expected_state {
            let response = tiny_http::Response::from_string("invalid OAuth state")
                .with_status_code(tiny_http::StatusCode(400))
                .with_header(cors_header("access-control-allow-origin", "*")?);
            let _ = request.respond(response);
            continue;
        }

        let firebase_id_token = required_form_value(&form, "idToken")?;
        let firebase_refresh_token = required_form_value(&form, "refreshToken")?;
        let uid = required_form_value(&form, "uid")?;
        let email = form.get("email").cloned().unwrap_or_default();
        let firebase_api_key = form
            .get("apiKey")
            .cloned()
            .unwrap_or_else(|| DEFAULT_FIREBASE_API_KEY.to_string());
        let expires_in = form
            .get("expiresIn")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(3600);

        let response = tiny_http::Response::from_string(
            "<html>\
             <head><style>body { font-family: sans-serif; text-align: center; padding-top: 50px; background-color: #f7f9fa; color: #1c1e21; }</style></head>\
             <body>\
               <h1>Authentication Successful</h1>\
               <p>You can now close this browser tab and return to your terminal.</p>\
             </body>\
             </html>",
        )
        .with_header(tiny_http::Header::from_bytes("content-type", "text/html").unwrap())
        .with_header(cors_header("access-control-allow-origin", "*")?);
        let _ = request.respond(response);

        return Ok(AuthCredentials {
            firebase_refresh_token,
            firebase_id_token,
            expires_at_secs: Utc::now().timestamp() as u64 + expires_in,
            email,
            uid,
            firebase_api_key: Some(firebase_api_key),
        });
    }
    bail!("server shut down without receiving Firebase callback")
}

fn required_form_value(form: &BTreeMap<String, String>, key: &str) -> Result<String> {
    form.get(key)
        .filter(|value| !value.is_empty())
        .cloned()
        .with_context(|| format!("missing callback field {key}"))
}

fn parse_form_body(body: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for pair in body.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        values.insert(percent_decode_form(key)?, percent_decode_form(value)?);
    }
    Ok(values)
}

fn percent_decode_form(value: &str) -> Result<String> {
    percent_decode(&value.replace('+', " "))
}

fn cors_header(name: &str, value: &str) -> Result<tiny_http::Header> {
    tiny_http::Header::from_bytes(name, value)
        .map_err(|_| anyhow::anyhow!("invalid static CORS header"))
}

fn percent_decode(value: &str) -> Result<String> {
    percent_decode_str(value)
        .decode_utf8()
        .context("decode OAuth callback query parameter")
        .map(|value| value.into_owned())
}

fn restrict_file_permissions(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

// OS-specific open browser helper
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
    use serde_json::json;

    #[test]
    fn user_id_from_jwt_prefers_user_id_claim() {
        let header = base64url_encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64url_encode(br#"{"sub":"sub-1","user_id":"uid-1"}"#);
        let token = format!("{header}.{payload}.sig");

        assert_eq!(
            user_id_from_token(&token).expect("uid"),
            Some("uid-1".to_string())
        );
    }

    #[test]
    fn user_id_from_jwt_falls_back_to_sub() {
        let header = base64url_encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64url_encode(br#"{"sub":"sub-2"}"#);
        let token = format!("{header}.{payload}.sig");

        assert_eq!(
            user_id_from_token(&token).expect("uid"),
            Some("sub-2".to_string())
        );
    }

    #[test]
    fn user_id_from_token_returns_none_for_non_jwt() {
        assert_eq!(user_id_from_token("not-a-jwt").expect("no uid"), None);
    }

    #[test]
    fn refresh_credential_clearing_is_conservative() {
        assert!(should_clear_cached_credentials(
            400,
            &json!({"error":"invalid_grant"}).to_string()
        ));
        assert!(should_clear_cached_credentials(
            401,
            &json!({"error":{"message":"INVALID_REFRESH_TOKEN"}}).to_string()
        ));
        assert!(!should_clear_cached_credentials(500, "server error"));
        assert!(!should_clear_cached_credentials(
            400,
            "temporarily unavailable"
        ));
    }
}
