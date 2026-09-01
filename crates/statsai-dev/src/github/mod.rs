use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const REPOSITORY: &str = "starkdmi/statsai";
pub(crate) const WORKFLOW_FILE: &str = "dev-build.yml";
const GITHUB_API: &str = "https://api.github.com";
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
/// Enough for a JSON reply, and short enough that an unreachable API gives up
/// promptly.
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// `ureq`'s request timeout covers reading the body too, so the API bound also
/// capped every artifact download. Thirty seconds for an archive that may reach
/// the 256 MiB ceiling demands roughly 68 Mbit/s sustained, which turned an
/// ordinary home connection into an intermittent "timed out reading response".
/// Ten minutes covers the ceiling at about 3.5 Mbit/s.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// Overrides [`DOWNLOAD_TIMEOUT`], in seconds. Zero waits indefinitely.
const DOWNLOAD_TIMEOUT_ENV: &str = "STATSAI_DEV_DOWNLOAD_TIMEOUT_SECONDS";
/// One initial attempt plus two retries.
const MAX_DOWNLOAD_ATTEMPTS: u32 = 3;
const DOWNLOAD_RETRY_BASE_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuildRequest {
    Main,
    Pr(u64),
    Sha(String),
}

impl BuildRequest {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Main => "main".to_string(),
            Self::Pr(number) => format!("PR #{number}"),
            Self::Sha(sha) => format!("commit {}", short_sha(sha)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedBuild {
    pub(crate) request: BuildRequest,
    pub(crate) sha: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct WorkflowRun {
    pub(crate) id: u64,
    #[serde(default = "default_attempt")]
    pub(crate) run_attempt: u64,
    pub(crate) status: String,
    pub(crate) conclusion: Option<String>,
    pub(crate) head_sha: String,
    #[serde(default)]
    pub(crate) updated_at: String,
    #[serde(default)]
    pub(crate) html_url: String,
}

impl WorkflowRun {
    pub(crate) fn summary(&self) -> String {
        if self.status == "completed" {
            format!(
                "{} (run #{} / attempt {})",
                self.conclusion.as_deref().unwrap_or("unknown"),
                self.id,
                self.run_attempt
            )
        } else {
            format!(
                "{} (run #{} / attempt {})",
                self.status, self.id, self.run_attempt
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuildLookup {
    Successful(WorkflowRun),
    Pending(WorkflowRun),
    Failed(WorkflowRun),
    Missing,
}

impl BuildLookup {
    pub(crate) fn description(&self) -> String {
        match self {
            Self::Successful(run) => format!("successful: {}", run.summary()),
            Self::Pending(run) => format!("pending: {}", run.summary()),
            Self::Failed(run) => format!("failed: {}", run.summary()),
            Self::Missing => "no workflow run found".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetryAdvice {
    pub(crate) delay: Option<Duration>,
}

#[derive(Debug)]
struct GitHubRequestFailure {
    message: String,
    retry: Option<RetryAdvice>,
}

impl fmt::Display for GitHubRequestFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GitHubRequestFailure {}

#[derive(Debug, Clone)]
pub(crate) struct GitHubClient {
    api_base: String,
    repository: String,
    token: Option<String>,
}

impl GitHubClient {
    pub(crate) fn new() -> Self {
        Self {
            api_base: GITHUB_API.to_string(),
            repository: REPOSITORY.to_string(),
            token: github_token(),
        }
    }

    pub(crate) fn resolve(&self, request: BuildRequest) -> Result<ResolvedBuild> {
        let sha = match &request {
            BuildRequest::Main => self.main_sha()?,
            BuildRequest::Pr(number) => self.pull_request_sha(*number)?,
            BuildRequest::Sha(sha) => {
                validate_full_sha(sha)?;
                sha.to_ascii_lowercase()
            }
        };
        validate_full_sha(&sha).context("GitHub returned an invalid commit SHA")?;
        Ok(ResolvedBuild { request, sha })
    }

    pub(crate) fn is_authenticated(&self) -> bool {
        self.token.is_some()
    }

    pub(crate) fn current_sha(&self, request: &BuildRequest) -> Result<String> {
        match request {
            BuildRequest::Main => self.main_sha(),
            BuildRequest::Pr(number) => self.pull_request_sha(*number),
            BuildRequest::Sha(sha) => Ok(sha.clone()),
        }
    }

    fn main_sha(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct CommitResponse {
            sha: String,
        }

        let response: CommitResponse = self.get_json(
            self.request(&format!(
                "{}/repos/{}/commits/main",
                self.api_base, self.repository
            )),
            "resolve current main HEAD",
        )?;
        Ok(response.sha)
    }

    fn pull_request_sha(&self, number: u64) -> Result<String> {
        #[derive(Deserialize)]
        struct PullResponse {
            head: PullHead,
        }
        #[derive(Deserialize)]
        struct PullHead {
            sha: String,
        }

        let response: PullResponse = self.get_json(
            self.request(&format!(
                "{}/repos/{}/pulls/{number}",
                self.api_base, self.repository
            )),
            &format!("resolve PR #{number} HEAD"),
        )?;
        Ok(response.head.sha)
    }

    pub(crate) fn lookup_build(&self, sha: &str) -> Result<BuildLookup> {
        #[derive(Deserialize)]
        struct RunsResponse {
            workflow_runs: Vec<WorkflowRun>,
        }

        validate_full_sha(sha)?;
        let request = self
            .request(&format!(
                "{}/repos/{}/actions/workflows/{WORKFLOW_FILE}/runs",
                self.api_base, self.repository
            ))
            .query("head_sha", sha)
            .query("per_page", "100");
        let response: RunsResponse =
            self.get_json(request, &format!("look up dev build for {sha}"))?;
        // GitHub exposes only the current attempt's artifacts for a rerun. The
        // workflow-runs response already represents that retrievable attempt;
        // historical attempt metadata must not be treated as installable.
        Ok(classify_runs(response.workflow_runs, sha))
    }

    pub(crate) fn download_artifacts(&self, run: &WorkflowRun, sha: &str) -> Result<Vec<Vec<u8>>> {
        #[derive(Deserialize)]
        struct ArtifactsResponse {
            artifacts: Vec<Artifact>,
        }
        #[derive(Deserialize)]
        struct Artifact {
            id: u64,
            name: String,
            expired: bool,
            archive_download_url: String,
            #[serde(default)]
            updated_at: String,
        }

        let expected_name = format!("statsai-dev-{sha}");
        let response: ArtifactsResponse = self.get_json(
            self.request(&format!(
                "{}/repos/{}/actions/runs/{}/artifacts",
                self.api_base, self.repository, run.id
            ))
            .query("per_page", "100"),
            &format!("list artifacts for workflow run #{}", run.id),
        )?;
        let mut artifacts: Vec<_> = response
            .artifacts
            .into_iter()
            .filter(|artifact| artifact.name == expected_name && !artifact.expired)
            .collect();
        if artifacts.is_empty() {
            bail!(
                "successful workflow run #{} has no unexpired artifact named {expected_name}",
                run.id
            );
        }
        artifacts.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.id.cmp(&left.id))
        });

        let mut downloads = Vec::with_capacity(artifacts.len());
        let mut errors = Vec::new();
        for artifact in artifacts {
            match self.download_with_retries(&artifact.archive_download_url, &expected_name) {
                Ok(bytes) => downloads.push(bytes),
                Err(error) => errors.push(format!("artifact #{}: {error:#}", artifact.id)),
            }
        }
        if downloads.is_empty() {
            bail!(
                "could not download any artifact named {expected_name} from workflow run #{}: {}",
                run.id,
                errors.join("; ")
            );
        }
        Ok(downloads)
    }

    /// The wait loop that precedes this retries the *build lookup*, not the
    /// download, so a transfer cut short by a slow link failed the whole install
    /// on the first try. A body cannot be resumed, so a retry re-requests it.
    fn download_with_retries(&self, url: &str, expected_name: &str) -> Result<Vec<u8>> {
        retrying_download(
            || self.download(url, expected_name),
            std::thread::sleep,
            |message| eprintln!("{message}"),
        )
    }

    fn download(&self, url: &str, expected_name: &str) -> Result<Vec<u8>> {
        let response = self.call(
            self.request_with_timeout(url, download_timeout()),
            &format!("download artifact {expected_name}"),
        )?;
        let mut reader = response.into_reader().take(MAX_ARTIFACT_BYTES + 1);
        let mut bytes = Vec::new();
        // A read that dies partway is as transient as one that never connected,
        // but it surfaces here rather than from `call`, so it has to be marked
        // retryable explicitly or the loop above would give up on it.
        if let Err(error) = reader.read_to_end(&mut bytes) {
            return Err(GitHubRequestFailure {
                message: format!("read downloaded artifact {expected_name}: {error}"),
                retry: Some(RetryAdvice { delay: None }),
            }
            .into());
        }
        if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
            bail!("artifact {expected_name} exceeds the 256 MiB safety limit");
        }
        Ok(bytes)
    }

    fn request(&self, url: &str) -> ureq::Request {
        self.request_with_timeout(url, Some(API_TIMEOUT))
    }

    fn request_with_timeout(&self, url: &str, timeout: Option<Duration>) -> ureq::Request {
        let request = ureq::get(url);
        let request = match timeout {
            Some(timeout) => request.timeout(timeout),
            None => request,
        };
        let request = request
            .set(
                "User-Agent",
                &format!("statsai-dev/{}", env!("CARGO_PKG_VERSION")),
            )
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = self.token.as_deref() {
            request.set("Authorization", &format!("Bearer {token}"))
        } else {
            request
        }
    }

    fn get_json<T: DeserializeOwned>(&self, request: ureq::Request, action: &str) -> Result<T> {
        self.call(request, action)?
            .into_json()
            .with_context(|| format!("parse GitHub response while attempting to {action}"))
    }

    fn call(&self, request: ureq::Request, action: &str) -> Result<ureq::Response> {
        match request.call() {
            Ok(response) => Ok(response),
            Err(ureq::Error::Status(code, response)) => {
                let retry_after = response.header("Retry-After").map(str::to_string);
                let rate_limit_remaining =
                    response.header("X-RateLimit-Remaining").map(str::to_string);
                let rate_limit_reset = response.header("X-RateLimit-Reset").map(str::to_string);
                let body = response.into_string().unwrap_or_default();
                let message = github_error_message(&body);
                let auth_hint = if self.token.is_none() && matches!(code, 401 | 403) {
                    "; set GH_TOKEN or authenticate with `gh auth login`"
                } else {
                    ""
                };
                let retry = http_retry_advice(
                    code,
                    retry_after.as_deref(),
                    rate_limit_remaining.as_deref(),
                    rate_limit_reset.as_deref(),
                    &message,
                    unix_timestamp(),
                );
                Err(GitHubRequestFailure {
                    message: format!(
                        "GitHub returned HTTP {code} while attempting to {action}: {message}{auth_hint}"
                    ),
                    retry,
                }
                .into())
            }
            Err(error) => Err(GitHubRequestFailure {
                message: format!("GitHub request failed while attempting to {action}: {error}"),
                retry: Some(RetryAdvice { delay: None }),
            }
            .into()),
        }
    }
}

pub(crate) fn retry_advice(error: &anyhow::Error) -> Option<RetryAdvice> {
    error
        .downcast_ref::<GitHubRequestFailure>()
        .and_then(|failure| failure.retry)
}

#[cfg(test)]
pub(crate) fn retryable_test_error(delay: Option<Duration>) -> anyhow::Error {
    GitHubRequestFailure {
        message: "transient GitHub test failure".to_string(),
        retry: Some(RetryAdvice { delay }),
    }
    .into()
}

fn classify_runs(runs: Vec<WorkflowRun>, sha: &str) -> BuildLookup {
    let mut current_attempts = BTreeMap::new();
    for run in runs
        .into_iter()
        .filter(|run| run.head_sha.eq_ignore_ascii_case(sha))
    {
        current_attempts
            .entry(run.id)
            .and_modify(|current: &mut WorkflowRun| {
                if compare_attempts(&run, current).is_gt() {
                    *current = run.clone();
                }
            })
            .or_insert(run);
    }
    let exact_runs: Vec<_> = current_attempts.into_values().collect();
    if let Some(run) =
        newest_run(exact_runs.iter().filter(|run| {
            run.status == "completed" && run.conclusion.as_deref() == Some("success")
        }))
    {
        return BuildLookup::Successful(run.clone());
    }
    if let Some(run) = newest_run(exact_runs.iter().filter(|run| run.status != "completed")) {
        return BuildLookup::Pending(run.clone());
    }
    newest_run(exact_runs.iter())
        .cloned()
        .map(BuildLookup::Failed)
        .unwrap_or(BuildLookup::Missing)
}

fn compare_attempts(left: &WorkflowRun, right: &WorkflowRun) -> Ordering {
    left.run_attempt
        .cmp(&right.run_attempt)
        .then_with(|| left.updated_at.cmp(&right.updated_at))
}

fn newest_run<'a>(runs: impl Iterator<Item = &'a WorkflowRun>) -> Option<&'a WorkflowRun> {
    runs.max_by(|left, right| compare_runs(left, right))
}

fn compare_runs(left: &WorkflowRun, right: &WorkflowRun) -> Ordering {
    left.updated_at
        .cmp(&right.updated_at)
        .then_with(|| left.run_attempt.cmp(&right.run_attempt))
        .then_with(|| left.id.cmp(&right.id))
}

fn retrying_download<D, S, W>(mut download: D, mut sleep: S, mut warn: W) -> Result<Vec<u8>>
where
    D: FnMut() -> Result<Vec<u8>>,
    S: FnMut(Duration),
    W: FnMut(String),
{
    let mut attempt = 1;
    loop {
        let error = match download() {
            Ok(bytes) => return Ok(bytes),
            Err(error) => error,
        };
        let Some(retry) = retry_advice(&error) else {
            return Err(error);
        };
        if attempt >= MAX_DOWNLOAD_ATTEMPTS {
            return Err(error.context(format!("download failed {MAX_DOWNLOAD_ATTEMPTS} times")));
        }
        let delay = retry.delay.unwrap_or(DOWNLOAD_RETRY_BASE_DELAY * attempt);
        warn(format!(
            "warning: {error:#}; retrying download ({attempt} of {}) in {} seconds",
            MAX_DOWNLOAD_ATTEMPTS - 1,
            delay.as_secs()
        ));
        sleep(delay);
        attempt += 1;
    }
}

fn download_timeout() -> Option<Duration> {
    let Ok(raw) = std::env::var(DOWNLOAD_TIMEOUT_ENV) else {
        return Some(DOWNLOAD_TIMEOUT);
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(seconds) => Some(Duration::from_secs(seconds)),
        Err(_) => Some(DOWNLOAD_TIMEOUT),
    }
}

fn github_token() -> Option<String> {
    for variable in ["STATSAI_DEV_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(value) = std::env::var(variable) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let token = String::from_utf8(output.stdout).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn github_error_message(body: &str) -> String {
    #[derive(Deserialize)]
    struct ErrorResponse {
        message: String,
    }
    serde_json::from_str::<ErrorResponse>(body)
        .map(|response| response.message)
        .unwrap_or_else(|_| body.trim().chars().take(200).collect())
}

fn http_retry_advice(
    status: u16,
    retry_after: Option<&str>,
    rate_limit_remaining: Option<&str>,
    rate_limit_reset: Option<&str>,
    message: &str,
    now: u64,
) -> Option<RetryAdvice> {
    let message_mentions_rate_limit = message.to_ascii_lowercase().contains("rate limit");
    let is_rate_limit = status == 429
        || (status == 403
            && (retry_after.is_some()
                || rate_limit_remaining == Some("0")
                || message_mentions_rate_limit));
    if is_rate_limit {
        let delay = retry_after
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
            .or_else(|| rate_limit_reset_delay(rate_limit_reset, now))
            .unwrap_or(Duration::from_secs(60));
        return Some(RetryAdvice { delay: Some(delay) });
    }

    if status == 408 || (500..=599).contains(&status) {
        let delay = retry_after
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        return Some(RetryAdvice { delay });
    }

    None
}

fn rate_limit_reset_delay(reset: Option<&str>, now: u64) -> Option<Duration> {
    let reset = reset?.parse::<u64>().ok()?;
    Some(Duration::from_secs(reset.saturating_sub(now).max(1)))
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn validate_full_sha(sha: &str) -> Result<()> {
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("commit SHA must contain exactly 40 hexadecimal characters");
    }
    Ok(())
}

pub(crate) fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

const fn default_attempt() -> u64 {
    1
}

#[cfg(test)]
mod tests;
