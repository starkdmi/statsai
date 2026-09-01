use super::support::*;
use super::*;

struct InterruptingArchiveAdapter;

impl ProviderAdapter for InterruptingArchiveAdapter {
    fn id(&self) -> &'static str {
        "interrupting-archive-test"
    }

    fn version(&self) -> &'static str {
        "0"
    }

    fn provider(&self) -> &'static str {
        "archive_test"
    }

    fn discover(&self) -> Vec<SourceLocation> {
        Vec::new()
    }

    fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        Ok(Vec::new())
    }

    fn scan(
        &self,
        _source: &SourceLocation,
        _options: &ScanOptions,
    ) -> Result<statsai_adapters::AdapterScan> {
        Ok(statsai_adapters::AdapterScan::default())
    }

    fn collect_archive(
        &self,
        _source: &SourceLocation,
        selected_cache_keys: Option<&HashSet<String>>,
    ) -> Result<statsai_adapters::ArchiveScan> {
        let selected = selected_cache_keys
            .and_then(|keys| keys.iter().next())
            .context("selected archive cache key")?;
        if selected == "second" {
            bail!("synthetic archive interruption");
        }
        let mut scan = statsai_adapters::ArchiveScan::default();
        scan.diagnostics.files_scanned = 1;
        Ok(scan)
    }
}

#[test]
fn archive_collection_commits_each_candidate_before_the_next() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "archive_test",
        "interrupting-archive-test",
        "0",
        Path::new("/tmp/archive-test"),
        LocationOrigin::Configured,
    );
    let candidates = [
        ScanCandidateFile {
            path: PathBuf::from("first"),
            cache_key: "first".to_string(),
            cache_signature: "signature-first".to_string(),
            compatible_cache_signatures: Vec::new(),
        },
        ScanCandidateFile {
            path: PathBuf::from("second"),
            cache_key: "second".to_string(),
            cache_signature: "signature-second".to_string(),
            compatible_cache_signatures: Vec::new(),
        },
    ];
    let entries = scan_file_state_entries(&candidates);

    let result = collect_archive_source_entries(
        &store,
        &InterruptingArchiveAdapter,
        &source,
        &candidates,
        &entries,
        false,
    );
    assert!(result.is_err());

    let pending = store
        .pending_archive_import_entries(&source.source_id, &entries)
        .expect("pending archive entries");
    assert_eq!(pending, vec![entries[1].clone()]);
}

#[test]
fn archive_group_parsing_preserves_file_order() {
    struct OrderedArchiveAdapter;

    impl ProviderAdapter for OrderedArchiveAdapter {
        fn id(&self) -> &'static str {
            "ordered-archive-test"
        }
        fn version(&self) -> &'static str {
            "0"
        }
        fn provider(&self) -> &'static str {
            "archive_test"
        }
        fn discover(&self) -> Vec<SourceLocation> {
            Vec::new()
        }
        fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
            Ok(Vec::new())
        }
        fn scan(
            &self,
            _source: &SourceLocation,
            _options: &ScanOptions,
        ) -> Result<statsai_adapters::AdapterScan> {
            Ok(statsai_adapters::AdapterScan::default())
        }
        fn collect_archive(
            &self,
            _source: &SourceLocation,
            selected_cache_keys: Option<&HashSet<String>>,
        ) -> Result<statsai_adapters::ArchiveScan> {
            let selected = selected_cache_keys
                .and_then(|keys| keys.iter().next())
                .context("selected archive cache key")?;
            // The earlier files are the slow ones, so a run that returned
            // results as they arrived would reorder them.
            let index: u64 = selected.parse().context("cache key index")?;
            std::thread::sleep(std::time::Duration::from_millis(40 - index * 4));
            let mut scan = statsai_adapters::ArchiveScan::default();
            scan.diagnostics.files_scanned = index;
            Ok(scan)
        }
    }

    let source = SourceLocation::local_adapter(
        "archive_test",
        "ordered-archive-test",
        "0",
        Path::new("/tmp/archive-order-test"),
        LocationOrigin::Configured,
    );
    let entries = (0..10)
        .map(|index| ScanFileStateEntry {
            cache_key: index.to_string(),
            cache_signature: format!("signature-{index}"),
        })
        .collect::<Vec<_>>();

    let scans = parse_archive_group(
        &OrderedArchiveAdapter,
        &source,
        &entries,
        &vec![0; entries.len()],
    );

    let order = scans
        .into_iter()
        .map(|scan| scan.expect("collected archive").diagnostics.files_scanned)
        .collect::<Vec<_>>();
    assert_eq!(order, (0..10).collect::<Vec<_>>());
}

