use super::*;

pub(crate) struct ExportRecord {
    metadata: FilteredConversationMetadata,
    day: Option<String>,
}

pub(crate) fn export(
    store: &Store,
    store_path: &Path,
    format: &str,
    output: &Path,
    provider: Option<&str>,
) -> Result<()> {
    if format != "jsonl" {
        bail!("unsupported privacy export format: {format}")
    }
    let provider = canonical_provider(provider)?;
    eprintln!("privacy runtime: verifying configured files");
    let config = load_runtime(store_path)?;
    let metadata = policy_metadata(&config);
    let policy_fingerprint = privacy_policy_fingerprint(&metadata);
    let verifier = store.privacy_key_verifier()?;
    let key = load_pseudonym_key(store_path, verifier.as_deref())?
        .context("privacy pseudonym key is unavailable; filter the selected conversations first")?;
    validate_pseudonym_key_state(store, Some(&key))?;
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let conversation_count = store.with_read_snapshot(|store| {
        let summaries = store.list_archive_conversations(provider, usize::MAX)?;
        let mut records = Vec::with_capacity(summaries.len());
        for summary in summaries {
            let record = store
                .filtered_conversation_metadata(&summary.conversation_id)?
                .with_context(|| {
                    format!("conversation is not filtered: {}", summary.conversation_id)
                })?;
            let conversation = store
                .archive_conversation_for_privacy(&summary.conversation_id)?
                .context("archived conversation disappeared during export")?;
            if conversation.completeness != ArchiveCompleteness::Complete {
                bail!(
                    "archived conversation is partial: {}",
                    summary.conversation_id
                )
            }
            let input = archive_privacy_input_fingerprint(&conversation)?;
            if record.policy_fingerprint != policy_fingerprint || record.input_fingerprint != input
            {
                bail!(
                    "filtered conversation is stale: {}",
                    summary.conversation_id
                )
            }
            if store.filtered_conversation_has_newer_failure(
                &summary.conversation_id,
                record.succeeded_at,
            )? {
                bail!(
                    "filtered conversation has a newer failed attempt: {}",
                    summary.conversation_id
                )
            }
            records.push(ExportRecord {
                metadata: record,
                day: summary.started_at.or(summary.updated_at).map(|timestamp| {
                    format!(
                        "{:04}-{:02}-{:02}",
                        timestamp.year(),
                        timestamp.month(),
                        timestamp.day()
                    )
                }),
            });
        }
        records.sort_by(|left, right| {
            (&left.day, &left.metadata.dataset_key).cmp(&(&right.day, &right.metadata.dataset_key))
        });
        let manifest = FilteredDatasetManifest {
            schema_version: FILTERED_DATASET_SCHEMA_VERSION.to_string(),
            policy_fingerprint: policy_fingerprint.clone(),
            conversation_schema: FILTERED_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversations: records.len() as u64,
            pseudonym_namespace: pseudonym_namespace(&key),
            detectors: metadata.clone(),
        };
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer(&mut writer, &manifest)?;
        writer.write_all(b"\n")?;
        for record in &records {
            let payload = store
                .filtered_conversation_payload(&record.metadata)?
                .with_context(|| {
                    format!(
                        "filtered conversation changed during export: {}",
                        record.metadata.conversation_id
                    )
                })?;
            writer.write_all(payload.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        Ok(records.len())
    })?;
    temporary.as_file().sync_all()?;
    temporary.persist(output).map_err(|error| error.error)?;
    println!(
        "exported {} filtered conversations to {}",
        conversation_count,
        output.display()
    );
    Ok(())
}
