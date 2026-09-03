//! Exact-SHA development build launcher for StatsAI.

mod artifact;
mod cli;
mod data;
mod github;
mod launcher;
mod state;

use anyhow::{bail, Context, Result};
use artifact::{InstalledBuild, TARGET};
use chrono::{DateTime, Local, Utc};
use clap::Parser;
use cli::{Cli, Command, DataCommand, UseArgs};
use github::{short_sha, BuildLookup, BuildRequest, GitHubClient, ResolvedBuild, WorkflowRun};
use state::{BuildSource, Environment, Paths, SelectedBuild, State};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const BUILD_WAIT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MISSING_RUN_GRACE_PERIOD: Duration = Duration::from_secs(30);
const MAX_CONSECUTIVE_GITHUB_FAILURES: u32 = 6;
const WORKFLOW_LABEL: &str = "dev-build";

#[derive(Debug, Clone)]
struct PollBackoff {
    initial: Duration,
    current: Duration,
    maximum: Duration,
}

impl PollBackoff {
    fn new(authenticated: bool) -> Self {
        let (initial, maximum) = if authenticated {
            (Duration::from_secs(5), Duration::from_secs(30))
        } else {
            (Duration::from_secs(60), Duration::from_secs(120))
        };
        Self {
            initial,
            current: initial,
            maximum,
        }
    }

    fn reset(&mut self) {
        self.current = self.initial;
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = self.current.saturating_mul(2).min(self.maximum);
        delay
    }
}

