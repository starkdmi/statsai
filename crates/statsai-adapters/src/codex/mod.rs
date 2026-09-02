mod parse;
mod preview;
mod quota;
mod telemetry;

pub(crate) use parse::*;
pub(crate) use preview::*;
pub(crate) use quota::*;
pub(crate) use telemetry::*;

use crate::*;
use anyhow::Result;
use statsai_core::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[cfg(test)]
use crate::cache::{build_scan_cache_signature, SCAN_CACHE_SIGNATURE_VERSION};
#[cfg(test)]
use crate::tests::{options, options_without_tasks, write_git_fixture};
#[cfg(test)]
use std::io::Write;

#[derive(Debug, Default)]
pub struct CodexAdapter;

impl ProviderAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        "codex-local-jsonl"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn provider(&self) -> &'static str {
        CODEX_PROVIDER
    }

    fn discover(&self) -> Vec<SourceLocation> {
        let mut sources = Vec::new();
        let mut seen = HashSet::new();
        if let Ok(value) = std::env::var("CODEX_HOME") {
            for root in split_paths(&value) {
                if seen.insert(canonical_display(&root)) {
                    sources.push(codex_source_for_root(self, &root, LocationOrigin::Env));
                }
            }
            return sources;
        }

        if let Some(home) = home_dir() {
            let root = home.join(".codex");
            if root.exists() {
                sources.push(codex_source_for_root(self, &root, LocationOrigin::Default));
            }
        }

        sources
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        codex_scan_candidates(source, self.version())
    }

    fn probe_verified_source_state(
        &self,
        source: &SourceLocation,
    ) -> Result<VerifiedSourceObservation> {
        let Some(root) = source_root_path(source) else {
            return Ok(VerifiedSourceObservation::Unavailable);
        };
        let root = codex_source_root(&root);
        Ok(codex_auth_snapshot(&root)
            .map(Box::new)
            .map(VerifiedSourceObservation::Verified)
            .unwrap_or(VerifiedSourceObservation::Unavailable))
    }

    fn collect_account_evidence(
        &self,
        source: &SourceLocation,
        checkpoints: &[AccountEvidenceCheckpointV1],
    ) -> Result<AccountEvidenceScan> {
        let Some(root) = source_root_path(source) else {
            return Ok(AccountEvidenceScan::default());
        };
        collect_codex_account_evidence(source, &codex_source_root(&root), checkpoints)
    }

    fn scan(&self, source: &SourceLocation, options: &ScanOptions) -> Result<AdapterScan> {
        scan_codex_source(self, source, options)
    }
}

pub(crate) fn codex_source_for_root(
    adapter: &CodexAdapter,
    root: &Path,
    origin: LocationOrigin,
) -> SourceLocation {
    SourceLocation::local_adapter(
        adapter.provider(),
        adapter.id(),
        adapter.version(),
        root,
        origin,
    )
}

