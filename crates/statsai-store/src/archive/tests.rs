use super::*;
use statsai_core::{
    archive_content_id, archive_conversation_id, archive_item_id, ArchiveContentKind,
};

fn sample_conversation() -> ArchiveConversation {
    let native_id = "thread-1";
    let conversation_id = archive_conversation_id("codex", native_id);
    let item_id = archive_item_id("codex", native_id, Some("message-1"), 0, "hello");
    ArchiveConversation {
        schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
        conversation_id,
        provider: "codex".to_string(),
        source_id: SourceId("source-1".to_string()),
        native_conversation_id: native_id.to_string(),
        title: Some("Example thread".to_string()),
        project: None,
        started_at: Some(DateTime::<Utc>::UNIX_EPOCH),
        updated_at: Some(DateTime::<Utc>::UNIX_EPOCH),
        completeness: ArchiveCompleteness::Complete,
        missing_content_count: 0,
        missing_content_scope_id: None,
        discarded_source_record_ids: Vec::new(),
        superseded_conversation_ids: Vec::new(),
        items: vec![ArchiveItem {
            item_id: item_id.clone(),
            native_item_id: Some("message-1".to_string()),
            source_record_id: Some("line:1".to_string()),
            ordinal: 0,
            kind: ArchiveItemKind::Message,
            role: Some(ArchiveRole::User),
            created_at: Some(DateTime::<Utc>::UNIX_EPOCH),
            model: None,
            tool_name: None,
            tool_call_id: None,
            status: None,
            usage: None,
            parts_authoritative: true,
            parts: vec![
                ArchiveContentPart::text(
                    archive_content_id(&item_id, 0),
                    0,
                    ArchiveContentKind::Text,
                    "hello searchable archive".to_string(),
                ),
                ArchiveContentPart::binary(
                    archive_content_id(&item_id, 1),
                    1,
                    ArchiveContentKind::Image,
                    Some("image/png".to_string()),
                    Some("image.png".to_string()),
                    BASE64.encode([0, 1, 2, 255]),
                )
                .unwrap(),
            ],
        }],
    }
}

#[test]
fn archive_round_trips_text_and_binary_and_is_searchable() {
    let store = Store::in_memory().expect("store");
    let conversation = sample_conversation();
    let result = store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("upsert");
    assert_eq!(result.binary_bytes, 4);

    let restored = store
        .archive_conversation(&conversation.conversation_id)
        .expect("read")
        .expect("conversation");
    assert_eq!(restored, conversation);
    let privacy_view = store
        .archive_conversation_for_privacy(&conversation.conversation_id)
        .expect("read privacy view")
        .expect("privacy conversation");
    assert_eq!(
        privacy_view.items[0].parts[0].text.as_deref(),
        Some("hello searchable archive")
    );
    assert!(privacy_view.items[0].parts[1].data_base64.is_none());
    let hits = store.search_archive("searchable", 10).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].conversation_id, conversation.conversation_id);
}

#[test]
fn source_record_reclassification_removes_the_stale_item_copy() {
    let store = Store::in_memory().expect("store");
    let original = sample_conversation();
    store
        .upsert_archive_conversations(std::slice::from_ref(&original))
        .expect("original upsert");

    let mut corrected = original.clone();
    corrected.native_conversation_id = "thread-1:agent:alpha".to_string();
    corrected.conversation_id = archive_conversation_id("codex", &corrected.native_conversation_id);
    corrected.superseded_conversation_ids = vec![original.conversation_id.clone()];
    let item = &mut corrected.items[0];
    item.item_id = archive_item_id(
        "codex",
        &corrected.native_conversation_id,
        item.native_item_id.as_deref(),
        item.ordinal,
        "hello",
    );
    item.parts = vec![ArchiveContentPart::text(
        archive_content_id(&item.item_id, 0),
        0,
        ArchiveContentKind::Text,
        "hello searchable archive".to_string(),
    )];
    store
        .upsert_archive_conversations(std::slice::from_ref(&corrected))
        .expect("corrected upsert");

    assert!(store
        .archive_conversation(&original.conversation_id)
        .expect("read stale conversation")
        .is_none());
    let restored = store
        .archive_conversation(&corrected.conversation_id)
        .expect("read corrected conversation")
        .expect("corrected conversation");
    assert_eq!(restored.items, corrected.items);
    let hits = store.search_archive("searchable", 10).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].conversation_id, corrected.conversation_id);
    assert_eq!(store.archive_stats().expect("stats").items, 1);
}

