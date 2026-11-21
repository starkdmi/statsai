use super::models::{label_effort, ModelNames};
use super::proto::{Message, WireValue};
use super::*;
use rusqlite::Connection;
use statsai_core::ReasoningLevel;
use std::collections::HashSet;

mod fixtures;
mod proto_writer;

use proto_writer::Pb;

const SCHEMA: &str = "
CREATE TABLE `trajectory_meta` (`trajectory_id` text,`cascade_id` text,`trajectory_type` integer,`source` integer,PRIMARY KEY (`trajectory_id`));
CREATE TABLE `steps` (`idx` integer,`step_type` integer NOT NULL DEFAULT 0,`status` integer NOT NULL DEFAULT 0,`has_subtrajectory` numeric NOT NULL DEFAULT false,`metadata` blob,`error_details` blob,`permissions` blob,`task_details` blob,`render_info` blob,`step_payload` blob,`step_format` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`));
CREATE TABLE `gen_metadata` (`idx` integer,`data` blob,`size` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`));
CREATE TABLE `executor_metadata` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE TABLE `parent_references` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE TABLE `trajectory_metadata_blob` (`id` text DEFAULT \"main\",`data` blob,PRIMARY KEY (`id`));
CREATE TABLE `battle_mode_infos` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE INDEX `idx_steps_status` ON `steps`(`status`);
CREATE INDEX `idx_steps_step_type` ON `steps`(`step_type`);
";

const T0: u64 = 1_790_000_000;

/// `ModelUsageStats` with invented counts.
#[derive(Debug, Clone, Copy, Default)]
struct Usage {
    model: u64,
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
    provider: u64,
    thinking: u64,
    response: u64,
}

impl Usage {
    fn new(
        model: u64,
        provider: u64,
        input: u64,
        cache_read: u64,
        thinking: u64,
        response: u64,
    ) -> Self {
        Self {
            model,
            input,
            output: thinking + response,
            cache_write: 0,
            cache_read,
            provider,
            thinking,
            response,
        }
    }

    fn encode(&self) -> Pb {
        let mut pb = Pb::new();
        for (field, value) in [
            (1, self.model),
            (2, self.input),
            (3, self.output),
            (4, self.cache_write),
            (5, self.cache_read),
            (6, self.provider),
            (9, self.thinking),
            (10, self.response),
        ] {
            if value > 0 {
                pb = pb.varint(field, value);
            }
        }
        pb.message(
            8,
            Pb::new()
                .string(1, "sessionID")
                .string(2, "-1234567890123456789"),
        )
        .string(11, "response-fixture")
    }
}

fn timestamp(seconds: u64, millis: u64) -> Pb {
    Pb::new().varint(1, seconds).varint(2, millis * 1_000_000)
}

fn planner_metadata(conversation: &str, step: i64, usage: Option<Usage>, created: u64) -> Vec<u8> {
    // created → viewable is the first token, created → finished the latency;
    // started_at repeats finished_generating_at, as Antigravity records it.
    let mut pb = Pb::new()
        .message(1, timestamp(created, 0))
        .varint(3, 2)
        .message(6, timestamp(created + 2, 500))
        .message(7, timestamp(created + 6, 0))
        .message(8, timestamp(created + 6, 0));
    if let Some(usage) = usage {
        pb = pb.message(9, usage.encode());
    }
    pb.varint(11, usage.map(|usage| usage.model).unwrap_or(1026))
        .string(12, "00000000-0000-4000-8000-00000000e001")
        .message(
            20,
            Pb::new()
                .string(1, "00000000-0000-4000-8000-00000000a001")
                .varint(2, step as u64)
                .string(4, conversation),
        )
        .message(32, timestamp(created + 6, 0))
        .finish()
}

fn checkpoint_metadata(
    conversation: &str,
    step: i64,
    attempts: &[Option<Usage>],
    duplicate_in_model_usage: bool,
    created: u64,
) -> Vec<u8> {
    let mut pb = Pb::new()
        .message(1, timestamp(created, 0))
        .varint(3, 5)
        .message(8, timestamp(created + 2, 0));
    if duplicate_in_model_usage {
        if let Some(Some(usage)) = attempts.last() {
            pb = pb.message(9, usage.encode());
        }
    }
    pb = pb.message(
        20,
        Pb::new()
            .string(1, "00000000-0000-4000-8000-00000000a001")
            .varint(2, step as u64)
            .string(4, conversation),
    );
    for attempt in attempts {
        let mut retry = Pb::new();
        if let Some(usage) = attempt {
            retry = retry.message(2, usage.encode());
        }
        pb = pb.message(28, retry.string(4, "retry-fixture"));
    }
    pb.message(32, timestamp(created, 0)).finish()
}

/// A `gen_metadata` row. The prompt (`1.9`) and the request settings
/// (`3.13`) are deliberately not protobuf, so reading them would fail.
fn gen_row(
    model: u64,
    served: Option<&str>,
    label: Option<&str>,
    attempts: &[Option<Usage>],
) -> Vec<u8> {
    let garbage = [0xffu8, 0xff, 0xff, 0xff, 0x0f, 0x07, 0x07];
    let mut pb = Pb::new().bytes(2, &[1, 2, 3]);
    if let Some(label) = label {
        pb = pb.message(
            3,
            Pb::new()
                .varint(1, model)
                .varint(6, 65_536)
                .bytes(13, &garbage)
                .string(28, label),
        );
    }
    pb = pb
        .string(4, "00000000-0000-4000-8000-00000000e001")
        .bytes(8, &[0; 16]);
    let mut request = Pb::new().varint(3, model).bytes(9, &garbage);
    for attempt in attempts {
        let mut entry = Pb::new();
        if let Some(usage) = attempt {
            entry = entry.message(2, usage.encode());
        }
        request = request.message(17, entry.string(4, "attempt-fixture"));
    }
    if let Some(served) = served {
        request = request.string(19, served);
    }
    request = request.message(
        20,
        Pb::new()
            .string(1, "model_enum")
            .string(2, "MODEL_PLACEHOLDER_M26"),
    );
    pb.message(1, request).finish()
}

#[derive(Default)]
struct Trajectory<'a> {
    workspace: Option<&'a str>,
    repository: Option<(&'a str, &'a str)>,
    branch: Option<&'a str>,
    parent: Option<&'a str>,
    agent: Option<&'a str>,
}