pub(crate) fn scan_codex_source(
    adapter: &CodexAdapter,
    source: &SourceLocation,
    options: &ScanOptions,
) -> Result<AdapterScan> {
    let mut scan = AdapterScan::default();
    let Some(path_label) = source
        .path_label
        .as_deref()
        .filter(|label| !label.is_empty())
    else {
        return Ok(scan);
    };
    let source_path = PathBuf::from(path_label);
    let root = codex_source_root(&source_path);
    let cache_namespaces = scan_cache_namespaces(source, adapter.version());
    let thread_titles = if options.should_collect_tasks() {
        load_codex_thread_titles(&root)
    } else {
        HashMap::new()
    };
    let mut indexed_candidates = Vec::new();
    for (index, candidate) in codex_jsonl_candidates(source, &source_path, &cache_namespaces)?
        .into_iter()
        .enumerate()
    {
        if !options.should_scan(&candidate.cache_key) {
            scan.diagnostics.files_skipped_unchanged += 1;
            continue;
        }
        indexed_candidates.push((index, candidate));
    }

    let mut seen = EventDedupIndex::new();
    if indexed_candidates.len() <= 1 {
        for (_, candidate) in indexed_candidates {
            let file_scan = scan_codex_candidate_file(
                adapter,
                source,
                options,
                &root,
                &thread_titles,
                &candidate,
            )?;
            merge_adapter_scan(&mut scan, &mut seen, file_scan);
        }
    } else {
        let worker_count = std::thread::available_parallelism()
            .map(|count| count.get().min(8))
            .unwrap_or(1)
            .min(indexed_candidates.len());
        let chunk_size = indexed_candidates.len().div_ceil(worker_count);
        let mut merged_results = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for chunk in indexed_candidates.chunks(chunk_size) {
                let chunk = chunk.to_vec();
                let root = root.clone();
                let thread_titles = thread_titles.clone();
                let source = source.clone();
                let options = options.clone();
                handles.push(scope.spawn(move || -> Result<Vec<(usize, AdapterScan)>> {
                    let mut results = Vec::with_capacity(chunk.len());
                    for (index, candidate) in chunk {
                        let file_scan = scan_codex_candidate_file(
                            adapter,
                            &source,
                            &options,
                            &root,
                            &thread_titles,
                            &candidate,
                        )?;
                        results.push((index, file_scan));
                    }
                    Ok(results)
                }));
            }

            let mut results = Vec::new();
            for handle in handles {
                results.extend(handle.join().expect("codex scan worker panicked")?);
            }
            Ok::<Vec<(usize, AdapterScan)>, anyhow::Error>(results)
        })?;
        merged_results.sort_by_key(|(index, _)| *index);
        for (_, file_scan) in merged_results {
            merge_adapter_scan(&mut scan, &mut seen, file_scan);
        }
    }
    scan.verified_source_state = codex_auth_snapshot(&root);
    scan.diagnostics.accepted_events = scan.events.len() as u64;
    Ok(scan)
}

pub(crate) fn scan_codex_candidate_file(
    adapter: &CodexAdapter,
    source: &SourceLocation,
    options: &ScanOptions,
    root: &Path,
    thread_titles: &HashMap<String, String>,
    candidate: &ScanCandidateFile,
) -> Result<AdapterScan> {
    let usage_root = codex_usage_root_for_file(root, &candidate.path);
    let mut scan = AdapterScan::default();
    scan.diagnostics.files_scanned = 1;
    let mut seen = EventDedupIndex::new();
    let mut ctx = FileParseContext {
        adapter,
        source,
        options,
        scan: &mut scan,
        seen: &mut seen,
    };
    parse_codex_file(&mut ctx, root, &usage_root, thread_titles, &candidate.path)?;
    Ok(scan)
}

pub(crate) fn codex_source_root(path: &Path) -> PathBuf {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "sessions" | "archived_sessions"))
    {
        return path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf());
    }
    path.to_path_buf()
}

pub(crate) fn codex_usage_roots(path: &Path) -> Vec<PathBuf> {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "sessions" | "archived_sessions"))
    {
        return if path.is_dir() {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        };
    }

    ["sessions", "archived_sessions"]
        .into_iter()
        .map(|child| path.join(child))
        .filter(|candidate| candidate.is_dir())
        .collect()
}

pub(crate) fn codex_scan_candidates(
    source: &SourceLocation,
    adapter_version: &str,
) -> Result<Vec<ScanCandidateFile>> {
    let Some(path_label) = source
        .path_label
        .as_deref()
        .filter(|label| !label.is_empty())
    else {
        return Ok(Vec::new());
    };
    let source_path = PathBuf::from(path_label);
    let cache_namespaces = scan_cache_namespaces(source, adapter_version);
    codex_jsonl_candidates(source, &source_path, &cache_namespaces)
}

