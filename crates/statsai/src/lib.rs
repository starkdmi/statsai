pub mod auth;
pub mod privacy;
pub mod privacy_cli;
pub mod service;
pub mod snapshot;

use chrono::Utc;
use getrandom::getrandom;
use statsai_core::{hash_text, home_dir};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
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

    device_id_at(&device_id_path())
}

/// Loads or claims the persisted device identity at `path`.
fn device_id_at(path: &std::path::Path) -> String {
    // An identity is only ever this process's if this process won the claim.
    // Every other outcome, including a transient failure while staging, falls
    // back to reading whatever is already on disk rather than keeping an
    // identity nobody else will agree with. `generate_device_id` seeds itself
    // with the current PID and nanosecond, so two callers that both decide to
    // mint one produce different answers, and any path that returns an unclaimed
    // identity splits one install's rows across an identity recorded nowhere.
    for _ in 0..DEVICE_ID_CLAIM_ATTEMPTS {
        if let Some(existing) = read_device_id(path) {
            return existing;
        }
        let candidate = generate_device_id();
        if claim_device_id(path, &candidate) {
            return candidate;
        }
    }
    // Someone holds it and we could not read it; the next run will agree with
    // them, and only this run is left guessing.
    read_device_id(path).unwrap_or_else(generate_device_id)
}

const DEVICE_ID_CLAIM_ATTEMPTS: usize = 8;

fn read_device_id(path: &std::path::Path) -> Option<String> {
    let existing = std::fs::read_to_string(path).ok()?;
    let existing = existing.trim();
    (!existing.is_empty()).then(|| existing.to_string())
}

/// Publishes `device_id` at `path`, reporting whether this caller claimed it.
///
/// The contents are staged in a private file and the claim is a hard link,
/// which cannot succeed twice and cannot publish a half-written file: creating
/// the target first and writing afterwards would expose an empty file that a
/// concurrent caller reads as "no identity yet".
fn claim_device_id(path: &std::path::Path, device_id: &str) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let _ = std::fs::create_dir_all(parent);
    let staged = parent.join(format!(
        ".device-id.{}.{}",
        std::process::id(),
        device_id.trim_start_matches("dev_")
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let staged_written = match options.open(&staged) {
        Ok(mut file) => file
            .write_all(format!("{device_id}\n").as_bytes())
            .and_then(|()| file.sync_all())
            .is_ok(),
        Err(_) => false,
    };
    if !staged_written {
        let _ = std::fs::remove_file(&staged);
        return false;
    }
    let claimed = std::fs::hard_link(&staged, path).is_ok();
    let _ = std::fs::remove_file(&staged);
    claimed
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

/// Loads or creates the per-install capability token for the loopback daemon.
pub fn default_daemon_auth_token() -> anyhow::Result<String> {
    let path = daemon_auth_token_path();
    daemon_auth_token_at(&path)
}

fn daemon_auth_token_at(path: &std::path::Path) -> anyhow::Result<String> {
    if let Ok(token) = read_daemon_auth_token(path) {
        return Ok(token);
    }

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("daemon token path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }

    let mut random = [0u8; 32];
    getrandom(&mut random)
        .map_err(|error| anyhow::anyhow!("generate daemon authentication token: {error}"))?;
    let token = hex::encode(random);

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            Ok(token)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => read_daemon_auth_token(path),
        Err(error) => Err(error.into()),
    }
}

/// Returns the path clients can read to authenticate to the loopback daemon.
pub fn daemon_auth_token_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".statsai")
        .join("daemon-token")
}

fn read_daemon_auth_token(path: &std::path::Path) -> anyhow::Result<String> {
    let token = std::fs::read_to_string(path)?;
    let token = token.trim();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid daemon authentication token in {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_first_runs_settle_on_one_device_identity() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("state").join("device-id");

        // Every one of these is a first run: none finds a file, so each mints a
        // distinct identity from its own PID and nanosecond. Only one may end up
        // owning the install, and the rest have to adopt it rather than keep an
        // identity recorded nowhere.
        let claimed = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    let path = path.clone();
                    scope.spawn(move || device_id_at(&path))
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("claim device id"))
                .collect::<std::collections::BTreeSet<_>>()
        });

        assert_eq!(
            claimed.len(),
            1,
            "concurrent first runs kept separate identities: {claimed:?}"
        );
        let persisted = std::fs::read_to_string(&path).expect("device id file");
        let persisted = persisted.trim();
        assert_eq!(claimed.iter().next().expect("claimed id"), persisted);
        // A later run adopts the same identity rather than minting another.
        assert_eq!(device_id_at(&path), persisted);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("device id metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn daemon_auth_token_is_random_persistent_and_private() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("state").join("daemon-token");

        let created = daemon_auth_token_at(&path).expect("create token");
        let loaded = daemon_auth_token_at(&path).expect("load token");

        assert_eq!(created.len(), 64);
        assert!(created.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(loaded, created);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("token metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().expect("token parent"))
                    .expect("parent metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn corrupt_daemon_auth_token_is_not_silently_replaced() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("daemon-token");
        std::fs::write(&path, "not-a-token\n").expect("write corrupt token");

        let error = daemon_auth_token_at(&path).expect_err("reject corrupt token");

        assert!(error
            .to_string()
            .contains("invalid daemon authentication token"));
        assert_eq!(
            std::fs::read_to_string(path).expect("read corrupt token"),
            "not-a-token\n"
        );
    }
}
