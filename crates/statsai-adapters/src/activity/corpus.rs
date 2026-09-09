//! Dev-only synthetic activity corpus expander.
//!
//! Not shipped. Expands Phase 0 fixtures into many sessions under a temp root
//! so scan overhead can be measured without reading a developer's private stores.

use std::fs;
use std::path::{Path, PathBuf};

const CODEX_NATIVE: &str = include_str!(
    "../../tests/fixtures/codex/activity-native/sessions/2026/01/01/rollout-fixture-activity-native.jsonl"
);

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
}
