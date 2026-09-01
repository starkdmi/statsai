use super::support::*;
use super::*;

#[test]
fn refreshes_semantic_duplicate_with_new_event_id_without_double_counting() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-semantic"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let old_event = test_store_event(&source, now, "old-record");
    let old_event_id = old_event.event_id.clone();
    let mut new_event = old_event.clone();
    new_event.event_id = event_id("codex", &source.source_id, "semantic-record", None, now);
    new_event.source.source_record_id = Some("usage_key_new".to_string());
    new_event.parse_evidence = None;

    assert!(store.insert_event(&old_event).expect("insert old"));
    assert!(!store.insert_event(&new_event).expect("refresh semantic"));
    assert_eq!(store.event_count().expect("count"), 1);

    assert_eq!(store.events().expect("events")[0].event_id, old_event_id);
}

#[test]
fn refreshes_legacy_reasoning_level_upgrade_without_double_counting() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-reasoning-upgrade"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let mut old_event = test_store_event(&source, now, "legacy-record");
    old_event.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });

    let mut new_event = old_event.clone();
    new_event.event_id = event_id("codex", &source.source_id, "reasoning-record", None, now);
    new_event.source.source_record_id = Some("usage_key_reasoning".to_string());
    new_event.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::Low),
        reasoning_level_raw: Some("low".to_string()),
    });

    assert!(store.insert_event(&old_event).expect("insert old"));
    assert!(!store
        .insert_event(&new_event)
        .expect("refresh reasoning upgrade"));

    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_id, old_event.event_id);
    assert_eq!(
        events[0]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level),
        Some(ReasoningLevel::Low)
    );
    assert_eq!(
        events[0]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level_raw.as_deref()),
        Some("low")
    );
}

#[test]
fn refresh_duplicate_without_reasoning_does_not_erase_enriched_reasoning() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-reasoning-preserve"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let mut enriched = test_store_event(&source, now, "enriched-record");
    enriched.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::Low),
        reasoning_level_raw: Some("low".to_string()),
    });

    let mut less_enriched = enriched.clone();
    less_enriched.event_id = event_id("codex", &source.source_id, "less-enriched", None, now);
    less_enriched.source.source_record_id = Some("usage_key_less_enriched".to_string());
    less_enriched.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });

    assert!(store.insert_event(&enriched).expect("insert enriched"));
    assert!(!store
        .insert_event(&less_enriched)
        .expect("refresh less-enriched duplicate"));

    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level),
        Some(ReasoningLevel::Low)
    );
    assert_eq!(
        events[0]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level_raw.as_deref()),
        Some("low")
    );
}

#[test]
fn exact_event_id_refresh_without_reasoning_does_not_erase_enriched_reasoning() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-reasoning-exact-id-preserve"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let mut enriched = test_store_event(&source, now, "same-record");
    enriched.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::Medium),
        reasoning_level_raw: Some("medium".to_string()),
    });

    let mut less_enriched = enriched.clone();
    less_enriched.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });

    assert!(store.insert_event(&enriched).expect("insert enriched"));
    assert!(!store
        .insert_event(&less_enriched)
        .expect("refresh exact-id duplicate"));

    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level),
        Some(ReasoningLevel::Medium)
    );
    assert_eq!(
        events[0]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level_raw.as_deref()),
        Some("medium")
    );
}

#[test]
fn keeps_explicit_reasoning_levels_as_distinct_events() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-reasoning-distinct"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let mut low = test_store_event(&source, now, "low-record");
    low.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::Low),
        reasoning_level_raw: Some("low".to_string()),
    });

    let mut high = low.clone();
    high.event_id = event_id("codex", &source.source_id, "high-record", None, now);
    high.source.source_record_id = Some("high-record".to_string());
    high.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::High),
        reasoning_level_raw: Some("high".to_string()),
    });

    assert!(store.insert_event(&low).expect("insert low"));
    assert!(store.insert_event(&high).expect("insert high"));

    let events = store.events().expect("events");
    assert_eq!(events.len(), 2);
    assert!(events.iter().any(|event| {
        event.model.as_ref().and_then(|model| model.reasoning_level) == Some(ReasoningLevel::Low)
    }));
    assert!(events.iter().any(|event| {
        event.model.as_ref().and_then(|model| model.reasoning_level) == Some(ReasoningLevel::High)
    }));
}

