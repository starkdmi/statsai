pub(crate) use super::*;

mod parse;
mod preview;
mod quota;
mod telemetry;

#[cfg(unix)]
#[test]
fn a_compressed_rollout_keeps_its_session_id_under_symlinked_folders() {
    let temp = tempfile::tempdir().expect("temp");
    let name = "rollout-2026-05-28T10-00-00-019e733b-6b32-7fb2-b570-e8f68052bb36.jsonl";
    let real_sessions = temp.path().join("real-sessions");
    std::fs::create_dir_all(real_sessions.join("2026/05/28")).expect("real sessions");
    std::fs::write(
        real_sessions.join(format!("2026/05/28/{name}.zst")),
        ruzstd::encoding::compress_to_vec(
            &b"{}\n"[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        ),
    )
    .expect("rollout");
    // `sessions/` itself a symlink, then the whole root reached through one.
    let root = temp.path().join("codex");
    std::fs::create_dir_all(&root).expect("root");
    std::os::unix::fs::symlink(&real_sessions, root.join("sessions")).expect("link sessions");
    let linked_root = temp.path().join("linked-codex");
    std::os::unix::fs::symlink(&root, &linked_root).expect("link root");

    for root in [&root, &linked_root] {
        let usage_root = root.join("sessions");
        let listed =
            crate::compressed::collect_jsonl_files_with_compressed(&usage_root).expect("list");
        assert_eq!(listed.len(), 1);
        // What the plain file, listed under `sessions/`, was identified as.
        assert_eq!(
            codex_session_id(&codex_usage_root_for_file(root, &listed[0]), &listed[0]),
            "2026/05/28/rollout-2026-05-28T10-00-00-019e733b-6b32-7fb2-b570-e8f68052bb36",
            "under {}",
            root.display()
        );
    }
}
