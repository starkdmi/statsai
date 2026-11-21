use crate::artifact::{load_cached, InstalledBuild};
use crate::pricing::{build_pricing_target, refresh_disabled_by_env};
use crate::state::{Environment, Paths, SelectedBuild, State};
use anyhow::{bail, Context, Result};
use statsai_store::{database_pricing_state, database_schema_version, DatabasePricingState};
use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, ExitCode, ExitStatus};

/// `--prod-data` used to pick the store independently of the backend, which made
/// the two mismatched pairings reachable. Kept as a named error so the launcher and
/// the argument parser reject it identically.
pub(crate) const PROD_DATA_REPLACED_BY_ENVIRONMENT: &str =
    "`--prod-data` has been removed: the environment now selects the store. Run `statsai-dev env prod` to work against production data, or `statsai-dev env dev` for the isolated clone";

/// Returns the selected build together with the cached binary it names, refusing
/// anything the recorded selection no longer describes exactly.
pub(crate) fn selected_installed_build<'a>(
    paths: &Paths,
    state: &'a State,
) -> Result<(&'a SelectedBuild, InstalledBuild)> {
    let selected = state
        .build
        .as_ref()
        .context("no development build selected; run `statsai-dev use main` or `statsai-dev use pr <number>`")?;
    let installed = load_cached(paths, &selected.sha)?.with_context(|| {
        format!(
            "selected build {} is missing from the cache; select it again with `statsai-dev use {}`",
            selected.sha, selected.sha
        )
    })?;
    if installed.binary_sha256 != selected.binary_sha256 {
        bail!(
            "selected build checksum no longer matches state; run `statsai-dev use {}` again",
            selected.sha
        );
    }
    if installed.manifest.workflow_run_id != selected.workflow_run_id
        || installed.manifest.workflow_attempt != selected.workflow_attempt
    {
        bail!(
            "selected build workflow metadata no longer matches state; run `statsai-dev use {}` again",
            selected.sha
        );
    }
    Ok((selected, installed))
}

/// Runs the selected build's `store migrate` against `store`.
///
/// This is the only place a development build is allowed to change the schema of
/// a database it did not clone, so the caller decides whether it may -- see
/// [`crate::data::plan_prod_upgrade`].
pub(crate) fn migrate_store(
    installed: &InstalledBuild,
    store: &Path,
    environment: Environment,
) -> Result<()> {
    let mut command = Command::new(&installed.binary_path);
    command.arg("--store").arg(store).args(["store", "migrate"]);
    apply_environment(&mut command, environment);
    let status = command.status().with_context(|| {
        format!(
            "run selected StatsAI build {}",
            installed.binary_path.display()
        )
    })?;
    if !status.success() {
        bail!(
            "`statsai store migrate` failed ({}) against {}; a build that predates `statsai store migrate` cannot upgrade a database, so select a newer main build",
            describe_status(status),
            store.display()
        );
    }
    Ok(())
}

fn describe_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => "terminated by a signal".to_string(),
    }
}

