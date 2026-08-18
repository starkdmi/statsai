use chrono::{Duration, TimeZone, Utc};
use serde_json::Value;
use statsai_core::{
    event_id, Confidence, CostInfo, EventSource, LocationOrigin, PrivacyInfo, PrivacyMode,
    SessionInfo, SourceKind, SourceLocation, UsageCounts, UsageEvent, USAGE_EVENT_SCHEMA_VERSION,
};
use statsai_store::Store;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn run_statsai(store: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_statsai"))
        .arg("--store")
        .arg(store)
        .args(args)
        .output()
        .expect("run statsai");
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 stdout")
}

fn test_event(
    source: &SourceLocation,
    started_at: chrono::DateTime<Utc>,
    tokens: u64,
) -> UsageEvent {
    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: event_id(
            "codex",
            &source.source_id,
            &started_at.to_rfc3339(),
            None,
            started_at,
        ),
        device_id: "report-cli-test".to_string(),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        subscription_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "jsonl".to_string(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some(started_at.to_rfc3339()),
            parse_confidence: Confidence::High,
        },
        session: SessionInfo {
            session_id: "session".to_string(),
            local_session_id_hash: None,
            title: None,
            started_at,
            ended_at: None,
            duration_seconds: None,
        },
        model: None,
        usage: UsageCounts {
            total_tokens: Some(tokens),
            ..UsageCounts::default()
        },
        runtime: None,
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: None,
            provider_reported_micro_usd: None,
            pricing_source: None,
            pricing_version: None,
            confidence: Confidence::Low,
        },
        parse_evidence: None,
        project: None,
        git: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        created_at: started_at,
        imported_at: started_at,
    }
}

#[test]
fn report_range_cli_filters_events_between_from_and_to() {
    let dir = tempdir().expect("temp dir");
    let store_path = dir.path().join("statsai.sqlite3");
    let store = Store::open(&store_path).expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        dir.path().join("codex").as_path(),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let before = Utc
        .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
        .single()
        .expect("before");
    let inside = Utc
        .with_ymd_and_hms(2026, 5, 10, 18, 0, 0)
        .single()
        .expect("inside");
    let after = Utc
        .with_ymd_and_hms(2026, 5, 20, 9, 0, 0)
        .single()
        .expect("after");
    store
        .insert_events(&[
            test_event(&source, before, 50),
            test_event(&source, inside, 100),
            test_event(&source, after, 200),
        ])
        .expect("insert events");
    drop(store);

    let report: Value = serde_json::from_str(&run_statsai(
        &store_path,
        &[
            "report",
            "range",
            "--from",
            "2026-05-01",
            "--to",
            "2026-05-15",
            "--json",
        ],
    ))
    .expect("range JSON");

    assert_eq!(report["label"], "2026-05-01 to 2026-05-15");
    assert_eq!(report["total_events"], 1);
    assert_eq!(report["total_tokens"]["total"], 100);
    assert_eq!(
        report["since"],
        (Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
            .single()
            .expect("since"))
        .to_rfc3339()
    );
    assert_eq!(
        report["until"],
        (Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 0)
            .single()
            .expect("next day")
            - Duration::nanoseconds(1))
        .to_rfc3339()
    );
}
