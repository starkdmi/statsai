use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use getrandom::getrandom;
use hmac::{Hmac, Mac};
use keyring::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const RUNTIME_CONFIG_VERSION: &str = "privacy_runtime.v1";
pub const DEFAULT_MLX_MEMORY_LIMIT_MIB: u64 = 4 * 1024;
pub const DEFAULT_MLX_CACHE_LIMIT_MIB: u64 = 256;
pub const DEFAULT_MLX_MAX_BATCH_TOKENS: usize = statsai_privacy::MLX_FIXED_TRACE_PADDED_TOKENS;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PrivacyRuntimeConfig {
    pub schema_version: String,
    pub mlx_server: PathBuf,
    pub mlx_model: PathBuf,
    pub kingfisher_helper: PathBuf,
    pub viterbi_calibration_sha256: String,
    pub mlx_server_sha256: String,
    pub mlx_model_sha256: String,
    pub kingfisher_sha256: String,
    pub mlx_memory_limit_mib: u64,
    pub mlx_cache_limit_mib: u64,
    pub mlx_max_batch_tokens: usize,
}

impl PrivacyRuntimeConfig {
    #[must_use]
    pub fn model_revision(&self) -> String {
        format!(
            "model:{};helper:{};calibration:{}",
            self.mlx_model_sha256, self.mlx_server_sha256, self.viterbi_calibration_sha256
        )
    }
}

pub fn inspect_runtime(
    mlx_server: &Path,
    mlx_model: &Path,
    kingfisher_helper: &Path,
) -> Result<PrivacyRuntimeConfig> {
    validate_regular_file(mlx_server, "MLX server")?;
    validate_regular_file(mlx_model, "MLX model")?;
    validate_regular_file(kingfisher_helper, "Kingfisher helper")?;
    let calibration = mlx_model
        .parent()
        .context("MLX model path has no parent")?
        .join("viterbi_calibration.json");
    validate_regular_file(&calibration, "Viterbi calibration")?;
    let config = PrivacyRuntimeConfig {
        schema_version: RUNTIME_CONFIG_VERSION.to_string(),
        mlx_server: absolute_path(mlx_server)?,
        mlx_model: absolute_path(mlx_model)?,
        kingfisher_helper: absolute_path(kingfisher_helper)?,
        viterbi_calibration_sha256: hash_file(&calibration)?,
        mlx_server_sha256: hash_file(mlx_server)?,
        mlx_model_sha256: hash_file(mlx_model)?,
        kingfisher_sha256: hash_file(kingfisher_helper)?,
        mlx_memory_limit_mib: DEFAULT_MLX_MEMORY_LIMIT_MIB,
        mlx_cache_limit_mib: DEFAULT_MLX_CACHE_LIMIT_MIB,
        mlx_max_batch_tokens: DEFAULT_MLX_MAX_BATCH_TOKENS,
    };
    Ok(config)
}

pub fn save_runtime(store_path: &Path, config: &PrivacyRuntimeConfig) -> Result<()> {
    write_private_json(&runtime_config_path(store_path)?, config)
}

pub fn load_runtime(store_path: &Path) -> Result<PrivacyRuntimeConfig> {
    let path = runtime_config_path(store_path)?;
    let legacy_path = legacy_runtime_config_path(store_path)?;
    let (bytes, migrate_legacy) = match std::fs::read(&path) {
        Ok(bytes) => (bytes, false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let bytes = std::fs::read(&legacy_path).with_context(|| {
                format!(
                    "privacy runtime is not configured; run `statsai privacy setup`: {}",
                    path.display()
                )
            })?;
            (bytes, true)
        }
        Err(error) => return Err(error).context("read privacy runtime configuration"),
    };
    let config: PrivacyRuntimeConfig =
        serde_json::from_slice(&bytes).context("parse privacy runtime configuration")?;
    if config.schema_version != RUNTIME_CONFIG_VERSION {
        bail!("unsupported privacy runtime configuration version")
    }
    validate_runtime_limits(&config)?;
    validate_regular_file(&config.mlx_server, "MLX server")?;
    validate_regular_file(&config.mlx_model, "MLX model")?;
    validate_regular_file(&config.kingfisher_helper, "Kingfisher helper")?;
    let calibration = config
        .mlx_model
        .parent()
        .context("MLX model path has no parent")?
        .join("viterbi_calibration.json");
    verify_hash(&config.mlx_server, &config.mlx_server_sha256, "MLX server")?;
    verify_hash(&config.mlx_model, &config.mlx_model_sha256, "MLX model")?;
    verify_hash(
        &config.kingfisher_helper,
        &config.kingfisher_sha256,
        "Kingfisher helper",
    )?;
    verify_hash(
        &calibration,
        &config.viterbi_calibration_sha256,
        "Viterbi calibration",
    )?;
    if migrate_legacy {
        write_private_json(&path, &config)?;
    }
    Ok(config)
}

