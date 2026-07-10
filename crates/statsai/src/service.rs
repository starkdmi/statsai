use anyhow::{bail, Result};
use serde::Serialize;

#[cfg(target_os = "macos")]
use anyhow::Context;
#[cfg(target_os = "macos")]
use serde::Deserialize;
#[cfg(target_os = "macos")]
use statsai_core::home_dir;
#[cfg(target_os = "macos")]
use std::fs;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
const LAUNCH_AGENT_LABEL: &str = "dev.statsai.daemon";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Install,
    Uninstall,
    Status,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundServiceState {
    pub plist_installed: bool,
    pub launch_agent_loaded: bool,
    pub daemon_reachable: bool,
    pub daemon_version: Option<String>,
    pub stale: bool,
}

#[cfg(not(target_os = "macos"))]
pub fn background_service_state() -> Result<BackgroundServiceState> {
    Ok(BackgroundServiceState {
        plist_installed: false,
        launch_agent_loaded: false,
        daemon_reachable: false,
        daemon_version: None,
        stale: false,
    })
}

#[cfg(target_os = "macos")]
pub fn background_service_state() -> Result<BackgroundServiceState> {
    let plist_path = launch_agent_path()?;
    let plist_installed = plist_path.exists();
    let domain = gui_domain()?;
    let launch_agent_loaded = launch_agent_is_loaded(&domain);
    let daemon_reachable = daemon_reachable("127.0.0.1:8765");
    let daemon_version = daemon_health("127.0.0.1:8765").and_then(|health| health.version);
    let stale = daemon_is_stale(
        launch_agent_loaded,
        daemon_reachable,
        daemon_version.as_deref(),
    );
    Ok(BackgroundServiceState {
        plist_installed,
        launch_agent_loaded,
        daemon_reachable,
        daemon_version,
        stale,
    })
}

pub fn service(action: ServiceAction) -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = action;
        bail!("service commands are only supported on macOS");
    }

    #[cfg(target_os = "macos")]
    match action {
        ServiceAction::Install => install_launch_agent(),
        ServiceAction::Uninstall => uninstall_launch_agent(),
        ServiceAction::Status => status_launch_agent(),
    }
}

#[cfg(target_os = "macos")]
fn launch_agent_path() -> Result<PathBuf> {
    let home = home_dir().context("home directory is required for LaunchAgent install")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn daemon_log_dir() -> Result<PathBuf> {
    let home = home_dir().context("home directory is required for daemon logs")?;
    let dir = home.join(".statsai").join("logs");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

#[cfg(target_os = "macos")]
fn resolve_statsai_binary() -> Result<PathBuf> {
    let current = std::env::current_exe().context("resolve current statsai binary path")?;
    if current.file_name().and_then(|name| name.to_str()) == Some("statsai") {
        return Ok(current);
    }
    which_statsai().with_context(|| {
        format!(
            "could not find statsai binary (current executable: {})",
            current.display()
        )
    })
}

#[cfg(target_os = "macos")]
fn which_statsai() -> Result<PathBuf> {
    let output = Command::new("which")
        .arg("statsai")
        .output()
        .context("run which statsai")?;
    if !output.status.success() {
        bail!("statsai is not on PATH");
    }
    let path = String::from_utf8(output.stdout).context("decode which output")?;
    let path = path.trim();
    if path.is_empty() {
        bail!("statsai is not on PATH");
    }
    Ok(PathBuf::from(path))
}

#[cfg(target_os = "macos")]
fn launch_agent_plist(statsai_binary: &Path, stdout: &Path, stderr: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCH_AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>daemon</string>
        <string>--watch</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
</dict>
</plist>
"#,
        xml_escape(statsai_binary.display().to_string().as_str()),
        xml_escape(stdout.display().to_string().as_str()),
        xml_escape(stderr.display().to_string().as_str()),
    )
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "macos")]
fn gui_domain() -> Result<String> {
    let output = Command::new("id").arg("-u").output().context("run id -u")?;
    if !output.status.success() {
        bail!("failed to resolve GUI user id");
    }
    let uid = String::from_utf8(output.stdout).context("decode id output")?;
    let uid = uid.trim();
    if uid.is_empty() {
        bail!("failed to resolve GUI user id");
    }
    Ok(format!("gui/{uid}"))
}

#[cfg(target_os = "macos")]
fn launch_agent_target(domain: &str) -> String {
    format!("{domain}/{LAUNCH_AGENT_LABEL}")
}