impl Trajectory<'_> {
    fn encode(&self) -> Vec<u8> {
        let mut workspace = Pb::new();
        if let Some(uri) = self.workspace {
            workspace = workspace.string(1, uri).string(2, uri);
        }
        if let Some((first, second)) = self.repository {
            workspace = workspace.message(3, Pb::new().string(1, first).string(2, second));
        }
        if let Some(branch) = self.branch {
            workspace = workspace.string(4, branch);
        }
        let mut pb = Pb::new()
            .message(1, workspace)
            .message(2, timestamp(T0, 0))
            .string(3, "00000000-0000-4000-8000-00000000b001");
        if let Some(agent) = self.agent {
            pb = pb.message(4, Pb::new().string(1, agent));
        }
        if let Some(parent) = self.parent {
            pb = pb.string(5, parent);
        }
        if let Some(uri) = self.workspace {
            pb = pb.string(7, uri);
        }
        if let Some(agent) = self.agent {
            pb = pb.message(8, Pb::new().string(1, agent).varint(7, 1026));
        }
        pb.finish()
    }
}

struct ConversationDb {
    connection: Connection,
}

impl ConversationDb {
    fn create(path: &Path, conversation: &str, trajectory: &Trajectory<'_>) -> Self {
        let connection = Connection::open(path).expect("db");
        connection.execute_batch(SCHEMA).expect("schema");
        connection
            .execute(
                "INSERT INTO trajectory_meta VALUES (?1, ?2, 4, 1)",
                rusqlite::params!["00000000-0000-4000-8000-00000000a001", conversation],
            )
            .expect("trajectory_meta");
        connection
            .execute(
                "INSERT INTO trajectory_metadata_blob VALUES ('main', ?1)",
                [trajectory.encode()],
            )
            .expect("trajectory");
        Self { connection }
    }

    fn step(&self, idx: i64, step_type: i64, metadata: Vec<u8>) -> &Self {
        self.connection
            .execute(
                "INSERT INTO steps (idx, step_type, status, metadata, step_payload) VALUES (?1, ?2, 3, ?3, ?4)",
                rusqlite::params![idx, step_type, metadata, b"not protobuf".to_vec()],
            )
            .expect("step");
        self
    }