#[test]
fn refreshes_legacy_codex_token_count_duplicate_without_double_counting() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-token-count"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let mut old_event = test_store_event(&source, now, "legacy-record");
    old_event.session.session_id = "session-a".to_string();
    old_event.session.local_session_id_hash = Some("session-a".to_string());
    old_event.model = Some(ModelInfo {
        name: Some("gpt-5".to_string()),
        normalized_name: Some("gpt-5".to_string()),
        provider_model_id: Some("gpt-5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    old_event.parse_evidence = Some(ParseEvidence {
        event_key_version: "semantic_usage_event.v1".to_string(),
        source_file_path_hash: Some("active-hash".to_string()),
        source_line_number: Some(12),
        source_record_id: Some(
            "semantic_usage_event.v1:codex_token_count:session-a:1715510400000:gpt-5:12:0:3:0:15"
                .to_string(),
        ),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: statsai_core::IdentitySource::Unresolved,
    });

    let mut new_event = old_event.clone();
    new_event.event_id = event_id("codex", &source.source_id, "modern-record", None, now);
    new_event.session.session_id = "session-b".to_string();
    new_event.session.local_session_id_hash = Some("session-b".to_string());
    new_event.parse_evidence = Some(ParseEvidence {
        event_key_version: "semantic_usage_event.v2".to_string(),
        source_file_path_hash: Some("branch-hash".to_string()),
        source_line_number: Some(48),
        source_record_id: Some(
            "semantic_usage_event.v2:codex_token_count:1715510400000:gpt-5:12:0:3:0:15".to_string(),
        ),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: statsai_core::IdentitySource::Unresolved,
    });

    assert!(store.insert_event(&old_event).expect("insert old"));
    let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
        provider: &old_event.provider,
        source_id: &old_event.source_id,
        started_at: old_event.session.started_at,
        session_hash: old_event.session.local_session_id_hash.as_deref(),
        project_key: None,
        model_name: model_key(&old_event),
        input_tokens: old_event.usage.input_tokens,
        cache_read_tokens: old_event.usage.cache_read_tokens,
        cache_creation_tokens: old_event.usage.cache_creation_tokens,
        output_tokens: old_event.usage.output_tokens,
        reasoning_tokens: old_event.usage.reasoning_tokens,
        total_tokens: old_event.usage.computed_total(),
    });
    store
        .conn
        .execute(
            "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
            params![legacy_fingerprint, &old_event.event_id.0],
        )
        .expect("downgrade fingerprint");

    assert!(!store
        .insert_event(&new_event)
        .expect("refresh legacy duplicate"));
    assert_eq!(store.event_count().expect("count"), 1);
    assert_eq!(
        store.events().expect("events")[0].event_id,
        old_event.event_id
    );
}

#[test]
fn refreshes_legacy_projectless_codex_token_count_without_double_counting() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-token-count-project-upgrade"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let project = ProjectInfo {
        project_id: "project_shared".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("example/statsai".to_string()),
        branch_hash: Some("branch-hash".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/workspace/ai-stats".to_string()),
    };

    let mut old_event = test_store_event(&source, now, "legacy-projectless-record");
    old_event.session.session_id = "session-a".to_string();
    old_event.session.local_session_id_hash = Some("session-a".to_string());
    old_event.model = Some(ModelInfo {
        name: Some("gpt-5".to_string()),
        normalized_name: Some("gpt-5".to_string()),
        provider_model_id: Some("gpt-5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    old_event.project = None;
    old_event.parse_evidence = Some(ParseEvidence {
        event_key_version: "semantic_usage_event.v2".to_string(),
        source_file_path_hash: Some("active-hash".to_string()),
        source_line_number: Some(12),
        source_record_id: Some(
            "semantic_usage_event.v2:codex_token_count:1715510400000:gpt-5:12:0:3:0:15".to_string(),
        ),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: statsai_core::IdentitySource::Unresolved,
    });

    let mut new_event = old_event.clone();
    new_event.event_id = event_id(
        "codex",
        &source.source_id,
        "modern-projectful-record",
        None,
        now,
    );
    new_event.session.session_id = "session-b".to_string();
    new_event.session.local_session_id_hash = Some("session-b".to_string());
    new_event.project = Some(project.clone());
    new_event.parse_evidence = Some(ParseEvidence {
        event_key_version: "semantic_usage_event.v4".to_string(),
        source_file_path_hash: Some("branch-hash".to_string()),
        source_line_number: Some(48),
        source_record_id: Some(format!(
            "semantic_usage_event.v4:codex_token_count:{}:1715510400000:gpt-5:12:0:3:0:15",
            project_bucket_key(Some(&project))
        )),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: statsai_core::IdentitySource::Unresolved,
    });

    assert!(store.insert_event(&old_event).expect("insert old"));
    let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
        provider: &old_event.provider,
        source_id: &old_event.source_id,
        started_at: old_event.session.started_at,
        session_hash: old_event.session.local_session_id_hash.as_deref(),
        project_key: None,
        model_name: model_key(&old_event),
        input_tokens: old_event.usage.input_tokens,
        cache_read_tokens: old_event.usage.cache_read_tokens,
        cache_creation_tokens: old_event.usage.cache_creation_tokens,
        output_tokens: old_event.usage.output_tokens,
        reasoning_tokens: old_event.usage.reasoning_tokens,
        total_tokens: old_event.usage.computed_total(),
    });
    store
        .conn
        .execute(
            "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
            params![legacy_fingerprint, &old_event.event_id.0],
        )
        .expect("downgrade fingerprint");

    assert_eq!(
        store
            .insert_events(std::slice::from_ref(&new_event))
            .expect("refresh legacy projectless duplicate"),
        0
    );
    assert_eq!(store.event_count().expect("count"), 1);
    assert_eq!(
        store.events().expect("events")[0].event_id,
        old_event.event_id
    );
}