/// Runs the `statsai-dev` command-line application.
///
/// # Errors
///
/// Returns an error when the requested operation cannot be completed safely.
pub fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let paths = Paths::discover()?;
    if cli.prod_data {
        bail!(launcher::PROD_DATA_REPLACED_BY_ENVIRONMENT);
    }

    match cli.command {
        Command::Use(arguments) => use_build(&paths, arguments),
        Command::Env(arguments) => {
            select_environment(&paths, arguments.environment)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Data(arguments) => {
            data_command(&paths, arguments.command)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Status => {
            print_status(&paths)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Clean => {
            clean_build_cache(&paths)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Statsai(arguments) => {
            let state = state::load(&paths)?;
            launcher::forward(&paths, &state, &arguments)
        }
    }
}

fn use_build(paths: &Paths, arguments: UseArgs) -> Result<ExitCode> {
    ensure_supported_host()?;
    let request = arguments.request()?;
    let github = GitHubClient::new();
    let resolved = github.resolve(request)?;
    println!(
        "Resolved {} @ {}.",
        resolved.request.label(),
        short_sha(&resolved.sha)
    );

    let selected_from_cache = {
        let _lock = paths.lock_state_exclusive()?;
        match artifact::load_cached(paths, &resolved.sha) {
            Ok(Some(installed)) => {
                select_installed_build_locked(paths, &resolved, &installed, arguments.environment)?;
                true
            }
            Ok(None) => false,
            Err(error) => {
                eprintln!(
                    "warning: cached build {} failed verification and will be downloaded again: {error:#}",
                    short_sha(&resolved.sha)
                );
                false
            }
        }
    };
    if selected_from_cache {
        report_installed(&github, &resolved);
        return Ok(ExitCode::SUCCESS);
    }

    let Some(run) = wait_for_build(&github, &resolved, arguments.no_wait)? else {
        return Ok(ExitCode::from(2));
    };
    println!(
        "Downloading statsai-dev-{} from workflow run #{} / attempt {}...",
        resolved.sha, run.id, run.run_attempt
    );
    let archives = github.download_artifacts(&run, &resolved.sha)?;
    let mut verification_errors = Vec::new();
    let mut verified = None;
    for archive in archives {
        match artifact::verify_download(&archive, &resolved.sha, &run) {
            Ok(candidate) => {
                verified = Some(candidate);
                break;
            }
            Err(error) => verification_errors.push(format!("{error:#}")),
        }
    }
    let verified = verified.with_context(|| {
        format!(
            "no downloaded artifact matched workflow run #{} / attempt {}: {}",
            run.id,
            run.run_attempt,
            verification_errors.join("; ")
        )
    })?;

    {
        let _lock = paths.lock_state_exclusive()?;
        let installed = artifact::install(paths, &verified)?;
        select_installed_build_locked(paths, &resolved, &installed, arguments.environment)?;
    }
    report_installed(&github, &resolved);
    Ok(ExitCode::SUCCESS)
}

fn wait_for_build(
    github: &GitHubClient,
    resolved: &ResolvedBuild,
    no_wait: bool,
) -> Result<Option<WorkflowRun>> {
    let started = Instant::now();
    wait_for_build_with(
        resolved,
        no_wait,
        github.is_authenticated(),
        BUILD_WAIT_TIMEOUT,
        || github.lookup_build(&resolved.sha),
        std::thread::sleep,
        || started.elapsed(),
        || source_advanced_note(github, resolved),
    )
}

#[allow(clippy::too_many_arguments)]
fn wait_for_build_with<L, S, E, A>(
    resolved: &ResolvedBuild,
    no_wait: bool,
    authenticated: bool,
    wait_timeout: Duration,
    mut lookup_build: L,
    mut sleep: S,
    mut elapsed: E,
    mut source_advanced: A,
) -> Result<Option<WorkflowRun>>
where
    L: FnMut() -> Result<BuildLookup>,
    S: FnMut(Duration),
    E: FnMut() -> Duration,
    A: FnMut() -> String,
{
    let mut last_description = String::new();
    let mut backoff = PollBackoff::new(authenticated);
    let mut consecutive_github_failures = 0;
    loop {
        if !no_wait {
            ensure_wait_remaining(elapsed(), wait_timeout, resolved, &last_description)?;
        }
        let lookup = match lookup_build() {
            Ok(lookup) => {
                consecutive_github_failures = 0;
                lookup
            }
            Err(error) if !no_wait => {
                let Some(retry) = github::retry_advice(&error) else {
                    return Err(error);
                };
                consecutive_github_failures += 1;
                if consecutive_github_failures >= MAX_CONSECUTIVE_GITHUB_FAILURES {
                    return Err(error.context(format!(
                        "GitHub failed {consecutive_github_failures} consecutive times while waiting for the exact build"
                    )));
                }
                let delay = retry
                    .delay
                    .unwrap_or_else(|| backoff.next_delay())
                    .max(Duration::from_secs(1));
                eprintln!(
                    "warning: {error:#}; retrying exact build lookup in {} second{}",
                    delay.as_secs(),
                    if delay == Duration::from_secs(1) {
                        ""
                    } else {
                        "s"
                    }
                );
                wait_before_next_lookup(
                    delay,
                    wait_timeout,
                    resolved,
                    &last_description,
                    &mut sleep,
                    &mut elapsed,
                )?;
                continue;
            }
            Err(error) => return Err(error),
        };
        let description = lookup.description();
        if description != last_description {
            eprintln!(
                "Build {} @ {}: {description}",
                resolved.request.label(),
                short_sha(&resolved.sha)
            );
            last_description = description;
            backoff.reset();
        }
        match lookup {
            BuildLookup::Successful(run) => return Ok(Some(run)),
            BuildLookup::Pending(_) if no_wait => return Ok(None),
            BuildLookup::Pending(_) => wait_before_next_lookup(
                backoff.next_delay(),
                wait_timeout,
                resolved,
                &last_description,
                &mut sleep,
                &mut elapsed,
            )?,
            BuildLookup::Failed(_) if no_wait => return Ok(None),
            BuildLookup::Failed(run) => {
                let advanced = source_advanced();
                let link = if run.html_url.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", run.html_url)
                };
                bail!(
                    "exact dev build {} @ {} did not succeed: {}{link}{advanced}",
                    resolved.request.label(),
                    short_sha(&resolved.sha),
                    run.summary()
                );
            }
            BuildLookup::Missing if no_wait => return Ok(None),
            BuildLookup::Missing if elapsed() < MISSING_RUN_GRACE_PERIOD => {
                let grace_remaining = MISSING_RUN_GRACE_PERIOD.saturating_sub(elapsed());
                let delay = backoff.next_delay().min(grace_remaining);
                wait_before_next_lookup(
                    delay,
                    wait_timeout,
                    resolved,
                    &last_description,
                    &mut sleep,
                    &mut elapsed,
                )?;
            }
            BuildLookup::Missing => bail!(
                "no {WORKFLOW_LABEL} workflow run exists for exact SHA {}; no other commit will be substituted",
                resolved.sha
            ),
        }
    }
}

fn wait_before_next_lookup<S, E>(
    requested: Duration,
    wait_timeout: Duration,
    resolved: &ResolvedBuild,
    last_description: &str,
    sleep: &mut S,
    elapsed: &mut E,
) -> Result<()>
where
    S: FnMut(Duration),
    E: FnMut() -> Duration,
{
    let elapsed_now = elapsed();
    ensure_wait_remaining(elapsed_now, wait_timeout, resolved, last_description)?;
    sleep(requested.min(wait_timeout.saturating_sub(elapsed_now)));
    Ok(())
}

fn ensure_wait_remaining(
    elapsed: Duration,
    wait_timeout: Duration,
    resolved: &ResolvedBuild,
    last_description: &str,
) -> Result<()> {
    if elapsed < wait_timeout {
        return Ok(());
    }
    let last_state = if last_description.is_empty() {
        "unavailable"
    } else {
        last_description
    };
    bail!(
        "timed out after {} minutes waiting for exact dev build {} @ {}; last state: {last_state}; no other commit will be substituted",
        wait_timeout.as_secs().div_ceil(60),
        resolved.request.label(),
        short_sha(&resolved.sha)
    )
}

fn select_installed_build_locked(
    paths: &Paths,
    resolved: &ResolvedBuild,
    installed: &InstalledBuild,
    environment: Option<Environment>,
) -> Result<()> {
    let mut state = state::load(paths)?;
    let previous_sha = state.build.as_ref().map(|build| build.sha.clone());
    let (source, pr) = match resolved.request {
        BuildRequest::Main => (BuildSource::Main, None),
        BuildRequest::Pr(number) => (BuildSource::Pr, Some(number)),
        BuildRequest::Sha(_) => (BuildSource::Sha, None),
    };
    state.build = Some(SelectedBuild {
        source,
        pr,
        sha: resolved.sha.clone(),
        workflow_run_id: installed.manifest.workflow_run_id,
        workflow_attempt: installed.manifest.workflow_attempt,
        target: installed.manifest.target.clone(),
        binary_sha256: installed.binary_sha256.clone(),
    });
    if let Some(environment) = environment {
        state.environment = environment;
    }
    state::save(paths, &state)?;

    if previous_sha.as_deref() == Some(resolved.sha.as_str()) {
        return Ok(());
    }
    let mut keep = vec![resolved.sha.as_str()];
    if let Some(previous_sha) = previous_sha.as_deref().filter(|sha| *sha != resolved.sha) {
        keep.push(previous_sha);
    }
    if let Err(error) = artifact::prune(paths, &keep) {
        eprintln!("warning: selected build, but could not prune old cache entries: {error:#}");
    }
    Ok(())
}

fn report_installed(github: &GitHubClient, resolved: &ResolvedBuild) {
    println!(
        "Installed {} @ {}.",
        resolved.request.label(),
        short_sha(&resolved.sha)
    );
    match &resolved.request {
        BuildRequest::Main | BuildRequest::Pr(_) => match github.current_sha(&resolved.request) {
            Ok(current) if !current.eq_ignore_ascii_case(&resolved.sha) => println!(
                "{} has since advanced to {}.",
                resolved.request.label(),
                short_sha(&current)
            ),
            Ok(_) => {}
            Err(error) => eprintln!(
                "warning: installed the exact resolved SHA, but could not recheck {} HEAD: {error:#}",
                resolved.request.label()
            ),
        },
        BuildRequest::Sha(_) => {}
    }
}

fn source_advanced_note(github: &GitHubClient, resolved: &ResolvedBuild) -> String {
    match github.current_sha(&resolved.request) {
        Ok(current) if !current.eq_ignore_ascii_case(&resolved.sha) => format!(
            "; {} has since advanced to {}",
            resolved.request.label(),
            short_sha(&current)
        ),
        _ => String::new(),
    }
}

fn select_environment(paths: &Paths, environment: Environment) -> Result<()> {
    let _lock = paths.lock_state_exclusive()?;
    let mut state = state::load(paths)?;
    state.environment = environment;
    state::save(paths, &state)?;
    println!("Environment set to {}.", environment.name());
    println!("API: {}", environment.api_url());
    println!("Web: {}", environment.web_url());
    Ok(())
}

fn data_command(paths: &Paths, command: DataCommand) -> Result<()> {
    match command {
        DataCommand::Status => {
            let state = state::load(paths)?;
            print_data_status(paths, &state)
        }
        DataCommand::Refresh => {
            let _data_lock = paths.lock_data_exclusive()?;
            let _state_lock = paths.lock_state_exclusive()?;
            let mut state = state::load(paths)?;
            let cloned = data::refresh(paths, &mut state)?;
            println!(
                "Refreshed {} from {} as an APFS clone (schema {}, {}).",
                paths.display(&paths.dev_store),
                paths.display(&paths.prod_store),
                cloned.schema_version,
                data::human_size(cloned.logical_size)
            );
            Ok(())
        }
        DataCommand::Clean => {
            let _data_lock = paths.lock_data_exclusive()?;
            let _state_lock = paths.lock_state_exclusive()?;
            let mut state = state::load(paths)?;
            let removed = data::clean(paths, &mut state)?;
            println!(
                "Removed development database {} ({} file{}).",
                paths.display(&paths.dev_store),
                removed,
                if removed == 1 { "" } else { "s" }
            );
            println!("Run `statsai-dev data refresh` before the next forwarded command.");
            Ok(())
        }
    }
}

fn clean_build_cache(paths: &Paths) -> Result<()> {
    let _lock = paths.lock_state_exclusive()?;
    let state = state::load(paths)?;
    let keep: Vec<_> = state
        .build
        .as_ref()
        .map(|build| vec![build.sha.as_str()])
        .unwrap_or_default();
    let removed = artifact::prune(paths, &keep)?;
    println!(
        "Removed {removed} obsolete cached build{}; development data was not touched.",
        if removed == 1 { "" } else { "s" }
    );
    Ok(())
}

fn print_status(paths: &Paths) -> Result<()> {
    let state = state::load(paths)?;
    println!("Build");
    if let Some(build) = &state.build {
        println!("  source:        {}", selected_source_label(build));
        println!("  commit:        {}", short_sha(&build.sha));
        println!(
            "  workflow:      #{} / attempt {}",
            build.workflow_run_id, build.workflow_attempt
        );
        println!("  target:        {}", build.target);
        match artifact::load_cached(paths, &build.sha) {
            Ok(Some(_)) => {}
            Ok(None) => println!("  cache:         MISSING"),
            Err(error) => println!("  cache:         CORRUPT ({error})"),
        }
    } else {
        println!("  source:        none selected");
    }

    println!();
    println!("Environment");
    println!("  profile:       {}", state.environment.name());
    println!("  API:           {}", state.environment.api_url());
    println!("  Web:           {}", state.environment.web_url());
    println!();
    print_data_status(paths, &state)?;

    if let Some(build) = &state.build {
        match build.source {
            BuildSource::Pr => print_source_update(
                "PR",
                build,
                BuildRequest::Pr(
                    build
                        .pr
                        .context("selected PR build is missing its PR number")?,
                ),
            ),
            BuildSource::Main => print_source_update("Main", build, BuildRequest::Main),
            BuildSource::Sha => {}
        }
    }
    Ok(())
}

fn print_source_update(section: &str, build: &SelectedBuild, request: BuildRequest) {
    println!();
    println!("{section}");
    println!("  installed:     {}", short_sha(&build.sha));
    match GitHubClient::new().current_sha(&request) {
        Ok(current) => {
            println!("  current HEAD:  {}", short_sha(&current));
            if !current.eq_ignore_ascii_case(&build.sha) {
                println!("  status:        UPDATE AVAILABLE");
            }
        }
        Err(error) => println!("  current HEAD:  unavailable ({error})"),
    }
}

fn print_data_status(paths: &Paths, state: &State) -> Result<()> {
    let status = data::inspect(paths, state)?;
    println!("Data");
    // The environment picks the store, so reporting the clone unconditionally would
    // describe the wrong database exactly when it matters most.
    if matches!(state.environment, Environment::Prod) {
        println!("  mode:          PRODUCTION database (prod environment)");
        println!("  store:         {}", paths.display(&paths.prod_store));
    } else {
        println!(
            "  mode:          isolated APFS dev clone ({} environment)",
            state.environment.name()
        );
        println!("  source:        {}", paths.display(&paths.prod_store));
        println!("  store:         {}", paths.display(&paths.dev_store));
    }
    println!(
        "  refreshed:     {}",
        status
            .refreshed_at
            .map(format_timestamp)
            .unwrap_or_else(|| "never".to_string())
    );
    println!(
        "  prod schema:   {}",
        schema_label(status.prod_schema, status.source_exists)
    );
    println!(
        "  dev schema:    {}",
        schema_label(status.dev_schema, status.dev_exists)
    );
    println!(
        "  logical size:  {}",
        status
            .logical_size
            .map(data::human_size)
            .unwrap_or_else(|| "—".to_string())
    );
    Ok(())
}

fn schema_label(schema: Option<i64>, exists: bool) -> String {
    if exists {
        schema
            .map(|version| version.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        "missing".to_string()
    }
}

fn selected_source_label(build: &SelectedBuild) -> String {
    match build.source {
        BuildSource::Main => "main".to_string(),
        BuildSource::Pr => build
            .pr
            .map(|number| format!("PR #{number}"))
            .unwrap_or_else(|| "PR (unknown)".to_string()),
        BuildSource::Sha => "exact SHA".to_string(),
    }
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn ensure_supported_host() -> Result<()> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Ok(());
    }
    bail!(
        "development artifacts currently target {TARGET}; this host is {}-{}",
        std::env::consts::ARCH,
        std::env::consts::OS
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use artifact::{BuildManifest, ManifestSource};

    fn installed_build(paths: &Paths, sha: &str) -> InstalledBuild {
        InstalledBuild {
            manifest: BuildManifest {
                schema: 2,
                repository: github::REPOSITORY.to_string(),
                sha: sha.to_string(),
                target: TARGET.to_string(),
                source: ManifestSource::Main,
                workflow_run_id: 104,
                workflow_attempt: 1,
                store_schema_version: 17,
                pricing_ruleset_version: 1,
            },
            binary_sha256: "b".repeat(64),
            binary_path: paths.builds_dir.join(sha).join("statsai"),
        }
    }

    #[test]
    fn selected_source_labels_preserve_pr_identity() {
        let build = SelectedBuild {
            source: BuildSource::Pr,
            pr: Some(12),
            sha: "a".repeat(40),
            workflow_run_id: 104,
            workflow_attempt: 1,
            target: TARGET.to_string(),
            binary_sha256: "b".repeat(64),
        };
        assert_eq!(selected_source_label(&build), "PR #12");
    }

    #[test]
    fn repeated_selection_preserves_the_previous_cached_build() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let previous_sha = "a".repeat(40);
        let current_sha = "c".repeat(40);
        std::fs::create_dir(paths.builds_dir.join(&previous_sha)).expect("create previous build");
        std::fs::create_dir(paths.builds_dir.join(&current_sha)).expect("create current build");
        let installed = installed_build(&paths, &current_sha);
        let resolved = ResolvedBuild {
            request: BuildRequest::Main,
            sha: current_sha.clone(),
        };
        let state = State {
            build: Some(SelectedBuild {
                source: BuildSource::Main,
                pr: None,
                sha: current_sha,
                workflow_run_id: installed.manifest.workflow_run_id,
                workflow_attempt: installed.manifest.workflow_attempt,
                target: TARGET.to_string(),
                binary_sha256: installed.binary_sha256.clone(),
            }),
            ..State::default()
        };
        state::save(&paths, &state).expect("save current selection");

        select_installed_build_locked(&paths, &resolved, &installed, None)
            .expect("repeat current selection");

        assert!(paths.builds_dir.join(previous_sha).is_dir());
    }

    #[test]
    fn anonymous_polling_stays_below_the_primary_hourly_limit() {
        let mut backoff = PollBackoff::new(false);
        let mut elapsed = Duration::ZERO;
        let mut polls = 1;
        while elapsed < Duration::from_secs(60 * 60) {
            elapsed += backoff.next_delay();
            polls += 1;
        }

        assert!(polls <= 60, "anonymous polling made {polls} requests/hour");
    }

    #[test]
    fn pending_build_wait_stops_at_the_deadline() {
        let sha = "a".repeat(40);
        let resolved = ResolvedBuild {
            request: BuildRequest::Pr(12),
            sha: sha.clone(),
        };
        let elapsed = std::cell::Cell::new(Duration::ZERO);
        let run = WorkflowRun {
            id: 104,
            run_attempt: 1,
            status: "queued".to_string(),
            conclusion: None,
            head_sha: sha,
            updated_at: String::new(),
            html_url: String::new(),
        };

        let result = wait_for_build_with(
            &resolved,
            false,
            true,
            Duration::from_secs(10),
            || Ok(BuildLookup::Pending(run.clone())),
            |delay| elapsed.set(elapsed.get() + delay),
            || elapsed.get(),
            String::new,
        );

        let error = result.expect_err("a permanently queued build must time out");
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn transient_github_failure_is_retried_while_waiting() {
        let sha = "a".repeat(40);
        let resolved = ResolvedBuild {
            request: BuildRequest::Pr(12),
            sha: sha.clone(),
        };
        let elapsed = std::cell::Cell::new(Duration::ZERO);
        let attempts = std::cell::Cell::new(0);
        let successful = WorkflowRun {
            id: 104,
            run_attempt: 1,
            status: "completed".to_string(),
            conclusion: Some("success".to_string()),
            head_sha: sha,
            updated_at: String::new(),
            html_url: String::new(),
        };

        let result = wait_for_build_with(
            &resolved,
            false,
            true,
            Duration::from_secs(60),
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(github::retryable_test_error(Some(Duration::from_secs(2))))
                } else {
                    Ok(BuildLookup::Successful(successful.clone()))
                }
            },
            |delay| elapsed.set(elapsed.get() + delay),
            || elapsed.get(),
            String::new,
        )
        .expect("transient failure should recover");

        assert!(matches!(result, Some(WorkflowRun { id: 104, .. })));
        assert_eq!(attempts.get(), 2);
        assert_eq!(elapsed.get(), Duration::from_secs(2));
    }
}