    fn gen(&self, idx: i64, data: Vec<u8>) -> &Self {
        self.connection
            .execute(
                "INSERT INTO gen_metadata (idx, data, size) VALUES (?1, ?2, ?3)",
                rusqlite::params![idx, data.clone(), data.len() as i64],
            )
            .expect("gen");
        self
    }
}

fn conversation_id(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

fn store() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join(".gemini/antigravity");
    std::fs::create_dir_all(root.join("conversations")).expect("conversations");
    (dir, root)
}

fn db_path(root: &Path, conversation: &str) -> PathBuf {
    root.join("conversations")
        .join(format!("{conversation}.db"))
}

fn source_for(root: &Path) -> SourceLocation {
    SourceLocation::local_adapter(
        ANTIGRAVITY_PROVIDER,
        AntigravityAdapter.id(),
        "0",
        root,
        LocationOrigin::Configured,
    )
}

fn scan_root(root: &Path) -> AdapterScan {
    scan_antigravity_source(&AntigravityAdapter, &source_for(root), &options()).expect("scan")
}

fn record_key(event: &UsageEvent) -> &str {
    event
        .parse_evidence
        .as_ref()
        .and_then(|evidence| evidence.source_record_id.as_deref())
        .expect("record key")
}

fn is_checkpoint(event: &UsageEvent) -> bool {
    record_key(event).contains("antigravity_checkpoint")
}

/// One Gemini conversation: a planner response with thinking, one without
/// usage, and a checkpoint whose usage older files also copy into field 9.
fn write_gemini_conversation(root: &Path, conversation: &str) {
    let db = ConversationDb::create(
        &db_path(root, conversation),
        conversation,
        &Trajectory {
            workspace: Some("file:///workspace/sample-project"),
            repository: Some((
                "acme/sample-project",
                "https://example.invalid/acme/sample-project.git",
            )),
            branch: Some("fixture/main"),
            ..Trajectory::default()
        },
    );
    let usage = Usage::new(1132, 24, 1_000, 4_000, 300, 120);
    db.step(0, 14, Pb::new().varint(3, 1).finish())
        .step(
            1,
            STEP_PLANNER,
            planner_metadata(conversation, 1, Some(usage), T0),
        )
        .step(
            2,
            STEP_PLANNER,
            planner_metadata(conversation, 2, None, T0 + 10),
        )
        .step(
            3,
            STEP_CHECKPOINT,
            checkpoint_metadata(
                conversation,
                3,
                &[Some(Usage::new(1050, 24, 500, 0, 0, 40))],
                true,
                T0 + 20,
            ),
        )
        .gen(
            0,
            gen_row(1132, Some("gemini-3-flash-a"), None, &[Some(usage)]),
        )
        .gen(
            1,
            gen_row(
                1132,
                Some("gemini-3-flash-a"),
                Some("gemini-3-flash-agent"),
                &[None],
            ),
        );
}

const STEP_PLANNER: i64 = conversation::STEP_PLANNER_RESPONSE;
const STEP_CHECKPOINT: i64 = conversation::STEP_CHECKPOINT;

#[test]
fn planner_usage_maps_response_and_thinking_without_counting_thinking_twice() {
    let (_dir, root) = store();
    let conversation = conversation_id(1);
    write_gemini_conversation(&root, &conversation);

    let scan = scan_root(&root);

    let planner = scan
        .events
        .iter()
        .find(|event| !is_checkpoint(event))
        .expect("planner event");
    assert_eq!(planner.provider, ANTIGRAVITY_PROVIDER);
    assert_eq!(planner.usage.input_tokens, Some(1_000));
    assert_eq!(planner.usage.cache_read_tokens, Some(4_000));
    assert_eq!(planner.usage.cache_creation_tokens, None);
    // Output is the response alone; thinking is reasoning. Field 3 (their sum)
    // is never added on top.
    assert_eq!(planner.usage.output_tokens, Some(120));
    assert_eq!(planner.usage.reasoning_tokens, Some(300));
    assert_eq!(planner.usage.requests, Some(1));
    assert_eq!(planner.usage.computed_total(), 1_000 + 4_000 + 120 + 300);
    assert_eq!(planner.created_at.timestamp(), T0 as i64);
    let runtime = planner.runtime.as_ref().expect("runtime");
    assert_eq!(runtime.time_to_first_token_ms, Some(2_500));
    assert_eq!(runtime.latency_ms, Some(6_000));
    assert_eq!(runtime.latency_source, Some(LatencySource::Explicit));
    assert_eq!(
        planner.session.local_session_id_hash.as_deref(),
        Some(hash_text(&conversation).as_str())
    );
    assert_eq!(scan.diagnostics.raw_rows, 3);
    assert_eq!(
        scan.diagnostics.skipped_zero_events, 1,
        "the planner step without usage"
    );
    assert_eq!(scan.diagnostics.invalid_rows, 0);
}

