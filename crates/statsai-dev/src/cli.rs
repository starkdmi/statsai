use crate::github::{validate_full_sha, BuildRequest};
use crate::state::Environment;
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::ffi::OsString;

#[derive(Debug, Parser)]
#[command(
    name = "statsai-dev",
    version,
    about = "Run exact StatsAI development builds against isolated local data",
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    #[arg(
        long,
        global = true,
        help = "Use the production StatsAI database for this forwarded command only"
    )]
    pub(crate) prod_data: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    #[command(about = "Select an exact main, PR, or full-SHA development build")]
    Use(UseArgs),
    #[command(about = "Select the local, dev, or prod backend profile")]
    Env(EnvArgs),
    #[command(about = "Manage the reusable isolated development database")]
    Data(DataArgs),
    #[command(about = "Show selected build, environment, data, and update status")]
    Status,
    #[command(about = "Remove obsolete cached binaries without touching development data")]
    Clean,
    #[command(external_subcommand)]
    Statsai(Vec<OsString>),
}

#[derive(Debug, Args)]
pub(crate) struct UseArgs {
    #[arg(
        value_name = "main|pr|FULL_SHA",
        help = "Build source: main, pr <number>, or a full 40-character SHA"
    )]
    selector: String,
    #[arg(value_name = "PR_NUMBER", required_if_eq("selector", "pr"))]
    value: Option<String>,
    #[arg(long, help = "Return immediately when the exact build is not ready")]
    pub(crate) no_wait: bool,
    #[arg(
        long = "env",
        value_enum,
        help = "Also select a backend environment profile"
    )]
    pub(crate) environment: Option<Environment>,
}

impl UseArgs {
    pub(crate) fn request(&self) -> Result<BuildRequest> {
        match self.selector.as_str() {
            "main" => {
                if self.value.is_some() {
                    bail!("`statsai-dev use main` does not accept another value");
                }
                Ok(BuildRequest::Main)
            }
            "pr" => {
                let value = self
                    .value
                    .as_deref()
                    .context("`statsai-dev use pr` requires a PR number")?;
                let number = value
                    .parse::<u64>()
                    .with_context(|| format!("invalid PR number {value}"))?;
                if number == 0 {
                    bail!("PR number must be greater than zero");
                }
                Ok(BuildRequest::Pr(number))
            }
            sha => {
                if self.value.is_some() {
                    bail!("a full commit SHA does not accept another value");
                }
                validate_full_sha(sha)?;
                Ok(BuildRequest::Sha(sha.to_ascii_lowercase()))
            }
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct EnvArgs {
    #[arg(value_enum)]
    pub(crate) environment: Environment,
}

#[derive(Debug, Args)]
pub(crate) struct DataArgs {
    #[command(subcommand)]
    pub(crate) command: DataCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum DataCommand {
    #[command(about = "Show production and development database status")]
    Status,
    #[command(about = "Replace development data with a consistent APFS clone of production")]
    Refresh,
    #[command(about = "Delete the reusable development database")]
    Clean,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_syntax_parses_main_pr_and_exact_sha() {
        let main = Cli::try_parse_from(["statsai-dev", "use", "main"]).expect("parse main");
        let Command::Use(main) = main.command else {
            panic!("expected use command");
        };
        assert_eq!(main.request().expect("main request"), BuildRequest::Main);

        let pr = Cli::try_parse_from(["statsai-dev", "use", "pr", "12", "--env", "dev"])
            .expect("parse PR");
        let Command::Use(pr) = pr.command else {
            panic!("expected use command");
        };
        assert_eq!(pr.request().expect("PR request"), BuildRequest::Pr(12));
        assert_eq!(pr.environment, Some(Environment::Dev));

        let sha = "a".repeat(40);
        let parsed = Cli::try_parse_from(["statsai-dev", "use", &sha]).expect("parse SHA");
        let Command::Use(parsed) = parsed.command else {
            panic!("expected use command");
        };
        assert_eq!(
            parsed.request().expect("SHA request"),
            BuildRequest::Sha(sha)
        );
    }

    #[test]
    fn unknown_commands_are_forwarded_without_run_keyword() {
        let parsed = Cli::try_parse_from(["statsai-dev", "report", "monthly"])
            .expect("parse forwarded command");
        assert!(matches!(
            parsed.command,
            Command::Statsai(arguments)
                if arguments == [OsString::from("report"), OsString::from("monthly")]
        ));
    }
}
