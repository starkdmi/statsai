pub mod auth;
pub mod privacy;
pub mod privacy_cli;
pub mod service;
pub mod snapshot;

use anyhow::Result;
use chrono::Utc;
use getrandom::getrandom;
use statsai_core::{hash_text, home_dir};
use statsai_store::{RepricingReport, Store};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Opens a store for a command that reads or publishes price-derived data and
/// applies the compiled pricing ruleset first.
pub fn open_operational_store(path: &Path) -> Result<Store> {
    let store = Store::open(path)?;
    let report = store.ensure_current_pricing()?;
    log_repricing_report(&report);
    Ok(store)
}

pub fn log_repricing_report(report: &RepricingReport) {
    if !report.already_current {
        eprintln!("{report}");
    }
}

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
    fn open_operational_store_reprices_a_legacy_store() {
        use chrono::TimeZone;
        use statsai_core::{
            event_id, Confidence, CostInfo, EventSource, LocationOrigin, ModelInfo, PrivacyInfo,
            PrivacyMode, SessionInfo, SourceKind, SourceLocation, UsageCounts, UsageEvent,
            USAGE_EVENT_SCHEMA_VERSION,
        };
        use statsai_store::PRICING_RULESET_VERSION;
        use std::path::Path;

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("statsai.sqlite");
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 29, 12, 0, 0)
            .single()
            .expect("started_at");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-open-operational-reprice"),
            LocationOrigin::Configured,
        );
        {
            let store = Store::open(&path).expect("create store");
            store.upsert_source(&source).expect("source");
            let event = UsageEvent {
                schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
                event_id: event_id(
                    "codex",
                    &source.source_id,
                    "legacy-review",
                    None,
                    started_at,
                ),
                device_id: "device".to_string(),
                provider: "codex".to_string(),
                source_id: source.source_id.clone(),
                provider_account_id: None,
                subscription_id: None,
                source: EventSource {
                    adapter_id: "test".to_string(),
                    adapter_version: "0".to_string(),
                    source_kind: SourceKind::LocalAdapter,
                    location_origin: Some(LocationOrigin::Configured),
                    source_type: "jsonl".to_string(),
                    source_path_hash: source.path_hash.clone(),
                    source_record_id: Some("legacy-review".to_string()),
                    parse_confidence: Confidence::High,
                },
                session: SessionInfo {
                    session_id: "session".to_string(),
                    local_session_id_hash: Some("same-session".to_string()),
                    title: None,
                    started_at,
                    ended_at: None,
                    duration_seconds: None,
                },
                model: Some(ModelInfo {
                    name: Some("codex-auto-review".to_string()),
                    normalized_name: Some("codex-auto-review".to_string()),
                    provider_model_id: Some("codex-auto-review".to_string()),
                    speed: None,
                    reasoning_level: None,
                    reasoning_level_raw: None,
                }),
                usage: UsageCounts {
                    input_tokens: Some(1_000_000),
                    cache_creation_tokens: Some(1_000_000),
                    cache_read_tokens: Some(1_000_000),
                    output_tokens: Some(1_000_000),
                    total_tokens: Some(4_000_000),
                    ..UsageCounts::default()
                },
                runtime: None,
                cost: CostInfo {
                    currency: "USD".to_string(),
                    estimated_api_equivalent_usd: None,
                    provider_reported_usd: None,
                    estimated_api_equivalent_micro_usd: None,
                    provider_reported_micro_usd: None,
                    pricing_source: Some("unknown".to_string()),
                    pricing_version: None,
                    confidence: Confidence::Low,
                },
                parse_evidence: None,
                project: None,
                git: None,
                privacy: PrivacyInfo {
                    mode: PrivacyMode::MetadataOnly,
                    contains_prompt_text: false,
                    contains_response_text: false,
                    contains_file_paths: false,
                },
                created_at: started_at,
                imported_at: started_at,
            };
            store.insert_event(&event).expect("legacy event");
            assert_eq!(store.applied_pricing_ruleset_version().expect("meta"), None);
            drop(store);
        }

        let store = open_operational_store(&path).expect("operational open");
        assert_eq!(
            store.applied_pricing_ruleset_version().expect("applied"),
            Some(PRICING_RULESET_VERSION)
        );
        let stored = store.events().expect("events");
        assert_eq!(stored.len(), 1);
        assert!(stored[0].cost.estimated_api_equivalent_usd.is_some());
    }

    #[test]
    fn open_operational_store_refuses_a_newer_pricing_ruleset() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("statsai.sqlite");
        let store = Store::open(&path).expect("create store");
        store
            .set_metadata_value(statsai_store::APPLIED_PRICING_RULESET_VERSION_KEY, "99")
            .expect("future ruleset");
        drop(store);

        let error = match open_operational_store(&path) {
            Ok(_) => panic!("forward pricing must refuse"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("pricing ruleset version 99 is newer than this StatsAI binary supports"));
        assert_eq!(
            statsai_store::database_applied_pricing_ruleset_version(&path).expect("unchanged"),
            Some(99)
        );
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
