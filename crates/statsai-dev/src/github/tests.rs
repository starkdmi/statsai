use super::*;
use std::sync::{Mutex, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn a_cut_off_download_is_retried_rather_than_failing_the_install() {
    // The wait loop retries the build lookup, not the transfer, so before
    // this a single interrupted read ended the whole install.
    let mut attempts = 0;
    let mut slept = Vec::new();
    let mut warnings = Vec::new();
    let bytes = retrying_download(
        || {
            attempts += 1;
            if attempts < 3 {
                Err(GitHubRequestFailure {
                    message: "timed out reading response".to_string(),
                    retry: Some(RetryAdvice { delay: None }),
                }
                .into())
            } else {
                Ok(vec![7, 7, 7])
            }
        },
        |delay| slept.push(delay),
        |message| warnings.push(message),
    )
    .expect("third attempt succeeds");

    assert_eq!(bytes, vec![7, 7, 7]);
    assert_eq!(attempts, 3);
    assert_eq!(
        slept,
        vec![DOWNLOAD_RETRY_BASE_DELAY, DOWNLOAD_RETRY_BASE_DELAY * 2]
    );
    assert_eq!(warnings.len(), 2);
    assert!(warnings[0].contains("retrying download"));
}

#[test]
fn a_download_gives_up_after_the_attempt_limit() {
    let mut attempts = 0;
    let error = retrying_download(
        || {
            attempts += 1;
            Err(GitHubRequestFailure {
                message: "timed out reading response".to_string(),
                retry: Some(RetryAdvice { delay: None }),
            }
            .into())
        },
        |_| {},
        |_| {},
    )
    .expect_err("always fails");

    assert_eq!(attempts, MAX_DOWNLOAD_ATTEMPTS);
    assert!(format!("{error:#}").contains("download failed"));
}

#[test]
fn an_oversized_artifact_is_not_retried() {
    // Re-requesting cannot make it smaller.
    let mut attempts = 0;
    let error = retrying_download(
        || {
            attempts += 1;
            bail!("artifact exceeds the 256 MiB safety limit")
        },
        |_| panic!("must not sleep"),
        |_| panic!("must not warn"),
    )
    .expect_err("permanent failure");

    assert_eq!(attempts, 1);
    assert!(format!("{error:#}").contains("safety limit"));
}

#[test]
fn the_download_timeout_is_generous_and_overridable() {
    let _guard = env_lock().lock().unwrap_or_else(|error| error.into_inner());
    std::env::remove_var(DOWNLOAD_TIMEOUT_ENV);
    // A 256 MiB artifact cannot arrive inside the API timeout on an
    // ordinary connection, which is what made downloads fail at random.
    assert_eq!(download_timeout(), Some(DOWNLOAD_TIMEOUT));
    assert!(DOWNLOAD_TIMEOUT > API_TIMEOUT);

    std::env::set_var(DOWNLOAD_TIMEOUT_ENV, "45");
    assert_eq!(download_timeout(), Some(Duration::from_secs(45)));

    std::env::set_var(DOWNLOAD_TIMEOUT_ENV, "0");
    assert_eq!(download_timeout(), None, "zero waits indefinitely");

    std::env::set_var(DOWNLOAD_TIMEOUT_ENV, "not-a-number");
    assert_eq!(download_timeout(), Some(DOWNLOAD_TIMEOUT));

    std::env::remove_var(DOWNLOAD_TIMEOUT_ENV);
}

fn run(id: u64, attempt: u64, status: &str, conclusion: Option<&str>, sha: &str) -> WorkflowRun {
    WorkflowRun {
        id,
        run_attempt: attempt,
        status: status.to_string(),
        conclusion: conclusion.map(str::to_string),
        head_sha: sha.to_string(),
        updated_at: format!("2026-08-19T00:00:{id:02}Z"),
        html_url: String::new(),
    }
}

#[test]
fn newest_successful_attempt_for_exact_sha_wins() {
    let sha = "a".repeat(40);
    let other = "b".repeat(40);
    let lookup = classify_runs(
        vec![
            run(1, 1, "completed", Some("success"), &sha),
            run(2, 2, "completed", Some("failure"), &sha),
            run(3, 3, "completed", Some("success"), &sha),
            run(4, 4, "completed", Some("success"), &other),
        ],
        &sha,
    );

    assert!(matches!(
        lookup,
        BuildLookup::Successful(WorkflowRun { id: 3, .. })
    ));
}

#[test]
fn another_sha_is_never_substituted() {
    let requested = "a".repeat(40);
    let other = "b".repeat(40);

    assert_eq!(
        classify_runs(
            vec![run(9, 1, "completed", Some("success"), &other)],
            &requested
        ),
        BuildLookup::Missing
    );
}

#[test]
fn pending_exact_run_is_reported_without_falling_back() {
    let sha = "a".repeat(40);
    assert!(matches!(
        classify_runs(vec![run(7, 1, "in_progress", None, &sha)], &sha),
        BuildLookup::Pending(WorkflowRun { id: 7, .. })
    ));
}

#[test]
fn full_sha_validation_rejects_abbreviations() {
    assert!(validate_full_sha("abc123").is_err());
    assert!(validate_full_sha(&"a".repeat(40)).is_ok());
    assert!(validate_full_sha(&format!("{}g", "a".repeat(39))).is_err());
}

#[test]
fn failed_rerun_does_not_select_an_inaccessible_historical_artifact() {
    let sha = "a".repeat(40);
    let lookup = classify_runs(
        vec![
            run(104, 2, "completed", Some("failure"), &sha),
            run(104, 1, "completed", Some("success"), &sha),
        ],
        &sha,
    );

    assert!(matches!(
        lookup,
        BuildLookup::Failed(WorkflowRun {
            id: 104,
            run_attempt: 2,
            ..
        })
    ));
}

#[test]
fn rate_limit_retry_honors_github_response_headers() {
    assert_eq!(
        http_retry_advice(429, Some("45"), None, None, "rate limited", 100),
        Some(RetryAdvice {
            delay: Some(Duration::from_secs(45)),
        })
    );
    assert_eq!(
        http_retry_advice(
            403,
            None,
            Some("0"),
            Some("160"),
            "API rate limit exceeded",
            100,
        ),
        Some(RetryAdvice {
            delay: Some(Duration::from_secs(60)),
        })
    );
}

#[test]
fn transient_server_failures_retry_but_ordinary_client_errors_do_not() {
    assert_eq!(
        http_retry_advice(503, None, None, None, "unavailable", 100),
        Some(RetryAdvice { delay: None })
    );
    assert_eq!(
        http_retry_advice(403, None, None, None, "forbidden", 100),
        None
    );
    assert_eq!(
        http_retry_advice(404, None, None, None, "not found", 100),
        None
    );
}

#[test]
fn being_main_and_being_contained_in_main_are_different_answers() {
    assert_eq!(
        classify_main_ancestry("identical"),
        MainAncestry::IsMainHead
    );
    // Contained in main, but a later commit may have reverted it.
    assert_eq!(classify_main_ancestry("behind"), MainAncestry::BehindMain);
    assert_eq!(classify_main_ancestry("ahead"), MainAncestry::NotOnMain);
    assert_eq!(classify_main_ancestry("diverged"), MainAncestry::NotOnMain);
    // An answer this code does not recognise must not be read as permission to
    // migrate production data.
    assert_eq!(classify_main_ancestry(""), MainAncestry::NotOnMain);
}