#[test]
fn output_falls_back_to_total_output_less_thinking_when_response_is_missing() {
    let usage = Pb::new().varint(2, 10).varint(3, 70).varint(9, 30).finish();
    let counts = conversation::usage_counts(&Message::parse(&usage).expect("message"));

    assert_eq!(counts.output_tokens, Some(40));
    assert_eq!(counts.reasoning_tokens, Some(30));
}

#[test]
fn checkpoints_count_each_retry_attempt_once_even_when_field_nine_copies_it() {
    let (_dir, root) = store();
    let conversation = conversation_id(2);
    let db = ConversationDb::create(
        &db_path(&root, &conversation),
        &conversation,
        &Trajectory::default(),
    );
    let first = Usage::new(1050, 24, 300, 0, 0, 20);
    let second = Usage::new(1050, 24, 310, 0, 0, 25);
    db.step(
        1,
        STEP_CHECKPOINT,
        checkpoint_metadata(
            &conversation,
            1,
            &[None, Some(first), Some(second)],
            true,
            T0,
        ),
    )
    // An older checkpoint with no retry_infos at all keeps its field 9 usage.
    .step(
        2,
        STEP_CHECKPOINT,
        Pb::new()
            .message(1, timestamp(T0 + 30, 0))
            .message(9, Usage::new(330, 24, 50, 0, 0, 5).encode())
            .finish(),
    );

    let scan = scan_root(&root);

    let mut checkpoints = scan
        .events
        .iter()
        .filter(|event| is_checkpoint(event))
        .map(|event| (event.usage.input_tokens, event.usage.output_tokens))
        .collect::<Vec<_>>();
    checkpoints.sort();
    assert_eq!(
        checkpoints,
        vec![
            (Some(50), Some(5)),
            (Some(300), Some(20)),
            (Some(310), Some(25))
        ]
    );
    let ids = scan
        .events
        .iter()
        .map(|event| &event.event_id)
        .collect::<HashSet<_>>();
    assert_eq!(ids.len(), 3, "every attempt is its own event");
    // Checkpoints record no output timestamps.
    assert!(scan.events.iter().all(|event| event.runtime.is_none()));
}

#[test]
fn model_names_prefer_the_selected_label_learned_across_conversations() {
    let (_dir, root) = store();
    // The first conversation switched models: 1035 was replaced by 1026, and
    // only the latest choice kept its label.
    let switched = conversation_id(3);
    let db = ConversationDb::create(
        &db_path(&root, &switched),
        &switched,
        &Trajectory::default(),
    );
    let sonnet = Usage::new(1035, 26, 100, 0, 0, 10);
    let opus = Usage::new(1026, 26, 200, 0, 0, 20);
    db.step(
        1,
        STEP_PLANNER,
        planner_metadata(&switched, 1, Some(sonnet), T0),
    )
    .step(
        2,
        STEP_PLANNER,
        planner_metadata(&switched, 2, Some(opus), T0 + 10),
    )
    .step(
        3,
        STEP_PLANNER,
        planner_metadata(&switched, 3, Some(Usage::new(999, 24, 1, 0, 0, 1)), T0 + 20),
    )
    .step(
        4,
        STEP_PLANNER,
        planner_metadata(&switched, 4, Some(Usage::new(777, 24, 1, 0, 0, 1)), T0 + 30),
    )
    .gen(
        0,
        gen_row(1035, Some("claude-sonnet-4-6"), None, &[Some(sonnet)]),
    )
    .gen(
        1,
        gen_row(
            1026,
            Some("claude-opus-4-6-thinking"),
            Some("claude-opus-4-6-thinking"),
            &[Some(opus)],
        ),
    )
    .gen(2, gen_row(777, Some("gemini-3-flash-a"), None, &[None]));
    // Another conversation selected 1035 with an effort suffix.
    let other = conversation_id(4);
    ConversationDb::create(&db_path(&root, &other), &other, &Trajectory::default()).gen(
        0,
        gen_row(
            1035,
            Some("claude-sonnet-4-6"),
            Some("claude-sonnet-4-6-low"),
            &[None],
        ),
    );

    let scan = scan_root(&root);

    let model_for = |model: &str| {
        scan.events
            .iter()
            .map(|event| event.model.as_ref().expect("model"))
            .find(|info| info.name.as_deref() == Some(model))
            .cloned()
    };
    let sonnet = model_for("claude-sonnet-4-6-low").expect("label from the other conversation");
    assert_eq!(
        sonnet.provider_model_id.as_deref(),
        Some("claude-sonnet-4-6")
    );
    assert_eq!(sonnet.reasoning_level, Some(ReasoningLevel::Low));
    assert_eq!(sonnet.reasoning_level_raw.as_deref(), Some("low"));
    assert_eq!(
        sonnet.normalized_name.as_deref(),
        Some(normalize_model_name("claude-sonnet-4-6-low").as_str())
    );
    let opus = model_for("claude-opus-4-6-thinking").expect("local label");
    assert_eq!(
        opus.provider_model_id.as_deref(),
        Some("claude-opus-4-6-thinking")
    );
    // The served name stands in when no conversation has a label.
    let served = model_for("gemini-3-flash-a").expect("served name");
    assert_eq!(
        served.provider_model_id.as_deref(),
        Some("gemini-3-flash-a")
    );
    // An id no conversation names gets a stable synthetic name.
    let unnamed = model_for("antigravity-model-999").expect("synthetic name");
    assert_eq!(unnamed.provider_model_id, None);
    assert_eq!(scan.diagnostics.model_fallbacks, 1);
}

