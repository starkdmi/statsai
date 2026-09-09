//! Dev-only synthetic activity corpus expander.
//!
//! Not shipped. Expands Phase 0 fixtures into many sessions under a temp root
//! so scan overhead can be measured without reading a developer's private stores.

use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};

const CODEX_NATIVE: &str = include_str!(
    "../../tests/fixtures/codex/activity-native/sessions/2026/01/01/rollout-fixture-activity-native.jsonl"
);
const CLAUDE_SESSION: &str = include_str!(
    "../../tests/fixtures/claude/activity/projects/-workspace-activity/session-activity.jsonl"
);
const GROK_EVENTS: &str = include_str!(
    "../../tests/fixtures/grok/activity-events/sessions/workspace-fixture/ses_fixture_activity_events/events.jsonl"
);
const GROK_SUMMARY: &str = include_str!(
    "../../tests/fixtures/grok/activity-events/sessions/workspace-fixture/ses_fixture_activity_events/summary.json"
);

/// Layout used by `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `OPENCODE_DATA_DIR`, and `GROK_HOME`.
#[derive(Debug, Clone)]
pub struct SyntheticActivityCorpus {
    pub codex_home: PathBuf,
    pub claude_config_dir: PathBuf,
    pub opencode_data_dir: PathBuf,
    pub grok_home: PathBuf,
}

/// Write a mixed synthetic corpus. OpenCode receives `opencode_parts` tool `part` rows.
pub fn write_synthetic_activity_corpus(
    root: &Path,
    jsonl_copies: usize,
    opencode_parts: usize,
) -> std::io::Result<SyntheticActivityCorpus> {
    let codex_home = root.join("codex");
    let claude_config_dir = root.join("claude");
    let opencode_data_dir = root.join("opencode");
    let grok_home = root.join("grok");
    write_codex_native_corpus(&codex_home, jsonl_copies)?;
    write_claude_corpus(&claude_config_dir, jsonl_copies)?;
    write_grok_corpus(&grok_home, jsonl_copies)?;
    write_opencode_corpus(&opencode_data_dir, opencode_parts)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    Ok(SyntheticActivityCorpus {
        codex_home,
        claude_config_dir,
        opencode_data_dir,
        grok_home,
    })
}

/// Write `copies` Codex native sessions under `root` (`CODEX_HOME` layout).
pub fn write_codex_native_corpus(root: &Path, copies: usize) -> std::io::Result<PathBuf> {
    let sessions = root.join("sessions").join("2026").join("01").join("01");
    fs::create_dir_all(&sessions)?;
    for index in 0..copies {
        let contents = CODEX_NATIVE
            .replace("thread_fixture_001", &format!("thread_fixture_{index:08}"))
            .replace("item_fixture_", &format!("item_fixture_{index:08}_"))
            .replace(
                "id_fixture_activity_native",
                &format!("id_fixture_activity_native_{index:08}"),
            );
        fs::write(
            sessions.join(format!("rollout-fixture-activity-native-{index:08}.jsonl")),
            contents,
        )?;
    }
    Ok(sessions)
}

fn write_claude_corpus(root: &Path, copies: usize) -> std::io::Result<PathBuf> {
    let projects = root.join("projects").join("-workspace-activity-corpus");
    fs::create_dir_all(&projects)?;
    for index in 0..copies {
        let contents = CLAUDE_SESSION
            .replace("toolu_fixture_", &format!("toolu_fixture_{index:08}_"))
            .replace("u1", &format!("u1_{index:08}"))
            .replace("u2", &format!("u2_{index:08}"))
            .replace("s_fixture_001", &format!("s_fixture_{index:08}"));
        fs::write(
            projects.join(format!("session-activity-{index:08}.jsonl")),
            contents,
        )?;
    }
    Ok(projects)
}