pub(crate) fn codex_jsonl_candidates(
    _source: &SourceLocation,
    path: &Path,
    cache_namespaces: &ScanCacheNamespaces,
) -> Result<Vec<ScanCandidateFile>> {
    let roots = codex_usage_roots(path);
    let legacy_auth_dependencies = vec![codex_legacy_auth_dependency_signature(
        &codex_source_root(path),
    )];
    let mut candidates = Vec::new();
    for usage_root in roots {
        for candidate_path in collect_jsonl_files(&usage_root)? {
            candidates.push(scan_candidate_with_compatible_dependencies(
                candidate_path,
                None,
                &legacy_auth_dependencies,
                cache_namespaces,
            ));
        }
    }
    Ok(candidates)
}

pub(crate) fn codex_usage_root_for_file(root: &Path, path: &Path) -> PathBuf {
    for child in ["sessions", "archived_sessions"] {
        let usage_root = root.join(child);
        if path.starts_with(&usage_root) {
            return usage_root;
        }
    }
    root.to_path_buf()
}

pub(crate) fn codex_legacy_auth_dependency_signature(root: &Path) -> String {
    file_metadata_signature(&root.join("auth.json"))
}

#[derive(Debug)]
pub(crate) struct CodexAuthClaims {
    pub(crate) provider_user_id: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) plan_type: Option<String>,
    pub(crate) auth_mode: Option<String>,
    pub(crate) authenticated_at: Option<DateTime<Utc>>,
    pub(crate) subscription_checked_at: Option<DateTime<Utc>>,
    pub(crate) active_from: Option<DateTime<Utc>>,
    pub(crate) active_until: Option<DateTime<Utc>>,
}

pub(crate) fn codex_auth_claims(auth_path: &Path) -> Option<CodexAuthClaims> {
    let value = std::fs::read_to_string(auth_path).ok()?;
    let value: Value = serde_json::from_str(&value).ok()?;
    let payload = string_at_any(
        &value,
        &["id_token", "idToken", "/tokens/id_token", "/tokens/idToken"],
    )
    .and_then(|token| jwt_payload_value(&token));
    let auth = payload
        .as_ref()
        .and_then(|payload| payload.pointer("/https:~1~1api.openai.com~1auth"))
        .or_else(|| value.pointer("/https:~1~1api.openai.com~1auth"));

    let provider_user_id = auth
        .and_then(|auth| string_at_any(auth, &["chatgpt_account_id", "chatgpt_user_id", "user_id"]))
        .or_else(|| {
            string_at_any(
                &value,
                &[
                    "account_id",
                    "accountId",
                    "chatgpt_account_id",
                    "chatgpt_user_id",
                    "/tokens/account_id",
                    "/tokens/accountId",
                ],
            )
        });
    let email = payload
        .as_ref()
        .and_then(|payload| {
            string_at_any(
                payload,
                &["email", "/https:~1~1api.openai.com~1profile~1email"],
            )
        })
        .or_else(|| string_at_any(&value, &["email", "user_email"]))
        .map(|email| email.to_ascii_lowercase());
    if provider_user_id.is_none() && email.is_none() {
        return None;
    }

    let plan_type = auth.and_then(|auth| string_at_any(auth, &["chatgpt_plan_type"]));
    let auth_mode = string_at_any(&value, &["auth_mode", "authMode"]);
    let authenticated_at = payload
        .as_ref()
        .and_then(|payload| timestamp_at_any(payload, &["auth_time", "iat"]))
        .or_else(|| file_modified_at(auth_path));
    // An auth-file mtime or a fresh ID token proves a refreshed local session, not
    // that the embedded subscription claims were refreshed at the same time.
    let subscription_checked_at =
        auth.and_then(|auth| timestamp_at_any(auth, &["chatgpt_subscription_last_checked"]));
    let active_from =
        auth.and_then(|auth| timestamp_at_any(auth, &["chatgpt_subscription_active_start"]));
    let active_until =
        auth.and_then(|auth| timestamp_at_any(auth, &["chatgpt_subscription_active_until"]));
    Some(CodexAuthClaims {
        provider_user_id,
        email,
        plan_type,
        auth_mode,
        authenticated_at,
        subscription_checked_at,
        active_from,
        active_until,
    })
}

