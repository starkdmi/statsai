use crate::artifact::load_cached;
use crate::state::{Environment, Paths, State};
use anyhow::{bail, Context, Result};
use statsai_store::database_schema_version;
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
        eprintln!(
            "WARNING: running development build {} against the production database {} (schema {})",
            &selected.sha[..8],
            paths.display(&paths.prod_store),
            production_schema
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

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
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
    fn production_data_requires_an_exact_schema_match() {
        assert!(ensure_production_schema_compatible(17, 17).is_ok());
        assert!(ensure_production_schema_compatible(17, 18).is_err());
        assert!(ensure_production_schema_compatible(18, 17).is_err());
    }

    #[test]
    fn production_data_refuses_a_build_that_would_migrate_the_store() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        let production = statsai_store::Store::open(&paths.prod_store)
            .expect("create production store at current schema");
        drop(production);

        let sha = "a".repeat(40);
        let mut binary = vec![0u8; 64];
        binary[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        binary[4..8].copy_from_slice(&[0x0c, 0x00, 0x00, 0x01]);
        let binary_sha256 = hex::encode(Sha256::digest(&binary));
        let installed = artifact::install(
            &paths,
            &VerifiedArtifact {
                manifest: BuildManifest {
                    schema: 1,
                    repository: REPOSITORY.to_string(),
                    sha: sha.clone(),
                    target: TARGET.to_string(),
                    source: ManifestSource::PullRequest { number: 12 },
                    workflow_run_id: 104,
                    workflow_attempt: 1,
                    store_schema_version: statsai_store::CURRENT_SCHEMA_VERSION + 1,
                },
                binary,
                binary_sha256: binary_sha256.clone(),
            },
        )
        .expect("install future-schema fixture");
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

        let error = forward(&paths, &state, &arguments(&["status"]), true)
            .expect_err("future-schema build must not run against production");

        assert!(error.to_string().contains("may not migrate"));
        assert_eq!(
            database_schema_version(&paths.prod_store).expect("read unchanged production schema"),
            Some(statsai_store::CURRENT_SCHEMA_VERSION)
        );
    }
}
