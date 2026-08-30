use statsai::snapshot::AppSnapshot;
use statsai::{default_store_path, snapshot};
use statsai_store::Store;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

fn statsai_command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    for key in ["STATSAI_API_URL", "STATSAI_WEB_URL", "STATSAI_SYNC_TOKEN"] {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    command
}

pub(super) fn fetch_snapshot() -> Result<AppSnapshot, String> {
    let store = Store::open(&default_store_path()).map_err(|err| err.to_string())?;
    snapshot::collect(&store).map_err(|err| err.to_string())
}

fn statsai_command_timeout(args: &[&str]) -> Duration {
    match args.first() {
        Some(&"scan") => Duration::from_secs(10 * 60),
        Some(&"sync") => Duration::from_secs(5 * 60),
        Some(&"auth") => Duration::from_secs(10 * 60),
        Some(&"service") => Duration::from_secs(2 * 60),
        _ => Duration::from_secs(5 * 60),
    }
}

pub(super) fn run_statsai_capture(args: &[&str]) -> Result<String, String> {
    let binary = statsai_binary()?;
    let timeout = statsai_command_timeout(args);
    let mut child = statsai_command(&binary)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run {}: {err}", binary.display()))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture statsai stdout".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture statsai stderr".to_string())?;

    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_handle.join();
                    let _ = stderr_handle.join();
                    return Err(format!(
                        "statsai {} timed out after {} seconds",
                        args.join(" "),
                        timeout.as_secs()
                    ));
                }
                thread::sleep(Duration::from_millis(200));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                return Err(format!("failed to wait for {}: {err}", binary.display()));
            }
        }
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();

    let mut message = String::from_utf8_lossy(&stdout).trim().to_string();
    let stderr_message = String::from_utf8_lossy(&stderr).trim().to_string();
    if !stderr_message.is_empty() {
        if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(&stderr_message);
    }
    if message.is_empty() {
        message = format!("statsai {} exited with {}", args.join(" "), status);
    }

    if status.success() {
        Ok(truncate_for_alert(&message, 1200))
    } else {
        Err(truncate_for_alert(&message, 1200))
    }
}

pub(super) fn run_statsai_capture_dynamic(args: Vec<String>) -> Result<String, String> {
    let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_statsai_capture(&borrowed)
}

fn truncate_for_alert(message: &str, max_chars: usize) -> String {
    if message.chars().count() <= max_chars {
        return message.to_string();
    }
    let mut end = max_chars;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}

fn statsai_binary() -> Result<PathBuf, String> {
    static BINARY: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BINARY.get_or_init(resolve_statsai_binary).clone()
}

fn resolve_statsai_binary() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("STATSAI_CLI") {
        let path = path.trim();
        if !path.is_empty() {
            return validate_cli_path(&PathBuf::from(path));
        }
    }

    let current_exe = std::env::current_exe().ok();

    if let Some(exe) = current_exe.as_deref() {
        if let Some(bundle_binary) = bundled_statsai_binary(exe) {
            return validate_cli_path(&bundle_binary);
        }

        if let Some(parent) = exe.parent() {
            let sibling = parent.join("statsai");
            if sibling.is_file() {
                if let Ok(path) = validate_cli_path(&sibling) {
                    return Ok(path);
                }
            }
        }
    }

    if let Ok(path) = which_statsai() {
        if current_exe
            .as_deref()
            .is_none_or(|exe| !same_executable(&path, exe))
        {
            return validate_cli_path(&path);
        }
    }

    Err("StatsAI could not find its background service.".to_string())
}

fn validate_cli_path(path: &Path) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if file_name == "statsai-menubar" || file_name == "StatsAI" {
        return Err(format!(
            "{} is the menu bar app, not the CLI.",
            path.display()
        ));
    }

    let output = Command::new(path)
        .arg("--help")
        .output()
        .map_err(|err| format!("failed to execute {}: {err}", path.display()))?;
    let help = String::from_utf8_lossy(&output.stdout);
    if help.contains("snapshot") && help.contains("scan") {
        Ok(path.to_path_buf())
    } else {
        Err(format!(
            "{} does not look like the statsai CLI.",
            path.display()
        ))
    }
}

fn bundled_statsai_binary(exe: &Path) -> Option<PathBuf> {
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    if contents.file_name().and_then(|name| name.to_str()) != Some("Contents") {
        return None;
    }
    if contents
        .parent()
        .and_then(|path| path.extension())
        .and_then(|ext| ext.to_str())
        != Some("app")
    {
        return None;
    }

    let cli = macos_dir.join("statsai");
    if cli.is_file() && !same_executable(&cli, exe) {
        return Some(cli);
    }
    None
}

fn which_statsai() -> Result<PathBuf, ()> {
    let output = Command::new("which")
        .arg("statsai")
        .output()
        .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    let path = String::from_utf8(output.stdout).map_err(|_| ())?;
    let path = path.trim();
    if path.is_empty() {
        return Err(());
    }
    Ok(PathBuf::from(path))
}

fn same_executable(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}
