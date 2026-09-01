use super::*;
use ::keyring::{Entry, Error as KeyringError};

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn keyring_backend_key(api_base_url: &str) -> String {
    backend_namespace_key(api_base_url)
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn legacy_keyring_backend_key(api_base_url: &str) -> String {
    api_base_url.replace([':', '/', '.', ' '], "_")
}

pub(crate) fn legacy_refresh_keyring_account(api_base_url: &str) -> String {
    format!("cf-refresh-{}", legacy_keyring_backend_key(api_base_url))
}

pub(crate) fn legacy_access_keyring_account(api_base_url: &str) -> String {
    format!("cf-access-{}", legacy_keyring_backend_key(api_base_url))
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn session_entry(api_base_url: &str) -> Result<Entry> {
    Entry::new(
        "statsai",
        &format!("cf-session-{}", keyring_backend_key(api_base_url)),
    )
    .context("failed to open keyring for auth session")
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn legacy_session_entry(api_base_url: &str) -> Result<Entry> {
    Entry::new(
        "statsai",
        &format!("cf-session-{}", legacy_keyring_backend_key(api_base_url)),
    )
    .context("failed to open legacy keyring for auth session")
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn legacy_refresh_entry(api_base_url: &str) -> Result<Entry> {
    Entry::new("statsai", &legacy_refresh_keyring_account(api_base_url))
        .context("failed to open legacy keyring refresh token")
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn legacy_access_entry(api_base_url: &str) -> Result<Entry> {
    Entry::new("statsai", &legacy_access_keyring_account(api_base_url))
        .context("failed to open legacy keyring access token")
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn load_secret_from_keyring(entry: &Entry, label: &str) -> Result<Option<String>> {
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
pub(crate) struct KeyringSession {
    #[serde(default)]
    pub(crate) api_base_url: Option<String>,
    #[serde(default)]
    pub(crate) refresh_token: Option<String>,
    #[serde(default)]
    pub(crate) access_token: Option<String>,
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn load_session_from_keyring(api_base_url: &str) -> Result<Option<KeyringSession>> {
    let Some(secret) = load_secret_from_keyring(&session_entry(api_base_url)?, "auth session")?
    else {
        return Ok(None);
    };
    let session: KeyringSession =
        serde_json::from_str(&secret).context("parse auth session stored in OS keyring")?;
    Ok(keyring_session_matches_backend(&session, api_base_url).then_some(session))
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn load_legacy_session_from_keyring(
    api_base_url: &str,
) -> Result<Option<KeyringSession>> {
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
pub(crate) fn load_legacy_split_session_from_keyring(
    api_base_url: &str,
) -> Result<Option<KeyringSession>> {
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

pub(crate) fn hydrate_credentials_from_keyring(credentials: &mut AuthCredentials) -> Result<()> {
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

pub(crate) fn hydrate_credentials_from_sessions(
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

pub(crate) fn merge_legacy_keyring_sessions(
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

pub(crate) fn write_tokens_to_keyring(credentials: &AuthCredentials) -> Result<()> {
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

pub(crate) fn delete_tokens_from_keyring(credentials: &AuthCredentials) {
    #[cfg(not(test))]
    {
        let api_base = credentials
            .api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_CLOUDFLARE_API_URL);
        let _ = delete_tokens_from_keyring_for_api_base_url(api_base);
    }
    #[cfg(test)]
    {
        let _ = credentials;
    }
}

pub(crate) fn delete_tokens_from_keyring_for_api_base_url(api_base_url: &str) -> Result<()> {
    #[cfg(not(test))]
    {
        let entries = [
            ("auth session", session_entry(api_base_url)),
            ("legacy auth session", legacy_session_entry(api_base_url)),
            (
                "legacy auth refresh token",
                legacy_refresh_entry(api_base_url),
            ),
            (
                "legacy auth access token",
                legacy_access_entry(api_base_url),
            ),
        ];
        let mut failures = Vec::new();
        for (label, entry) in entries {
            let result = entry.and_then(|entry| match entry.delete_credential() {
                Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
                Err(error) => Err(error).with_context(|| format!("delete {label} from OS keyring")),
            });
            if let Err(error) = result {
                failures.push(format!("{error:#}"));
            }
        }
        if !failures.is_empty() {
            bail!("{}", failures.join("; "));
        }
    }
    #[cfg(test)]
    {
        let _ = api_base_url;
    }
    Ok(())
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn delete_legacy_tokens_from_keyring(api_base_url: &str) {
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
pub(crate) fn store_session_in_keyring(api_base_url: &str, session: &KeyringSession) -> Result<()> {
    let mut session = session.clone();
    session.api_base_url = Some(normalize_base_url(api_base_url));
    let payload =
        serde_json::to_string(&session).context("serialize auth session for OS keyring")?;
    session_entry(api_base_url)?
        .set_secret(payload.as_bytes())
        .context("store auth session in OS keyring")?;
    Ok(())
}

pub(crate) fn keyring_session_matches_backend(
    session: &KeyringSession,
    api_base_url: &str,
) -> bool {
    session
        .api_base_url
        .as_deref()
        .is_some_and(|stored| normalize_base_url(stored) == normalize_base_url(api_base_url))
}
