use crate::github::{validate_full_sha, WorkflowRun, REPOSITORY};
use crate::state::{create_private_dir, restrict_file_permissions, Paths};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

pub(crate) const TARGET: &str = "aarch64-apple-darwin";
const MANIFEST_SCHEMA: u32 = 2;
const MAX_BINARY_BYTES: u64 = 192 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BuildManifest {
    pub(crate) schema: u32,
    pub(crate) repository: String,
    pub(crate) sha: String,
    pub(crate) target: String,
    pub(crate) source: ManifestSource,
    pub(crate) workflow_run_id: u64,
    pub(crate) workflow_attempt: u64,
    pub(crate) store_schema_version: i64,
    pub(crate) pricing_ruleset_version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ManifestSource {
    Main,
    PullRequest { number: u64 },
}

#[derive(Debug)]
pub(crate) struct VerifiedArtifact {
    pub(crate) manifest: BuildManifest,
    pub(crate) binary: Vec<u8>,
    pub(crate) binary_sha256: String,
}

#[derive(Debug, Clone)]
pub(crate) struct InstalledBuild {
    pub(crate) manifest: BuildManifest,
    pub(crate) binary_sha256: String,
    pub(crate) binary_path: PathBuf,
}

pub(crate) fn verify_download(
    archive: &[u8],
    expected_sha: &str,
    run: &WorkflowRun,
) -> Result<VerifiedArtifact> {
    validate_full_sha(expected_sha)?;
    let mut zip = ZipArchive::new(Cursor::new(archive)).context("open downloaded artifact ZIP")?;
    if zip
        .has_overlapping_files()
        .context("inspect artifact ZIP layout")?
    {
        bail!("artifact ZIP contains overlapping file data");
    }
    let mut binary = None;
    let mut manifest_bytes = None;
    let mut checksums = None;
    let mut seen = BTreeSet::new();

    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .with_context(|| format!("read artifact ZIP entry {index}"))?;
        if entry.is_dir() {
            bail!("artifact contains unexpected directory {}", entry.name());
        }
        let enclosed = entry
            .enclosed_name()
            .context("artifact contains an unsafe ZIP path")?;
        let name = enclosed
            .to_str()
            .context("artifact contains a non-UTF-8 filename")?;
        if name.contains('/') || name.contains('\\') {
            bail!("artifact entry must be at its root: {name}");
        }
        if !seen.insert(name.to_string()) {
            bail!("artifact contains duplicate entry {name}");
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("artifact contains symbolic link {name}");
        }
        match name {
            "statsai" => binary = Some(read_limited(&mut entry, MAX_BINARY_BYTES, name)?),
            "build.json" => {
                manifest_bytes = Some(read_limited(&mut entry, MAX_METADATA_BYTES, name)?)
            }
            "SHA256SUMS" => checksums = Some(read_limited(&mut entry, MAX_METADATA_BYTES, name)?),
            _ => bail!("artifact contains unexpected entry {name}"),
        }
    }

    let binary = binary.context("artifact is missing statsai")?;
    let manifest_bytes = manifest_bytes.context("artifact is missing build.json")?;
    let checksums = checksums.context("artifact is missing SHA256SUMS")?;
    let manifest = parse_manifest(&manifest_bytes, "parse artifact build.json")?;
    validate_manifest(&manifest, expected_sha, Some(run))?;
    validate_macho_arm64(&binary)?;
    let expected_checksum = parse_statsai_checksum(&checksums)?;
    let binary_sha256 = sha256_bytes(&binary);
    if !binary_sha256.eq_ignore_ascii_case(&expected_checksum) {
        bail!(
            "artifact checksum mismatch for statsai: expected {expected_checksum}, got {binary_sha256}"
        );
    }

    Ok(VerifiedArtifact {
        manifest,
        binary,
        binary_sha256,
    })
}