#[test]
fn discarded_source_record_removes_previously_archived_content() {
    let store = Store::in_memory().expect("store");
    let original = sample_conversation();
    store
        .upsert_archive_conversations(std::slice::from_ref(&original))
        .expect("original upsert");

    let mut corrected = original.clone();
    corrected.items.clear();
    corrected.discarded_source_record_ids = vec!["line:1".to_string()];
    store
        .upsert_archive_conversations(&[corrected])
        .expect("discarded record upsert");

    let restored = store
        .archive_conversation(&original.conversation_id)
        .expect("read conversation")
        .expect("conversation metadata");
    assert!(restored.items.is_empty());
    assert!(store.search_archive("searchable", 10).unwrap().is_empty());
    assert_eq!(store.archive_stats().expect("stats").items, 0);
}

#[test]
fn superseded_conversation_with_remaining_items_is_preserved() {
    let store = Store::in_memory().expect("store");
    let mut original = sample_conversation();
    let mut retained_item = original.items[0].clone();
    retained_item.item_id = archive_item_id(
        "codex",
        &original.native_conversation_id,
        Some("message-2"),
        1,
        "retained",
    );
    retained_item.native_item_id = Some("message-2".to_string());
    retained_item.source_record_id = Some("line:2".to_string());
    retained_item.ordinal = 1;
    retained_item.parts = vec![ArchiveContentPart::text(
        archive_content_id(&retained_item.item_id, 0),
        0,
        ArchiveContentKind::Text,
        "retained parent message".to_string(),
    )];
    original.items.push(retained_item.clone());
    store
        .upsert_archive_conversations(std::slice::from_ref(&original))
        .expect("original upsert");

    let mut corrected = sample_conversation();
    corrected.native_conversation_id = "thread-1:agent:alpha".to_string();
    corrected.conversation_id = archive_conversation_id("codex", &corrected.native_conversation_id);
    corrected.superseded_conversation_ids = vec![original.conversation_id.clone()];
    let item = &mut corrected.items[0];
    item.item_id = archive_item_id(
        "codex",
        &corrected.native_conversation_id,
        item.native_item_id.as_deref(),
        item.ordinal,
        "hello",
    );
    item.parts[0].content_id = archive_content_id(&item.item_id, 0);
    item.parts[1].content_id = archive_content_id(&item.item_id, 1);
    store
        .upsert_archive_conversations(&[corrected])
        .expect("corrected upsert");

    let parent = store
        .archive_conversation(&original.conversation_id)
        .expect("read parent")
        .expect("parent retained");
    assert_eq!(parent.items, [retained_item]);
}

#[test]
fn archive_size_metrics_count_utf8_bytes() {
    let store = Store::in_memory().expect("store");
    let mut conversation = sample_conversation();
    let text = "\u{00e9}\u{1f642}";
    conversation.items[0].parts[0] = ArchiveContentPart::text(
        archive_content_id(&conversation.items[0].item_id, 0),
        0,
        ArchiveContentKind::Text,
        text.to_string(),
    );
    store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("upsert");

    let summary = store
        .list_archive_conversations(None, 10)
        .expect("list")
        .pop()
        .expect("summary");
    let stats = store.archive_stats().expect("stats");
    assert_eq!(stats.text_bytes, text.len() as u64);
    assert_eq!(stats.binary_bytes, 4);
    assert_eq!(summary.content_bytes, text.len() as u64 + 4);
}

#[test]
fn archive_list_accepts_an_unbounded_host_limit() {
    let store = Store::in_memory().expect("store");
    let conversation = sample_conversation();
    store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("upsert");

    let summaries = store
        .list_archive_conversations(None, usize::MAX)
        .expect("list without a practical limit");

    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].conversation_id, conversation.conversation_id);
}