#[test]
fn selected_labels_carry_their_effort() {
    assert_eq!(label_effort("claude-sonnet-5-5-low"), Some("low"));
    assert_eq!(label_effort("gpt-oss-120b-medium"), Some("medium"));
    assert_eq!(
        label_effort("gemini-3.5-flash-extra-low"),
        Some("extra-low")
    );
    assert_eq!(label_effort("gemini-3.8-flash-high"), Some("high"));
    assert_eq!(label_effort("model-xhigh"), Some("xhigh"));
    assert_eq!(label_effort("claude-opus-4-6-thinking"), None);
    assert_eq!(label_effort("gemini-3-flash-agent"), None);
    assert_eq!(label_effort("low"), None);
}

#[test]
fn conflicting_names_resolve_the_same_way_in_any_order() {
    let mut forward = ModelNames::default();
    forward.add_label(1, "b-model");
    forward.add_label(1, "a-model");
    let mut backward = ModelNames::default();
    backward.add_label(1, "a-model");
    backward.add_label(1, "b-model");

    assert_eq!(forward.label(1), Some("a-model"));
    assert_eq!(backward.label(1), Some("a-model"));

    forward.add_label(1, "b-model");
    assert_eq!(forward.label(1), Some("b-model"), "the most frequent wins");
    forward.add_label(2, "not a model name");
    assert_eq!(forward.label(2), None);
}

#[test]
fn rescans_keep_event_ids_and_add_only_new_steps() {
    let (_dir, root) = store();
    let conversation = conversation_id(5);
    write_gemini_conversation(&root, &conversation);
    let unchanged = conversation_id(6);
    write_gemini_conversation(&root, &unchanged);
    let source = source_for(&root);
    let before_candidates = antigravity_scan_candidates(&source, "0").expect("candidates");
    let before = scan_root(&root);

    let db = Connection::open(db_path(&root, &conversation)).expect("db");
    db.execute(
        "INSERT INTO steps (idx, step_type, status, metadata) VALUES (4, ?1, 3, ?2)",
        rusqlite::params![
            STEP_PLANNER,
            planner_metadata(
                &conversation,
                4,
                Some(Usage::new(1132, 24, 9, 0, 1, 2)),
                T0 + 40
            )
        ],
    )
    .expect("new step");
    drop(db);
    let after_candidates = antigravity_scan_candidates(&source, "0").expect("candidates");
    let changed = before_candidates
        .iter()
        .zip(&after_candidates)
        .filter(|(before, after)| before.cache_signature != after.cache_signature)
        .map(|(_, after)| after.cache_key.clone())
        .collect::<HashSet<_>>();
    assert_eq!(
        changed.len(),
        1,
        "only the conversation that changed is read again"
    );

    let rescan = scan_antigravity_source(
        &AntigravityAdapter,
        &source,
        &ScanOptions {
            selected_cache_keys: Some(changed),
            ..options()
        },
    )
    .expect("rescan");

    assert_eq!(rescan.diagnostics.files_scanned, 1);
    assert_eq!(rescan.diagnostics.files_skipped_unchanged, 1);
    let previous = before
        .events
        .iter()
        .filter(|event| {
            event.session.local_session_id_hash.as_deref()
                == Some(hash_text(&conversation).as_str())
        })
        .map(|event| event.event_id.clone())
        .collect::<HashSet<_>>();
    let current = rescan
        .events
        .iter()
        .map(|event| event.event_id.clone())
        .collect::<HashSet<_>>();
    assert_eq!(previous.len(), 2);
    assert!(previous.is_subset(&current));
    assert_eq!(current.difference(&previous).count(), 1, "one new event");
}

