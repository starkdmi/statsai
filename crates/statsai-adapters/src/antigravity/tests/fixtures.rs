//! Checks the adapter against the exported fixture store.
//!
//! `tests/fixtures/antigravity/expected.json` is written by the Antigravity
//! export in `build_fixtures.py`, with its own decoder, from the same files.
//! The two implementations must agree on every conversation.

use super::*;
use serde_json::Value;
use std::collections::BTreeMap;

const COVERAGE: &[&str] = &[
    "gemini-thinking",
    "claude-vertex",
    "gpt-oss",
    "checkpoint",
    "retried-request",
    "planner-without-usage",
    "subagent",
];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/antigravity")
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Totals {
    calls: u64,
    input: u64,
    cache_write: u64,
    cache_read: u64,
    thinking: u64,
    response: u64,
}

impl Totals {
    fn add(&mut self, usage: &UsageCounts) {
        self.calls += usage.requests.unwrap_or(0);
        self.input += usage.input_tokens.unwrap_or(0);
        self.cache_write += usage.cache_creation_tokens.unwrap_or(0);
        self.cache_read += usage.cache_read_tokens.unwrap_or(0);
        self.thinking += usage.reasoning_tokens.unwrap_or(0);
        self.response += usage.output_tokens.unwrap_or(0);
    }

    fn from_json(value: &Value) -> Self {
        let field = |name: &str| value.get(name).and_then(Value::as_u64).unwrap_or(0);
        Self {
            calls: field("calls"),
            input: field("input"),
            cache_write: field("cache_write"),
            cache_read: field("cache_read"),
            thinking: field("thinking"),
            response: field("response"),
        }
    }
}

fn model_key(event: &UsageEvent) -> String {
    event
        .model
        .as_ref()
        .map(|model| {
            format!(
                "{}|{}",
                model.name.as_deref().unwrap_or(""),
                model.provider_model_id.as_deref().unwrap_or("")
            )
        })
        .unwrap_or_else(|| "|".to_string())
}

pub(super) fn check_fixture_store() {
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(fixture_dir().join("expected.json")).expect("expected.json"),
    )
    .expect("expected json");
    let covered = expected["covers"]
        .as_array()
        .expect("covers")
        .iter()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    for scenario in COVERAGE {
        assert!(covered.contains(scenario), "fixtures cover {scenario}");
    }

    let root = fixture_dir().join("store");
    let scan = scan_root(&root);

    assert_eq!(
        scan.diagnostics.unreadable_history_files,
        expected["unreadable_history_files"]
            .as_u64()
            .expect("count")
    );
    assert_eq!(scan.diagnostics.invalid_rows, 0);
    let conversations = expected["conversations"]
        .as_object()
        .expect("conversations");
    let mut planner = BTreeMap::<String, Totals>::new();
    let mut checkpoints = BTreeMap::<String, Totals>::new();
    let mut models = BTreeMap::<String, BTreeMap<String, u64>>::new();
    let by_hash = conversations
        .keys()
        .map(|id| (hash_text(id), id.clone()))
        .collect::<BTreeMap<_, _>>();
    for event in &scan.events {
        let hash = event
            .session
            .local_session_id_hash
            .as_deref()
            .expect("session");
        let id = by_hash
            .get(hash)
            .expect("event of an expected conversation")
            .clone();
        let totals = if is_checkpoint(event) {
            &mut checkpoints
        } else {
            &mut planner
        };
        totals.entry(id.clone()).or_default().add(&event.usage);
        *models
            .entry(id)
            .or_default()
            .entry(model_key(event))
            .or_default() += 1;
    }
    for (id, conversation) in conversations {
        assert_eq!(
            planner.remove(id).unwrap_or_default(),
            Totals::from_json(&conversation["planner"]),
            "planner usage of {id}"
        );
        assert_eq!(
            checkpoints.remove(id).unwrap_or_default(),
            Totals::from_json(&conversation["checkpoint"]),
            "checkpoint usage of {id}"
        );
        let expected_models = conversation["models"]
            .as_object()
            .expect("models")
            .iter()
            .map(|(key, count)| (key.clone(), count.as_u64().expect("count")))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            models.remove(id).unwrap_or_default(),
            expected_models,
            "models of {id}"
        );
        let name = scan
            .session_names
            .iter()
            .find(|name| name.local_session_id_hash == hash_text(id));
        match conversation["parent"].as_str() {
            Some(parent) => {
                let name = name.expect("sub-agent session name");
                assert_eq!(
                    name.parent_local_session_id_hash.as_deref(),
                    Some(hash_text(parent).as_str())
                );
                assert_eq!(
                    Some(name.title.as_str()),
                    conversation["agent"].as_str().or(Some("sub-agent"))
                );
            }
            None => assert!(name.is_none(), "{id} is not a sub-agent"),
        }
        let branch = scan
            .events
            .iter()
            .filter(|event| {
                event.session.local_session_id_hash.as_deref() == Some(hash_text(id).as_str())
            })
            .find_map(|event| event.project.as_ref())
            .and_then(|project| project.branch_label.clone());
        if conversation["planner"]["calls"].as_u64().unwrap_or(0) > 0 {
            assert_eq!(
                branch.as_deref(),
                conversation["branch"].as_str(),
                "branch of {id}"
            );
        }
    }
    assert!(planner.is_empty() && checkpoints.is_empty());

    // Every request becomes exactly one event, and a second scan builds the same ids.
    let rescan = scan_root(&root);
    let ids = |scan: &AdapterScan| {
        scan.events
            .iter()
            .map(|event| event.event_id.clone())
            .collect::<HashSet<_>>()
    };
    assert_eq!(ids(&scan), ids(&rescan));
    assert_eq!(ids(&scan).len(), scan.events.len());
}

/// Totals per usage kind over a real store, in the shape
/// `reference_totals.py` prints, for comparing with an independent decoder on
/// the maintainer's machine. Reads `STATSAI_ANTIGRAVITY_ACCEPTANCE_ROOT`, the
/// folder that holds `conversations/`, read-only.
#[test]
#[ignore = "reads a local Antigravity store named by STATSAI_ANTIGRAVITY_ACCEPTANCE_ROOT"]
fn antigravity_acceptance_totals() {
    let root = std::env::var("STATSAI_ANTIGRAVITY_ACCEPTANCE_ROOT").expect("root");
    let scan = scan_root(Path::new(&root));
    let mut planner = Totals::default();
    let mut checkpoint = Totals::default();
    let mut models = BTreeMap::<String, u64>::new();
    for event in &scan.events {
        if is_checkpoint(event) {
            checkpoint.add(&event.usage);
        } else {
            planner.add(&event.usage);
        }
        *models.entry(model_key(event)).or_default() += 1;
    }
    println!("planner    {planner:?}");
    println!("checkpoint {checkpoint:?}");
    for (model, count) in models {
        println!("model {model} x{count}");
    }
    println!("diagnostics {:?}", scan.diagnostics);
}
