use super::*;
use chrono::Utc;
use getrandom::getrandom;

pub fn get_or_refresh_token() -> Result<Option<String>> {
    let api_base_url = cloudflare_api_url();
    get_or_refresh_token_from_base(&auth_base_dir(), &api_base_url)
}

pub(crate) fn get_or_refresh_token_from_base(
    auth_base: &Path,
    api_base_url: &str,
) -> Result<Option<String>> {
    validate_credential_transport_url(api_base_url, "authentication API")?;
    if !auth_record_candidate_exists(auth_base, api_base_url)? {
        return Ok(None);
    }

    let scoped_path = auth_path_for_api_base_url(auth_base, api_base_url);
    let _refresh_lock = acquire_auth_refresh_lock(&scoped_path)?;
    let Some((path, mut credentials)) = auth_record_for_backend(auth_base, api_base_url)? else {
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

    let access_token = refresh_cloudflare_access_token(&path, &mut credentials, api_base_url)?;
    Ok(Some(access_token))
}

pub(crate) fn refresh_cloudflare_access_token(
    path: &Path,
    credentials: &mut AuthCredentials,
    api_base_url: &str,
) -> Result<String> {
    validate_credential_transport_url(api_base_url, "authentication API")?;
    let refresh_token = credentials
        .cloudflare_refresh_token
        .clone()
        .filter(|token| !token.trim().is_empty())
        .context("Cloudflare refresh token missing; run `statsai auth login`")?;
    let rotation_id = load_or_create_pending_refresh_rotation(path, &refresh_token)?;
    let url = format!("{}/api/devices/token", api_base_url.trim_end_matches('/'));
    let response = retry_once_if(
        || {
            ureq::post(&url)
                .timeout(AUTH_HTTP_TIMEOUT)
                .send_json(token_refresh_request_payload(&refresh_token, &rotation_id))
                .map_err(Box::new)
        },
        |error| matches!(error.as_ref(), ureq::Error::Transport(_)),
    );
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            match *error {
                ureq::Error::Status(code, response) => {
                    let body = response.into_string().unwrap_or_default();
                    if code == 400 || code == 401 {
                        let _ = std::fs::remove_file(path);
                        let _ = clear_pending_refresh_rotation(path);
                        delete_tokens_from_keyring(credentials);
                        bail!("Cloudflare device session expired. Please run 'statsai auth login' again.");
                    }
                    bail!("Cloudflare token refresh failed (HTTP {}): {}", code, body);
                }
                error => bail!("Cloudflare token refresh failed: {}", error),
            }
        }
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
    clear_pending_refresh_rotation(path)?;
    Ok(access_token)
}

pub(crate) fn retry_once_if<T, E>(
    mut operation: impl FnMut() -> std::result::Result<T, E>,
    should_retry: impl Fn(&E) -> bool,
) -> std::result::Result<T, E> {
    match operation() {
        Err(error) if should_retry(&error) => operation(),
        result => result,
    }
}

pub(crate) fn generate_refresh_rotation_id() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom(&mut bytes).context("generate refresh rotation id")?;
    Ok(hex::encode(bytes))
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PendingRefreshRotation {
    refresh_token_fingerprint: String,
    rotation_id: String,
}

pub(crate) struct AuthRefreshLock {
    _file: std::fs::File,
}

pub(crate) fn acquire_auth_refresh_lock(auth_path: &Path) -> Result<AuthRefreshLock> {
    let lock_path = auth_path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create auth directory {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open auth refresh lock {}", lock_path.display()))?;
    restrict_file_permissions(&lock_path)?;
    file.lock()
        .with_context(|| format!("lock auth refresh state {}", lock_path.display()))?;
    Ok(AuthRefreshLock { _file: file })
}

pub(crate) fn pending_refresh_rotation_path(auth_path: &Path) -> PathBuf {
    auth_path.with_extension("rotation.json")
}

pub(crate) fn load_or_create_pending_refresh_rotation(
    auth_path: &Path,
    refresh_token: &str,
) -> Result<String> {
    let path = pending_refresh_rotation_path(auth_path);
    let fingerprint = hex::encode(Sha256::digest(refresh_token.as_bytes()));
    if path.exists() {
        let file = std::fs::File::open(&path)
            .with_context(|| format!("open pending refresh rotation {}", path.display()))?;
        let pending: PendingRefreshRotation = serde_json::from_reader(file)
            .with_context(|| format!("parse pending refresh rotation {}", path.display()))?;
        if pending.refresh_token_fingerprint == fingerprint {
            if is_valid_refresh_rotation_id(&pending.rotation_id) {
                return Ok(pending.rotation_id);
            }
            bail!("pending refresh rotation is invalid: {}", path.display());
        }
    }

    let rotation_id = generate_refresh_rotation_id()?;
    write_auth_metadata_atomically(
        &path,
        &PendingRefreshRotation {
            refresh_token_fingerprint: fingerprint,
            rotation_id: rotation_id.clone(),
        },
    )?;
    Ok(rotation_id)
}

pub(crate) fn clear_pending_refresh_rotation(auth_path: &Path) -> Result<()> {
    remove_file_if_present(&pending_refresh_rotation_path(auth_path)).map(|_| ())
}

pub(crate) fn is_valid_refresh_rotation_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
