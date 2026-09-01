use super::*;

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