pub(crate) fn forward(paths: &Paths, state: &State, arguments: &[OsString]) -> Result<ExitCode> {
    validate_forward_arguments(arguments)?;
    validate_forward_for_environment(arguments, state.environment)?;
    let (selected, installed) = selected_installed_build(paths, state)?;

    let _data_lock = paths.lock_data_shared()?;
    // The environment owns the store rather than a separate flag. Both stores carry
    // the same device id, so the server keys that device's `last_batch_id` to
    // whichever store synced last: pairing the dev backend with production data, or
    // production with the clone, leaves the local sync pointer unreachable and turns
    // the next sync into a full-history upload of the entire account.
    if state.inherited_legacy_prod {
        eprintln!(
            "NOTE: the stored `prod` selection predates the store following the environment, when it meant the production backend against the isolated clone. Using the dev environment instead; run `statsai-dev env prod` to confirm the new meaning, which also opens {}",
            paths.display(&paths.prod_store)
        );
    }
    let uses_production_data = matches!(state.environment, Environment::Prod);
    let store = if uses_production_data {
        &paths.prod_store
    } else {
        &paths.dev_store
    };
    if !store
        .try_exists()
        .with_context(|| format!("check store path {}", store.display()))?
    {
        if uses_production_data {
            bail!("production database does not exist at {}", store.display());
        }
        bail!(
            "isolated development database does not exist at {}; run `statsai-dev data refresh` first",
            paths.display(store)
        );
    }
    if uses_production_data {
        let production_schema = database_schema_version(store)?
            .context("production database disappeared while checking its schema")?;
        ensure_production_schema_compatible(
            production_schema,
            installed.manifest.store_schema_version,
        )?;
        let production_pricing = database_pricing_state(store)?
            .context("production database disappeared while checking its pricing")?;
        let applied_pricing = ensure_production_pricing_compatible(
            &production_pricing,
            installed.manifest.pricing_ruleset_version,
            installed.manifest.pricing_format,
            refresh_disabled_by_env(),
        )?;
        eprintln!(
            "WARNING: prod environment: running development build {} against the production database {} (schema {}, pricing version {})",
            &selected.sha[..8],
            paths.display(&paths.prod_store),
            production_schema,
            applied_pricing
        );
    }

    let mut command = Command::new(&installed.binary_path);
    command.arg("--store").arg(store).args(arguments);
    apply_environment(&mut command, state.environment);
    let status = command.status().with_context(|| {
        format!(
            "run selected StatsAI build {}",
            installed.binary_path.display()
        )
    })?;
    if let Some(code) = status.code() {
        return Ok(ExitCode::from(code.clamp(0, 255) as u8));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            bail!("selected StatsAI build terminated by signal {signal}");
        }
    }
    bail!("selected StatsAI build terminated without an exit code")
}

fn ensure_production_schema_compatible(production_schema: i64, build_schema: i64) -> Result<()> {
    if production_schema != build_schema {
        bail!(
            "refusing the prod environment: selected build supports store schema {build_schema}, but production uses schema {production_schema}; development builds may not migrate or open an incompatible production database. Run `statsai-dev env dev` to use the isolated clone instead"
        );
    }
    Ok(())
}

/// Allows the prod environment only when the selected build would price
/// production with exactly the version its estimates were priced with, so that it
/// neither reprices production nor prices new events with a different version.
/// Returns that version.
fn ensure_production_pricing_compatible(
    production: &DatabasePricingState,
    build_version: u64,
    build_format: Option<u32>,
    refresh_disabled_by_env: bool,
) -> Result<u64> {
    const USE_THE_CLONE: &str = "Run `statsai-dev env dev` to use the isolated clone instead";
    let target = build_pricing_target(
        build_version,
        build_format,
        production,
        refresh_disabled_by_env,
    )
    .map_err(|error| anyhow::anyhow!("refusing the prod environment: {error}. {USE_THE_CLONE}"))?;
    let Some(applied) = production.applied_version else {
        bail!(
            "refusing the prod environment: production database has no applied pricing version, and the selected build would price it with pricing version {target}; development builds may not reprice production data. {USE_THE_CLONE}"
        );
    };
    match target.cmp(&applied) {
        Ordering::Equal => Ok(applied),
        Ordering::Greater => bail!(
            "refusing the prod environment: the selected build would price production with pricing version {target}, but production's estimates were priced with pricing version {applied}, so the build would reprice production; development builds may not reprice production data. {USE_THE_CLONE}"
        ),
        Ordering::Less => bail!(
            "refusing the prod environment: the selected build would price new events with pricing version {target}, older than the pricing version {applied} production's estimates were priced with; {}. {USE_THE_CLONE}",
            older_target_remedy(build_format)
        ),
    }
}

/// What would let a build that is behind production's pricing run against it.
///
/// A build that reads pricing files keeps pricing with a stored copy at
/// production's applied version whether or not the refresh is on, so only a
/// newer build helps.
fn older_target_remedy(build_format: Option<u32>) -> &'static str {
    if build_format.is_none() {
        return "the selected build predates pricing files and prices only with its own, so select a newer build";
    }
    "select a build whose pricing is at least that version"
}

fn apply_environment(command: &mut Command, environment: Environment) {
    match environment {
        Environment::Local | Environment::Dev => {
            command.env("STATSAI_API_URL", environment.api_url());
            command.env("STATSAI_WEB_URL", environment.web_url());
        }
        Environment::Prod => {
            command.env_remove("STATSAI_API_URL");
            command.env_remove("STATSAI_WEB_URL");
            // A testing override: a file adopted from it would stay in
            // production's store for good, since versions never go down.
            command.env_remove(PRICING_URL_ENV);
        }
    }
}

