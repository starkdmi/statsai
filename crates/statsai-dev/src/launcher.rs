use crate::artifact::load_cached;
use crate::state::{Environment, Paths, State};
use anyhow::{bail, Context, Result};
use statsai_store::{database_applied_pricing_ruleset_version, database_schema_version};
use std::ffi::{OsStr, OsString};
use std::process::{Command, ExitCode};

pub(crate) fn forward(
    paths: &Paths,
    state: &State,
    arguments: &[OsString],
    prod_data: bool,
) -> Result<ExitCode> {
    validate_forward_arguments(arguments)?;
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

    let _data_lock = paths.lock_data_shared()?;
    let store = if prod_data {
        &paths.prod_store
    } else {
        &paths.dev_store
    };
    if !store
        .try_exists()
        .with_context(|| format!("check store path {}", store.display()))?
    {
        if prod_data {
            bail!("production database does not exist at {}", store.display());
        }
        bail!(
            "isolated development database does not exist at {}; run `statsai-dev data refresh` first",
            paths.display(store)
        );
    }
    if prod_data {
        let production_schema = database_schema_version(store)?
            .context("production database disappeared while checking its schema")?;
        ensure_production_schema_compatible(
            production_schema,
            installed.manifest.store_schema_version,
        )?;
        let production_pricing = database_applied_pricing_ruleset_version(store)?;
        ensure_production_pricing_compatible(
            production_pricing,
            installed.manifest.pricing_ruleset_version,
        )?;
        eprintln!(
            "WARNING: running development build {} against the production database {} (schema {}, pricing ruleset {})",
            &selected.sha[..8],
            paths.display(&paths.prod_store),
            production_schema,
            installed.manifest.pricing_ruleset_version
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
            "refusing `--prod-data`: selected build supports store schema {build_schema}, but production uses schema {production_schema}; development builds may not migrate or open an incompatible production database"
        );
    }
    Ok(())
}