pub(crate) fn collect_codex_account_evidence(
    source: &SourceLocation,
    root: &Path,
    checkpoints: &[AccountEvidenceCheckpointV1],
) -> Result<AccountEvidenceScan> {
    let mut scan = AccountEvidenceScan::default();
    collect_codex_auth_evidence(source, root, &mut scan);
    collect_codex_telemetry_evidence(source, root, checkpoints, &mut scan)?;
    collect_codex_reset_history_evidence(source, root, &mut scan);
    collect_codex_login_evidence(source, root, &mut scan)?;
    dedupe_codex_account_evidence(&mut scan);
    Ok(scan)
}

pub(crate) fn collect_codex_auth_evidence(
    source: &SourceLocation,
    root: &Path,
    scan: &mut AccountEvidenceScan,
) {
    push_codex_auth_snapshot(source, &root.join("auth.json"), true, scan);
    // Multi-account setups keep the other logins next to the live one as
    // `auth-<label>.json` and switch by swapping files. Each is a genuine auth
    // artifact for a real past login, so it is read as a historical snapshot:
    // it proves the source was that account when the file was written, but it
    // is never current and its plan claims are dated, not live.
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut variant_paths = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("auth-") && name.ends_with(".json"))
        })
        .collect::<Vec<_>>();
    variant_paths.sort();
    for variant_path in variant_paths {
        push_codex_auth_snapshot(source, &variant_path, false, scan);
    }
}