pub(crate) fn install(paths: &Paths, artifact: &VerifiedArtifact) -> Result<InstalledBuild> {
    paths.ensure_cache_dirs()?;
    let destination = build_dir(paths, &artifact.manifest.sha)?;
    if destination.try_exists()? {
        remove_build_dir(paths, &destination)?;
    }

    let staging = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(&paths.builds_dir)
        .with_context(|| {
            format!(
                "create build staging directory in {}",
                paths.builds_dir.display()
            )
        })?;
    create_private_dir(staging.path())?;
    let binary_path = staging.path().join("statsai");
    write_binary(&binary_path, &artifact.binary)?;
    let manifest_path = staging.path().join("build.json");
    write_json(&manifest_path, &artifact.manifest)?;
    let sums_path = staging.path().join("SHA256SUMS");
    write_private(
        &sums_path,
        format!("{}  statsai\n", artifact.binary_sha256).as_bytes(),
    )?;
    File::open(staging.path())?.sync_all()?;

    let staged_path = staging.keep();
    if let Err(error) = fs::rename(&staged_path, &destination) {
        let _ = fs::remove_dir_all(&staged_path);
        return Err(error)
            .with_context(|| format!("publish cached build {}", artifact.manifest.sha.as_str()));
    }
    File::open(&paths.builds_dir)?.sync_all()?;

    Ok(InstalledBuild {
        manifest: artifact.manifest.clone(),
        binary_sha256: artifact.binary_sha256.clone(),
        binary_path: destination.join("statsai"),
    })
}

pub(crate) fn load_cached(paths: &Paths, sha: &str) -> Result<Option<InstalledBuild>> {
    let directory = build_dir(paths, sha)?;
    if !directory.try_exists()? {
        return Ok(None);
    }
    let manifest_path = directory.join("build.json");
    let manifest_bytes = fs::read(&manifest_path)
        .with_context(|| format!("read cached manifest {}", manifest_path.display()))?;
    let manifest = parse_manifest(
        &manifest_bytes,
        &format!("parse cached manifest {}", manifest_path.display()),
    )?;
    validate_manifest(&manifest, sha, None)?;

    let sums_path = directory.join("SHA256SUMS");
    let checksums = fs::read(&sums_path)
        .with_context(|| format!("read cached checksums {}", sums_path.display()))?;
    let expected_checksum = parse_statsai_checksum(&checksums)?;
    let binary_path = directory.join("statsai");
    let binary_sha256 = sha256_file(&binary_path)?;
    if !binary_sha256.eq_ignore_ascii_case(&expected_checksum) {
        bail!(
            "cached build checksum mismatch for {}: expected {expected_checksum}, got {binary_sha256}",
            binary_path.display()
        );
    }
    validate_cached_macho(&binary_path)?;

    Ok(Some(InstalledBuild {
        manifest,
        binary_sha256,
        binary_path,
    }))
}

pub(crate) fn prune(paths: &Paths, keep: &[&str]) -> Result<usize> {
    if !paths.builds_dir.try_exists()? {
        return Ok(0);
    }
    let keep: BTreeSet<_> = keep.iter().copied().collect();
    let mut removed = 0;
    for entry in fs::read_dir(&paths.builds_dir)
        .with_context(|| format!("read build cache {}", paths.builds_dir.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if keep.contains(name.as_ref()) {
            continue;
        }
        let path = entry.path();
        remove_build_dir(paths, &path)?;
        removed += 1;
    }
    File::open(&paths.builds_dir)?.sync_all()?;
    Ok(removed)
}

fn build_dir(paths: &Paths, sha: &str) -> Result<PathBuf> {
    validate_full_sha(sha)?;
    Ok(paths.builds_dir.join(sha.to_ascii_lowercase()))
}

fn remove_build_dir(paths: &Paths, directory: &Path) -> Result<()> {
    if directory.parent() != Some(paths.builds_dir.as_path()) {
        bail!("refusing to remove path outside the statsai-dev build cache");
    }
    fs::remove_dir_all(directory)
        .with_context(|| format!("remove cached build {}", directory.display()))
}

fn parse_manifest(bytes: &[u8], context: &str) -> Result<BuildManifest> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).with_context(|| context.to_string())?;
    match value.get("schema").and_then(serde_json::Value::as_u64) {
        Some(schema) if schema == u64::from(MANIFEST_SCHEMA) => {}
        Some(schema) => {
            bail!("unsupported build manifest schema {schema} (expected {MANIFEST_SCHEMA})")
        }
        None => bail!("build manifest is missing schema (expected {MANIFEST_SCHEMA})"),
    }
    serde_json::from_value(value).with_context(|| context.to_string())
}

