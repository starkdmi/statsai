pub mod auth;
pub mod service;
pub mod snapshot;

use chrono::Utc;
use statsai_core::{hash_text, home_dir};
use std::path::PathBuf;

pub fn default_store_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".statsai")
        .join("statsai.sqlite")
}

pub fn default_device_id() -> String {
    if let Ok(value) = std::env::var("STATSAI_DEVICE_ID") {
        let value = value.trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }

    let path = device_id_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return existing.to_string();
        }
    }

    let device_id = generate_device_id();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format!("{device_id}\n"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    device_id
}

pub fn generate_device_id() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(read_hostname)
        .unwrap_or_else(|| "unknown-host".to_string());
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown-user".to_string());
    let home = home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .to_string_lossy()
        .to_string();
    let seed = format!(
        "{}:{}:{}:{}:{}",
        host,
        user,
        home,
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    format!("dev_{}", &hash_text(&seed)[..16])
}

fn device_id_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".statsai")
        .join("device-id")
}

fn read_hostname() -> Option<String> {
    let output = std::process::Command::new("hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let host = String::from_utf8(output.stdout).ok()?;
    let host = host.trim();
    (!host.is_empty()).then(|| host.to_string())
}