/// The variable that points the pricing refresh at another URL, for testing.
const PRICING_URL_ENV: &str = "STATSAI_PRICING_URL";

fn validate_forward_arguments(arguments: &[OsString]) -> Result<()> {
    if arguments.is_empty() {
        bail!("missing StatsAI command");
    }
    if arguments.iter().any(|argument| {
        argument == OsStr::new("--store")
            || argument
                .to_str()
                .is_some_and(|value| value.starts_with("--store="))
    }) {
        bail!(
            "`--store` cannot be forwarded through statsai-dev; the environment selects the store, so use `statsai-dev env dev` for the isolated clone or `statsai-dev env prod` for production data"
        );
    }
    if arguments.iter().any(|argument| {
        argument == OsStr::new("--prod-data")
            || argument == OsStr::new("--prod")
            || argument
                .to_str()
                .is_some_and(|value| value.starts_with("--prod-data="))
    }) {
        bail!(PROD_DATA_REPLACED_BY_ENVIRONMENT);
    }

    let semantic_arguments = without_global_device_id(arguments)?;
    match semantic_arguments.first().and_then(|argument| argument.to_str()) {
        Some("daemon") => bail!(
            "development daemon commands are disabled because StatsAI v1 has a single production daemon endpoint"
        ),
        Some("service")
            if matches!(
                semantic_arguments
                    .get(1)
                    .and_then(|argument| argument.to_str()),
                Some("install" | "uninstall")
            ) =>
        {
            bail!(
                "development service install/uninstall is disabled to protect the production LaunchAgent"
            )
        }
        _ => Ok(()),
    }
}

/// Refuses commands that exist only to change a store's pricing state when the
/// store is production's: `statsai pricing refresh` fetches and stores a pricing
/// file and reprices, and `statsai pricing auto-refresh` changes production's
/// refresh setting. Both run under `env dev` and `env local`.
fn validate_forward_for_environment(
    arguments: &[OsString],
    environment: Environment,
) -> Result<()> {
    if !matches!(environment, Environment::Prod) {
        return Ok(());
    }
    let semantic_arguments = without_global_device_id(arguments)?;
    let mut words = semantic_arguments
        .iter()
        .map(|argument| argument.to_str())
        .filter(|argument| !argument.is_some_and(|value| value.starts_with('-')));
    if words.next() != Some(Some("pricing")) {
        return Ok(());
    }
    match words.next().flatten() {
        Some(subcommand @ ("refresh" | "auto-refresh")) => bail!(
            "`statsai pricing {subcommand}` is disabled in the prod environment because it changes production's pricing state; run it under `statsai-dev env dev`, or with the released `statsai`"
        ),
        _ => Ok(()),
    }
}

