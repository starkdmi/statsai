use super::*;

pub(crate) fn event_fingerprint(event: &UsageEvent) -> String {
    let project_key = path_independent_project_key(event);
    semantic_event_fingerprint(&SemanticFingerprintInput {
        provider: &event.provider,
        source_id: &event.source_id,
        started_at: event.session.started_at,
        session_hash: session_hash_for_fingerprint(event),
        project_key: project_key.as_deref(),
        model_name: event
            .model
            .as_ref()
            .and_then(|model| model.normalized_name.as_deref().or(model.name.as_deref())),
        input_tokens: event.usage.input_tokens,
        cache_read_tokens: event.usage.cache_read_tokens,
        cache_creation_tokens: event.usage.cache_creation_tokens,
        output_tokens: event.usage.output_tokens,
        reasoning_tokens: event.usage.reasoning_tokens,
        total_tokens: event.usage.computed_total(),
    })
}

pub(crate) fn semantically_same_event(left: &UsageEvent, right: &UsageEvent) -> bool {
    let uses_path_independent =
        uses_path_independent_codex_dedupe(left) && uses_path_independent_codex_dedupe(right);
    let session_matches = if uses_path_independent {
        true
    } else {
        left.session.local_session_id_hash == right.session.local_session_id_hash
    };
    let project_matches = if uses_path_independent {
        path_independent_projects_match(left, right)
    } else {
        true
    };
    left.provider == right.provider
        && left.source_id == right.source_id
        && left.session.started_at == right.session.started_at
        && session_matches
        && project_matches
        && model_key(left) == model_key(right)
        && optional_value_matches(
            left.model.as_ref().and_then(|model| model.speed.as_deref()),
            right
                .model
                .as_ref()
                .and_then(|model| model.speed.as_deref()),
        )
        && reasoning_matches_for_dedupe(left.model.as_ref(), right.model.as_ref())
        && usage_counts_equivalent(&left.provider, &left.usage, &right.usage)
        && left.usage.computed_total() == right.usage.computed_total()
}

fn reasoning_matches_for_dedupe(left: Option<&ModelInfo>, right: Option<&ModelInfo>) -> bool {
    optional_value_matches(
        left.and_then(|model| model.reasoning_level),
        right.and_then(|model| model.reasoning_level),
    ) && optional_value_matches(
        left.and_then(|model| model.reasoning_level_raw.as_deref()),
        right.and_then(|model| model.reasoning_level_raw.as_deref()),
    )
}

fn optional_value_matches<T: PartialEq>(left: Option<T>, right: Option<T>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn path_independent_projects_match(left: &UsageEvent, right: &UsageEvent) -> bool {
    let left_key = path_independent_project_key(left);
    let right_key = path_independent_project_key(right);
    left_key == right_key
        || legacy_opaque_path_independent_project_match(left, right_key.as_deref())
        || legacy_opaque_path_independent_project_match(right, left_key.as_deref())
}

fn legacy_opaque_path_independent_project_match(
    legacy_candidate: &UsageEvent,
    other_project_key: Option<&str>,
) -> bool {
    other_project_key.is_some_and(|project_key| {
        project_key != "none"
            && legacy_candidate
                .parse_evidence
                .as_ref()
                .map(|evidence| evidence.event_key_version.as_str())
                != Some("semantic_usage_event.v4")
            && match legacy_candidate.project.as_ref() {
                None => true,
                Some(project) => {
                    project.repo_remote_hash.is_none()
                        && project.path_hash.is_none()
                        && project.branch_hash.is_none()
                }
            }
    })
}

fn usage_counts_equivalent(provider: &str, left: &UsageCounts, right: &UsageCounts) -> bool {
    if left.input_tokens == right.input_tokens
        && left.cache_read_tokens == right.cache_read_tokens
        && left.cache_creation_tokens == right.cache_creation_tokens
        && left.output_tokens == right.output_tokens
        && left.reasoning_tokens == right.reasoning_tokens
    {
        return true;
    }
    if provider != "codex" || left.cache_creation_tokens != right.cache_creation_tokens {
        return false;
    }

    let left_matches_right_legacy = left.input_tokens
        == right
            .input_tokens
            .map(|value| value.saturating_add(right.cache_read_tokens.unwrap_or(0)))
        && left.output_tokens
            == right
                .output_tokens
                .map(|value| value.saturating_add(right.reasoning_tokens.unwrap_or(0)))
        && left.cache_read_tokens == right.cache_read_tokens
        && left.reasoning_tokens == right.reasoning_tokens;
    let right_matches_left_legacy = right.input_tokens
        == left
            .input_tokens
            .map(|value| value.saturating_add(left.cache_read_tokens.unwrap_or(0)))
        && right.output_tokens
            == left
                .output_tokens
                .map(|value| value.saturating_add(left.reasoning_tokens.unwrap_or(0)))
        && right.cache_read_tokens == left.cache_read_tokens
        && right.reasoning_tokens == left.reasoning_tokens;

    left_matches_right_legacy || right_matches_left_legacy
}

pub(crate) fn model_key(event: &UsageEvent) -> Option<&str> {
    event
        .model
        .as_ref()
        .and_then(|model| model.normalized_name.as_deref().or(model.name.as_deref()))
}

#[derive(Debug)]
pub(crate) struct ConflictCandidate {
    pub(crate) event_id: String,
    pub(crate) event: UsageEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConflictLookupKey {
    pub(crate) provider: String,
    pub(crate) source_id: String,
    pub(crate) fingerprint: String,
}

pub(crate) fn conflict_lookup_key(event: &UsageEvent, fingerprint: &str) -> ConflictLookupKey {
    ConflictLookupKey {
        provider: event.provider.clone(),
        source_id: event.source_id.0.clone(),
        fingerprint: fingerprint.to_string(),
    }
}

pub(crate) fn exact_or_semantic_conflict<'a>(
    candidates: Option<&'a [ConflictCandidate]>,
    event: &UsageEvent,
) -> Option<&'a ConflictCandidate> {
    let candidates = candidates?;
    candidates
        .iter()
        .find(|candidate| candidate.event_id == event.event_id.0)
        .or_else(|| {
            candidates
                .iter()
                .find(|candidate| semantically_same_event(&candidate.event, event))
        })
}