#[test]
fn refreshes_legacy_codex_turn_usage_duplicate_without_double_counting() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-turn-usage"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let project = ProjectInfo {
        project_id: "project_shared".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("example/statsai".to_string()),
        branch_hash: Some("branch-hash".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/workspace/ai-stats".to_string()),
    };

    let mut old_event = test_store_event(&source, now, "legacy-record");
    old_event.session.session_id = "session-a".to_string();
    old_event.session.local_session_id_hash = Some("session-a".to_string());
    old_event.project = Some(project.clone());
    old_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v3".to_string(),
            source_file_path_hash: Some("active-hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some(
                format!(
                    "semantic_usage_event.v3:codex_turn_usage:{}:1715510400000:1715510405000:gpt-5:12:0:3:0:15",
                    project.project_id
                ),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });
    old_event.model = Some(ModelInfo {
        name: Some("gpt-5".to_string()),
        normalized_name: Some("gpt-5".to_string()),
        provider_model_id: Some("gpt-5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    old_event.session.ended_at = Some(now + chrono::Duration::seconds(5));
    old_event.session.duration_seconds = Some(5);

    let mut new_event = old_event.clone();
    new_event.event_id = event_id("codex", &source.source_id, "modern-record", None, now);
    new_event.session.session_id = "session-b".to_string();
    new_event.session.local_session_id_hash = Some("session-b".to_string());
    new_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("branch-hash".to_string()),
            source_line_number: Some(48),
            source_record_id: Some(
                format!(
                    "semantic_usage_event.v4:codex_turn_usage:{}:1715510400000:1715510405000:gpt-5:12:0:3:0:15",
                    project_bucket_key(Some(&project))
                ),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

    assert!(store.insert_event(&old_event).expect("insert old"));
    let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
        provider: &old_event.provider,
        source_id: &old_event.source_id,
        started_at: old_event.session.started_at,
        session_hash: old_event.session.local_session_id_hash.as_deref(),
        project_key: Some("repo:repo-hash|path:path-hash"),
        model_name: model_key(&old_event),
        input_tokens: old_event.usage.input_tokens,
        cache_read_tokens: old_event.usage.cache_read_tokens,
        cache_creation_tokens: old_event.usage.cache_creation_tokens,
        output_tokens: old_event.usage.output_tokens,
        reasoning_tokens: old_event.usage.reasoning_tokens,
        total_tokens: old_event.usage.computed_total(),
    });
    store
        .conn
        .execute(
            "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
            params![legacy_fingerprint, &old_event.event_id.0],
        )
        .expect("downgrade fingerprint");

    assert!(!store
        .insert_event(&new_event)
        .expect("refresh legacy duplicate"));
    assert_eq!(store.event_count().expect("count"), 1);
    assert_eq!(
        store.events().expect("events")[0].event_id,
        old_event.event_id
    );
}

#[test]
fn refreshes_legacy_projectless_codex_turn_usage_without_double_counting() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-turn-usage-project-upgrade"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let project = ProjectInfo {
        project_id: "project_shared".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("example/statsai".to_string()),
        branch_hash: Some("branch-hash".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/workspace/ai-stats".to_string()),
    };

    let mut old_event = test_store_event(&source, now, "legacy-projectless-turn");
    old_event.session.session_id = "session-a".to_string();
    old_event.session.local_session_id_hash = Some("session-a".to_string());
    old_event.project = None;
    old_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v3".to_string(),
            source_file_path_hash: Some("active-hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some(
                "semantic_usage_event.v3:codex_turn_usage:1715510400000:1715510405000:gpt-5:12:0:3:0:15"
                    .to_string(),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });
    old_event.model = Some(ModelInfo {
        name: Some("gpt-5".to_string()),
        normalized_name: Some("gpt-5".to_string()),
        provider_model_id: Some("gpt-5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    old_event.session.ended_at = Some(now + chrono::Duration::seconds(5));
    old_event.session.duration_seconds = Some(5);

    let mut new_event = old_event.clone();
    new_event.event_id = event_id(
        "codex",
        &source.source_id,
        "modern-projectful-turn",
        None,
        now,
    );
    new_event.session.session_id = "session-b".to_string();
    new_event.session.local_session_id_hash = Some("session-b".to_string());
    new_event.project = Some(project.clone());
    new_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("branch-hash".to_string()),
            source_line_number: Some(48),
            source_record_id: Some(
                format!(
                    "semantic_usage_event.v4:codex_turn_usage:{}:1715510400000:1715510405000:gpt-5:12:0:3:0:15",
                    project_bucket_key(Some(&project))
                ),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

    assert!(store.insert_event(&old_event).expect("insert old"));
    let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
        provider: &old_event.provider,
        source_id: &old_event.source_id,
        started_at: old_event.session.started_at,
        session_hash: old_event.session.local_session_id_hash.as_deref(),
        project_key: None,
        model_name: model_key(&old_event),
        input_tokens: old_event.usage.input_tokens,
        cache_read_tokens: old_event.usage.cache_read_tokens,
        cache_creation_tokens: old_event.usage.cache_creation_tokens,
        output_tokens: old_event.usage.output_tokens,
        reasoning_tokens: old_event.usage.reasoning_tokens,
        total_tokens: old_event.usage.computed_total(),
    });
    store
        .conn
        .execute(
            "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
            params![legacy_fingerprint, &old_event.event_id.0],
        )
        .expect("downgrade fingerprint");

    assert!(!store
        .insert_event(&new_event)
        .expect("refresh legacy projectless turn duplicate"));
    assert_eq!(store.event_count().expect("count"), 1);
    assert_eq!(
        store.events().expect("events")[0].event_id,
        old_event.event_id
    );
}

#[test]
fn refreshes_legacy_project_id_only_codex_token_count_without_double_counting() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-token-count-project-id-upgrade"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let legacy_project = ProjectInfo {
        project_id: "project_shared".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: None,
        path_label: None,
    };
    let project = ProjectInfo {
        project_id: "project_shared".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("example/statsai".to_string()),
        branch_hash: Some("branch-hash".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/workspace/ai-stats".to_string()),
    };

    let mut old_event = test_store_event(&source, now, "legacy-project-id-token-count");
    old_event.session.session_id = "session-a".to_string();
    old_event.session.local_session_id_hash = Some("session-a".to_string());
    old_event.model = Some(ModelInfo {
        name: Some("gpt-5".to_string()),
        normalized_name: Some("gpt-5".to_string()),
        provider_model_id: Some("gpt-5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    old_event.project = Some(legacy_project.clone());
    old_event.parse_evidence = Some(ParseEvidence {
        event_key_version: "semantic_usage_event.v2".to_string(),
        source_file_path_hash: Some("active-hash".to_string()),
        source_line_number: Some(12),
        source_record_id: Some(format!(
            "semantic_usage_event.v2:codex_token_count:{}:1715510400000:gpt-5:12:0:3:0:15",
            legacy_project.project_id
        )),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: statsai_core::IdentitySource::Unresolved,
    });

    let mut new_event = old_event.clone();
    new_event.event_id = event_id(
        "codex",
        &source.source_id,
        "modern-projectful-token-count",
        None,
        now,
    );
    new_event.session.session_id = "session-b".to_string();
    new_event.session.local_session_id_hash = Some("session-b".to_string());
    new_event.project = Some(project.clone());
    new_event.parse_evidence = Some(ParseEvidence {
        event_key_version: "semantic_usage_event.v4".to_string(),
        source_file_path_hash: Some("branch-hash".to_string()),
        source_line_number: Some(48),
        source_record_id: Some(format!(
            "semantic_usage_event.v4:codex_token_count:{}:1715510400000:gpt-5:12:0:3:0:15",
            project_bucket_key(Some(&project))
        )),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: statsai_core::IdentitySource::Unresolved,
    });

    assert!(store.insert_event(&old_event).expect("insert old"));
    let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
        provider: &old_event.provider,
        source_id: &old_event.source_id,
        started_at: old_event.session.started_at,
        session_hash: old_event.session.local_session_id_hash.as_deref(),
        project_key: Some(legacy_project.project_id.as_str()),
        model_name: model_key(&old_event),
        input_tokens: old_event.usage.input_tokens,
        cache_read_tokens: old_event.usage.cache_read_tokens,
        cache_creation_tokens: old_event.usage.cache_creation_tokens,
        output_tokens: old_event.usage.output_tokens,
        reasoning_tokens: old_event.usage.reasoning_tokens,
        total_tokens: old_event.usage.computed_total(),
    });
    store
        .conn
        .execute(
            "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
            params![legacy_fingerprint, &old_event.event_id.0],
        )
        .expect("downgrade fingerprint");

    assert!(!store
        .insert_event(&new_event)
        .expect("refresh legacy project-id duplicate"));
    assert_eq!(store.event_count().expect("count"), 1);
    assert_eq!(
        store.events().expect("events")[0].event_id,
        old_event.event_id
    );
}

#[test]
fn refreshes_legacy_project_id_only_codex_turn_usage_without_double_counting() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-turn-usage-project-id-upgrade"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let legacy_project = ProjectInfo {
        project_id: "project_shared".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: None,
        path_label: None,
    };
    let project = ProjectInfo {
        project_id: "project_shared".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("example/statsai".to_string()),
        branch_hash: Some("branch-hash".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/workspace/ai-stats".to_string()),
    };

    let mut old_event = test_store_event(&source, now, "legacy-project-id-turn");
    old_event.session.session_id = "session-a".to_string();
    old_event.session.local_session_id_hash = Some("session-a".to_string());
    old_event.project = Some(legacy_project.clone());
    old_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v3".to_string(),
            source_file_path_hash: Some("active-hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some(
                format!(
                    "semantic_usage_event.v3:codex_turn_usage:{}:1715510400000:1715510405000:gpt-5:12:0:3:0:15",
                    legacy_project.project_id
                ),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });
    old_event.model = Some(ModelInfo {
        name: Some("gpt-5".to_string()),
        normalized_name: Some("gpt-5".to_string()),
        provider_model_id: Some("gpt-5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    old_event.session.ended_at = Some(now + chrono::Duration::seconds(5));
    old_event.session.duration_seconds = Some(5);

    let mut new_event = old_event.clone();
    new_event.event_id = event_id(
        "codex",
        &source.source_id,
        "modern-projectful-turn",
        None,
        now,
    );
    new_event.session.session_id = "session-b".to_string();
    new_event.session.local_session_id_hash = Some("session-b".to_string());
    new_event.project = Some(project.clone());
    new_event.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("branch-hash".to_string()),
            source_line_number: Some(48),
            source_record_id: Some(
                format!(
                    "semantic_usage_event.v4:codex_turn_usage:{}:1715510400000:1715510405000:gpt-5:12:0:3:0:15",
                    project_bucket_key(Some(&project))
                ),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

    assert!(store.insert_event(&old_event).expect("insert old"));
    let legacy_fingerprint = semantic_event_fingerprint(&SemanticFingerprintInput {
        provider: &old_event.provider,
        source_id: &old_event.source_id,
        started_at: old_event.session.started_at,
        session_hash: old_event.session.local_session_id_hash.as_deref(),
        project_key: Some(legacy_project.project_id.as_str()),
        model_name: model_key(&old_event),
        input_tokens: old_event.usage.input_tokens,
        cache_read_tokens: old_event.usage.cache_read_tokens,
        cache_creation_tokens: old_event.usage.cache_creation_tokens,
        output_tokens: old_event.usage.output_tokens,
        reasoning_tokens: old_event.usage.reasoning_tokens,
        total_tokens: old_event.usage.computed_total(),
    });
    store
        .conn
        .execute(
            "UPDATE usage_events SET semantic_fingerprint = ?1 WHERE event_id = ?2",
            params![legacy_fingerprint, &old_event.event_id.0],
        )
        .expect("downgrade fingerprint");

    assert!(!store
        .insert_event(&new_event)
        .expect("refresh legacy project-id duplicate"));
    assert_eq!(store.event_count().expect("count"), 1);
    assert_eq!(
        store.events().expect("events")[0].event_id,
        old_event.event_id
    );
}

#[test]
fn refreshes_legacy_codex_usage_shape_after_normalization_without_double_counting() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-normalized"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let mut old_event = test_store_event(&source, now, "legacy-inclusive");
    old_event.model = Some(ModelInfo {
        name: Some("gpt-5-codex".to_string()),
        normalized_name: Some("gpt-5-codex".to_string()),
        provider_model_id: Some("gpt-5-codex".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    old_event.usage = UsageCounts {
        input_tokens: Some(100),
        cache_read_tokens: Some(30),
        output_tokens: Some(10),
        reasoning_tokens: Some(5),
        total_tokens: Some(110),
        requests: Some(1),
        ..UsageCounts::default()
    };

    let mut new_event = old_event.clone();
    new_event.event_id = event_id("codex", &source.source_id, "normalized", None, now);
    new_event.usage = UsageCounts {
        input_tokens: Some(70),
        cache_read_tokens: Some(30),
        output_tokens: Some(5),
        reasoning_tokens: Some(5),
        total_tokens: Some(110),
        requests: Some(1),
        ..UsageCounts::default()
    };

    assert!(store.insert_event(&old_event).expect("insert old"));
    assert!(!store
        .insert_event(&new_event)
        .expect("refresh normalized duplicate"));

    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_id, old_event.event_id);
    assert_eq!(events[0].usage.input_tokens, Some(70));
    assert_eq!(events[0].usage.cache_read_tokens, Some(30));
    assert_eq!(events[0].usage.output_tokens, Some(5));
    assert_eq!(events[0].usage.reasoning_tokens, Some(5));
}

/// One provider request can appear in several Claude files, so a scan that
/// re-reads only a file holding an early streaming snapshot must not shrink an
/// event the store already holds at its final snapshot.
fn claude_provider_record_event(
    source: &statsai_core::SourceLocation,
    now: chrono::DateTime<Utc>,
    output_tokens: u64,
) -> UsageEvent {
    let mut event = test_store_event(source, now, "claude-provider-record");
    event.provider = "claude_code".to_string();
    event.usage = UsageCounts {
        input_tokens: Some(2),
        cache_creation_tokens: Some(1000),
        cache_read_tokens: Some(8000),
        output_tokens: Some(output_tokens),
        requests: Some(1),
        ..UsageCounts::default()
    };
    event.parse_evidence = Some(ParseEvidence {
        event_key_version: "provider_record_usage_event.v1".to_string(),
        source_file_path_hash: Some("file-hash".to_string()),
        source_line_number: Some(1),
        source_record_id: Some("provider_record_usage_event.v1:claude_message_usage:x".to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: statsai_core::IdentitySource::Unresolved,
    });
    event
}

#[test]
fn partial_claude_snapshot_does_not_shrink_a_stored_provider_record_event() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-partial-snapshot"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let final_snapshot = claude_provider_record_event(&source, now, 240);
    let partial_snapshot = claude_provider_record_event(&source, now, 100);
    assert_eq!(final_snapshot.event_id, partial_snapshot.event_id);

    assert!(store.insert_event(&final_snapshot).expect("insert final"));
    assert!(!store
        .insert_event(&partial_snapshot)
        .expect("partial snapshot refresh"));

    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].usage.output_tokens, Some(240));
    assert_eq!(events[0].usage.computed_total(), 9242);
    assert_eq!(store.token_total().expect("token total"), 9242);
}

#[test]
fn later_claude_snapshot_still_replaces_a_stored_partial_provider_record_event() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-later-snapshot"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    assert!(store
        .insert_event(&claude_provider_record_event(&source, now, 100))
        .expect("insert partial"));
    assert!(!store
        .insert_event(&claude_provider_record_event(&source, now, 240))
        .expect("final snapshot refresh"));

    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].usage.output_tokens, Some(240));
    assert_eq!(store.token_total().expect("token total"), 9242);
}