/// A file already imported under an earlier reconstruction must be read
/// again, or the archive keeps results the current code would never
/// produce. This is the whole purpose of the import revision, and it only
/// works if the revision takes part in the cached signature.
#[test]
fn entries_imported_under_an_earlier_revision_are_pending() {
    let store = Store::in_memory().expect("store");
    let conversation = sample_conversation();
    let entry = ScanFileStateEntry {
        cache_key: "/archive/thread.jsonl".to_string(),
        cache_signature: "record-signature".to_string(),
    };
    store
        .store_archive_scan_with_code_changes(
            &conversation.source_id,
            std::slice::from_ref(&conversation),
            std::slice::from_ref(&entry),
            &[],
            &[],
            CoverageStatus::Unavailable,
            &[],
        )
        .expect("store archive scan");
    assert!(
        store
            .pending_archive_import_entries(&conversation.source_id, std::slice::from_ref(&entry))
            .expect("unchanged entry")
            .is_empty(),
        "an unchanged file was re-read"
    );

    // The same file, recorded as an earlier revision had left it.
    store
        .conn
        .execute(
            "UPDATE archive_import_state SET cache_signature = ?1
                 WHERE source_id = ?2 AND cache_key = ?3",
            params![
                statsai_core::hash_text(&format!("archive.v7:{}", entry.cache_signature)),
                &conversation.source_id.0,
                &entry.cache_key,
            ],
        )
        .expect("record an earlier revision");

    assert_eq!(
        store
            .pending_archive_import_entries(&conversation.source_id, &[entry])
            .expect("earlier revision")
            .len(),
        1,
        "a file imported under an earlier revision was not re-read"
    );
}

#[test]
fn artifact_metadata_changes_make_cached_archive_entry_pending() {
    let dir = tempfile::tempdir().expect("temp dir");
    let artifact = dir.path().join("artifact.bin");
    let store = Store::in_memory().expect("store");
    let conversation = sample_conversation();
    let entry = ScanFileStateEntry {
        cache_key: "/archive/thread.jsonl".to_string(),
        cache_signature: "record-signature".to_string(),
    };
    let dependency = ArchiveArtifactDependency {
        cache_key: entry.cache_key.clone(),
        path: artifact.clone(),
        metadata_signature: archive_artifact_metadata_signature(&artifact),
    };
    store
        .store_archive_scan_with_code_changes(
            &conversation.source_id,
            std::slice::from_ref(&conversation),
            std::slice::from_ref(&entry),
            std::slice::from_ref(&dependency),
            &[],
            CoverageStatus::Unavailable,
            &[],
        )
        .expect("store archive scan");
    assert!(store
        .pending_archive_import_entries(&conversation.source_id, std::slice::from_ref(&entry))
        .expect("unchanged dependencies")
        .is_empty());

    std::fs::write(&artifact, [0, 1, 2, 255]).expect("create artifact");
    assert_eq!(
        store
            .pending_archive_import_entries(&conversation.source_id, &[entry])
            .expect("changed dependencies")
            .len(),
        1
    );
}

#[test]
fn partial_imports_preserve_earliest_start_and_latest_update() {
    let store = Store::in_memory().expect("store");
    let older = DateTime::<Utc>::from_timestamp(100, 0).unwrap();
    let newer = DateTime::<Utc>::from_timestamp(200, 0).unwrap();
    let mut conversation = sample_conversation();
    conversation.started_at = Some(newer);
    conversation.updated_at = Some(newer);
    store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("newer import");

    let mut earlier_import = conversation.clone();
    earlier_import.started_at = Some(older);
    earlier_import.updated_at = Some(older);
    earlier_import.items.clear();
    store
        .upsert_archive_conversations(&[earlier_import])
        .expect("earlier import");

    let restored = store
        .archive_conversation(&conversation.conversation_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.started_at, Some(older));
    assert_eq!(restored.updated_at, Some(newer));
}

#[test]
fn rescanning_is_idempotent_and_does_not_remove_old_items() {
    let store = Store::in_memory().expect("store");
    let conversation = sample_conversation();
    store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("first upsert");
    let mut update = conversation.clone();
    update.items.clear();
    store
        .upsert_archive_conversations(&[update])
        .expect("metadata update");

    let restored = store
        .archive_conversation(&conversation.conversation_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.items.len(), 1);
    assert_eq!(restored.completeness, ArchiveCompleteness::Complete);
    assert_eq!(store.archive_stats().unwrap().conversations, 1);
}

