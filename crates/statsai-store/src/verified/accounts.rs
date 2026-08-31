use super::*;

pub fn verified_source_state_hash(
    verified_state: Option<&VerifiedSourceState>,
) -> Result<Option<String>> {
    verified_state
        .map(|verified_state| serde_json::to_string(verified_state).map(|json| hash_text(&json)))
        .transpose()
        .map_err(Into::into)
}

pub fn verified_source_observation_hash(
    observation: &VerifiedSourceObservation,
) -> Result<Option<String>> {
    match observation {
        VerifiedSourceObservation::Unavailable => Ok(None),
        VerifiedSourceObservation::Verified(state) => Ok(verified_source_state_hash(Some(state))?
            .map(|hash| format!("{VERIFIED_SOURCE_OBSERVATION_HASH_PREFIX}{hash}"))),
        VerifiedSourceObservation::Inferred {
            identity,
            basis,
            settings_modified_at,
        } => {
            let payload = serde_json::to_string(&(
                "verified_source_observation.inferred.v1",
                basis,
                identity,
                settings_modified_at,
            ))?;
            Ok(Some(format!(
                "{INFERRED_SOURCE_OBSERVATION_HASH_PREFIX}{}",
                hash_text(&payload)
            )))
        }
        VerifiedSourceObservation::AttributionBlocked { blocked_since } => {
            let payload = serde_json::to_string(&(
                "verified_source_observation.attribution_blocked.v2",
                blocked_since,
            ))?;
            Ok(Some(format!(
                "{ATTRIBUTION_BLOCKED_OBSERVATION_HASH_PREFIX}{}",
                hash_text(&payload)
            )))
        }
    }
}

pub(crate) fn requires_conservative_verified_recovery(hash: &str) -> bool {
    hash.starts_with(ATTRIBUTION_BLOCKED_OBSERVATION_HASH_PREFIX)
        // Before observation hashes were typed, both verified profiles and
        // blocked states (with or without timestamps) were stored as bare
        // digests. A digest cannot reveal which payload produced it, so only
        // an active matching assignment can prove uninterrupted continuity.
        || !hash.starts_with(VERIFIED_SOURCE_OBSERVATION_HASH_PREFIX)
}

pub fn find_existing_provider_account(
    store: &Store,
    provider: &str,
    provider_user_id: Option<&str>,
    email: Option<&str>,
) -> Result<Option<ProviderAccount>> {
    let normalized_provider_user_id = provider_user_id
        .map(normalize_provider_user_id)
        .filter(|provider_user_id| !provider_user_id.is_empty());
    let normalized_email = email.map(normalize_email).filter(|email| !email.is_empty());
    let accounts = store.list_accounts()?;
    let mut matches: Vec<(&'static str, ProviderAccount)> = Vec::new();

    if let Some(email) = normalized_email.as_deref() {
        if let Some(account) = accounts.iter().find(|account| {
            account.provider == provider
                && account.email.as_deref().map(normalize_email).as_deref() == Some(email)
        }) {
            matches.push(("email", account.clone()));
        }
    }
    if let Some(provider_user_id) = normalized_provider_user_id.as_deref() {
        if let Some(account) = accounts.iter().find(|account| {
            account.provider == provider
                && account
                    .provider_user_id
                    .as_deref()
                    .map(normalize_provider_user_id)
                    .as_deref()
                    == Some(provider_user_id)
        }) {
            matches.push(("provider_user_id", account.clone()));
        }
    }
    if let Some(provider_account_id) = provider_account_id_from_identity(
        provider,
        normalized_provider_user_id.as_deref(),
        normalized_email.as_deref(),
    ) {
        if let Some(account) = store.account(&provider_account_id)? {
            matches.push(("provider_account_id", account));
        }
    }

    let mut unique_matches: Vec<(&'static str, ProviderAccount)> = Vec::new();
    for (match_kind, account) in matches {
        if !unique_matches
            .iter()
            .any(|(_, existing)| existing.provider_account_id == account.provider_account_id)
        {
            unique_matches.push((match_kind, account));
        }
    }

    if unique_matches.len() > 1 {
        let details = unique_matches
            .iter()
            .map(|(match_kind, account)| {
                format!("{match_kind} matched {}", account.provider_account_id.0)
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!("conflicting provider account identifiers for {provider}: {details}");
    }

    Ok(unique_matches
        .into_iter()
        .next()
        .map(|(_, account)| account))
}

#[derive(Debug, Clone)]
pub struct UpsertProviderAccountInput<'a> {
    pub provider: &'a str,
    pub provider_user_id: Option<&'a str>,
    pub email: Option<&'a str>,
    pub label: Option<String>,
    pub plan_name: Option<String>,
    pub identity_source: Option<IdentitySource>,
    pub verified_at: Option<DateTime<Utc>>,
}

pub fn upsert_provider_account(
    store: &Store,
    input: UpsertProviderAccountInput<'_>,
) -> Result<ProviderAccount> {
    let UpsertProviderAccountInput {
        provider,
        provider_user_id,
        email,
        label,
        plan_name,
        identity_source,
        verified_at,
    } = input;
    let normalized_provider_user_id = provider_user_id
        .map(normalize_provider_user_id)
        .filter(|provider_user_id| !provider_user_id.is_empty());
    let normalized_email = email.map(normalize_email).filter(|email| !email.is_empty());
    let existing = find_existing_provider_account(
        store,
        provider,
        normalized_provider_user_id.as_deref(),
        normalized_email.as_deref(),
    )?;
    let provider_account_id = existing
        .as_ref()
        .map(|account| account.provider_account_id.clone())
        .or_else(|| {
            provider_account_id_from_identity(
                provider,
                normalized_provider_user_id.as_deref(),
                normalized_email.as_deref(),
            )
        })
        .with_context(|| format!("missing canonical account identity for {provider}"))?;
    let now = Utc::now();
    let mut account = existing.unwrap_or(ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: provider_account_id.clone(),
        provider: provider.to_string(),
        identity_source: identity_source
            .clone()
            .unwrap_or(IdentitySource::UserConfigured),
        provider_user_id: None,
        email: None,
        provider_user_id_hash: None,
        email_hash: None,
        org_id_hash: None,
        account_label: None,
        plan_name: None,
        confidence: Confidence::High,
        verified_at: None,
        created_at: now,
        updated_at: now,
    });
    if let Some(provider_user_id) = normalized_provider_user_id.as_deref() {
        account.provider_user_id = Some(provider_user_id.to_string());
        account.provider_user_id_hash = Some(hash_text(provider_user_id));
    }
    if let Some(email) = normalized_email.as_deref() {
        account.email_hash = Some(hash_text(email));
        account.email = Some(email.to_string());
    }
    if let Some(label) = label {
        account.account_label = Some(label);
    }
    if let Some(plan_name) = plan_name.filter(|plan_name| !plan_name.trim().is_empty()) {
        account.plan_name = Some(plan_name);
    }
    if let Some(identity_source) = identity_source {
        account.identity_source = merge_identity_source(&account.identity_source, identity_source);
    }
    account.verified_at = max_datetime(account.verified_at, verified_at);
    account.provider = provider.to_string();
    account.confidence = if account.provider_user_id.is_some() || account.email.is_some() {
        Confidence::High
    } else {
        account.confidence
    };
    account.updated_at = now;
    store.upsert_account(&account)?;
    Ok(account)
}
