use super::support::*;
use super::*;

#[test]
fn scan_file_replacement_rolls_back_deletions_when_cache_update_fails() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-atomic-replacement"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let cache_key = "/tmp/codex-atomic-replacement/session.jsonl".to_string();
    let file_hash = hash_text(&cache_key);
    let mut old_event = test_store_event(&source, now, "old-record");
    old_event.parse_evidence = Some(statsai_core::ParseEvidence {
        event_key_version: "v1".to_string(),
        source_file_path_hash: Some(file_hash.clone()),
        source_line_number: Some(1),
        source_record_id: Some("old-record".to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::Unresolved,
    });
    let mut old_summary = test_store_summary(&source, now, 15);
    old_summary.parse_evidence = old_event.parse_evidence.clone();
    store.insert_event(&old_event).expect("old event");
    store.upsert_summary(&old_summary).expect("old summary");
    store
        .record_scan_file_entries(
            &source.source_id,
            &[ScanFileStateEntry {
                cache_key: cache_key.clone(),
                cache_signature: "old-signature".to_string(),
            }],
        )
        .expect("old cache entry");
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER fail_scan_cache_update
                 BEFORE UPDATE ON scan_file_state
                 WHEN NEW.cache_signature = 'fail-signature'
                 BEGIN
                   SELECT RAISE(FAIL, 'injected scan cache failure');
                 END;",
        )
        .expect("failure trigger");

    let replacement = ScanFileStateEntry {
        cache_key,
        cache_signature: "fail-signature".to_string(),
    };
    let error = store
        .replace_scan_file_records(ScanFileReplacement {
            source_id: &source.source_id,
            reconciled_file_hashes: &[file_hash],
            events: &[],
            summaries: &[],
            pending_entries: &[replacement],
            compatible_entries_to_upgrade: &[],
            removed_cache_keys: &[],
        })
        .expect_err("replacement should fail");

    assert!(error.to_string().contains("injected scan cache failure"));
    assert_eq!(store.event_count().expect("event count"), 1);
    assert_eq!(store.summary_count().expect("summary count"), 1);
    assert_eq!(
        store
            .scan_file_entries(&source.source_id)
            .expect("cache entries")[0]
            .cache_signature,
        "old-signature"
    );
}

#[test]
fn scan_update_rolls_back_nested_writes_when_cache_update_fails() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-atomic-scan"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let old_event = test_store_event(&source, Utc::now(), "old-record");
    store.insert_event(&old_event).expect("old event");
    let cache_key = "/tmp/codex-atomic-scan/session.jsonl".to_string();
    store
        .record_scan_file_entries(
            &source.source_id,
            &[ScanFileStateEntry {
                cache_key: cache_key.clone(),
                cache_signature: "old-signature".to_string(),
            }],
        )
        .expect("old cache entry");
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER fail_atomic_scan_cache_update
                 BEFORE UPDATE ON scan_file_state
                 WHEN NEW.cache_signature = 'fail-signature'
                 BEGIN
                   SELECT RAISE(FAIL, 'injected atomic scan cache failure');
                 END;",
        )
        .expect("failure trigger");

    let error = store
        .apply_scan_update(|store| {
            store.delete_events_for_sources(std::slice::from_ref(&source.source_id))?;
            store.record_scan_file_entries(
                &source.source_id,
                &[ScanFileStateEntry {
                    cache_key,
                    cache_signature: "fail-signature".to_string(),
                }],
            )?;
            Ok(())
        })
        .expect_err("scan update should fail");

    assert!(error
        .to_string()
        .contains("injected atomic scan cache failure"));
    assert_eq!(store.events().expect("events"), vec![old_event]);
    assert_eq!(
        store
            .scan_file_entries(&source.source_id)
            .expect("cache entries")[0]
            .cache_signature,
        "old-signature"
    );
}

#[test]
fn scan_file_state_tracks_only_changed_entries() {
    let store = Store::in_memory().expect("store");
    let source_id = SourceId("src_scan_cache".to_string());
    let first = vec![
        ScanFileStateEntry {
            cache_key: "/tmp/a.jsonl".to_string(),
            cache_signature: "sig-a-1".to_string(),
        },
        ScanFileStateEntry {
            cache_key: "/tmp/b.jsonl".to_string(),
            cache_signature: "sig-b-1".to_string(),
        },
    ];

    let pending = store
        .pending_scan_file_entries(&source_id, &first)
        .expect("initial pending");
    assert_eq!(pending, first);
    store
        .record_scan_file_entries(&source_id, &pending)
        .expect("record");

    let unchanged = store
        .pending_scan_file_entries(&source_id, &first)
        .expect("unchanged");
    assert!(unchanged.is_empty());

    let changed = vec![
        ScanFileStateEntry {
            cache_key: "/tmp/a.jsonl".to_string(),
            cache_signature: "sig-a-2".to_string(),
        },
        ScanFileStateEntry {
            cache_key: "/tmp/b.jsonl".to_string(),
            cache_signature: "sig-b-1".to_string(),
        },
        ScanFileStateEntry {
            cache_key: "/tmp/c.jsonl".to_string(),
            cache_signature: "sig-c-1".to_string(),
        },
    ];
    let pending = store
        .pending_scan_file_entries(&source_id, &changed)
        .expect("changed pending");
    assert_eq!(pending.len(), 2);
    assert!(pending
        .iter()
        .any(|entry| entry.cache_key == "/tmp/a.jsonl"));
    assert!(pending
        .iter()
        .any(|entry| entry.cache_key == "/tmp/c.jsonl"));
}

#[test]
fn scan_file_state_accepts_compatible_signatures() {
    let store = Store::in_memory().expect("store");
    let source_id = SourceId("src_scan_cache_compat".to_string());
    let legacy = ScanFileStateEntry {
        cache_key: "/tmp/a.jsonl".to_string(),
        cache_signature: "legacy-auth-signature".to_string(),
    };
    store
        .record_scan_file_entries(&source_id, std::slice::from_ref(&legacy))
        .expect("record legacy cache state");

    let current = ScanFileStateEntry {
        cache_key: legacy.cache_key.clone(),
        cache_signature: "current-signature".to_string(),
    };
    let compatible_signatures = HashMap::from([(
        current.cache_key.clone(),
        vec![legacy.cache_signature.clone()],
    )]);

    let selection = store
        .select_scan_file_state_entries_with_task_requirement_and_compatibility(
            &source_id,
            std::slice::from_ref(&current),
            false,
            &compatible_signatures,
        )
        .expect("compatible selection");

    assert!(selection.pending_entries.is_empty());
    assert_eq!(
        selection.compatible_entries_to_upgrade,
        vec![current.clone()]
    );

    store
        .upgrade_scan_file_entries(&source_id, &selection.compatible_entries_to_upgrade)
        .expect("upgrade compatible entries");

    let stored_entries = store
        .scan_file_entries(&source_id)
        .expect("stored scan file entries");
    assert_eq!(stored_entries, vec![current]);
}