pub(crate) fn push_codex_auth_snapshot(
    source: &SourceLocation,
    auth_path: &Path,
    is_current: bool,
    scan: &mut AccountEvidenceScan,
) {
    let Some(claims) = codex_auth_claims(auth_path) else {
        return;
    };
    // A historical file with no derivable timestamp would have to be dated
    // "now", which would assert a login that is not happening. Skip it.
    if !is_current && claims.authenticated_at.is_none() {
        return;
    }
    // Two different facts live in one file and they are not observed together.
    // `chatgpt_subscription_last_checked` moves when the plan claims are
    // revalidated, which can be long before the current login: signing out of
    // A and into B rewrites the account without refreshing it. Dating the
    // identity by that stale stamp said "this source was B" at a moment it was
    // still A, and `ends_source_attribution` turns an AuthSnapshot into a
    // source-wide boundary, so A's interval was truncated back to the last
    // subscription check and every event in between lost its account.
    let authenticated_at = claims.authenticated_at.unwrap_or_else(Utc::now);
    let plan_observed_at = claims
        .subscription_checked_at
        .or(claims.authenticated_at)
        .unwrap_or(authenticated_at);
    let observed_at = authenticated_at;
    let provider_account_id = provider_account_id_from_identity(
        CODEX_PROVIDER,
        claims.provider_user_id.as_deref(),
        claims.email.as_deref(),
    );
    let artifact_path_hash = hash_text(&canonical_display(auth_path));
    // The current file keeps its v1 fingerprint so re-scans stay idempotent;
    // variants add their path, since two of them can carry identical claims.
    let fingerprint_scope = if is_current {
        String::new()
    } else {
        format!("{artifact_path_hash}:")
    };
    // A historical variant proves a real past login and its plan claims, but
    // nothing can ever reopen an interval it would close: `ends_source_attribution`
    // demands a kind that can also restart attribution, and a swapped-out auth
    // file never becomes current again. Recording it as an AuthSnapshot made it
    // a source-wide boundary that left every later event permanently
    // unattributed, so it is recorded as the login it evidences instead.
    let identity_kind = if is_current {
        AccountEvidenceKind::AuthSnapshot
    } else {
        AccountEvidenceKind::LoginSuccess
    };
    let record_fingerprint = hash_text(&format!(
        "codex-auth-evidence.v1:{fingerprint_scope}{}:{}:{}:{}:{}:{}",
        claims.provider_user_id.as_deref().unwrap_or("none"),
        claims.email.as_deref().unwrap_or("none"),
        claims.plan_type.as_deref().unwrap_or("none"),
        claims.auth_mode.as_deref().unwrap_or("none"),
        claims
            .active_from
            .map_or_else(|| "none".to_string(), |value| value.to_rfc3339()),
        claims
            .active_until
            .map_or_else(|| "none".to_string(), |value| value.to_rfc3339()),
    ));
    scan.accounts.push(ObservedProviderAccount {
        provider_user_id: claims.provider_user_id.clone(),
        email: claims.email.clone(),
        plan_name: claims.plan_type.as_deref().map(normalize_plan_name),
        observed_at,
    });
    scan.identity_observations
        .push(AccountIdentityObservationV1 {
            schema_version: ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: account_identity_observation_id(
                &source.source_id,
                identity_kind,
                observed_at,
                &record_fingerprint,
            ),
            provider: CODEX_PROVIDER.to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: provider_account_id.clone(),
            provider_user_id_hash: claims.provider_user_id.as_deref().map(hash_text),
            email_hash: claims
                .email
                .as_deref()
                .map(normalize_email)
                .as_deref()
                .map(hash_text),
            conversation_id_hash: None,
            turn_id_hash: None,
            observed_at,
            evidence_kind: identity_kind,
            confidence: Confidence::High,
            auth_mode: claims.auth_mode.clone(),
            application_version: None,
            parser_version: CODEX_ACCOUNT_EVIDENCE_PARSER_VERSION.to_string(),
            artifact_kind: "auth_json".to_string(),
            artifact_path_hash: artifact_path_hash.clone(),
            record_fingerprint: record_fingerprint.clone(),
        });

    let plan_allowed = claims.auth_mode.as_deref().is_none_or(|mode| {
        !matches!(
            mode.trim().to_ascii_lowercase().as_str(),
            "api" | "api_key" | "apikey"
        )
    });
    if plan_allowed {
        if let Some(raw_plan_name) = claims.plan_type {
            let plan_name = normalize_plan_name(&raw_plan_name);
            let observation_id = account_plan_observation_id(
                &source.source_id,
                provider_account_id.as_ref(),
                &raw_plan_name,
                &plan_name,
                plan_observed_at,
                AccountEvidenceKind::AuthSnapshot,
            );
            scan.plan_observations.push(AccountPlanObservationV1 {
                schema_version: ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
                observation_id,
                provider: CODEX_PROVIDER.to_string(),
                source_id: source.source_id.clone(),
                provider_account_id,
                plan_name,
                raw_plan_name,
                observed_at: plan_observed_at,
                active_from: claims.active_from,
                active_until: claims.active_until,
                is_current_snapshot: is_current,
                evidence_kind: AccountEvidenceKind::AuthSnapshot,
                confidence: Confidence::High,
                parser_version: CODEX_ACCOUNT_EVIDENCE_PARSER_VERSION.to_string(),
                artifact_path_hash,
                record_fingerprint,
            });
        }
    }
}

pub(crate) fn dedupe_codex_account_evidence(scan: &mut AccountEvidenceScan) {
    let mut account_keys = HashSet::new();
    scan.accounts.retain(|account| {
        account_keys.insert((account.provider_user_id.clone(), account.email.clone()))
    });
    let mut identity_ids = HashSet::new();
    scan.identity_observations
        .retain(|observation| identity_ids.insert(observation.observation_id.clone()));
    collapse_codex_identity_observation_runs(scan);
    let mut plan_ids = HashSet::new();
    scan.plan_observations
        .retain(|observation| plan_ids.insert(observation.observation_id.clone()));
    let mut binding_ids = HashSet::new();
    scan.conversation_bindings
        .retain(|binding| binding_ids.insert(binding.binding_id.clone()));
}