#[cfg(target_os = "macos")]
fn daemon_is_stale(
    launch_agent_loaded: bool,
    daemon_reachable: bool,
    daemon_version: Option<&str>,
) -> bool {
    launch_agent_loaded && daemon_reachable && daemon_version != Some(env!("CARGO_PKG_VERSION"))
}

#[cfg(target_os = "macos")]
fn launch_agent_is_loaded(domain: &str) -> bool {
    Command::new("launchctl")
        .args(["print", launch_agent_target(domain).as_str()])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn bootout_launch_agent(domain: &str) -> Result<bool> {
    if !launch_agent_is_loaded(domain) {
        return Ok(false);
    }
    let target = launch_agent_target(domain);
    let output = Command::new("launchctl")
        .args(["bootout", target.as_str()])
        .output()
        .context("launchctl bootout")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!("launchctl bootout failed with status {}", output.status);
        }
        bail!("launchctl bootout failed: {stderr}");
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
fn install_launch_agent() -> Result<()> {
    let statsai_binary = resolve_statsai_binary()?;
    let log_dir = daemon_log_dir()?;
    let stdout = log_dir.join("daemon.stdout.log");
    let stderr = log_dir.join("daemon.stderr.log");
    let plist_path = launch_agent_path()?;
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let domain = gui_domain()?;
    bootout_launch_agent(&domain)?;

    let plist = launch_agent_plist(&statsai_binary, &stdout, &stderr);
    fs::write(&plist_path, plist).with_context(|| format!("write {}", plist_path.display()))?;

    let status = Command::new("launchctl")
        .args(["bootstrap", &domain, plist_path.to_string_lossy().as_ref()])
        .status()
        .context("launchctl bootstrap")?;
    if !status.success() {
        bail!("launchctl bootstrap failed with status {status}");
    }
    if !launch_agent_is_loaded(&domain) {
        bail!("launchctl bootstrap succeeded but {LAUNCH_AGENT_LABEL} is not loaded");
    }

    println!("installed LaunchAgent {LAUNCH_AGENT_LABEL}");
    println!("plist: {}", plist_path.display());
    println!("daemon: {}", statsai_binary.display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_launch_agent() -> Result<()> {
    let plist_path = launch_agent_path()?;
    let domain = gui_domain()?;
    bootout_launch_agent(&domain)?;
    if plist_path.exists() {
        fs::remove_file(&plist_path).with_context(|| format!("remove {}", plist_path.display()))?;
    }
    println!("removed LaunchAgent {LAUNCH_AGENT_LABEL}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn status_launch_agent() -> Result<()> {
    let background = background_service_state()?;

    if background.stale {
        println!("Auto-collect: stale daemon — run `statsai service install` to restart it");
    } else if background.launch_agent_loaded && background.daemon_reachable {
        println!("Auto-collect: on (watching Claude & Codex logs)");
    } else if background.launch_agent_loaded {
        println!("Auto-collect: installed but paused — check ~/.statsai/logs/");
    } else if background.daemon_reachable {
        println!("Auto-collect: off (collecting in this terminal session only)");
        println!("Turn on: statsai service install");
    } else if background.plist_installed {
        println!("Auto-collect: installed but not running");
        println!("Turn on: statsai service install");
    } else {
        println!("Auto-collect: off");
        println!("Turn on: statsai service install");
    }

    let plist_path = launch_agent_path()?;
    println!("LaunchAgent plist: {}", plist_path.display());
    if let Some(version) = background.daemon_version {
        println!("Daemon version: {version}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[derive(Debug, Deserialize)]
struct DaemonHealth {
    version: Option<String>,
}

#[cfg(target_os = "macos")]
fn daemon_health(api: &str) -> Option<DaemonHealth> {
    ureq::get(&format!("http://{api}/health"))
        .timeout(std::time::Duration::from_millis(400))
        .call()
        .ok()?
        .into_json()
        .ok()
}

#[cfg(target_os = "macos")]
fn daemon_reachable(api: &str) -> bool {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let Ok(addr) = api.parse::<SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn launch_agent_target_combines_domain_and_label() {
        assert_eq!(launch_agent_target("gui/501"), "gui/501/dev.statsai.daemon");
    }

    #[test]
    fn loaded_legacy_daemon_without_version_is_stale() {
        assert!(daemon_is_stale(true, true, None));
        assert!(!daemon_is_stale(
            true,
            true,
            Some(env!("CARGO_PKG_VERSION"))
        ));
        assert!(!daemon_is_stale(true, false, None));
    }
}
