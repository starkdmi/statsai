//! The scan lock keeps two scanners apart, whether they are threads with their
//! own opens of the lock file or separate processes.

use statsai_store::{acquire_scan_lock_with_timeout, scan_lock_path, try_acquire_scan_lock};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Set only on the child process the cross-process test spawns.
const CHILD_LOCK_PATH_ENV: &str = "STATSAI_SCAN_LOCK_TEST_CHILD_LOCK_PATH";
const CHILD_HOLDS_LOCK: &str = "STATSAI-TEST-CHILD-HOLDS-THE-SCAN-LOCK";

fn lock_path(directory: &tempfile::TempDir) -> PathBuf {
    scan_lock_path(&directory.path().join("statsai.sqlite"))
}

#[test]
fn a_held_lock_is_busy_for_another_open_until_released() {
    let directory = tempfile::tempdir().expect("tempdir");
    let lock = lock_path(&directory);

    let held = try_acquire_scan_lock(&lock)
        .expect("acquire")
        .expect("uncontended");
    assert!(try_acquire_scan_lock(&lock).expect("try").is_none());
    let started = Instant::now();
    assert!(
        acquire_scan_lock_with_timeout(&lock, Duration::from_millis(150))
            .expect("wait")
            .is_none()
    );
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "gave up before the timeout"
    );

    drop(held);
    assert!(try_acquire_scan_lock(&lock).expect("retry").is_some());
    assert!(lock.exists(), "the lock file outlives its holders");
}

#[test]
fn a_waiter_gets_the_lock_when_the_holder_releases_it() {
    let directory = tempfile::tempdir().expect("tempdir");
    let lock = lock_path(&directory);
    let held = try_acquire_scan_lock(&lock)
        .expect("acquire")
        .expect("uncontended");

    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            acquire_scan_lock_with_timeout(&lock, Duration::from_secs(30))
                .expect("wait")
                .is_some()
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(!waiter.is_finished(), "the waiter did not wait");
        drop(held);
        assert!(waiter.join().expect("waiter thread"));
    });
}

#[test]
fn threads_with_their_own_opens_hold_the_lock_one_at_a_time() {
    let directory = tempfile::tempdir().expect("tempdir");
    let lock = lock_path(&directory);
    let inside = AtomicUsize::new(0);
    let most_inside = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..6 {
            scope.spawn(|| {
                for _ in 0..5 {
                    let guard = acquire_scan_lock_with_timeout(&lock, Duration::from_secs(30))
                        .expect("wait")
                        .expect("the lock frees up within the timeout");
                    let now_inside = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    most_inside.fetch_max(now_inside, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(2));
                    inside.fetch_sub(1, Ordering::SeqCst);
                    drop(guard);
                }
            });
        }
    });

    assert_eq!(most_inside.load(Ordering::SeqCst), 1);
}

#[test]
fn a_lock_held_by_another_process_is_busy_until_it_exits() {
    let directory = tempfile::tempdir().expect("tempdir");
    let lock = lock_path(&directory);
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "hold_the_scan_lock_for_the_parent_test",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_LOCK_PATH_ENV, &lock)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the lock-holding child");
    let mut lines = BufReader::new(child.stdout.take().expect("child stdout")).lines();
    loop {
        let line = lines
            .next()
            .expect("the child exited before taking the lock")
            .expect("read child stdout");
        if line.contains(CHILD_HOLDS_LOCK) {
            break;
        }
    }

    assert!(try_acquire_scan_lock(&lock).expect("try").is_none());
    assert!(
        acquire_scan_lock_with_timeout(&lock, Duration::from_millis(100))
            .expect("wait")
            .is_none()
    );

    // Closing its stdin tells the child to release the lock and exit.
    drop(child.stdin.take());
    assert!(child.wait().expect("child exit").success());
    assert!(
        acquire_scan_lock_with_timeout(&lock, Duration::from_secs(10))
            .expect("wait after exit")
            .is_some()
    );
}

/// The other process in the test above. Does nothing when run on its own.
#[test]
#[ignore = "the child process of a_lock_held_by_another_process_is_busy_until_it_exits"]
fn hold_the_scan_lock_for_the_parent_test() {
    let Some(lock) = std::env::var_os(CHILD_LOCK_PATH_ENV) else {
        return;
    };
    let guard = try_acquire_scan_lock(Path::new(&lock))
        .expect("acquire")
        .expect("the parent test leaves the lock free for the child");
    println!("{CHILD_HOLDS_LOCK}");
    std::io::stdout().flush().expect("flush");
    let mut rest = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut rest);
    drop(guard);
}