/// Re-importing an unchanged conversation must not rewrite its content.
///
/// The rows and the search index are identical either way, so the only
/// thing a rewrite produces is work — and on a provider whose whole archive
/// re-imports whenever any part of it changes, that work is the import.
#[test]
fn unchanged_content_is_not_rewritten_on_reimport() {
    let store = Store::in_memory().expect("store");
    let conversation = sample_conversation();
    let first = store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("first upsert");
    assert_eq!(first.content_parts, 2);
    assert_eq!(first.binary_bytes, 4);

    let repeated = store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("identical re-import");

    assert_eq!(repeated.content_parts, 0, "identical parts were rewritten");
    assert_eq!(repeated.binary_bytes, 0);
    // Skipping the write must not lose content or searchability.
    let restored = store
        .archive_conversation(&conversation.conversation_id)
        .expect("read")
        .expect("conversation");
    assert_eq!(restored, conversation);
    assert_eq!(store.search_archive("searchable", 10).unwrap().len(), 1);
    let stats = store.archive_stats().expect("stats");
    assert_eq!(stats.binary_parts, 1);
    assert_eq!(stats.text_parts, 1);
}

/// A better reconstruction can correct how content is described without
/// changing a byte of it. Skipping the write because the bytes match would
/// leave the archive holding the description the old parser produced.
#[test]
fn corrected_metadata_is_stored_even_when_the_content_is_unchanged() {
    let store = Store::in_memory().expect("store");
    let conversation = sample_conversation();
    store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("first upsert");

    let mut corrected = conversation.clone();
    let text_part = &mut corrected.items[0].parts[0];
    text_part.kind = ArchiveContentKind::Json;
    let binary_part = &mut corrected.items[0].parts[1];
    binary_part.mime_type = Some("image/webp".to_string());
    binary_part.name = Some("corrected.webp".to_string());
    let result = store
        .upsert_archive_conversations(std::slice::from_ref(&corrected))
        .expect("metadata correction");

    assert_eq!(result.content_parts, 2, "corrections were skipped");
    let restored = store
        .archive_conversation(&conversation.conversation_id)
        .expect("read")
        .expect("conversation");
    assert_eq!(restored.items[0].parts, corrected.items[0].parts);
    // The content itself is unchanged, so it stays searchable.
    assert_eq!(store.search_archive("searchable", 10).unwrap().len(), 1);
}

#[test]
fn base64_decoded_len_matches_the_decoded_payload() {
    for bytes in [
        [0u8].as_slice(),
        [0, 1].as_slice(),
        [0, 1, 2].as_slice(),
        [0, 1, 2, 255].as_slice(),
        [7; 61].as_slice(),
    ] {
        let encoded = BASE64.encode(bytes);
        assert_eq!(
            base64_decoded_len(&encoded),
            bytes.len() as u64,
            "length of {encoded}"
        );
    }
    assert_eq!(base64_decoded_len(""), 0);
}

#[test]
fn reduced_item_rescan_preserves_richer_existing_parts() {
    let store = Store::in_memory().expect("store");
    let conversation = sample_conversation();
    store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("first upsert");

    let mut reduced = conversation.clone();
    let mut truncated = ArchiveContentPart::text(
        archive_content_id(&reduced.items[0].item_id, 0),
        0,
        ArchiveContentKind::Text,
        "short".to_string(),
    );
    truncated.content_hash = conversation.items[0].parts[0].content_hash.clone();
    truncated.original_bytes = conversation.items[0].parts[0].original_bytes;
    truncated.truncated = true;
    reduced.items[0].parts_authoritative = false;
    reduced.items[0].parts = vec![truncated];
    let reduced_result = store
        .upsert_archive_conversations(&[reduced.clone()])
        .expect("reduced upsert");
    assert_eq!(reduced_result.content_parts, 0);

    reduced.items[0].parts.clear();
    store
        .upsert_archive_conversations(&[reduced])
        .expect("empty upsert");

    let restored = store
        .archive_conversation(&conversation.conversation_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.items[0].parts, conversation.items[0].parts);
}

