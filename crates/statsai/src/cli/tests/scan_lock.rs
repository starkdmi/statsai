use super::*;

fn parse(args: &[&str]) -> Command {
    Cli::try_parse_from(args).expect("parse").command
}

#[test]
fn scanning_commands_take_the_scan_lock_and_others_do_not() {
    let takes = |args: &[&str]| command_takes_scan_lock(&parse(args));
    assert!(takes(&["statsai", "scan"]));
    assert!(takes(&["statsai", "scan", "--preview"]));
    assert!(takes(&["statsai", "sync"]));
    assert!(takes(&[
        "statsai",
        "sync",
        "--sink",
        "http",
        "--since-last"
    ]));
    assert!(takes(&["statsai", "sync", "--dry-run"]));
    // It clears the sync tracking a running sync records.
    assert!(takes(&[
        "statsai",
        "sync",
        "--sink",
        "http",
        "--reset-remote",
        "--yes"
    ]));
    assert!(!takes(&["statsai", "sync", "--status"]));
    assert!(!takes(&["statsai", "sync", "--verify"]));
    // The watch daemon takes it per pass, not for its lifetime.
    assert!(!takes(&["statsai", "daemon", "--watch"]));
    assert!(!takes(&["statsai", "report", "weekly"]));
    assert!(!takes(&["statsai", "status"]));
}

#[test]
fn a_scanning_command_reports_a_busy_lock_after_waiting() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store_path = directory.path().join("statsai.sqlite");
    let lock_path = statsai::scan_lock_path(&store_path);
    let wait = StdDuration::from_millis(100);
    let other_scanner = statsai::try_acquire_scan_lock(&lock_path)
        .expect("acquire")
        .expect("uncontended lock");

    let started = std::time::Instant::now();
    match acquire_command_scan_lock(&parse(&["statsai", "scan"]), &store_path, wait) {
        CommandScanLock::Busy(reported) => assert_eq!(reported, lock_path),
        _ => panic!("scan must see the lock as busy"),
    }
    assert!(started.elapsed() >= wait, "scan gave up without waiting");
    assert!(matches!(
        acquire_command_scan_lock(&parse(&["statsai", "status"]), &store_path, wait),
        CommandScanLock::NotNeeded
    ));

    drop(other_scanner);
    let held = match acquire_command_scan_lock(&parse(&["statsai", "sync"]), &store_path, wait) {
        CommandScanLock::Held(guard) => guard,
        _ => panic!("sync must take the free lock"),
    };
    assert_eq!(held.path(), lock_path);
    assert!(statsai::try_acquire_scan_lock(&lock_path)
        .expect("try")
        .is_none());
}

#[test]
fn a_scan_lock_that_cannot_be_opened_is_reported_rather_than_fatal() {
    let directory = tempfile::tempdir().expect("tempdir");
    // A regular file where the store's directory should be: the lock file
    // beneath it can be neither created nor opened.
    let not_a_directory = directory.path().join("not-a-directory");
    std::fs::write(&not_a_directory, b"").expect("regular file");
    let store_path = not_a_directory.join("statsai.sqlite");

    match acquire_command_scan_lock(
        &parse(&["statsai", "scan"]),
        &store_path,
        StdDuration::from_millis(100),
    ) {
        CommandScanLock::Unavailable { path, error } => {
            assert_eq!(path, statsai::scan_lock_path(&store_path));
            assert!(
                format!("{error:#}").contains("scan lock"),
                "the error names the lock: {error:#}"
            );
        }
        _ => panic!("an unusable lock must be reported as unavailable"),
    }
}
