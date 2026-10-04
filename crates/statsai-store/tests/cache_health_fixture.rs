//! The shared prompt-cache fixture.
//!
//! Synthetic Claude Code, Codex, and Cursor usage goes through the store; the
//! daily summaries it syncs and the totals `statsai report cache` prints are
//! written to `fixtures/cache_health.json`. Receivers of the summaries test
//! against a copy of that file, so the CLI report, the synced daily objects,
//! and anything aggregating them agree on one set of numbers.
//!
//! Set `STATSAI_UPDATE_FIXTURES=1` to rewrite the file after an intended change.

use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;
use statsai_core::{
    sanitize_summary_for_sync, CallContext, Confidence, CostInfo, EventId, EventSource,
    IdentitySource, LocationOrigin, ModelCall, ModelInfo, ParseEvidence, PrivacyInfo, PrivacyMode,
    SessionInfo, SourceId, SourceKind, UsageCounts, UsageEvent, USAGE_EVENT_SCHEMA_VERSION,
};
use statsai_store::{CacheReportQuery, Store};
use std::path::Path;

const FIXTURE: &str = "tests/fixtures/cache_health.json";

fn at(day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, day, hour, minute, second)
        .single()
        .expect("time")
}

struct Call {
    provider: &'static str,
    session: &'static str,
    file: &'static str,
    line: u64,
    at: DateTime<Utc>,
    input: u64,
    read: u64,
    write: Option<u64>,
}