#[test]
fn changed_shorter_content_replaces_stale_materialized_content() {
    let store = Store::in_memory().expect("store");
    let conversation = sample_conversation();
    store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("first upsert");

    let mut updated = conversation.clone();
    updated.items[0].parts[0] = ArchiveContentPart::text(
        archive_content_id(&updated.items[0].item_id, 0),
        0,
        ArchiveContentKind::Text,
        "short update".to_string(),
    );
    store
        .upsert_archive_conversations(&[updated.clone()])
        .expect("updated upsert");

    let restored = store
        .archive_conversation(&conversation.conversation_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.items[0].parts, updated.items[0].parts);
    assert!(store.search_archive("searchable", 10).unwrap().is_empty());
    assert_eq!(store.search_archive("short", 10).unwrap().len(), 1);
}

#[test]
fn changed_external_reference_replaces_stale_materialized_content() {
    let store = Store::in_memory().expect("store");
    let mut conversation = sample_conversation();
    conversation.items[0].kind = ArchiveItemKind::ToolResult;
    conversation.items[0].role = Some(ArchiveRole::Tool);
    store
        .upsert_archive_conversations(std::slice::from_ref(&conversation))
        .expect("materialized upsert");

    let mut secured = conversation.clone();
    let content_id = secured.items[0].parts[1].content_id.clone();
    let external_uri = "file:///tmp/untrusted-secret";
    secured.completeness = ArchiveCompleteness::Partial;
    secured.missing_content_count = 1;
    secured.items[0].parts_authoritative = false;
    secured.items[0].parts[1] = ArchiveContentPart {
        content_id,
        ordinal: 1,
        kind: ArchiveContentKind::File,
        mime_type: None,
        name: None,
        text: None,
        data_base64: None,
        external_uri: Some(external_uri.to_string()),
        content_hash: statsai_core::hash_text(external_uri),
        original_bytes: 0,
        truncated: false,
    };
    store
        .upsert_archive_conversations(&[secured])
        .expect("secured upsert");

    let restored = store
        .archive_conversation(&conversation.conversation_id)
        .unwrap()
        .unwrap();
    let artifact = &restored.items[0].parts[1];
    assert_eq!(artifact.external_uri.as_deref(), Some(external_uri));
    assert!(artifact.data_base64.is_none());
    assert_eq!(store.archive_stats().unwrap().binary_parts, 0);
}

#[test]
fn authoritative_item_update_removes_obsolete_parts() {
    let store = Store::in_memory().expect("store");
    let mut original = sample_conversation();
    let item = &mut original.items[0];
    item.parts.push(ArchiveContentPart::text(
        archive_content_id(&item.item_id, 2),
        2,
        ArchiveContentKind::Text,
        "obsolete searchable attachment note".to_string(),
    ));
    store
        .upsert_archive_conversations(std::slice::from_ref(&original))
        .expect("first upsert");

    let mut updated = original.clone();
    updated.items[0].parts.truncate(1);
    store
        .upsert_archive_conversations(std::slice::from_ref(&updated))
        .expect("authoritative update");

    let restored = store
        .archive_conversation(&original.conversation_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.items[0].parts, updated.items[0].parts);
    assert!(store.search_archive("obsolete", 10).unwrap().is_empty());
    assert_eq!(store.archive_stats().unwrap().binary_parts, 0);
}

#[test]
fn sparse_item_rescan_preserves_existing_optional_metadata() {
    let store = Store::in_memory().expect("store");
    let mut enriched = sample_conversation();
    let item = &mut enriched.items[0];
    item.kind = ArchiveItemKind::ToolResult;
    item.role = Some(ArchiveRole::Tool);
    item.model = Some(ModelInfo {
        name: Some("gpt-test".to_string()),
        normalized_name: Some("gpt-test".to_string()),
        provider_model_id: Some("provider/gpt-test".to_string()),
        ..ModelInfo::default()
    });
    item.tool_name = Some("shell".to_string());
    item.tool_call_id = Some("call-1".to_string());
    item.status = Some("completed".to_string());
    item.usage = Some(UsageCounts {
        input_tokens: Some(12),
        output_tokens: Some(3),
        total_tokens: Some(15),
        ..UsageCounts::default()
    });
    store
        .upsert_archive_conversations(std::slice::from_ref(&enriched))
        .expect("enriched upsert");

    let mut sparse = enriched.clone();
    let item = &mut sparse.items[0];
    item.native_item_id = None;
    item.source_record_id = None;
    item.role = None;
    item.created_at = None;
    item.model = None;
    item.tool_name = None;
    item.tool_call_id = None;
    item.status = None;
    item.usage = None;
    item.parts_authoritative = false;
    item.parts.clear();
    store
        .upsert_archive_conversations(&[sparse])
        .expect("sparse upsert");

    let restored = store
        .archive_conversation(&enriched.conversation_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.items[0], enriched.items[0]);
}

