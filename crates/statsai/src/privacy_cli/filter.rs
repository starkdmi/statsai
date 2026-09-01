use super::*;

#[derive(Debug, Serialize)]
pub(crate) struct FilterSummary {
    selected: u64,
    filtered: u64,
    unchanged: u64,
    failed: u64,
    unprocessed: u64,
    findings: u64,
    replacements: BTreeMap<String, u64>,
    detector_findings: BTreeMap<String, u64>,
    cross_detector_overlaps: u64,
    detection_passes: u64,
    preview: bool,
}

pub(crate) struct FilterCandidate {
    conversation_id: String,
    input_fingerprint: String,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn filter(
    store: &Store,
    store_path: &Path,
    provider: Option<&str>,
    conversation_id: Option<&str>,
    force: bool,
    preview: bool,
    json_output: bool,
    verbose: bool,
) -> Result<()> {
    let provider = canonical_provider(provider)?;
    eprintln!("privacy runtime: verifying configured files");
    let config = load_runtime(store_path)?;
    let metadata = policy_metadata(&config);
    let policy_fingerprint = privacy_policy_fingerprint(&metadata);
    let summaries = if let Some(conversation_id) = conversation_id {
        let conversation = store
            .archive_conversation_for_privacy(conversation_id)?
            .with_context(|| format!("archived conversation not found: {conversation_id}"))?;
        if provider.is_some_and(|provider| provider != conversation.provider) {
            bail!("conversation does not match the provider filter")
        }
        vec![conversation_id.to_string()]
    } else {
        store
            .list_archive_conversations(provider, usize::MAX)?
            .into_iter()
            .map(|summary| summary.conversation_id)
            .collect()
    };
    let mut summary = FilterSummary {
        selected: summaries.len() as u64,
        filtered: 0,
        unchanged: 0,
        failed: 0,
        unprocessed: 0,
        findings: 0,
        replacements: BTreeMap::new(),
        detector_findings: BTreeMap::new(),
        cross_detector_overlaps: 0,
        detection_passes: 0,
        preview,
    };
    if summaries.is_empty() {
        print_filter_summary(&summary, json_output)?;
        return Ok(());
    }
    let key = if preview {
        [0u8; 32]
    } else {
        load_or_initialize_pseudonym_key(store, store_path)?
    };
    let mut candidates = Vec::with_capacity(summaries.len());
    for conversation_id in summaries {
        let conversation = store
            .archive_conversation_for_privacy(&conversation_id)?
            .context("archived conversation disappeared during privacy preflight")?;
        let input_fingerprint = archive_privacy_input_fingerprint(&conversation)?;
        if conversation.completeness != ArchiveCompleteness::Complete {
            summary.failed += 1;
            if !preview {
                store.record_privacy_failure(&PrivacyFailureRecord {
                    conversation_id,
                    input_fingerprint,
                    policy_fingerprint: policy_fingerprint.clone(),
                    failed_stage: "input".to_string(),
                    error_code: "archive_partial".to_string(),
                    attempted_at: Utc::now(),
                })?;
            }
            continue;
        }
        if !force
            && filtered_conversation_is_current(
                store,
                &conversation_id,
                &input_fingerprint,
                &policy_fingerprint,
            )?
        {
            summary.unchanged += 1;
            continue;
        }
        candidates.push(FilterCandidate {
            conversation_id,
            input_fingerprint,
        });
    }
    if candidates.is_empty() {
        return finish_filter_summary(&mut summary, json_output);
    }
    eprintln!("privacy runtime: starting local detectors");
    let mut detectors = match detector_set(&config, verbose) {
        Ok(detectors) => detectors,
        Err(error) => {
            summary.failed += candidates.len() as u64;
            if !preview {
                let error_code = startup_error_code(&error).to_string();
                record_candidate_failures(
                    store,
                    &candidates,
                    &policy_fingerprint,
                    "detector_startup",
                    &error_code,
                )?;
            }
            summary.unprocessed = summary
                .selected
                .saturating_sub(summary.filtered + summary.unchanged + summary.failed);
            print_filter_summary(&summary, json_output)?;
            return Err(error.context("start local privacy detectors"));
        }
    };
    eprintln!("privacy runtime: detectors ready");
    let mut preview_aliases = BTreeMap::<(PrivacyCategory, String), u64>::new();
    let mut preview_counts = BTreeMap::<PrivacyCategory, u64>::new();
    let candidate_count = candidates.len();
    for (index, candidate) in candidates.iter().enumerate() {
        let conversation_id = &candidate.conversation_id;
        let report_progress =
            verbose || candidate_count == 1 || index % 25 == 0 || index + 1 == candidate_count;
        let started = Instant::now();
        if report_progress {
            eprintln!(
                "privacy filtering {}/{}: {}",
                index + 1,
                candidate_count,
                conversation_id
            );
        }
        let conversation = store
            .archive_conversation_for_privacy(conversation_id)?
            .context("archived conversation disappeared during filtering")?;
        if conversation.completeness != ArchiveCompleteness::Complete {
            summary.failed += 1;
            if !preview {
                store.record_privacy_failure(&PrivacyFailureRecord {
                    conversation_id: conversation_id.clone(),
                    input_fingerprint: archive_privacy_input_fingerprint(&conversation)?,
                    policy_fingerprint: policy_fingerprint.clone(),
                    failed_stage: "input".to_string(),
                    error_code: "archive_partial".to_string(),
                    attempted_at: Utc::now(),
                })?;
            }
            if report_progress {
                eprintln!(
                    "privacy failed {}/{} in {:.1}s: archive is partial",
                    index + 1,
                    candidate_count,
                    started.elapsed().as_secs_f64()
                );
            }
            continue;
        }
        let input_fingerprint = archive_privacy_input_fingerprint(&conversation)?;
        let result = if preview {
            filter_archive_conversation(
                &conversation,
                dataset_key(&key, conversation_id),
                &mut detectors,
                |category, value| {
                    let normalized = normalize_private_value(category, value);
                    let digest = hmac_digest(&key, category.as_str(), &normalized);
                    let lookup = (category, digest);
                    if let Some(alias) = preview_aliases.get(&lookup) {
                        return Ok(*alias);
                    }
                    let next = preview_counts.entry(category).or_default();
                    *next += 1;
                    preview_aliases.insert(lookup, *next);
                    Ok(*next)
                },
            )
        } else {
            filter_archive_conversation(
                &conversation,
                dataset_key(&key, conversation_id),
                &mut detectors,
                |category, value| {
                    let normalized = normalize_private_value(category, value);
                    let digest = hmac_digest(&key, category.as_str(), &normalized);
                    store
                        .resolve_privacy_pseudonym(category.as_str(), &digest)
                        .map_err(|_| PrivacyError::PseudonymStore)
                },
            )
        };
        match result {
            Ok(result) => {
                summary.filtered += 1;
                summary.findings += result.findings.len() as u64;
                summary.cross_detector_overlaps +=
                    result.detector_observations.cross_detector_overlaps;
                summary.detection_passes += result.detector_observations.detection_passes;
                for (detector, count) in result.detector_observations.findings_by_detector {
                    *summary
                        .detector_findings
                        .entry(detector.as_str().to_string())
                        .or_default() += count;
                }
                for finding in &result.findings {
                    *summary
                        .replacements
                        .entry(finding.category.as_str().to_string())
                        .or_default() += 1;
                }
                if !preview {
                    let payload = serde_json::to_string(&result.conversation)?;
                    let records = result
                        .findings
                        .into_iter()
                        .map(|finding| PrivacyFindingRecord {
                            field_path: finding.field_path,
                            start: finding.start,
                            end: finding.end,
                            category: finding.category.as_str().to_string(),
                            detector: finding.detector.as_str().to_string(),
                            confidence: finding
                                .confidence
                                .map(|confidence| confidence.as_str().to_string()),
                            replacement: finding.replacement,
                        })
                        .collect::<Vec<_>>();
                    store.write_filtered_conversation(
                        &FilteredConversationRecord {
                            conversation_id: conversation_id.clone(),
                            dataset_key: result.conversation.dataset_key,
                            input_fingerprint: result.input_fingerprint,
                            policy_fingerprint: policy_fingerprint.clone(),
                            payload,
                            finding_count: records.len() as u64,
                            succeeded_at: Utc::now(),
                        },
                        &records,
                    )?;
                }
                if report_progress {
                    eprintln!(
                        "privacy filtered {}/{} in {:.1}s",
                        index + 1,
                        candidate_count,
                        started.elapsed().as_secs_f64()
                    );
                }
            }
            Err(error) => {
                let detector_unavailable = matches!(
                    &error,
                    PrivacyError::Io(_) | PrivacyError::Timeout | PrivacyError::Unavailable
                );
                summary.failed += 1;
                if !preview {
                    store.record_privacy_failure(&PrivacyFailureRecord {
                        conversation_id: conversation_id.clone(),
                        input_fingerprint,
                        policy_fingerprint: policy_fingerprint.clone(),
                        failed_stage: "filter".to_string(),
                        error_code: error.code().to_string(),
                        attempted_at: Utc::now(),
                    })?;
                }
                if report_progress {
                    eprintln!(
                        "privacy failed {}/{} in {:.1}s: {}",
                        index + 1,
                        candidate_count,
                        started.elapsed().as_secs_f64(),
                        error.code()
                    );
                }
                if verbose {
                    eprintln!("privacy detector detail: {error:?}");
                }
                if detector_unavailable {
                    let remaining = &candidates[index + 1..];
                    summary.failed += remaining.len() as u64;
                    if !preview && !remaining.is_empty() {
                        record_candidate_failures(
                            store,
                            remaining,
                            &policy_fingerprint,
                            "filter",
                            error.code(),
                        )?;
                    }
                    break;
                }
            }
        }
    }
    finish_filter_summary(&mut summary, json_output)
}

pub(crate) fn filtered_conversation_is_current(
    store: &Store,
    conversation_id: &str,
    input_fingerprint: &str,
    policy_fingerprint: &str,
) -> Result<bool> {
    let Some(record) = store.filtered_conversation(conversation_id)? else {
        return Ok(false);
    };
    Ok(record.input_fingerprint == input_fingerprint
        && record.policy_fingerprint == policy_fingerprint
        && !store.filtered_conversation_has_newer_failure(conversation_id, record.succeeded_at)?)
}

pub(crate) fn startup_error_code(error: &anyhow::Error) -> &str {
    error
        .downcast_ref::<PrivacyError>()
        .map_or("detector_startup", PrivacyError::code)
}

pub(crate) fn record_candidate_failures(
    store: &Store,
    candidates: &[FilterCandidate],
    policy_fingerprint: &str,
    failed_stage: &str,
    error_code: &str,
) -> Result<()> {
    let attempted_at = Utc::now();
    let failures = candidates
        .iter()
        .map(|candidate| PrivacyFailureRecord {
            conversation_id: candidate.conversation_id.clone(),
            input_fingerprint: candidate.input_fingerprint.clone(),
            policy_fingerprint: policy_fingerprint.to_string(),
            failed_stage: failed_stage.to_string(),
            error_code: error_code.to_string(),
            attempted_at,
        })
        .collect::<Vec<_>>();
    store.record_privacy_failures(&failures)
}

pub(crate) fn finish_filter_summary(summary: &mut FilterSummary, json_output: bool) -> Result<()> {
    summary.unprocessed = summary
        .selected
        .saturating_sub(summary.filtered + summary.unchanged + summary.failed);
    print_filter_summary(summary, json_output)?;
    if summary.failed > 0 {
        bail!(
            "privacy filtering failed closed for {} conversation(s)",
            summary.failed
        )
    }
    Ok(())
}

pub(crate) fn print_filter_summary(summary: &FilterSummary, json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(summary)?);
    } else {
        println!(
            "privacy filtering: selected={} filtered={} unchanged={} failed={} unprocessed={} findings={}{}",
            summary.selected,
            summary.filtered,
            summary.unchanged,
            summary.failed,
            summary.unprocessed,
            summary.findings,
            if summary.preview { " preview" } else { "" }
        );
        if !summary.replacements.is_empty() {
            println!(
                "replacements: {}",
                summary
                    .replacements
                    .iter()
                    .map(|(category, count)| format!("{category}={count}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        if !summary.detector_findings.is_empty() {
            println!(
                "detector findings before merge: {} overlaps={} passes={}",
                summary
                    .detector_findings
                    .iter()
                    .map(|(detector, count)| format!("{detector}={count}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                summary.cross_detector_overlaps,
                summary.detection_passes,
            );
        }
    }
    Ok(())
}
