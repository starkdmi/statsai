use super::*;
use crate::{AccountEvidenceScan, ObservedProviderAccount, CLAUDE_CODE_PROVIDER};
use statsai_core::{
    account_plan_observation_id, hash_text, provider_account_id_from_identity, AccountEvidenceKind,
    AccountPlanObservationV1, Confidence, SourceLocation, ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION,
};

pub(crate) const CLAUDE_ACCOUNT_EVIDENCE_PARSER_VERSION: &str = "claude-account-evidence.v1";

/// A recognized subscription plan cached for one Claude Code OAuth organization.
pub(crate) struct ClaudePlanClaim {
    pub(crate) provider_user_id: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) raw_plan_name: String,
    pub(crate) plan_name: &'static str,
    pub(crate) observed_at: DateTime<Utc>,
    pub(crate) record_fingerprint: String,
}

/// Map a cached organization type, and for Max its rate-limit tier, to a plan.
///
/// Values are already trimmed and lowercased. An unrecognized organization type
/// yields nothing rather than a guess: absent OAuth metadata is unknown, never
/// proof of a Free plan. An unrecognized Max tier still proves the Max family,
/// so it reports `Max` without a multiplier, and only Max consults the tier —
/// a Team or Enterprise organization is never relabelled by one.
pub(crate) fn claude_plan_name(
    organization_type: &str,
    organization_rate_limit_tier: Option<&str>,
) -> Option<&'static str> {
    Some(match organization_type {
        "claude_pro" => "Pro",
        "claude_max" => match organization_rate_limit_tier {
            Some("default_claude_max_5x") => "Max 5x",
            Some("default_claude_max_20x") => "Max 20x",
            _ => "Max",
        },
        "claude_team" => "Team",
        "claude_enterprise" => "Enterprise",
        _ => return None,
    })
}

/// Read one recognized plan claim from a cached profile.
///
/// Unlike identity inference this requires a parsed `profileFetchedAt`: the
/// observation is only meaningful as "what Claude Code had cached at that
/// moment", and neither file mtime nor scan time may stand in for it.
pub(crate) fn claude_profile_plan_claim(profile_path: &Path) -> Option<ClaudePlanClaim> {
    let claims = claude_profile_claims(profile_path)?;
    let observed_at = claims.profile_fetched_at?;
    let raw_plan_name = claims.raw_organization_type?;
    let organization_type = claims.organization_type?;
    let plan_name = claude_plan_name(
        &organization_type,
        claims.organization_rate_limit_tier.as_deref(),
    )?;
    // Semantic evidence only: JSON formatting, ignored profile fields, the
    // artifact path, file timestamps, and scan time must never move it. The
    // rate-limit tier is included because it selects the canonical Max subtype.
    // Fields are joined with an ASCII unit separator, which cannot appear in a
    // trimmed identifier, so no two field sets can render the same input.
    let record_fingerprint = hash_text(
        &[
            "claude-account-plan.v1",
            claims.provider_user_id.as_deref().unwrap_or("none"),
            claims.email.as_deref().unwrap_or("none"),
            &organization_type,
            claims
                .organization_rate_limit_tier
                .as_deref()
                .unwrap_or("none"),
            &observed_at.to_rfc3339(),
        ]
        .join("\u{1f}"),
    );
    Some(ClaudePlanClaim {
        provider_user_id: claims.provider_user_id,
        email: claims.email,
        raw_plan_name,
        plan_name,
        observed_at,
        record_fingerprint,
    })
}

pub(crate) fn collect_claude_account_evidence(
    source: &SourceLocation,
    root: &Path,
    location_origin: &LocationOrigin,
) -> AccountEvidenceScan {
    let managed_settings_root = claude_managed_settings_root();
    collect_claude_account_evidence_with_probe_context(
        source,
        root,
        location_origin,
        managed_settings_root.as_deref(),
    )
}

/// Collect the cached Claude subscription plan for one source.
///
/// This is cached evidence, not live billing verification: it reports what
/// Claude Code last wrote for a recognized OAuth organization at
/// `profileFetchedAt`, and cannot prove the subscription is still active at
/// scan time. Everything ambiguous — blocked attribution, an ambiguous or
/// missing profile, malformed JSON, no derivable identity, no parsed fetch
/// time, an unknown organization type — yields an empty scan, and evidence
/// that disappears never becomes a negative observation.
pub(crate) fn collect_claude_account_evidence_with_probe_context(
    source: &SourceLocation,
    root: &Path,
    location_origin: &LocationOrigin,
    managed_settings_root: Option<&Path>,
) -> AccountEvidenceScan {
    let mut scan = AccountEvidenceScan::default();
    let default_root = home_dir().map(|home| home.join(".claude"));
    let settings_root = claude_settings_root(root, location_origin, default_root.as_deref());
    if matches!(
        claude_durable_settings_attribution(root, settings_root, managed_settings_root),
        ClaudeAttribution::Blocked { .. }
    ) {
        return scan;
    }
    let ClaudeProfileResolution::Path(profile_path) =
        claude_profile_resolution(root, location_origin, default_root.as_deref())
    else {
        return scan;
    };
    let Some(claim) = claude_profile_plan_claim(&profile_path) else {
        return scan;
    };
    let Some(provider_account_id) = provider_account_id_from_identity(
        CLAUDE_CODE_PROVIDER,
        claim.provider_user_id.as_deref(),
        claim.email.as_deref(),
    ) else {
        return scan;
    };

    scan.accounts.push(ObservedProviderAccount {
        provider_user_id: claim.provider_user_id.clone(),
        email: claim.email.clone(),
        plan_name: Some(claim.plan_name.to_string()),
        observed_at: claim.observed_at,
    });
    scan.plan_observations.push(AccountPlanObservationV1 {
        schema_version: ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
        observation_id: account_plan_observation_id(
            &source.source_id,
            Some(&provider_account_id),
            &claim.raw_plan_name,
            claim.plan_name,
            claim.observed_at,
            AccountEvidenceKind::AuthSnapshot,
        ),
        provider: CLAUDE_CODE_PROVIDER.to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: Some(provider_account_id),
        raw_plan_name: claim.raw_plan_name,
        plan_name: claim.plan_name.to_string(),
        observed_at: claim.observed_at,
        // A cached profile carries no subscription interval, and inventing one
        // from the fetch time would claim a start or an end that never happened.
        active_from: None,
        active_until: None,
        // The profile was Claude Code's current cached OAuth profile at
        // `observed_at`; this does not assert that it is current at scan time.
        is_current_snapshot: true,
        evidence_kind: AccountEvidenceKind::AuthSnapshot,
        // Medium: a server-derived local cache of unknown freshness, with no
        // authenticated call available to confirm it still holds.
        confidence: Confidence::Medium,
        parser_version: CLAUDE_ACCOUNT_EVIDENCE_PARSER_VERSION.to_string(),
        artifact_path_hash: hash_text(&canonical_display(&profile_path)),
        record_fingerprint: claim.record_fingerprint,
    });
    scan
}
