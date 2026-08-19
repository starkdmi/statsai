use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use statsai_core::home_dir;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

const STATE_SCHEMA: u32 = 1;

#[derive(Debug, Clone)]
pub(crate) struct Paths {
    pub(crate) home: PathBuf,
    pub(crate) state_dir: PathBuf,
    pub(crate) cache_dir: PathBuf,
    pub(crate) state_file: PathBuf,
    pub(crate) builds_dir: PathBuf,
    pub(crate) data_dir: PathBuf,
    pub(crate) dev_store: PathBuf,
    pub(crate) prod_store: PathBuf,
    state_lock: PathBuf,
    data_lock: PathBuf,
}

impl Paths {
    pub(crate) fn discover() -> Result<Self> {
        let home = home_dir().context("cannot determine home directory")?;
        let state_dir = explicit_dir("STATSAI_DEV_STATE_DIR").unwrap_or_else(|| {
            xdg_dir("XDG_STATE_HOME")
                .unwrap_or_else(|| home.join(".local/state"))
                .join("statsai-dev")
        });
        let cache_dir = explicit_dir("STATSAI_DEV_CACHE_DIR").unwrap_or_else(|| {
            xdg_dir("XDG_CACHE_HOME")
                .unwrap_or_else(|| home.join(".cache"))
                .join("statsai-dev")
        });
        let prod_store = std::env::var_os("STATSAI_DEV_PROD_STORE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".statsai/statsai.sqlite"));
        Ok(Self::new(home, state_dir, cache_dir, prod_store))
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &Path) -> Self {
        Self::new(
            root.join("home"),
            root.join("state"),
            root.join("cache"),
            root.join("production.sqlite"),
        )
    }

    fn new(home: PathBuf, state_dir: PathBuf, cache_dir: PathBuf, prod_store: PathBuf) -> Self {
        let data_dir = cache_dir.join("data");
        Self {
            home,
            state_file: state_dir.join("state.json"),
            state_lock: state_dir.join("state.lock"),
            data_lock: state_dir.join("data.lock"),
            builds_dir: cache_dir.join("builds"),
            dev_store: data_dir.join("statsai.sqlite"),
            data_dir,
            state_dir,
            cache_dir,
            prod_store,
        }
    }

    pub(crate) fn ensure_state_dir(&self) -> Result<()> {
        create_private_dir(&self.state_dir)
    }

    pub(crate) fn ensure_cache_dirs(&self) -> Result<()> {
        create_private_dir(&self.cache_dir)?;
        create_private_dir(&self.builds_dir)?;
        create_private_dir(&self.data_dir)
    }

    pub(crate) fn lock_state_exclusive(&self) -> Result<AppLock> {
        AppLock::acquire(&self.state_lock, LockMode::Exclusive)
    }

    pub(crate) fn lock_data_exclusive(&self) -> Result<AppLock> {
        AppLock::acquire(&self.data_lock, LockMode::Exclusive)
    }

    pub(crate) fn lock_data_shared(&self) -> Result<AppLock> {
        AppLock::acquire(&self.data_lock, LockMode::Shared)
    }

    pub(crate) fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.home)
            .map(|relative| format!("~/{}", relative.display()))
            .unwrap_or_else(|_| path.display().to_string())
    }
}

fn explicit_dir(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn xdg_dir(variable: &str) -> Option<PathBuf> {
    explicit_dir(variable).filter(|path| path.is_absolute())
}

#[derive(Debug, Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

pub(crate) struct AppLock {
    file: File,
}

impl AppLock {
    fn acquire(path: &Path, mode: LockMode) -> Result<Self> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        create_private_dir(parent)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(path)
            .with_context(|| format!("open statsai-dev lock {}", path.display()))?;
        match mode {
            LockMode::Shared => file.lock_shared(),
            LockMode::Exclusive => file.lock(),
        }
        .with_context(|| format!("lock statsai-dev state {}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for AppLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct State {
    #[serde(default = "state_schema")]
    pub(crate) schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) build: Option<SelectedBuild>,
    #[serde(default)]
    pub(crate) environment: Environment,
    #[serde(default)]
    pub(crate) data: DataState,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA,
            build: None,
            environment: Environment::Dev,
            data: DataState::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SelectedBuild {
    pub(crate) source: BuildSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pr: Option<u64>,
    pub(crate) sha: String,
    pub(crate) workflow_run_id: u64,
    pub(crate) workflow_attempt: u64,
    pub(crate) target: String,
    pub(crate) binary_sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BuildSource {
    Main,
    Pr,
    Sha,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DataState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) refreshed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Environment {
    Local,
    #[default]
    Dev,
    Prod,
}

impl Environment {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Dev => "dev",
            Self::Prod => "prod",
        }
    }

    pub(crate) fn api_url(self) -> &'static str {
        match self {
            Self::Local => "http://127.0.0.1:8787",
            Self::Dev => "https://dev-api.statsai.dev",
            Self::Prod => "https://api.statsai.dev",
        }
    }

    pub(crate) fn web_url(self) -> &'static str {
        match self {
            Self::Local => "http://127.0.0.1:3000",
            Self::Dev => "https://dev.statsai.dev",
            Self::Prod => "https://statsai.dev",
        }
    }
}

const fn state_schema() -> u32 {
    STATE_SCHEMA
}

pub(crate) fn load(paths: &Paths) -> Result<State> {
    if !paths
        .state_file
        .try_exists()
        .with_context(|| format!("inspect state file {}", paths.state_file.display()))?
    {
        return Ok(State::default());
    }
    let file = File::open(&paths.state_file)
        .with_context(|| format!("open state file {}", paths.state_file.display()))?;
    let state: State = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parse state file {}", paths.state_file.display()))?;
    if state.schema > STATE_SCHEMA {
        bail!(
            "statsai-dev state schema {} is newer than this binary supports ({STATE_SCHEMA})",
            state.schema
        );
    }
    Ok(state)
}

pub(crate) fn save(paths: &Paths, state: &State) -> Result<()> {
    paths.ensure_state_dir()?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".state-")
        .tempfile_in(&paths.state_dir)
        .with_context(|| {
            format!(
                "create temporary state file in {}",
                paths.state_dir.display()
            )
        })?;
    restrict_file_permissions(temporary.path())?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), state)
        .context("serialize statsai-dev state")?;
    temporary.as_file_mut().write_all(b"\n")?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&paths.state_file)
        .map_err(|error| error.error)
        .with_context(|| format!("replace state file {}", paths.state_file.display()))?;
    restrict_file_permissions(&paths.state_file)?;
    File::open(&paths.state_dir)?.sync_all()?;
    Ok(())
}

pub(crate) fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("create private directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict directory permissions for {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn restrict_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict file permissions for {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_atomically() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        let expected = State {
            environment: Environment::Local,
            build: Some(SelectedBuild {
                source: BuildSource::Pr,
                pr: Some(12),
                sha: "a".repeat(40),
                workflow_run_id: 104,
                workflow_attempt: 2,
                target: "aarch64-apple-darwin".to_string(),
                binary_sha256: "b".repeat(64),
            }),
            data: DataState {
                refreshed_at: Some(Utc::now()),
            },
            ..State::default()
        };

        save(&paths, &expected).expect("save state");

        assert_eq!(load(&paths).expect("load state"), expected);
    }

    #[test]
    fn future_state_schema_is_rejected() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_state_dir().expect("create state directory");
        fs::write(&paths.state_file, r#"{"schema":2}"#).expect("write future state");

        let error = load(&paths).expect_err("future state must fail");

        assert!(error.to_string().contains("state schema 2 is newer"));
    }
}