fn validate_manifest(
    manifest: &BuildManifest,
    expected_sha: &str,
    run: Option<&WorkflowRun>,
) -> Result<()> {
    if manifest.schema != MANIFEST_SCHEMA {
        bail!(
            "unsupported build manifest schema {} (expected {MANIFEST_SCHEMA})",
            manifest.schema
        );
    }
    if manifest.repository != REPOSITORY {
        bail!(
            "build repository mismatch: expected {REPOSITORY}, got {}",
            manifest.repository
        );
    }
    if !manifest.sha.eq_ignore_ascii_case(expected_sha) {
        bail!(
            "build SHA mismatch: expected {expected_sha}, got {}",
            manifest.sha
        );
    }
    if manifest.target != TARGET {
        bail!(
            "build target mismatch: expected {TARGET}, got {}",
            manifest.target
        );
    }
    if manifest.store_schema_version < 0 {
        bail!(
            "build manifest contains invalid store schema version {}",
            manifest.store_schema_version
        );
    }
    if let Some(run) = run {
        if manifest.workflow_run_id != run.id || manifest.workflow_attempt != run.run_attempt {
            bail!(
                "build workflow metadata mismatch: expected run #{} / attempt {}, got #{} / attempt {}",
                run.id,
                run.run_attempt,
                manifest.workflow_run_id,
                manifest.workflow_attempt
            );
        }
    }
    Ok(())
}

fn parse_statsai_checksum(contents: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(contents).context("SHA256SUMS is not valid UTF-8")?;
    let mut matching = text.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let checksum = fields.next()?;
        let filename = fields.next()?.trim_start_matches('*');
        (filename == "statsai").then(|| checksum.to_string())
    });
    let checksum = matching
        .next()
        .context("SHA256SUMS has no checksum for statsai")?;
    if matching.next().is_some() {
        bail!("SHA256SUMS contains multiple checksums for statsai");
    }
    if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("SHA256SUMS contains an invalid checksum for statsai");
    }
    Ok(checksum.to_ascii_lowercase())
}

fn validate_macho_arm64(binary: &[u8]) -> Result<()> {
    if binary.len() < 12
        || binary[..4] != [0xcf, 0xfa, 0xed, 0xfe]
        || binary[4..8] != [0x0c, 0x00, 0x00, 0x01]
    {
        bail!("artifact statsai is not a 64-bit arm64 Mach-O executable");
    }
    Ok(())
}

fn validate_cached_macho(path: &Path) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("open cached executable {}", path.display()))?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header)
        .with_context(|| format!("read cached executable header {}", path.display()))?;
    validate_macho_arm64(&header)
}

fn read_limited(reader: &mut impl Read, maximum: u64, name: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read artifact entry {name}"))?;
    if bytes.len() as u64 > maximum {
        bail!("artifact entry {name} exceeds its safety limit");
    }
    Ok(bytes)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest).with_context(|| format!("hash {}", path.display()))?;
    Ok(hex::encode(digest.finalize()))
}

fn write_binary(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o755);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create cached executable {}", path.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(value).context("serialize cached build manifest")?;
    data.push(b'\n');
    write_private(path, &data)
}

fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create cached metadata {}", path.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    restrict_file_permissions(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Seek;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    fn fake_binary() -> Vec<u8> {
        let mut binary = vec![0u8; 64];
        binary[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        binary[4..8].copy_from_slice(&[0x0c, 0x00, 0x00, 0x01]);
        binary
    }

    fn manifest(sha: &str) -> BuildManifest {
        BuildManifest {
            schema: MANIFEST_SCHEMA,
            repository: REPOSITORY.to_string(),
            sha: sha.to_string(),
            target: TARGET.to_string(),
            source: ManifestSource::PullRequest { number: 12 },
            workflow_run_id: 104,
            workflow_attempt: 1,
            store_schema_version: 17,
            pricing_ruleset_version: 1,
        }
    }

    fn archive_raw_manifest(manifest: &[u8], binary: &[u8], checksum: &str) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        writer
            .start_file("statsai", options)
            .expect("start binary entry");
        writer.write_all(binary).expect("write binary entry");
        writer
            .start_file("build.json", options)
            .expect("start manifest entry");
        writer.write_all(manifest).expect("write manifest entry");
        writer
            .start_file("SHA256SUMS", options)
            .expect("start checksum entry");
        writeln!(writer, "{checksum}  statsai").expect("write checksum entry");
        let mut cursor = writer.finish().expect("finish artifact ZIP");
        cursor.rewind().expect("rewind artifact ZIP");
        cursor.into_inner()
    }

    fn run(sha: &str) -> WorkflowRun {
        WorkflowRun {
            id: 104,
            run_attempt: 1,
            status: "completed".to_string(),
            conclusion: Some("success".to_string()),
            head_sha: sha.to_string(),
            updated_at: String::new(),
            html_url: String::new(),
        }
    }

    fn archive(manifest: &BuildManifest, binary: &[u8], checksum: &str) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        writer
            .start_file("statsai", options)
            .expect("start binary entry");
        writer.write_all(binary).expect("write binary entry");
        writer
            .start_file("build.json", options)
            .expect("start manifest entry");
        serde_json::to_writer(&mut writer, manifest).expect("write manifest entry");
        writer
            .start_file("SHA256SUMS", options)
            .expect("start checksum entry");
        writeln!(writer, "{checksum}  statsai").expect("write checksum entry");
        let mut cursor = writer.finish().expect("finish artifact ZIP");
        cursor.rewind().expect("rewind artifact ZIP");
        cursor.into_inner()
    }

    #[test]
    fn verified_artifact_requires_matching_sha_repository_run_and_checksum() {
        let sha = "a".repeat(40);
        let binary = fake_binary();
        let checksum = sha256_bytes(&binary);
        let verified = verify_download(
            &archive(&manifest(&sha), &binary, &checksum),
            &sha,
            &run(&sha),
        )
        .expect("verify artifact");

        assert_eq!(verified.manifest.sha, sha);
        assert_eq!(verified.manifest.schema, MANIFEST_SCHEMA);
        assert_eq!(verified.manifest.pricing_ruleset_version, 1);
        assert_eq!(verified.binary_sha256, checksum);
    }

    #[test]
    fn shipped_schema_1_manifest_is_rejected() {
        let sha = "a".repeat(40);
        let binary = fake_binary();
        let checksum = sha256_bytes(&binary);
        let schema_1 = serde_json::json!({
            "schema": 1,
            "repository": REPOSITORY,
            "sha": sha,
            "target": TARGET,
            "source": { "kind": "pull_request", "number": 12 },
            "workflow_run_id": 104,
            "workflow_attempt": 1,
            "store_schema_version": 17
        });
        let error = verify_download(
            &archive_raw_manifest(
                &serde_json::to_vec(&schema_1).expect("serialize schema 1"),
                &binary,
                &checksum,
            ),
            &sha,
            &run(&sha),
        )
        .expect_err("shipped schema 1 must fail");

        assert!(
            error
                .to_string()
                .contains("unsupported build manifest schema 1 (expected 2)"),
            "{error}"
        );
    }

    #[test]
    fn schema_2_manifest_requires_pricing_ruleset_version() {
        let sha = "a".repeat(40);
        let binary = fake_binary();
        let checksum = sha256_bytes(&binary);
        let missing_pricing = serde_json::json!({
            "schema": 2,
            "repository": REPOSITORY,
            "sha": sha,
            "target": TARGET,
            "source": { "kind": "main" },
            "workflow_run_id": 104,
            "workflow_attempt": 1,
            "store_schema_version": 17
        });
        let error = verify_download(
            &archive_raw_manifest(
                &serde_json::to_vec(&missing_pricing).expect("serialize incomplete schema 2"),
                &binary,
                &checksum,
            ),
            &sha,
            &run(&sha),
        )
        .expect_err("schema 2 without pricing_ruleset_version must fail");

        let message = error.to_string();
        assert!(
            message.contains("pricing_ruleset_version") || message.contains("missing field"),
            "{message}"
        );
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let sha = "a".repeat(40);
        let binary = fake_binary();
        let error = verify_download(
            &archive(&manifest(&sha), &binary, &"0".repeat(64)),
            &sha,
            &run(&sha),
        )
        .expect_err("mismatched checksum must fail");

        assert!(error.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn install_cache_is_reverified_and_pruned_to_requested_builds() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        let first_sha = "a".repeat(40);
        let second_sha = "b".repeat(40);
        let binary = fake_binary();
        for sha in [&first_sha, &second_sha] {
            let artifact = VerifiedArtifact {
                manifest: manifest(sha),
                binary: binary.clone(),
                binary_sha256: sha256_bytes(&binary),
            };
            install(&paths, &artifact).expect("install cached build");
        }

        assert!(load_cached(&paths, &first_sha)
            .expect("verify cached build")
            .is_some());
        assert_eq!(prune(&paths, &[&second_sha]).expect("prune cache"), 1);
        assert!(load_cached(&paths, &first_sha)
            .expect("inspect pruned build")
            .is_none());
        assert!(load_cached(&paths, &second_sha)
            .expect("inspect retained build")
            .is_some());
    }
}