fn write_grok_corpus(root: &Path, copies: usize) -> std::io::Result<PathBuf> {
    for index in 0..copies {
        let session = root
            .join("sessions")
            .join("workspace-fixture")
            .join(format!("ses_fixture_activity_events_{index:08}"));
        fs::create_dir_all(&session)?;
        let events = GROK_EVENTS
            .replace("tc_fixture_", &format!("tc_fixture_{index:08}_"))
            .replace(
                "ses_fixture_activity_events",
                &format!("ses_fixture_activity_events_{index:08}"),
            );
        fs::write(session.join("events.jsonl"), events)?;
        fs::write(session.join("summary.json"), GROK_SUMMARY)?;
    }
    Ok(root.join("sessions"))
}

fn write_opencode_corpus(root: &Path, parts: usize) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(root)?;
    let db_path = root.join("opencode.db");
    let connection = Connection::open(&db_path)?;
    connection.execute_batch(
        r#"
        CREATE TABLE session (
          id TEXT PRIMARY KEY, title TEXT, model TEXT, cost REAL NOT NULL DEFAULT 0,
          tokens_input INTEGER NOT NULL DEFAULT 0, tokens_output INTEGER NOT NULL DEFAULT 0,
          tokens_reasoning INTEGER NOT NULL DEFAULT 0, tokens_cache_read INTEGER NOT NULL DEFAULT 0,
          tokens_cache_write INTEGER NOT NULL DEFAULT 0, time_created INTEGER NOT NULL,
          time_updated INTEGER NOT NULL, directory TEXT NOT NULL
        );
        CREATE TABLE part (
          id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
          time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
        );
        "#,
    )?;
    connection.execute(
        "INSERT INTO session VALUES (?1, ?2, ?3, 0, 1, 1, 0, 0, 0, ?4, ?5, ?6)",
        rusqlite::params![
            "ses_fixture_activity_corpus",
            "Synthetic corpus",
            "gpt-5",
            1_767_225_600_000i64,
            1_767_225_600_000i64 + parts as i64,
            "/fixture/project",
        ],
    )?;
    let mut insert = connection.prepare(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for index in 0..parts {
        let start = 1_767_225_600_000i64 + index as i64;
        let data = format!(
            r#"{{"type":"tool","callID":"call_fixture_{index:08}","tool":"read","state":{{"status":"completed","input":{{"filePath":"/fixture/file"}},"output":"...","title":"read","metadata":{{}},"time":{{"start":{start},"end":{end}}}}}}}"#,
            start = start,
            end = start + 3
        );
        insert.execute(rusqlite::params![
            format!("part_fixture_{index:08}"),
            format!("msg_fixture_{index:08}"),
            "ses_fixture_activity_corpus",
            start,
            start + 3,
            data,
        ])?;
    }
    Ok(db_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodexAdapter;
    use statsai_core::{LocationOrigin, SourceLocation};
    use std::fs;

    #[test]
    fn expanded_codex_corpus_extracts_activity() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_codex_native_corpus(dir.path(), 3).expect("corpus");
        let source = SourceLocation::local_adapter(
            crate::CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );
        let scan =
            crate::codex::scan_codex_source(&CodexAdapter, &source, &crate::tests::options())
                .expect("scan");
        assert!(scan.activity_invocations.len() >= 3);
        let bytes = fs::read_dir(dir.path().join("sessions/2026/01/01"))
            .expect("sessions")
            .count();
        assert_eq!(bytes, 3);
    }

    #[test]
    fn mixed_corpus_writes_all_provider_layouts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = write_synthetic_activity_corpus(dir.path(), 2, 8).expect("corpus");
        assert!(
            corpus
                .codex_home
                .join("sessions/2026/01/01")
                .read_dir()
                .expect("codex")
                .count()
                >= 2
        );
        assert!(corpus.claude_config_dir.join("projects").is_dir());
        assert!(corpus.opencode_data_dir.join("opencode.db").is_file());
        assert!(corpus.grok_home.join("sessions").is_dir());
    }
}
