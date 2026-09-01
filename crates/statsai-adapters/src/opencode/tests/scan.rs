use super::*;

#[test]
fn opencode_scan_candidates_change_when_wal_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("opencode.db"), "db").expect("db");
    std::fs::write(dir.path().join("opencode.db-wal"), "").expect("wal");
    let source = SourceLocation::local_adapter(
        OPENCODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let before = opencode_scan_candidates(&source, "0").expect("before");
    std::fs::write(dir.path().join("opencode.db-wal"), "wal-data").expect("updated wal");
    let after = opencode_scan_candidates(&source, "0").expect("after");

    assert_eq!(before.len(), 1);
    assert_eq!(after.len(), 1);
    assert_ne!(before[0].cache_signature, after[0].cache_signature);
}

#[test]
fn opencode_scan_candidates_ignore_shm_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("opencode.db"), "db").expect("db");
    std::fs::write(dir.path().join("opencode.db-shm"), "").expect("shm");
    let source = SourceLocation::local_adapter(
        OPENCODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let before = opencode_scan_candidates(&source, "0").expect("before");
    std::fs::write(dir.path().join("opencode.db-shm"), "shm-data").expect("updated shm");
    let after = opencode_scan_candidates(&source, "0").expect("after");

    assert_eq!(before.len(), 1);
    assert_eq!(after.len(), 1);
    assert_eq!(before[0].cache_signature, after[0].cache_signature);
}