pub fn validate_runtime_limits(config: &PrivacyRuntimeConfig) -> Result<()> {
    if config.mlx_memory_limit_mib == 0 {
        bail!("MLX memory limit must be greater than zero")
    }
    if config.mlx_cache_limit_mib == 0 || config.mlx_cache_limit_mib > config.mlx_memory_limit_mib {
        bail!("MLX cache limit must be positive and no greater than the memory limit")
    }
    if config.mlx_max_batch_tokens < statsai_privacy::MLX_FIXED_TRACE_PADDED_TOKENS
        || config.mlx_max_batch_tokens > 32_768
    {
        bail!(
            "MLX maximum batch tokens must be between {} and 32768",
            statsai_privacy::MLX_FIXED_TRACE_PADDED_TOKENS
        )
    }
    Ok(())
}

pub(crate) fn load_pseudonym_key(
    store_path: &Path,
    expected_verifier: Option<&str>,
) -> Result<Option<[u8; 32]>> {
    let entry = privacy_keyring_entry(store_path)?;
    match entry.get_secret() {
        Ok(secret) => {
            let key = parse_key(&secret).context("invalid privacy key in OS keyring")?;
            verify_loaded_key(&key, expected_verifier)?;
            return Ok(Some(key));
        }
        Err(KeyringError::NoEntry) => {}
        Err(_) => {}
    }
    let fallback_path = privacy_key_path(store_path)?;
    if fallback_path.symlink_metadata().is_ok() {
        let key = read_private_key_file(&fallback_path)?;
        verify_loaded_key(&key, expected_verifier)?;
        return Ok(Some(key));
    }
    if let Some(expected_verifier) = expected_verifier {
        let legacy_path = legacy_privacy_key_path(store_path)?;
        if legacy_path.symlink_metadata().is_ok() {
            let key =
                read_private_key_file(&legacy_path).context("invalid legacy privacy key file")?;
            verify_pseudonym_key(&key, expected_verifier)
                .context("legacy privacy key does not belong to this store")?;
            write_private_bytes(&fallback_path, &key)?;
            return Ok(Some(key));
        }
    }
    Ok(None)
}

pub(crate) fn load_or_create_pseudonym_key(
    store_path: &Path,
    identity_exists: bool,
    expected_verifier: Option<&str>,
) -> Result<[u8; 32]> {
    if let Some(key) = load_pseudonym_key(store_path, expected_verifier)? {
        return Ok(key);
    }
    if identity_exists {
        bail!("privacy pseudonym state exists but its HMAC key is unavailable")
    }
    let entry = privacy_keyring_entry(store_path)?;
    let fallback_path = privacy_key_path(store_path)?;
    let mut key = [0u8; 32];
    getrandom(&mut key).context("generate privacy pseudonym key")?;
    if entry.set_secret(&key).is_err() {
        write_private_bytes(&fallback_path, &key)?;
    }
    Ok(key)
}

#[must_use]
pub fn hmac_digest(key: &[u8; 32], category: &str, normalized_value: &str) -> String {
    hex::encode(
        keyed_hmac(key, category, normalized_value)
            .finalize()
            .into_bytes(),
    )
}

#[must_use]
pub fn pseudonym_key_verifier(key: &[u8; 32]) -> String {
    hmac_digest(key, "key_verifier", "filtered_dataset.v1")
}

pub(crate) fn verify_pseudonym_key(key: &[u8; 32], expected_verifier: &str) -> Result<()> {
    let expected = hex::decode(expected_verifier).context("invalid privacy key verifier")?;
    keyed_hmac(key, "key_verifier", "filtered_dataset.v1")
        .verify_slice(&expected)
        .map_err(|_| {
            anyhow::anyhow!("privacy pseudonym key does not match the initialized dataset")
        })
}