#[test]
fn subagents_link_to_their_parent_and_projects_come_from_the_trajectory() {
    let (dir, root) = store();
    let workspace = dir.path().join("workspace/sample-project");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let workspace_uri = url::Url::from_file_path(&workspace)
        .expect("uri")
        .to_string();
    let parent = conversation_id(7);
    let child = conversation_id(8);
    ConversationDb::create(&db_path(&root, &parent), &parent, &Trajectory::default());
    ConversationDb::create(
        &db_path(&root, &child),
        &child,
        &Trajectory {
            workspace: Some(&workspace_uri),
            // The slug and the remote, in either order.
            repository: Some((
                "git@example.invalid:acme/sample-project.git",
                "acme/sample-project",
            )),
            branch: Some("fixture/main"),
            parent: Some(&parent),
            agent: Some("research"),
        },
    )
    .step(
        1,
        STEP_PLANNER,
        planner_metadata(&child, 1, Some(Usage::new(1026, 26, 10, 0, 0, 1)), T0),
    );

    let scan = scan_root(&root);

    assert_eq!(
        scan.session_names,
        vec![statsai_core::SessionName {
            local_session_id_hash: hash_text(&child),
            title: "research".to_string(),
            parent_local_session_id_hash: Some(hash_text(&parent)),
        }]
    );
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(project.branch_label.as_deref(), Some("fixture/main"));
    assert_eq!(project.repo_label.as_deref(), Some("acme/sample-project"));
    assert_eq!(
        project.repo_remote_hash.as_deref(),
        Some(hash_text("example.invalid/acme/sample-project").as_str())
    );
    assert_eq!(
        project.path_label.as_deref(),
        Some(statsai_core::display_path(&workspace).as_str())
    );
}

#[test]
fn encrypted_history_is_counted_and_skipped() {
    let (_dir, root) = store();
    let readable = conversation_id(9);
    write_gemini_conversation(&root, &readable);
    let conversations = root.join("conversations");
    std::fs::write(
        conversations.join(format!("{}.pb", conversation_id(10))),
        [0x9a, 0x01, 0xfe],
    )
    .expect("pb");
    // A conversation that exists in both forms is read from its database.
    std::fs::write(
        conversations.join(format!("{readable}.pb")),
        [0x9a, 0x01, 0xfe],
    )
    .expect("pb");

    let candidates = antigravity_scan_candidates(&source_for(&root), "0").expect("candidates");
    let scan = scan_root(&root);

    assert_eq!(candidates.len(), 1);
    assert_eq!(scan.diagnostics.unreadable_history_files, 1);
    assert_eq!(scan.diagnostics.invalid_rows, 0);
    assert_eq!(scan.events.len(), 2);
}

#[test]
fn copies_beside_the_store_are_never_read() {
    let (dir, root) = store();
    let conversation = conversation_id(11);
    write_gemini_conversation(&root, &conversation);
    for copy in ["antigravity-ide", "antigravity-backup"] {
        let copy_dir = dir.path().join(".gemini").join(copy).join("conversations");
        std::fs::create_dir_all(&copy_dir).expect("copy dir");
        std::fs::copy(
            db_path(&root, &conversation),
            copy_dir.join(format!("{conversation}.db")),
        )
        .expect("copy");
    }

    let scan = scan_root(&root);
    // A source configured as the conversations folder reads the same store.
    let from_conversations = scan_root(&root.join("conversations"));
    // The parent of every copy is not a store.
    let from_parent = scan_root(&dir.path().join(".gemini"));

    assert_eq!(scan.events.len(), 2);
    assert_eq!(from_conversations.events.len(), 2);
    assert!(from_parent.events.is_empty());
    assert!(!antigravity_root_is_source(&dir.path().join(".gemini")));
}