#[test]
fn archive_group_parsing_stops_before_holding_too_much_content() {
    struct HeavyArchiveAdapter;

    impl ProviderAdapter for HeavyArchiveAdapter {
        fn id(&self) -> &'static str {
            "heavy-archive-test"
        }
        fn version(&self) -> &'static str {
            "0"
        }
        fn provider(&self) -> &'static str {
            "archive_test"
        }
        fn discover(&self) -> Vec<SourceLocation> {
            Vec::new()
        }
        fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
            Ok(Vec::new())
        }
        fn scan(
            &self,
            _source: &SourceLocation,
            _options: &ScanOptions,
        ) -> Result<statsai_adapters::AdapterScan> {
            Ok(statsai_adapters::AdapterScan::default())
        }
        fn collect_archive(
            &self,
            _source: &SourceLocation,
            _selected_cache_keys: Option<&HashSet<String>>,
        ) -> Result<statsai_adapters::ArchiveScan> {
            // A tiny record naming an artifact that materializes far
            // larger than the file it came from.
            let mut conversation = ArchiveConversation {
                schema_version: statsai_core::ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
                conversation_id: "conv_heavy".to_string(),
                provider: "archive_test".to_string(),
                source_id: statsai_core::SourceId("heavy".to_string()),
                native_conversation_id: "heavy".to_string(),
                title: None,
                project: None,
                started_at: None,
                updated_at: None,
                completeness: statsai_core::ArchiveCompleteness::Complete,
                missing_content_count: 0,
                missing_content_scope_id: None,
                discarded_source_record_ids: Vec::new(),
                superseded_conversation_ids: Vec::new(),
                items: Vec::new(),
            };
            let item_id = "item_heavy".to_string();
            conversation.items.push(statsai_core::ArchiveItem {
                item_id: item_id.clone(),
                native_item_id: None,
                source_record_id: None,
                ordinal: 0,
                kind: statsai_core::ArchiveItemKind::Message,
                role: None,
                created_at: None,
                model: None,
                tool_name: None,
                tool_call_id: None,
                status: None,
                usage: None,
                parts_authoritative: true,
                parts: vec![statsai_core::ArchiveContentPart::text(
                    statsai_core::archive_content_id(&item_id, 0),
                    0,
                    ArchiveContentKind::Text,
                    "x".repeat(ARCHIVE_COLLECTION_RETAINED_BYTES / 4),
                )],
            });
            // Held long enough that every worker has tried to claim before
            // any capacity is released.
            std::thread::sleep(std::time::Duration::from_millis(250));
            let mut scan = statsai_adapters::ArchiveScan::default();
            scan.conversations.push(conversation);
            Ok(scan)
        }
    }

    let source = SourceLocation::local_adapter(
        "archive_test",
        "heavy-archive-test",
        "0",
        Path::new("/tmp/archive-heavy-test"),
        LocationOrigin::Configured,
    );
    let entries = (0..ARCHIVE_COLLECTION_GROUP_FILES)
        .map(|index| ScanFileStateEntry {
            cache_key: index.to_string(),
            cache_signature: format!("signature-{index}"),
        })
        .collect::<Vec<_>>();
    // A quarter of the budget each, so only a few may be outstanding.
    let source_bytes =
        vec![ARCHIVE_COLLECTION_RETAINED_BYTES as u64 / 4; ARCHIVE_COLLECTION_GROUP_FILES];

    let scans = parse_archive_group(&HeavyArchiveAdapter, &source, &entries, &source_bytes);

    // Every worker reaches the budget before any of them finishes, which is
    // exactly when a check that does not reserve lets all of them through.
    assert!(!scans.is_empty(), "no file was reconstructed");
    assert!(
        scans.len() <= 4,
        "budget did not gate concurrent claims: {} files were taken",
        scans.len()
    );
}