fn keyed_hmac(key: &[u8; 32], category: &str, normalized_value: &str) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts 32-byte keys");
    mac.update(category.as_bytes());
    mac.update(&[0]);
    mac.update(normalized_value.as_bytes());
    mac
}

#[must_use]
pub fn dataset_key(key: &[u8; 32], conversation_id: &str) -> String {
    let digest = hmac_digest(key, "conversation", conversation_id);
    format!("dataset_{}", &digest[..24])
}

#[must_use]
pub fn pseudonym_namespace(key: &[u8; 32]) -> String {
    let digest = hmac_digest(key, "namespace", "filtered_dataset.v1");
    format!("namespace_{}", &digest[..24])
}

pub fn runtime_config_path(store_path: &Path) -> Result<PathBuf> {
    let canonical = canonical_store_path(store_path)?;
    let scope = privacy_store_scope(&canonical);
    Ok(canonical
        .parent()
        .context("store path has no parent")?
        .join(format!("privacy-runtime-{scope}.json")))
}

fn legacy_runtime_config_path(store_path: &Path) -> Result<PathBuf> {
    Ok(canonical_store_path(store_path)?
        .parent()
        .context("store path has no parent")?
        .join("privacy-runtime.json"))
}

fn privacy_key_path(store_path: &Path) -> Result<PathBuf> {
    let canonical = canonical_store_path(store_path)?;
    let scope = privacy_store_scope(&canonical);
    Ok(canonical
        .parent()
        .context("store path has no parent")?
        .join(format!("privacy-pseudonym-{scope}.key")))
}

fn legacy_privacy_key_path(store_path: &Path) -> Result<PathBuf> {
    Ok(canonical_store_path(store_path)?
        .parent()
        .context("store path has no parent")?
        .join("privacy-pseudonym.key"))
}

fn privacy_keyring_entry(store_path: &Path) -> Result<Entry> {
    Entry::new("statsai", &privacy_keyring_account(store_path)?)
        .context("open privacy keyring entry")
}

fn privacy_keyring_account(store_path: &Path) -> Result<String> {
    let canonical = canonical_store_path(store_path)?;
    Ok(format!(
        "privacy-pseudonym-key-{}",
        privacy_store_scope(&canonical)
    ))
}

fn privacy_store_scope(canonical_store_path: &Path) -> String {
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(canonical_store_path.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in canonical_store_path.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(canonical_store_path.to_string_lossy().as_bytes());
    let scope = format!("{:x}", hasher.finalize());
    scope[..16].to_string()
}

fn canonical_store_path(store_path: &Path) -> Result<PathBuf> {
    match std::fs::canonicalize(store_path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let absolute = absolute_path(store_path)?;
            let parent = absolute.parent().context("store path has no parent")?;
            let file_name = absolute
                .file_name()
                .context("store path has no file name")?;
            Ok(parent
                .canonicalize()
                .with_context(|| format!("canonicalize store directory: {}", parent.display()))?
                .join(file_name))
        }
        Err(error) => {
            Err(error).with_context(|| format!("canonicalize store path: {}", store_path.display()))
        }
    }
}

fn parse_key(secret: &[u8]) -> Result<[u8; 32]> {
    secret
        .try_into()
        .map_err(|_| anyhow::anyhow!("privacy key must contain exactly 32 bytes"))
}

fn validate_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = path
        .metadata()
        .with_context(|| format!("read {label} metadata: {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{label} is not a regular file: {}", path.display())
    }
    Ok(())
}

fn validate_private_key_file(path: &Path) -> Result<()> {
    let metadata = path
        .symlink_metadata()
        .with_context(|| format!("read privacy key metadata: {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("privacy key is not a regular file: {}", path.display())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "privacy key permissions are not private: {}",
                path.display()
            )
        }
    }
    Ok(())
}

fn read_private_key_file(path: &Path) -> Result<[u8; 32]> {
    validate_private_key_file(path)?;
    let secret = std::fs::read(path)?;
    parse_key(&secret).context("invalid privacy key file")
}