impl Call {
    fn event(&self) -> UsageEvent {
        let id = format!("{}_{}_{:03}", self.provider, self.session, self.line);
        UsageEvent {
            schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
            event_id: EventId(format!("event_{id}")),
            device_id: "device_fixture".to_string(),
            provider: self.provider.to_string(),
            source_id: SourceId(format!("src_fixture_{}", self.provider)),
            provider_account_id: None,
            subscription_id: None,
            source: EventSource {
                adapter_id: "fixture".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalAdapter,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "jsonl".to_string(),
                source_path_hash: None,
                source_record_id: None,
                parse_confidence: Confidence::High,
            },
            session: SessionInfo {
                session_id: self.session.to_string(),
                local_session_id_hash: None,
                title: None,
                started_at: self.at,
                ended_at: None,
                duration_seconds: None,
                turn_started_at: None,
            },
            model: Some(ModelInfo {
                // Priced models, so missed reuse has a cost to check.
                normalized_name: Some(
                    match self.provider {
                        "codex" => "gpt-6-sol",
                        "claude_code" => "claude-sonnet-4-5",
                        _ => "cursor-model",
                    }
                    .to_string(),
                ),
                ..ModelInfo::default()
            }),
            usage: UsageCounts {
                input_tokens: Some(self.input),
                output_tokens: Some(100),
                cache_creation_tokens: self.write,
                cache_read_tokens: Some(self.read),
                requests: Some(1),
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
            parse_evidence: Some(ParseEvidence {
                event_key_version: "fixture".to_string(),
                source_file_path_hash: Some(self.file.to_string()),
                source_line_number: Some(self.line),
                source_record_id: None,
                model_inferred: false,
                timestamp_inferred: false,
                account_identity_source: IdentitySource::Unresolved,
            }),
            project: None,
            git: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            created_at: self.at,
            imported_at: self.at,
            context: None,
        }
    }
}

fn claude(line: u64, at: DateTime<Utc>, input: u64, read: u64, write: u64) -> UsageEvent {
    Call {
        provider: "claude_code",
        session: "session_fixture_claude",
        file: "claude-main",
        line,
        at,
        input,
        read,
        write: Some(write),
    }
    .event()
}

fn events() -> Vec<UsageEvent> {
    let mut events = vec![
        // Day one: a cold start, growth, a short-gap partial loss, a pause.
        claude(1, at(1, 9, 0, 0), 20, 0, 40_000),
        claude(2, at(1, 9, 0, 30), 20, 40_000, 2_000),
        claude(3, at(1, 9, 1, 10), 20, 15_000, 27_500),
        claude(4, at(1, 9, 3, 0), 20, 42_500, 1_000),
        claude(5, at(1, 9, 9, 0), 20, 43_500, 900),
        // Across midnight after a long pause: a full loss.
        claude(6, at(1, 23, 50, 0), 20, 44_400, 600),
        claude(7, at(2, 1, 0, 0), 20, 0, 45_200),
        claude(8, at(2, 1, 0, 40), 20, 45_200, 800),
        // A request a router served without the cache: neither read nor written.
        claude(11, at(2, 1, 2, 0), 46_020, 0, 0),
    ];
    // A compaction rewrites the prompt: a restart, not a loss.
    let mut compacted = claude(9, at(2, 1, 5, 0), 20, 0, 9_000);
    compacted.context = Some(CallContext {
        after_compaction: true,
        ..CallContext::default()
    });
    events.push(compacted);
    // A sub-agent in the same session is its own stream.
    let mut agent = claude(10, at(1, 9, 2, 0), 20, 0, 8_000);
    agent.context = Some(CallContext {
        agent_hash: Some("fixture_agent".to_string()),
        ..CallContext::default()
    });
    events.push(agent);

    // A Codex turn listing its calls, with recorded request starts.
    let mut turn = Call {
        provider: "codex",
        session: "session_fixture_codex",
        file: "codex-rollout",
        line: 1,
        at: at(2, 10, 0, 0),
        input: 0,
        read: 0,
        write: Some(0),
    }
    .event();
    let calls = [
        (at(2, 10, 0, 10), at(2, 10, 0, 0), 30_000, 0),
        (at(2, 10, 0, 30), at(2, 10, 0, 12), 3_000, 29_952),
        (at(2, 10, 7, 0), at(2, 10, 6, 40), 26_000, 7_168),
    ];
    turn.usage.requests = Some(calls.len() as u64);
    turn.usage.input_tokens = Some(calls.iter().map(|call| call.2).sum());
    turn.usage.cache_read_tokens = Some(calls.iter().map(|call| call.3).sum());
    turn.created_at = at(2, 10, 7, 30);
    turn.context = Some(CallContext {
        calls: calls
            .iter()
            .map(|(completed_at, requested_at, input, read)| ModelCall {
                completed_at: *completed_at,
                requested_at: Some(*requested_at),
                input_tokens: Some(*input),
                cache_read_tokens: Some(*read),
                cache_creation_tokens: Some(0),
                after_compaction: false,
                model: None,
            })
            .collect(),
        ..CallContext::default()
    });
    events.push(turn);
    // An older Codex turn without a per-call breakdown and without write
    // telemetry: counted, not classified.
    let mut legacy = Call {
        provider: "codex",
        session: "session_fixture_codex",
        file: "codex-rollout",
        line: 2,
        at: at(2, 11, 0, 0),
        input: 4_000,
        read: 60_000,
        write: None,
    }
    .event();
    legacy.usage.requests = Some(2);
    events.push(legacy);

    // Cursor has no call order: token totals only.
    events.push(
        Call {
            provider: "cursor",
            session: "session_fixture_cursor",
            file: "cursor-export",
            line: 1,
            at: at(2, 12, 0, 0),
            input: 1_000,
            read: 9_000,
            write: Some(500),
        }
        .event(),
    );
    events
}

fn fixture() -> serde_json::Value {
    let store = Store::in_memory().expect("store");
    store.insert_events(&events()).expect("insert");
    let mut summaries = store
        .all_sync_rollup_summaries()
        .expect("summaries")
        .into_iter()
        .map(sanitize_summary_for_sync)
        .collect::<Vec<_>>();
    summaries.sort_by(|left, right| left.summary_id.0.cmp(&right.summary_id.0));
    let report = store
        .cache_report(
            &CacheReportQuery::for_range(Some("2026-07-01"), Some("2026-07-02"), at(3, 0, 0, 0))
                .expect("query"),
            &Utc,
        )
        .expect("report");
    json!({
        "summaries": summaries,
        "report": {
            "totals": report.totals,
            "diagnostics": report.diagnostics,
            "gap_histogram": report.gap_histogram,
            "days": report.days.iter().map(|day| json!({
                "day": day.day,
                "losses": day.diagnostics.as_ref().map_or(0, |health| health.losses),
                "missed": day.diagnostics.as_ref().map_or(0, |health| health.missed_tokens),
                "missed_cost_micro_usd": day
                    .diagnostics
                    .as_ref()
                    .map_or(0, |health| health.missed_cost_micro_usd),
            })).collect::<Vec<_>>(),
        },
    })
}

#[test]
fn the_shared_cache_health_fixture_is_current() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    let current = serde_json::to_string_pretty(&fixture()).expect("json") + "\n";
    if std::env::var_os("STATSAI_UPDATE_FIXTURES").is_some() {
        std::fs::write(&path, &current).expect("write fixture");
        return;
    }
    let stored = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        stored == current,
        "{FIXTURE} is out of date; rerun with STATSAI_UPDATE_FIXTURES=1 and copy it to its receivers"
    );
}

#[test]
fn the_fixture_report_matches_its_daily_summaries() {
    let fixture = fixture();
    let mut merged = statsai_core::CacheHealthV1::default();
    for summary in fixture["summaries"].as_array().expect("summaries") {
        if let Some(health) = summary.pointer("/metrics/cache_health") {
            merged.merge(&serde_json::from_value(health.clone()).expect("cache health"));
        }
    }
    assert_eq!(
        serde_json::to_value(&merged).expect("json"),
        fixture["report"]["diagnostics"]
    );
    assert_eq!((merged.unclassifiable, merged.boundaries), (2, 1));
}
