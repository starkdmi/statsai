use super::*;

pub fn logout(local_only: bool) -> Result<()> {
    let api_base_url = cloudflare_api_url();
    let removed = logout_from_base(
        &auth_base_dir(),
        &api_base_url,
        local_only,
        post_device_logout,
        delete_tokens_from_keyring_for_api_base_url,
    )?;
    if removed {
        println!("Successfully logged out.");
    } else {
        println!("Already logged out.");
    }
    Ok(())
}

/// Revokes the server session (unless `local_only`) and clears local
/// credentials, all under the auth refresh lock. Holding it from before the
/// refresh token is read until the credentials are gone keeps a concurrent
/// sync from rotating the token in between: the server would accept the
/// revocation of the old token while the rotated session stayed active.
pub(crate) fn logout_from_base(
    base: &Path,
    api_base_url: &str,
    local_only: bool,
    send: impl FnOnce(&str, &str) -> Result<()>,
    delete_keyring: impl FnOnce(&str) -> Result<()>,
) -> Result<bool> {
    // With no auth directory there is nothing on disk to race over, and
    // locking would create the directory just to log out.
    let _refresh_lock = if base.exists() {
        Some(acquire_auth_refresh_lock(&auth_path_for_api_base_url(
            base,
            api_base_url,
        ))?)
    } else {
        None
    };
    if !local_only {
        if let ServerRevocation::Failed(reason) = revoke_server_session(base, api_base_url, send) {
            eprintln!(
                "Warning: could not revoke this device's session on the server, so it may still be active: {reason}"
            );
            eprintln!(
                "Revoke it from the Devices page of the dashboard; otherwise it expires within 30 days."
            );
        }
    }
    logout_backend(base, api_base_url, delete_keyring)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ServerRevocation {
    /// No refresh token is stored, so there is no server session to revoke.
    NotAttempted,
    Revoked,
    Failed(String),
}

/// Best-effort server-side revocation of the stored device session. Never
/// fails: the caller clears local credentials whatever the outcome.
///
/// The caller holds the auth refresh lock. If an earlier refresh left a
/// pending rotation -- the server may have rotated the token and the response
/// was lost -- the stored token may already be revoked while its successor is
/// live, and revoking the stored one would change nothing. The rotation is
/// replayed first, as the next refresh would, and the successor is revoked.
pub(crate) fn revoke_server_session(
    base: &Path,
    api_base_url: &str,
    send: impl FnOnce(&str, &str) -> Result<()>,
) -> ServerRevocation {
    let refresh_token = match current_refresh_token(base, api_base_url) {
        Ok(Some(token)) => token,
        Ok(None) => return ServerRevocation::NotAttempted,
        Err(error) => return ServerRevocation::Failed(format!("{error:#}")),
    };
    match send(api_base_url, &refresh_token) {
        Ok(()) => ServerRevocation::Revoked,
        Err(error) => ServerRevocation::Failed(format!("{error:#}")),
    }
}

/// The refresh token that currently names this device's server session,
/// after finishing any interrupted rotation.
fn current_refresh_token(base: &Path, api_base_url: &str) -> Result<Option<String>> {
    if !auth_record_candidate_exists(base, api_base_url)? {
        return Ok(None);
    }
    let Some((path, mut credentials)) = auth_record_for_backend(base, api_base_url)? else {
        return Ok(None);
    };
    let has_token = credentials
        .cloudflare_refresh_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty());
    if !has_token {
        return Ok(None);
    }
    if pending_refresh_rotation_path(&path).exists() {
        refresh_cloudflare_access_token(&path, &mut credentials, api_base_url)
            .context("finish an interrupted token refresh")?;
    }
    Ok(credentials
        .cloudflare_refresh_token
        .filter(|token| !token.trim().is_empty()))
}

pub(crate) fn post_device_logout(api_base_url: &str, refresh_token: &str) -> Result<()> {
    validate_credential_transport_url(api_base_url, "authentication API")?;
    let url = format!("{}/api/devices/logout", api_base_url.trim_end_matches('/'));
    match ureq::post(&url)
        .timeout(AUTH_HTTP_TIMEOUT)
        .send_json(serde_json::json!({ "refreshToken": refresh_token }))
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, _)) => bail!("HTTP {code}"),
        Err(error) => bail!("{error}"),
    }
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