/// Parse-evidence key version for events identified by the provider's own
/// request identity, produced by the Claude adapter.
const PROVIDER_RECORD_EVENT_KEY_VERSION: &str = "provider_record_usage_event.v1";

/// Whether the stored event already holds a later snapshot of the same provider
/// request than the incoming one.
///
/// Claude writes one streamed response as several records whose cumulative usage
/// grows. An adapter scan reconciles those records across the files it reads,
/// but an incremental scan can re-read only a file holding an early snapshot
/// while the file holding the final one is skipped as unchanged. Both records
/// share a provider-record identity, so the incoming partial snapshot would
/// otherwise overwrite the complete one and shrink stored totals.
///
/// Keeping the larger snapshot pins the stored event until `scan --replace`
/// deletes it, so a future parser change that legitimately lowers usage for one
/// provider request needs a replacing scan to take effect.
fn stored_snapshot_supersedes(existing: &UsageEvent, incoming: &UsageEvent) -> bool {
    fn uses_provider_record_identity(event: &UsageEvent) -> bool {
        event
            .parse_evidence
            .as_ref()
            .is_some_and(|evidence| evidence.event_key_version == PROVIDER_RECORD_EVENT_KEY_VERSION)
    }

    uses_provider_record_identity(existing)
        && uses_provider_record_identity(incoming)
        && incoming.usage.computed_total() < existing.usage.computed_total()
}

pub(crate) fn refreshed_duplicate_event(
    existing: Option<&UsageEvent>,
    incoming: &UsageEvent,
    existing_id: &str,
) -> UsageEvent {
    // Whole-event selection keeps usage, cost, and evidence mutually consistent.
    if let Some(existing) =
        existing.filter(|existing| stored_snapshot_supersedes(existing, incoming))
    {
        let mut kept = existing.clone();
        kept.event_id.0 = existing_id.to_string();
        return kept;
    }

    let mut refreshed = incoming.clone();
    refreshed.event_id.0 = existing_id.to_string();

    let Some(existing_model) = existing.and_then(|event| event.model.as_ref()) else {
        return refreshed;
    };
    let Some(refreshed_model) = refreshed.model.as_mut() else {
        return refreshed;
    };

    if refreshed_model.reasoning_level.is_none() {
        refreshed_model.reasoning_level = existing_model.reasoning_level;
    }
    if refreshed_model.reasoning_level_raw.is_none() {
        refreshed_model.reasoning_level_raw = existing_model.reasoning_level_raw.clone();
    }
    if refreshed_model.speed.is_none() {
        refreshed_model.speed = existing_model.speed.clone();
    }

    refreshed
}

fn session_hash_for_fingerprint(event: &UsageEvent) -> Option<&str> {
    if uses_path_independent_codex_dedupe(event) {
        None
    } else {
        event.session.local_session_id_hash.as_deref()
    }
}

fn path_independent_project_key(event: &UsageEvent) -> Option<String> {
    uses_path_independent_codex_dedupe(event)
        .then(|| sync_rollup_project_key(event.project.as_ref()))
}

fn uses_path_independent_codex_dedupe(event: &UsageEvent) -> bool {
    event.provider == "codex"
        && event
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_record_id.as_deref())
            .is_some_and(|record_id| {
                record_id.contains(":codex_token_count:")
                    || record_id.contains(":codex_turn_usage:")
            })
}