/// Collapse consecutive telemetry/reload observations of the same identity.
///
/// A single conversation emits hundreds of telemetry events naming the same
/// account, and each one became a ledger row. The ledger is read back in full
/// on every reconcile, so that redundancy is paid for on every scan forever.
/// Only the run's endpoints carry information: the first observation is the
/// boundary and the last is the freshest confirmation. Everything between two
/// identical neighbours restates them. Runs are collapsed in time order, so
/// every alternation — the actual account-switch signal — survives exactly.
pub(crate) fn collapse_codex_identity_observation_runs(scan: &mut AccountEvidenceScan) {
    scan.identity_observations.sort_by(|left, right| {
        left.observed_at
            .cmp(&right.observed_at)
            .then_with(|| left.observation_id.cmp(&right.observation_id))
    });
    // Deliberately conversation-blind: parallel conversations interleave in
    // time, and a per-conversation key would break every run they straddle.
    // Per-conversation identity is carried by the bindings collection; the
    // ledger only has to preserve *which account, when*.
    let run_key = |observation: &AccountIdentityObservationV1| {
        (
            observation.evidence_kind,
            observation.provider_account_id.clone(),
            observation.email_hash.clone(),
        )
    };
    let collapsible = |observation: &AccountIdentityObservationV1| {
        matches!(
            observation.evidence_kind,
            AccountEvidenceKind::TelemetryIdentity | AccountEvidenceKind::AuthReload
        )
    };
    let mut keep = vec![true; scan.identity_observations.len()];
    let mut run_start: Option<usize> = None;
    for index in 0..=scan.identity_observations.len() {
        let continues_run = run_start.is_some_and(|start| {
            scan.identity_observations
                .get(index)
                .is_some_and(|observation| {
                    collapsible(observation)
                        && run_key(observation) == run_key(&scan.identity_observations[start])
                })
        });
        if continues_run {
            continue;
        }
        if let Some(start) = run_start.take() {
            // The run covers `start..index`; keep its first and last rows.
            let last = (index - 1).max(start + 1);
            for middle in &mut keep[start + 1..last] {
                *middle = false;
            }
        }
        if scan
            .identity_observations
            .get(index)
            .is_some_and(collapsible)
        {
            run_start = Some(index);
        }
    }
    let mut index = 0;
    scan.identity_observations.retain(|_| {
        let retained = keep[index];
        index += 1;
        retained
    });
}

pub(crate) fn codex_auth_snapshot(root: &Path) -> Option<VerifiedSourceState> {
    let claims = codex_auth_claims(&root.join("auth.json"))?;
    let plan_name = claims.plan_type.as_deref().map(display_codex_plan_name);
    let verified_at = claims.subscription_checked_at.or(claims.authenticated_at);
    Some(VerifiedSourceState {
        provider_user_id: claims.provider_user_id,
        email: claims.email,
        account_label: None,
        plan_name,
        authenticated_at: claims.authenticated_at,
        verified_at,
        // Provider plan detection is intentionally separate from user-entered billing facts.
        subscription: None,
    })
}

pub(crate) fn display_codex_plan_name(plan_type: &str) -> String {
    match plan_type.trim().to_ascii_lowercase().as_str() {
        "plus" => "Plus".to_string(),
        "pro" => "Pro".to_string(),
        "free" => "Free".to_string(),
        other => other
            .split(['_', '-', ' '])
            .filter(|part| !part.is_empty())
            .map(|part| {
                let mut chars = part.chars();
                let Some(first) = chars.next() else {
                    return String::new();
                };
                format!(
                    "{}{}",
                    first.to_ascii_uppercase(),
                    chars.as_str().to_ascii_lowercase()
                )
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

pub(crate) fn jwt_payload_value(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = decode_base64_url(payload)?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn decode_base64_url(value: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut bit_count = 0u8;
    let mut out = Vec::new();
    for byte in value.bytes() {
        if byte == b'=' {
            break;
        }
        let six = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32;
        bits = (bits << 6) | six;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            out.push(((bits >> bit_count) & 0xff) as u8);
        }
    }
    Some(out)
}

pub(crate) fn codex_session_id(usage_root: &Path, path: &Path) -> String {
    path.strip_prefix(usage_root)
        .unwrap_or(path)
        .with_extension("")
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests;