fn ensure_production_pricing_compatible(
    production_ruleset: Option<u64>,
    build_ruleset: u64,
) -> Result<()> {
    match production_ruleset {
        Some(applied) if applied == build_ruleset => Ok(()),
        Some(applied) => bail!(
            "refusing `--prod-data`: selected build supports pricing ruleset {build_ruleset}, but production uses pricing ruleset {applied}; development builds may not reprice an incompatible production database"
        ),
        None => bail!(
            "refusing `--prod-data`: production database has no applied pricing ruleset, but the selected build requires pricing ruleset {build_ruleset}; development builds may not reprice an incompatible production database"
        ),
    }
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
        }
    }
}

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
            "`--store` cannot be forwarded through statsai-dev; use the isolated dev database or the one-shot `--prod-data` flag"
        );
    }
    if arguments.iter().any(|argument| {
        argument == OsStr::new("--prod-data")
            || argument == OsStr::new("--prod")
            || argument
                .to_str()
                .is_some_and(|value| value.starts_with("--prod-data="))
    }) {
        bail!(
            "`--prod-data` must precede the forwarded command: `statsai-dev --prod-data <command>`"
        );
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
        APPLIED_PRICING_RULESET_VERSION_KEY, CURRENT_SCHEMA_VERSION, PRICING_RULESET_VERSION,
    };

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn install_selected_build(
        paths: &Paths,
        store_schema_version: i64,
        pricing_ruleset_version: u64,
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
                    schema: 2,
                    repository: REPOSITORY.to_string(),
                    sha: sha.clone(),
                    target: TARGET.to_string(),
                    source: ManifestSource::PullRequest { number: 12 },
                    workflow_run_id: 104,
                    workflow_attempt: 1,
                    store_schema_version,
                    pricing_ruleset_version,
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
    fn misplaced_prod_data_flag_is_rejected() {
        let error = validate_forward_arguments(&arguments(&["report", "monthly", "--prod-data"]))
            .expect_err("trailing --prod-data must not reach the inner binary");
        assert!(error.to_string().contains("must precede"));
        let equals = validate_forward_arguments(&arguments(&["scan", "--prod-data=true"]))
            .expect_err("trailing --prod-data= must not reach the inner binary");
        assert!(equals.to_string().contains("must precede"));
        let near_miss = validate_forward_arguments(&arguments(&["scan", "--prod"]))
            .expect_err("trailing --prod must not reach the inner binary");
        assert!(near_miss.to_string().contains("must precede"));
        assert!(validate_forward_arguments(&arguments(&["report", "monthly"])).is_ok());
    }

    #[test]
    fn production_data_requires_an_exact_schema_match() {
        assert!(ensure_production_schema_compatible(17, 17).is_ok());
        assert!(ensure_production_schema_compatible(17, 18).is_err());
        assert!(ensure_production_schema_compatible(18, 17).is_err());
    }

    #[test]
    fn production_data_requires_an_exact_pricing_match() {
        assert!(ensure_production_pricing_compatible(Some(1), 1).is_ok());
        assert!(ensure_production_pricing_compatible(None, 1).is_err());
        assert!(ensure_production_pricing_compatible(Some(1), 2).is_err());
        assert!(ensure_production_pricing_compatible(Some(2), 1).is_err());
    }

    #[test]
    fn production_data_refuses_a_build_that_would_migrate_the_store() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        let production = statsai_store::Store::open(&paths.prod_store)
            .expect("create production store at current schema");
        drop(production);

        let (state, _) = install_selected_build(&paths, CURRENT_SCHEMA_VERSION + 1, 1);

        let error = forward(&paths, &state, &arguments(&["status"]), true)
            .expect_err("future-schema build must not run against production");

        assert!(error.to_string().contains("may not migrate"));
        assert_eq!(
            database_schema_version(&paths.prod_store).expect("read unchanged production schema"),
            Some(CURRENT_SCHEMA_VERSION)
        );
    }

    #[test]
    fn production_data_refuses_a_missing_applied_pricing_ruleset() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        drop(
            statsai_store::Store::open(&paths.prod_store)
                .expect("create production store at current schema"),
        );
        let (state, _) =
            install_selected_build(&paths, CURRENT_SCHEMA_VERSION, PRICING_RULESET_VERSION);

        let error = forward(&paths, &state, &arguments(&["status"]), true)
            .expect_err("unpriced production store must not run against development builds");

        assert!(error.to_string().contains("no applied pricing ruleset"));
        assert_eq!(
            database_applied_pricing_ruleset_version(&paths.prod_store)
                .expect("read unchanged production pricing"),
            None
        );
    }

    #[test]
    fn production_data_refuses_older_and_newer_pricing_rulesets() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        let production = statsai_store::Store::open(&paths.prod_store)
            .expect("create production store at current schema");
        production
            .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "1")
            .expect("record older applied ruleset");
        drop(production);
        let (older_state, _) = install_selected_build(&paths, CURRENT_SCHEMA_VERSION, 2);
        let older = forward(&paths, &older_state, &arguments(&["status"]), true)
            .expect_err("older production ruleset must refuse");
        assert!(older.to_string().contains("pricing ruleset 2"));
        assert!(older.to_string().contains("pricing ruleset 1"));

        let production =
            statsai_store::Store::open(&paths.prod_store).expect("reopen production store");
        production
            .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "99")
            .expect("record newer applied ruleset");
        drop(production);
        let (newer_state, _) =
            install_selected_build(&paths, CURRENT_SCHEMA_VERSION, PRICING_RULESET_VERSION);
        let newer = forward(&paths, &newer_state, &arguments(&["status"]), true)
            .expect_err("newer production ruleset must refuse");
        assert!(newer.to_string().contains("pricing ruleset 99"));
        assert_eq!(
            database_applied_pricing_ruleset_version(&paths.prod_store)
                .expect("read unchanged production pricing"),
            Some(99)
        );
    }
}
