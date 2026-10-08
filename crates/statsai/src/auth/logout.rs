use super::*;

pub fn logout(local_only: bool) -> Result<()> {
    let api_base_url = cloudflare_api_url();
    let base = auth_base_dir();
    if !local_only {
        if let ServerRevocation::Failed(reason) =
            revoke_server_session(&base, &api_base_url, post_device_logout)
        {
            eprintln!("Warning: could not revoke this device's session on the server: {reason}");
            eprintln!(
                "It expires within 30 days, or you can revoke it now from the Devices page of the dashboard."
            );
        }
    }
    if logout_backend(&base, &api_base_url, |api_base_url| {
        delete_tokens_from_keyring_for_api_base_url(api_base_url)
    })? {
        println!("Successfully logged out.");
    } else {
        println!("Already logged out.");
    }
    Ok(())
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
pub(crate) fn revoke_server_session(
    base: &Path,
    api_base_url: &str,
    send: impl FnOnce(&str, &str) -> Result<()>,
) -> ServerRevocation {
    let refresh_token = match stored_refresh_token(base, api_base_url) {
        Ok(Some(token)) => token,
        Ok(None) => return ServerRevocation::NotAttempted,
        Err(error) => return ServerRevocation::Failed(format!("{error:#}")),
    };
    match send(api_base_url, &refresh_token) {
        Ok(()) => ServerRevocation::Revoked,
        Err(error) => ServerRevocation::Failed(format!("{error:#}")),
    }
}

fn stored_refresh_token(base: &Path, api_base_url: &str) -> Result<Option<String>> {
    if !auth_record_candidate_exists(base, api_base_url)? {
        return Ok(None);
    }
    let Some((_path, credentials)) = auth_record_for_backend(base, api_base_url)? else {
        return Ok(None);
    };
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
