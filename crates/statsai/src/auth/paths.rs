use super::*;

pub(crate) fn auth_base_dir() -> PathBuf {
    statsai_core::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".statsai")
}

pub(crate) fn legacy_auth_path(base: &Path) -> PathBuf {
    base.join("auth.json")
}

pub(crate) fn auth_path_for_api_base_url(base: &Path, api_base_url: &str) -> PathBuf {
    base.join(format!("auth-{}.json", backend_namespace_key(api_base_url)))
}

pub(crate) fn auth_device_id_path_for_api_base_url(base: &Path, api_base_url: &str) -> PathBuf {
    base.join(format!(
        "auth-device-{}",
        backend_namespace_key(api_base_url)
    ))
}

pub(crate) fn legacy_scoped_auth_path(base: &Path, api_base_url: &str) -> PathBuf {
    base.join(format!(
        "auth-{}.json",
        sanitize_backend_key(&normalize_base_url(api_base_url))
    ))
}

pub(crate) fn remove_auth_metadata_for_backend(base: &Path, api_base_url: &str) -> Result<bool> {
    let api_base_url = normalize_url(api_base_url, DEFAULT_CLOUDFLARE_API_URL);
    let scoped_path = auth_path_for_api_base_url(base, &api_base_url);
    let mut removed = false;
    let mut failures = Vec::new();
    match remove_auth_metadata_and_pending(&scoped_path) {
        Ok(path_removed) => removed |= path_removed,
        Err(error) => failures.push(format!("{error:#}")),
    }

    for path in [
        legacy_scoped_auth_path(base, &api_base_url),
        legacy_auth_path(base),
    ] {
        if path == scoped_path || !path.exists() {
            continue;
        }
        match load_credentials_from_file(&path)
            .with_context(|| format!("inspect legacy auth metadata {}", path.display()))
        {
            Ok(credentials) if credentials_match_backend(&credentials, &api_base_url) => {
                match remove_auth_metadata_and_pending(&path) {
                    Ok(path_removed) => removed |= path_removed,
                    Err(error) => failures.push(format!("{error:#}")),
                }
            }
            Ok(_) => {}
            Err(error) => failures.push(format!("{error:#}")),
        }
    }

    if failures.is_empty() {
        Ok(removed)
    } else {
        bail!("{}", failures.join("; "))
    }
}

pub(crate) fn remove_auth_metadata_and_pending(path: &Path) -> Result<bool> {
    let metadata_removed = remove_file_if_present(path)?;
    let pending_removed = remove_file_if_present(&pending_refresh_rotation_path(path))?;
    Ok(metadata_removed || pending_removed)
}

pub(crate) fn remove_file_if_present(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}