fn without_global_device_id(arguments: &[OsString]) -> Result<Vec<&OsStr>> {
    let mut semantic = Vec::with_capacity(arguments.len());
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_os_str();
        if argument == OsStr::new("--device-id") {
            if index + 1 >= arguments.len() {
                bail!("forwarded `--device-id` requires a value");
            }
            index += 2;
            continue;
        }
        if argument
            .to_str()
            .is_some_and(|value| value.starts_with("--device-id="))
        {
            index += 1;
            continue;
        }
        semantic.push(argument);
        index += 1;
    }
    Ok(semantic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::{self, BuildManifest, ManifestSource, VerifiedArtifact, TARGET};
    use crate::github::REPOSITORY;
    use crate::state::{BuildSource, SelectedBuild};
    use sha2::{Digest, Sha256};
    use statsai_store::{
        StoredPricingFileHeader, APPLIED_PRICING_RULESET_VERSION_KEY, CURRENT_SCHEMA_VERSION,
        STORED_PRICING_FILE_KEY,
    };

    /// Any pricing version: these builds are fixtures, never run.
    const BUILD_PRICING: u64 = 7;
    /// The pricing format of a build that reads pricing files.
    const READS_FORMAT_1: Option<u32> = Some(1);
    /// A build that predates pricing files.
    const LEGACY: Option<u32> = None;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    /// The variables `apply_environment` sets (`Some`) or removes (`None`).
    fn environment_changes(environment: Environment) -> Vec<(String, Option<String>)> {
        let mut command = Command::new("statsai");
        apply_environment(&mut command, environment);
        let mut changes = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<Vec<_>>();
        changes.sort();
        changes
    }

    #[test]
    fn the_prod_environment_removes_url_overrides_including_the_pricing_url() {
        assert_eq!(
            environment_changes(Environment::Prod),
            vec![
                ("STATSAI_API_URL".to_string(), None),
                ("STATSAI_PRICING_URL".to_string(), None),
                ("STATSAI_WEB_URL".to_string(), None),
            ]
        );
        for environment in [Environment::Local, Environment::Dev] {
            let changes = environment_changes(environment);
            assert!(
                changes.iter().all(|(key, _)| key != "STATSAI_PRICING_URL"),
                "{changes:?}"
            );
            assert!(
                changes
                    .iter()
                    .any(|(key, value)| key == "STATSAI_API_URL" && value.is_some()),
                "{changes:?}"
            );
        }
    }

    fn in_prod_environment(state: State) -> State {
        State {
            environment: Environment::Prod,
            ..state
        }
    }

    fn install_selected_build(
        paths: &Paths,
        store_schema_version: i64,
        pricing_ruleset_version: u64,
        pricing_format: Option<u32>,
    ) -> (State, artifact::InstalledBuild) {
        let sha = "a".repeat(40);
        let mut binary = vec![0u8; 64];
        binary[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        binary[4..8].copy_from_slice(&[0x0c, 0x00, 0x00, 0x01]);
        let binary_sha256 = hex::encode(Sha256::digest(&binary));
        let installed = artifact::install(
            paths,
            &VerifiedArtifact {
                manifest: BuildManifest {
                    schema: if pricing_format.is_some() { 3 } else { 2 },
                    repository: REPOSITORY.to_string(),
                    sha: sha.clone(),
                    target: TARGET.to_string(),
                    source: ManifestSource::PullRequest { number: 12 },
                    workflow_run_id: 104,
                    workflow_attempt: 1,
                    store_schema_version,
                    pricing_ruleset_version,
                    pricing_format,
                },
                binary,
                binary_sha256: binary_sha256.clone(),
            },
        )
        .expect("install fixture");
        let state = State {
            build: Some(SelectedBuild {
                source: BuildSource::Pr,
                pr: Some(12),
                sha,
                workflow_run_id: installed.manifest.workflow_run_id,
                workflow_attempt: installed.manifest.workflow_attempt,
                target: installed.manifest.target.clone(),
                binary_sha256,
            }),
            ..State::default()
        };
        (state, installed)
    }

    #[test]
    fn daemon_and_mutating_service_commands_are_blocked() {
        assert!(validate_forward_arguments(&arguments(&["daemon", "--watch"])).is_err());
        assert!(validate_forward_arguments(&arguments(&["service", "install"])).is_err());
        assert!(validate_forward_arguments(&arguments(&["service", "uninstall"])).is_err());
        assert!(validate_forward_arguments(&arguments(&["service", "status"])).is_ok());
        assert!(validate_forward_arguments(&arguments(&[
            "--device-id",
            "development",
            "daemon",
            "--watch"
        ]))
        .is_err());
        assert!(validate_forward_arguments(&arguments(&[
            "--device-id=development",
            "service",
            "install"
        ]))
        .is_err());
        assert!(validate_forward_arguments(&arguments(&[
            "service",
            "--device-id",
            "development",
            "uninstall"
        ]))
        .is_err());
    }

    #[test]
    fn pricing_commands_that_change_pricing_state_are_blocked_under_prod_only() {
        for forwarded in [
            &["pricing", "refresh"][..],
            &["pricing", "auto-refresh", "off"][..],
            &["pricing", "auto-refresh", "on"][..],
            &["--device-id", "development", "pricing", "refresh"][..],
            &["pricing", "--device-id=development", "auto-refresh", "on"][..],
        ] {
            let error = validate_forward_for_environment(&arguments(forwarded), Environment::Prod)
                .expect_err("prod must refuse a command that changes its pricing state");
            assert!(
                error.to_string().contains("prod environment"),
                "{forwarded:?}: {error}"
            );
            for environment in [Environment::Dev, Environment::Local] {
                assert!(
                    validate_forward_for_environment(&arguments(forwarded), environment).is_ok(),
                    "{forwarded:?} under {environment:?}"
                );
            }
        }
        for forwarded in [
            &["pricing", "status"][..],
            &["pricing", "status", "--json"][..],
            &["report", "monthly"][..],
            &["store", "supported-pricing-format"][..],
        ] {
            assert!(
                validate_forward_for_environment(&arguments(forwarded), Environment::Prod).is_ok(),
                "{forwarded:?}"
            );
        }
    }

    #[test]
    fn forwarded_store_override_is_blocked() {
        assert!(validate_forward_arguments(&arguments(&[
            "--store=/tmp/production.sqlite",
            "scan"
        ]))
        .is_err());
        assert!(validate_forward_arguments(&arguments(&["scan", "--store", "/tmp/db"])).is_err());
        assert!(validate_forward_arguments(&arguments(&["report", "monthly"])).is_ok());
    }

    #[test]
    fn prod_data_flag_is_rejected_and_points_at_the_environment() {
        for forwarded in [
            &["report", "monthly", "--prod-data"][..],
            &["scan", "--prod-data=true"][..],
            &["scan", "--prod"][..],
        ] {
            let error = validate_forward_arguments(&arguments(forwarded))
                .expect_err("--prod-data must not reach the inner binary");
            assert!(error.to_string().contains("statsai-dev env prod"));
        }
        assert!(validate_forward_arguments(&arguments(&["report", "monthly"])).is_ok());
    }

    #[test]
    fn the_environment_selects_the_store() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        let (mut state, _) = install_selected_build(
            &paths,
            CURRENT_SCHEMA_VERSION,
            BUILD_PRICING,
            READS_FORMAT_1,
        );

        // Neither store exists, so the store the launcher chose is named in the error
        // it fails with. Dev and local must never reach production data, and prod
        // must never fall back to the clone.
        for environment in [Environment::Dev, Environment::Local] {
            state.environment = environment;
            let error = forward(&paths, &state, &arguments(&["status"]))
                .expect_err("missing dev clone must fail");
            assert!(
                error.to_string().contains("isolated development database"),
                "{environment:?} must use the clone, got: {error:#}"
            );
        }

        state.environment = Environment::Prod;
        let error = forward(&paths, &state, &arguments(&["status"]))
            .expect_err("missing production store must fail");
        assert!(
            error
                .to_string()
                .contains("production database does not exist"),
            "prod must use production data, got: {error:#}"
        );
    }

    #[test]
    fn production_data_requires_an_exact_schema_match() {
        assert!(ensure_production_schema_compatible(17, 17).is_ok());
        assert!(ensure_production_schema_compatible(17, 18).is_err());
        assert!(ensure_production_schema_compatible(18, 17).is_err());
    }

    fn production(applied: Option<u64>, stored: Option<(u64, u64)>) -> DatabasePricingState {
        DatabasePricingState {
            applied_version: applied,
            stored_file: stored
                .map(|(format, version)| StoredPricingFileHeader { format, version }),
            stored_file_unreadable: false,
            refresh_setting_on: true,
        }
    }

    fn refusal(
        production: &DatabasePricingState,
        build_version: u64,
        build_format: Option<u32>,
        refresh_disabled_by_env: bool,
    ) -> String {
        ensure_production_pricing_compatible(
            production,
            build_version,
            build_format,
            refresh_disabled_by_env,
        )
        .expect_err("the prod environment must be refused")
        .to_string()
    }

    #[test]
    fn production_data_requires_the_build_to_price_with_the_applied_version() {
        for format in [READS_FORMAT_1, LEGACY] {
            assert_eq!(
                ensure_production_pricing_compatible(&production(Some(7), None), 7, format, false)
                    .expect("same pricing version"),
                7
            );
        }

        let missing = refusal(&production(None, None), 7, READS_FORMAT_1, false);
        assert!(missing.contains("no applied pricing version"), "{missing}");

        // A newer build would reprice every stored estimate.
        let newer = refusal(&production(Some(7), None), 8, READS_FORMAT_1, false);
        assert!(newer.contains("pricing version 8"), "{newer}");
        assert!(newer.contains("pricing version 7"), "{newer}");
        assert!(newer.contains("would reprice production"), "{newer}");

        // An older build would price new events differently from the stored ones.
        let older = refusal(&production(Some(8), None), 7, READS_FORMAT_1, false);
        assert!(older.contains("pricing version 7"), "{older}");
        assert!(older.contains("pricing version 8"), "{older}");
        assert!(older.contains("would price new events"), "{older}");
        assert!(older.contains("statsai-dev env dev"), "{older}");
    }

    #[test]
    fn a_stored_pricing_file_brings_a_build_up_to_production() {
        // Production adopted pricing version 9 from a refresh; the build embeds 7.
        let refreshed = production(Some(9), Some((1, 9)));
        assert_eq!(
            ensure_production_pricing_compatible(&refreshed, 7, READS_FORMAT_1, false)
                .expect("a build that reads the stored copy prices with version 9"),
            9
        );

        let legacy = refusal(&refreshed, 7, LEGACY, false);
        assert!(legacy.contains("pricing version 7"), "{legacy}");
        assert!(legacy.contains("pricing version 9"), "{legacy}");
        assert!(legacy.contains("predates pricing files"), "{legacy}");

        // With the refresh off, the build keeps pricing with the stored copy
        // production is priced with, as StatsAI does.
        assert_eq!(
            ensure_production_pricing_compatible(&refreshed, 7, READS_FORMAT_1, true)
                .expect("variable off"),
            9
        );
        assert_eq!(
            ensure_production_pricing_compatible(
                &DatabasePricingState {
                    refresh_setting_on: false,
                    ..refreshed
                },
                7,
                READS_FORMAT_1,
                false,
            )
            .expect("setting off"),
            9
        );

        // Production holds no copy of the version it is priced with: only a
        // newer build helps, whatever the refresh.
        for stored in [None, Some((1, 8))] {
            for refresh_disabled_by_env in [false, true] {
                let older = refusal(
                    &production(Some(9), stored),
                    7,
                    READS_FORMAT_1,
                    refresh_disabled_by_env,
                );
                assert!(older.contains("would price new events"), "{older}");
                assert!(
                    older.contains("select a build whose pricing is at least that version"),
                    "{older}"
                );
            }
        }
    }

    #[test]
    fn a_stored_pricing_file_newer_than_the_applied_version_is_refused() {
        // A refresh adopted version 10, and nothing has repriced production yet: the
        // build would be the one to do it.
        let adopted = production(Some(9), Some((1, 10)));
        for build_version in [7, 9, 10, 11] {
            let error = refusal(&adopted, build_version, READS_FORMAT_1, false);
            assert!(error.contains("would reprice production"), "{error}");
            assert!(error.contains("pricing version 9"), "{error}");
        }
        // With the refresh off the build ignores a stored copy production is not
        // priced with, so only its own version counts.
        assert_eq!(
            ensure_production_pricing_compatible(&adopted, 9, READS_FORMAT_1, true)
                .expect("the stored copy is ignored"),
            9
        );
        let older = refusal(&adopted, 7, READS_FORMAT_1, true);
        assert!(older.contains("would price new events"), "{older}");
    }

    #[test]
    fn a_stored_pricing_format_the_build_cannot_read_is_refused() {
        let unreadable = production(Some(9), Some((2, 9)));
        for refresh_disabled_by_env in [false, true] {
            let error = refusal(&unreadable, 9, READS_FORMAT_1, refresh_disabled_by_env);
            assert!(
                error.starts_with("refusing the prod environment"),
                "{error}"
            );
            assert!(error.contains("pricing file format 2"), "{error}");
            assert!(error.contains("it reads format 1"), "{error}");
            assert!(error.contains("statsai-dev env dev"), "{error}");
        }
        assert_eq!(
            ensure_production_pricing_compatible(&unreadable, 9, Some(2), false)
                .expect("a build that reads format 2"),
            9
        );
    }

    fn create_production(paths: &Paths, values: &[(&str, &str)]) {
        let production = statsai_store::Store::open(&paths.prod_store)
            .expect("create production store at current schema");
        for (key, value) in values {
            production
                .set_metadata_value(key, value)
                .expect("record production metadata");
        }
    }

    fn production_pricing(paths: &Paths) -> DatabasePricingState {
        database_pricing_state(&paths.prod_store)
            .expect("read production pricing")
            .expect("production exists")
    }

    #[test]
    fn production_data_refuses_a_build_that_would_migrate_the_store() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        create_production(&paths, &[]);

        let (state, _) = install_selected_build(
            &paths,
            CURRENT_SCHEMA_VERSION + 1,
            BUILD_PRICING,
            READS_FORMAT_1,
        );
        let state = in_prod_environment(state);

        let error = forward(&paths, &state, &arguments(&["status"]))
            .expect_err("future-schema build must not run against production");

        assert!(error.to_string().contains("may not migrate"));
        assert_eq!(
            database_schema_version(&paths.prod_store).expect("read unchanged production schema"),
            Some(CURRENT_SCHEMA_VERSION)
        );
    }

    #[test]
    fn production_data_refuses_a_missing_applied_pricing_version() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        create_production(&paths, &[]);
        let (state, _) = install_selected_build(
            &paths,
            CURRENT_SCHEMA_VERSION,
            BUILD_PRICING,
            READS_FORMAT_1,
        );
        let state = in_prod_environment(state);

        let error = forward(&paths, &state, &arguments(&["status"]))
            .expect_err("unpriced production store must not run against development builds");

        assert!(error.to_string().contains("no applied pricing version"));
        assert_eq!(production_pricing(&paths).applied_version, None);
    }

    #[test]
    fn production_data_refuses_older_and_newer_pricing_versions() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        create_production(&paths, &[(APPLIED_PRICING_RULESET_VERSION_KEY, "1")]);
        let (older_state, _) =
            install_selected_build(&paths, CURRENT_SCHEMA_VERSION, 2, READS_FORMAT_1);
        let older_state = in_prod_environment(older_state);
        let older = forward(&paths, &older_state, &arguments(&["status"]))
            .expect_err("older production pricing must refuse");
        assert!(older.to_string().contains("pricing version 2"), "{older}");
        assert!(older.to_string().contains("pricing version 1"), "{older}");

        create_production(&paths, &[(APPLIED_PRICING_RULESET_VERSION_KEY, "99")]);
        for format in [READS_FORMAT_1, LEGACY] {
            let (newer_state, _) =
                install_selected_build(&paths, CURRENT_SCHEMA_VERSION, BUILD_PRICING, format);
            let newer_state = in_prod_environment(newer_state);
            let newer = forward(&paths, &newer_state, &arguments(&["status"]))
                .expect_err("newer production pricing must refuse");
            assert!(newer.to_string().contains("pricing version 99"), "{newer}");
        }
        assert_eq!(production_pricing(&paths).applied_version, Some(99));
    }

    #[test]
    fn production_data_reads_the_stored_pricing_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        let (state, _) = install_selected_build(
            &paths,
            CURRENT_SCHEMA_VERSION,
            BUILD_PRICING,
            READS_FORMAT_1,
        );
        let state = in_prod_environment(state);

        create_production(
            &paths,
            &[
                (APPLIED_PRICING_RULESET_VERSION_KEY, "9"),
                (STORED_PRICING_FILE_KEY, r#"{"format": 2, "version": 9}"#),
            ],
        );
        let unknown_format = forward(&paths, &state, &arguments(&["status"]))
            .expect_err("a format the build cannot read must refuse");
        assert!(
            unknown_format.to_string().contains("pricing file format 2"),
            "{unknown_format}"
        );

        // Whether or not the refresh is on where this test runs, a build embedding 7
        // cannot price a store applied at 9 with a stored copy at 10 with version 9.
        create_production(
            &paths,
            &[(STORED_PRICING_FILE_KEY, r#"{"format": 1, "version": 10}"#)],
        );
        let newer_copy = forward(&paths, &state, &arguments(&["status"]))
            .expect_err("a stored copy newer than the applied version must refuse");
        assert!(
            newer_copy.to_string().contains("pricing version 9"),
            "{newer_copy}"
        );
        assert_eq!(
            production_pricing(&paths),
            production(Some(9), Some((1, 10)))
        );
    }
}