#[test]
fn a_damaged_conversation_does_not_stop_the_scan() {
    let (_dir, root) = store();
    write_gemini_conversation(&root, &conversation_id(12));
    std::fs::write(
        db_path(&root, &conversation_id(13)),
        b"not a sqlite database at all",
    )
    .expect("bad db");
    let empty = conversation_id(14);
    Connection::open(db_path(&root, &empty))
        .expect("empty db")
        .execute_batch("CREATE TABLE other (x);")
        .expect("schema");

    let scan = scan_root(&root);

    assert_eq!(scan.events.len(), 2);
    assert_eq!(scan.diagnostics.files_scanned, 3);
    assert_eq!(scan.diagnostics.invalid_rows, 1);
}

#[test]
fn conversations_still_open_in_wal_mode_are_read_with_their_journal() {
    let (_dir, root) = store();
    let conversation = conversation_id(15);
    let path = db_path(&root, &conversation);
    let db = ConversationDb::create(&path, &conversation, &Trajectory::default());
    db.connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .expect("wal");
    db.step(
        1,
        STEP_PLANNER,
        planner_metadata(
            &conversation,
            1,
            Some(Usage::new(1026, 26, 10, 0, 0, 1)),
            T0,
        ),
    );
    assert!(Path::new(&format!("{}-wal", path.display())).is_file());

    // The writer stays open, so the step is only in the WAL.
    let scan = scan_root(&root);

    assert_eq!(scan.events.len(), 1);
    drop(db);
}

#[test]
fn the_adapter_is_registered_for_its_provider() {
    assert_eq!(
        adapter_for_provider("antigravity")
            .expect("adapter")
            .provider(),
        ANTIGRAVITY_PROVIDER
    );
    assert!(default_adapters()
        .iter()
        .any(|adapter| adapter.provider() == ANTIGRAVITY_PROVIDER));
    assert_eq!(
        antigravity_root(Path::new("/x/.gemini/antigravity/conversations")),
        Path::new("/x/.gemini/antigravity")
    );
}

#[test]
fn gen_metadata_names_are_read_without_decoding_the_prompt() {
    let mut names = ModelNames::default();
    models::add_gen_metadata_names(
        &mut names,
        &gen_row(
            1533,
            Some("gemini-4-neon"),
            Some("gemini-4-neon-medium"),
            &[None],
        ),
    );
    assert_eq!(names.label(1533), Some("gemini-4-neon-medium"));
    assert_eq!(names.served(1533), Some("gemini-4-neon"));
}

#[test]
fn malformed_protobuf_is_rejected_whole() {
    assert!(Message::parse(&[]).is_some());
    // A length past the end.
    assert!(Message::parse(&[0x0a, 0x05, 0x01]).is_none());
    // Field number zero.
    assert!(Message::parse(&[0x00, 0x01]).is_none());
    // A group, which these records never use.
    assert!(Message::parse(&[0x0b]).is_none());
    // A varint longer than ten bytes.
    assert!(Message::parse(&[
        0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01
    ])
    .is_none());
    let message =
        Message::parse(&[0x08, 0x96, 0x01, 0x15, 1, 0, 0, 0, 0x12, 0x01, b'a']).expect("valid");
    assert_eq!(message.varint(1), Some(150));
    assert_eq!(message.string(2), Some("a"));
    assert_eq!(
        message.fields().map(|(field, _)| field).collect::<Vec<_>>(),
        vec![1, 2, 2]
    );
    assert!(message
        .fields()
        .any(|(field, value)| field == 2 && value == WireValue::Fixed32(1)));
}

#[test]
fn fixture_store_matches_the_independent_decoder() {
    fixtures::check_fixture_store();
}

#[test]
fn a_conversation_held_mid_commit_fails_the_scan_so_it_is_read_again() {
    let busy =
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None);
    let locked = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
        None,
    );
    let damaged = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
        None,
    );

    assert!(sqlite_busy(
        &anyhow::Error::new(busy).context("read conversation")
    ));
    assert!(sqlite_busy(&anyhow::Error::new(locked)));
    assert!(!sqlite_busy(&anyhow::Error::new(damaged)));
}