#[test]
fn repaired_content_clears_partial_completeness() {
    let store = Store::in_memory().expect("store");
    let complete = sample_conversation();
    let mut partial = complete.clone();
    partial.completeness = ArchiveCompleteness::Partial;
    partial.missing_content_count = 1;
    partial.items[0].parts = vec![ArchiveContentPart {
        content_id: archive_content_id(&partial.items[0].item_id, 0),
        ordinal: 0,
        kind: ArchiveContentKind::Image,
        mime_type: Some("image/png".to_string()),
        name: None,
        text: None,
        data_base64: None,
        external_uri: Some("https://example.test/image.png".to_string()),
        content_hash: statsai_core::hash_text("https://example.test/image.png"),
        original_bytes: 0,
        truncated: false,
    }];

    store
        .upsert_archive_conversations(&[partial])
        .expect("partial upsert");
    store
        .upsert_archive_conversations(std::slice::from_ref(&complete))
        .expect("repair upsert");

    let restored = store
        .archive_conversation(&complete.conversation_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.completeness, ArchiveCompleteness::Complete);
    assert_eq!(restored.missing_content_count, 0);
    assert!(restored.items[0]
        .parts
        .iter()
        .all(|part| part.external_uri.is_none()));
}

#[test]
fn non_materialized_missing_content_survives_nonempty_rescan() {
    let store = Store::in_memory().expect("store");
    let mut partial = sample_conversation();
    partial.completeness = ArchiveCompleteness::Partial;
    partial.missing_content_count = 1;
    partial.missing_content_scope_id = Some("scope-a".to_string());
    store
        .upsert_archive_conversations(&[partial.clone()])
        .expect("partial upsert");

    let mut update = partial;
    update.completeness = ArchiveCompleteness::Complete;
    update.missing_content_count = 0;
    update.missing_content_scope_id = Some("scope-b".to_string());
    let item = &mut update.items[0];
    item.item_id = archive_item_id("codex", "thread-1", Some("message-2"), 1, "new");
    item.native_item_id = Some("message-2".to_string());
    item.source_record_id = Some("line:2".to_string());
    item.ordinal = 1;
    item.parts = vec![ArchiveContentPart::text(
        archive_content_id(&item.item_id, 0),
        0,
        ArchiveContentKind::Text,
        "newly retained message".to_string(),
    )];
    store
        .upsert_archive_conversations(&[update])
        .expect("nonempty rescan");

    let restored = store
        .archive_conversation(&archive_conversation_id("codex", "thread-1"))
        .unwrap()
        .unwrap();
    assert_eq!(restored.items.len(), 2);
    assert_eq!(restored.completeness, ArchiveCompleteness::Partial);
    assert_eq!(restored.missing_content_count, 1);
}

#[test]
fn repaired_non_materialized_missing_content_clears_partial_completeness() {
    let store = Store::in_memory().expect("store");
    let mut partial = sample_conversation();
    partial.completeness = ArchiveCompleteness::Partial;
    partial.missing_content_count = 1;
    partial.missing_content_scope_id = Some("scope-a".to_string());
    store
        .upsert_archive_conversations(&[partial.clone()])
        .expect("partial upsert");

    partial.completeness = ArchiveCompleteness::Complete;
    partial.missing_content_count = 0;
    store
        .upsert_archive_conversations(&[partial])
        .expect("repaired upsert");

    let restored = store
        .archive_conversation(&archive_conversation_id("codex", "thread-1"))
        .unwrap()
        .unwrap();
    assert_eq!(restored.completeness, ArchiveCompleteness::Complete);
    assert_eq!(restored.missing_content_count, 0);
}