fn verify_loaded_key(key: &[u8; 32], expected_verifier: Option<&str>) -> Result<()> {
    if let Some(expected_verifier) = expected_verifier {
        verify_pseudonym_key(key, expected_verifier)?;
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_hash(path: &Path, expected: &str, label: &str) -> Result<()> {
    let actual = hash_file(path)?;
    if actual != expected {
        bail!(
            "{label} fingerprint changed since privacy setup: {}",
            path.display()
        )
    }
    Ok(())
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_private_bytes(path, &bytes)
}

fn write_private_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("private file path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".statsai-privacy-")
        .tempfile_in(parent)?;
    temporary.as_file_mut().write_all(bytes)?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_keys_and_value_hmacs_are_stable_and_domain_separated() {
        let key = [7u8; 32];
        assert_eq!(
            dataset_key(&key, "conversation"),
            dataset_key(&key, "conversation")
        );
        assert_ne!(
            hmac_digest(&key, "email", "person@example.com"),
            hmac_digest(&key, "person", "person@example.com")
        );
        let verifier = pseudonym_key_verifier(&key);
        verify_pseudonym_key(&key, &verifier).expect("matching verifier");
        assert!(verify_pseudonym_key(&[8u8; 32], &verifier).is_err());
    }

    #[test]
    fn runtime_configuration_is_private_and_rejects_changed_assets() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("statsai.sqlite");
        let second_store = directory.path().join("second.sqlite");
        let server = directory.path().join("opf-server");
        let model = directory.path().join("model.mlxfn");
        let calibration = directory.path().join("viterbi_calibration.json");
        let kingfisher = directory.path().join("kingfisher");
        std::fs::write(&server, "server").expect("server");
        std::fs::write(&model, "model").expect("model");
        std::fs::write(&calibration, "calibration").expect("calibration");
        std::fs::write(&kingfisher, "kingfisher").expect("kingfisher");
        let config = inspect_runtime(&server, &model, &kingfisher).expect("inspect");
        let mut invalid = config.clone();
        invalid.mlx_memory_limit_mib = 0;
        assert!(validate_runtime_limits(&invalid).is_err());
        invalid = config.clone();
        invalid.mlx_cache_limit_mib = invalid.mlx_memory_limit_mib + 1;
        assert!(validate_runtime_limits(&invalid).is_err());
        invalid = config.clone();
        invalid.mlx_max_batch_tokens = statsai_privacy::MLX_FIXED_TRACE_PADDED_TOKENS - 1;
        assert!(validate_runtime_limits(&invalid).is_err());
        invalid = config.clone();
        invalid.mlx_max_batch_tokens = 32_769;
        assert!(validate_runtime_limits(&invalid).is_err());
        save_runtime(&store, &config).expect("save");

        let mut second_config = config.clone();
        second_config.mlx_cache_limit_mib /= 2;
        save_runtime(&second_store, &second_config).expect("save second runtime");

        assert_eq!(load_runtime(&store).expect("load"), config);
        assert_eq!(
            load_runtime(&second_store).expect("load second runtime"),
            second_config
        );
        assert_ne!(
            runtime_config_path(&store).expect("first runtime path"),
            runtime_config_path(&second_store).expect("second runtime path")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(runtime_config_path(&store).expect("runtime config path"))
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::write(&server, "changed").expect("change server");
        assert!(load_runtime(&store).is_err());
    }

    #[test]
    fn legacy_runtime_configuration_migrates_without_removing_it() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("statsai.sqlite");
        let server = directory.path().join("opf-server");
        let model = directory.path().join("model.mlxfn");
        let calibration = directory.path().join("viterbi_calibration.json");
        let kingfisher = directory.path().join("kingfisher");
        std::fs::write(&server, "server").expect("server");
        std::fs::write(&model, "model").expect("model");
        std::fs::write(&calibration, "calibration").expect("calibration");
        std::fs::write(&kingfisher, "kingfisher").expect("kingfisher");
        let config = inspect_runtime(&server, &model, &kingfisher).expect("inspect");
        let legacy = legacy_runtime_config_path(&store).expect("legacy runtime path");
        let scoped = runtime_config_path(&store).expect("scoped runtime path");
        write_private_json(&legacy, &config).expect("write legacy runtime");

        assert!(!scoped.exists());
        assert_eq!(load_runtime(&store).expect("load legacy runtime"), config);
        assert!(scoped.exists());
        assert!(legacy.exists());
    }

    #[test]
    fn missing_key_fails_when_pseudonym_state_already_exists() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("unique-statsai.sqlite");

        let error = load_or_create_pseudonym_key(&store, true, None)
            .expect_err("missing established key must fail closed");

        assert!(error.to_string().contains("HMAC key is unavailable"));
        assert!(!privacy_key_path(&store).expect("privacy key path").exists());
    }

    #[cfg(unix)]
    #[test]
    fn fallback_key_rejects_group_or_world_access() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("permission-test.sqlite");
        let key_path = privacy_key_path(&store).expect("privacy key path");
        std::fs::write(&key_path, [1u8; 32]).expect("write key");
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644))
            .expect("widen permissions");

        let error = load_pseudonym_key(&store, None).expect_err("unsafe key must fail closed");

        assert!(error.to_string().contains("permissions are not private"));
    }

    #[test]
    fn fallback_keys_are_scoped_to_each_store_in_one_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first_store = directory.path().join("first.sqlite");
        let second_store = directory.path().join("second.sqlite");
        let first_key_path = privacy_key_path(&first_store).expect("first key path");
        let second_key_path = privacy_key_path(&second_store).expect("second key path");

        assert_ne!(first_key_path, second_key_path);
        write_private_bytes(&first_key_path, &[1u8; 32]).expect("write first key");
        write_private_bytes(&second_key_path, &[2u8; 32]).expect("write second key");

        assert_eq!(
            read_private_key_file(&first_key_path).expect("first key"),
            [1u8; 32]
        );
        assert_eq!(
            read_private_key_file(&second_key_path).expect("second key"),
            [2u8; 32]
        );
    }

    #[test]
    fn legacy_fallback_migrates_only_after_matching_the_store_verifier() {
        let directory = tempfile::tempdir().expect("tempdir");
        let existing_store = directory.path().join("existing.sqlite");
        let new_store = directory.path().join("new.sqlite");
        let mismatched_store = directory.path().join("mismatched.sqlite");
        let legacy_path = legacy_privacy_key_path(&existing_store).expect("legacy key path");
        let key = [3u8; 32];
        write_private_bytes(&legacy_path, &key).expect("write legacy key");

        let verifier = pseudonym_key_verifier(&key);
        assert_eq!(
            load_pseudonym_key(&existing_store, Some(&verifier)).expect("migrate legacy key"),
            Some(key)
        );
        assert_eq!(
            read_private_key_file(&privacy_key_path(&existing_store).expect("scoped key path"))
                .expect("migrated key"),
            key
        );

        assert_eq!(
            load_pseudonym_key(&new_store, None).expect("ignore legacy key for new store"),
            None
        );
        assert!(!privacy_key_path(&new_store)
            .expect("new store key path")
            .exists());

        let wrong_verifier = pseudonym_key_verifier(&[4u8; 32]);
        assert!(load_pseudonym_key(&mismatched_store, Some(&wrong_verifier)).is_err());
        assert!(!privacy_key_path(&mismatched_store)
            .expect("mismatched key path")
            .exists());
    }

    #[test]
    fn store_scoped_privacy_paths_use_one_canonical_identity() {
        let current = std::env::current_dir().expect("current directory");
        let directory = tempfile::Builder::new()
            .prefix("statsai-canonical-store-")
            .tempdir_in(&current)
            .expect("tempdir in current directory");
        let absolute = directory.path().join("statsai.sqlite");
        std::fs::write(&absolute, []).expect("create store fixture");
        let relative = absolute
            .strip_prefix(&current)
            .expect("store is below current directory");

        assert_eq!(
            privacy_keyring_account(relative).expect("relative account"),
            privacy_keyring_account(&absolute).expect("absolute account")
        );
        assert_eq!(
            runtime_config_path(relative).expect("relative runtime path"),
            runtime_config_path(&absolute).expect("absolute runtime path")
        );
        assert_eq!(
            privacy_key_path(relative).expect("relative key path"),
            privacy_key_path(&absolute).expect("absolute key path")
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&absolute, directory.path().join("store-link"))
                .expect("create store symlink");
            let linked = directory.path().join("store-link");
            assert_eq!(
                privacy_keyring_account(&linked).expect("linked account"),
                privacy_keyring_account(&absolute).expect("absolute account")
            );
            assert_eq!(
                runtime_config_path(&linked).expect("linked runtime path"),
                runtime_config_path(&absolute).expect("absolute runtime path")
            );
            assert_eq!(
                privacy_key_path(&linked).expect("linked key path"),
                privacy_key_path(&absolute).expect("absolute key path")
            );
        }
    }
}
