use crate::state::{save, Paths, State};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use statsai_store::{
    clone_database_to, database_applied_pricing_ruleset_version, database_schema_version,
    DatabaseClone, Store,
};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// A backup's name records the schema it was taken at and when it was taken; both
/// are read back by [`latest_backup`], so the two halves are spelled once.
const BACKUP_NAME_PREFIX: &str = "statsai-schema";
const BACKUP_TIMESTAMP_FORMAT: &str = "%Y%m%dT%H%M%SZ";

#[derive(Debug)]
pub(crate) struct DataStatus {
    pub(crate) source_exists: bool,
    pub(crate) dev_exists: bool,
    pub(crate) refreshed_at: Option<DateTime<Utc>>,
    pub(crate) prod_schema: Option<i64>,
    pub(crate) dev_schema: Option<i64>,
    pub(crate) logical_size: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct RefreshOutcome {
    pub(crate) clone: DatabaseClone,
    pub(crate) restored_sync_states: usize,
    /// The clone's schema once the refresh finished. Carrying sync cursors means
    /// opening the clone, and opening a store migrates it, so this can be ahead
    /// of the schema the clone was copied with.
    pub(crate) schema_after: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProdVersions {
    pub(crate) schema: i64,
    pub(crate) pricing: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProdUpgradePlan {
    AlreadyCurrent,
    Upgrade {
        from: ProdVersions,
        schema: i64,
        pricing: u64,
    },
}

#[derive(Debug)]
pub(crate) struct ProdBackup {
    pub(crate) path: PathBuf,
    pub(crate) clone: DatabaseClone,
}

pub(crate) fn refresh(paths: &Paths, state: &mut State) -> Result<RefreshOutcome> {
    if !paths
        .prod_store
        .try_exists()
        .with_context(|| format!("check production store path {}", paths.prod_store.display()))?
    {
        bail!(
            "production StatsAI database does not exist at {}",
            paths.prod_store.display()
        );
    }
    paths.ensure_cache_dirs()?;
    // The clone is the only place a dev-backend sync cursor exists: production never
    // syncs there, so its tracking tables have no rows for it -- or stale ones from
    // an earlier era. Letting the clone overwrite carry that in tells the next sync
    // it is behind a server that is actually ahead, and the CLI responds by clearing
    // tracking and re-uploading the entire account. That is a full-history sync on
    // every refresh, which is how a routine `data refresh` came to cost six figures
    // of D1 row reads.
    //
    // The outgoing clone is moved aside rather than overwritten, so it stays intact
    // until the new one has adopted its cursors. Publishing first would mean a
    // failure between the copy and the adoption -- a production schema this launcher
    // cannot open, say -- destroys the only copy of them, and retrying cannot help.
    // A refresh interrupted after staging leaves the clone under the staged suffix.
    // It holds the only copy of the dev cursors, so it is put back before anything
    // else -- otherwise staging would discard it and the interrupted run would have
    // cost exactly what this function exists to prevent.
    recover_staged_clone(&paths.dev_store)?;
    let previous = stage_previous_clone(&paths.dev_store)?;
    let refreshed = (|| {
        let cloned = clone_database_to(&paths.prod_store, &paths.dev_store).with_context(|| {
            format!(
                "refresh isolated development database from {}",
                paths.prod_store.display()
            )
        })?;
        let adopted = match previous.as_deref() {
            Some(previous) => adopt_sync_tracking(&paths.dev_store, previous)?,
            None => 0,
        };
        // Read rather than assume: adopting cursors opens the clone, and opening a
        // store migrates it -- to *this launcher's* linked schema, which is not
        // necessarily the selected build's. Reporting the schema the clone was
        // copied with would then describe a database that no longer exists.
        let schema_after =
            database_schema_version(&paths.dev_store)?.unwrap_or(cloned.schema_version);
        Ok(RefreshOutcome {
            clone: cloned,
            restored_sync_states: adopted,
            schema_after,
        })
    })();

    match refreshed {
        Ok(outcome) => {
            // The clone is published and its cursors are adopted, and neither can be
            // undone from here, so leftovers that refuse to be deleted are reported
            // rather than turned into a failed refresh. Their marker survives, which
            // is what keeps the next refresh from mistaking them for a cursor-carrying
            // clone.
            if let Some(previous) = previous.as_deref() {
                if let Err(error) = discard_staged_clone(previous) {
                    eprintln!("warning: {error:#}");
                }
            }
            state.data.refreshed_at = Some(Utc::now());
            save(paths, state)?;
            Ok(outcome)
        }
        Err(error) => {
            if let Some(previous) = previous.as_deref() {
                restore_staged_clone(previous, &paths.dev_store)?;
            }
            Err(error)
        }
    }
}

/// Puts back a clone left under the staged suffix by an interrupted refresh.
///
/// Whatever sits at the live path is either missing or a freshly written clone that
/// has not adopted anything yet, so the staged copy always wins: it is the one
/// carrying cursors.
fn recover_staged_clone(dev_store: &Path) -> Result<()> {
    let staged = path_with_suffix(dev_store, ".previous");
    // Which step was running is recorded, not inferred from which files survived it.
    // Inference cannot work here: staging, restoring and discarding all move or delete
    // the main file first, so a second interruption leaves file layouts they share --
    // and the same leftover write-ahead log is precious after one of them and garbage
    // after another.
    if discarding_marker(&staged)
        .try_exists()
        .with_context(|| format!("check discard marker for {}", staged.display()))?
    {
        // Cleanup was under way, so everything staged belongs to a clone already
        // finished with. Putting its write-ahead log back would rename it over the
        // live one, which is where a freshly adopted cursor still sits.
        discard_staged_clone(&staged)?;
        return Ok(());
    }

    // Any component left behind means an interrupted refresh, not just the main file.
    // Restoration moves the main file first, so stopping partway through it leaves the
    // main file live and the staged write-ahead log stranded -- and in WAL mode that
    // log holds the newest writes, so a run that looked only at the main file would
    // walk past it and staging would then delete it.
    let staged_anything = carrying_database_files(&staged)
        .iter()
        .any(|path| path.exists());
    if !staged_anything {
        let _ = fs::remove_file(staging_marker(&staged));
        return Ok(());
    }
    let staging_completed = staging_marker(&staged)
        .try_exists()
        .with_context(|| format!("check staging marker for {}", staged.display()))?;
    let staged_main_present = staged
        .try_exists()
        .with_context(|| format!("check staged development database {}", staged.display()))?;

    eprintln!(
        "note: recovering the development database left behind by an interrupted refresh ({})",
        staged.display()
    );
    // Whether staging finished decides what the live files are. Without the marker
    // staging stopped partway, so the files still at the live path belong to the same
    // database as the staged ones -- in WAL mode that side can hold the newest cursor,
    // and deleting it would throw away exactly what this recovery exists to save. With
    // it, staging completed and anything live is a partly written clone that owns
    // nothing.
    //
    // Unless the main file is already back: then a previous restoration moved it and
    // the live files *are* the staged clone. Deleting them would destroy the very
    // copy this function exists to put back, so only the leftovers are moved over.
    if staging_completed && staged_main_present {
        for path in database_files(dev_store) {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("remove incomplete development database {}", path.display())
                    });
                }
            }
        }
    }
    restore_staged_clone(&staged, dev_store)?;
    let _ = fs::remove_file(staging_marker(&staged));
    Ok(())
}

/// Written once every file has moved, so a later run can tell a completed staging
/// from one that stopped halfway through.
fn staging_marker(staged: &Path) -> PathBuf {
    path_with_suffix(staged, ".complete")
}

/// Written before the staged clone starts being thrown away, so a later run can tell
/// leftovers nobody wants from leftovers that are the only copy of a cursor.
fn discarding_marker(staged: &Path) -> PathBuf {
    path_with_suffix(staged, ".discarding")
}

/// Moves the existing clone out of the way, returning where it went. `None` means
/// there was nothing to move.
fn stage_previous_clone(dev_store: &Path) -> Result<Option<PathBuf>> {
    if !dev_store
        .try_exists()
        .with_context(|| format!("check development store path {}", dev_store.display()))?
    {
        return Ok(None);
    }
    let staged = path_with_suffix(dev_store, ".previous");
    // Staging into a path that still holds someone else's leftovers would mix two
    // clones' files, so a cleanup that cannot finish stops the refresh here -- before
    // anything has moved.
    discard_staged_clone(&staged)?;
    // A partial move is worse than not moving at all: every failure here returns
    // without handing `refresh` the staged path, so `refresh`'s own rollback never
    // runs. Anything already moved would be stranded under the staged suffix with no
    // development database at the live path at all.
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (from, to) in database_files(dev_store)
        .into_iter()
        .zip(database_files(&staged))
    {
        match fs::rename(&from, &to) {
            Ok(()) => moved.push((to, from)),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                undo_staging(moved);
                return Err(error)
                    .with_context(|| format!("set aside development database {}", from.display()));
            }
        }
    }
    // The marker is written last and is just as much a part of staging as the moves:
    // without it a later run reads the staged clone as a staging that stopped partway.
    // Every file has already moved by the time it is written, so failing here has to
    // put them back like any other failed step.
    if let Err(error) = fs::write(staging_marker(&staged), b"") {
        undo_staging(moved);
        return Err(error)
            .with_context(|| format!("record staging completion for {}", staged.display()));
    }
    Ok(Some(staged))
}

/// Puts back everything a failed staging had already moved aside.
fn undo_staging(moved: Vec<(PathBuf, PathBuf)>) {
    for (staged_path, live_path) in moved.into_iter().rev() {
        let _ = fs::rename(staged_path, live_path);
    }
}

fn restore_staged_clone(staged: &Path, dev_store: &Path) -> Result<()> {
    for (from, to) in database_files(staged)
        .into_iter()
        .zip(database_files(dev_store))
    {
        match fs::rename(&from, &to) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("restore development database {}", to.display()));
            }
        }
    }
    Ok(())
}

fn discard_staged_clone(staged: &Path) -> Result<()> {
    // Nothing is removed until the intent to remove it is on disk. Deleting first and
    // recording afterwards is what makes an interrupted cleanup indistinguishable from
    // an interrupted staging or restoration, and those want the opposite treatment. If
    // the intent cannot be recorded, leaving the clone alone is the recoverable
    // outcome -- the next refresh stages over it.
    fs::write(discarding_marker(staged), b"").with_context(|| {
        format!(
            "record the intent to discard the previous development database {}",
            staged.display()
        )
    })?;

    let mut failures = Vec::new();
    for path in [staging_marker(staged)]
        .into_iter()
        .chain(database_files(staged))
    {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }
    // The marker outlives a cleanup that could not finish, and the caller hears about
    // it. Clearing it while a write-ahead log is still stranded under the staged
    // suffix would tell the next refresh that those leftovers came from an interrupted
    // staging -- so it would move them back beside the live clone, where they belong to
    // no database and can make it unreadable.
    if !failures.is_empty() {
        bail!(
            "could not discard the previous development database {}: {}",
            staged.display(),
            failures.join("; ")
        );
    }
    // Only reached once nothing is left to protect, so a marker that outlives this is
    // harmless: the next refresh finds no staged files and simply clears it.
    if let Err(error) = fs::remove_file(discarding_marker(staged)) {
        if error.kind() != ErrorKind::NotFound {
            eprintln!(
                "note: discarded {} but left its marker behind ({error}); the next refresh clears it",
                staged.display()
            );
        }
    }
    Ok(())
}

fn adopt_sync_tracking(dev_store: &Path, previous: &Path) -> Result<usize> {
    let store = Store::open(dev_store).with_context(|| {
        format!(
            "reopen refreshed development database {}",
            dev_store.display()
        )
    })?;
    // Whether there is anything to lose, decided by reading the clone rather than by
    // guessing from a schema number. A corrupt one fails to open, a legacy one opens
    // and is migrated far enough for its cursors to be readable, an empty one simply
    // has none. All three mean "nothing recoverable", and refusing to refresh over
    // them would break the one command that repairs a broken clone.
    // Only two things prove there is nothing to lose: a file that is not a database
    // at all, and one with no schema. Anything else is a real database, and failing
    // to read it does not mean it is empty -- a clone left one version ahead by a
    // schema-changing PR is exactly the case this launcher exists for, and treating
    // its "newer than supported" error as "no cursors" would delete the only copy.
    match database_schema_version(previous) {
        Err(error) => {
            eprintln!(
                "note: the previous development database at {} is unreadable ({error:#}); it had no sync cursors to carry",
                previous.display()
            );
            return Ok(0);
        }
        // Schema zero is ambiguous -- a legacy store predating `schema_migrations`,
        // which the migration layer stamps and upgrades, or a file with nothing in
        // it. Only reading it tells them apart, and a legacy store can hold cursors.
        Ok(None) => return Ok(0),
        Ok(Some(_)) => {}
    }
    let carries_cursors = Store::open(previous)
        .and_then(|previous_store| previous_store.list_sync_states())
        .map(|states| !states.is_empty())
        .with_context(|| {
            format!(
                "read sync cursors from the previous development database {}; \
                 refusing to discard them. Run `statsai-dev data clean` to drop the \
                 development database deliberately, or select a build that can open it",
                previous.display()
            )
        })?;
    if !carries_cursors {
        return Ok(0);
    }
    // It does carry cursors, so failing here would discard something recoverable --
    // the very full-history re-upload this exists to prevent. Report it and let the
    // caller put the previous clone back.
    store.adopt_sync_tracking_from(previous).with_context(|| {
        format!(
            "carry sync cursors from the previous development database {}",
            previous.display()
        )
    })
}

pub(crate) fn read_prod_versions(paths: &Paths) -> Result<ProdVersions> {
    read_prod_versions_at(&paths.prod_store)
}

pub(crate) fn read_prod_versions_at(database: &Path) -> Result<ProdVersions> {
    let Some(schema) = database_schema_version(database)? else {
        bail!("StatsAI database does not exist at {}", database.display());
    };
    Ok(ProdVersions {
        schema,
        pricing: database_applied_pricing_ruleset_version(database)?,
    })
}

/// Decides what a build may do to production data.
///
/// Migrations and pricing rulesets only go forward, so a build behind production
/// has nothing to offer it and must not try. A build level with production has
/// nothing to do. Everything else is an upgrade, including stamping a ruleset
/// onto a database that has never recorded one.
pub(crate) fn plan_prod_upgrade(
    current: ProdVersions,
    build_schema: i64,
    build_pricing: u64,
) -> Result<ProdUpgradePlan> {
    if current.schema > build_schema {
        bail!(
            "production is at store schema {}, ahead of the selected build's {build_schema}; migrations only go forward, so select a build that supports at least schema {}",
            current.schema,
            current.schema
        );
    }
    if let Some(pricing) = current.pricing {
        if pricing > build_pricing {
            bail!(
                "production has pricing ruleset {pricing} applied, ahead of the selected build's {build_pricing}; select a build that carries at least ruleset {pricing}"
            );
        }
    }
    if current.schema == build_schema && current.pricing == Some(build_pricing) {
        return Ok(ProdUpgradePlan::AlreadyCurrent);
    }
    Ok(ProdUpgradePlan::Upgrade {
        from: current,
        schema: build_schema,
        pricing: build_pricing,
    })
}

/// Clones production aside before anything migrates it.
///
/// The clone is copy-on-write, so a backup of a multi-gigabyte database costs
/// almost no time and almost no space. It is taken under the same writer lock and
/// checkpoint as any other clone, so it is a consistent database rather than a
/// snapshot of a file mid-write. The name carries when it was taken, because the
/// clone inherits production's modification time rather than being stamped with
/// its own -- see [`latest_backup`].
pub(crate) fn back_up_production(paths: &Paths, at: DateTime<Utc>) -> Result<ProdBackup> {
    let versions = read_prod_versions(paths)?;
    let path = paths.prod_backups_dir.join(format!(
        "{BACKUP_NAME_PREFIX}{}-{}.sqlite",
        versions.schema,
        at.format(BACKUP_TIMESTAMP_FORMAT)
    ));
    if path.try_exists()? {
        bail!(
            "refusing to overwrite the existing backup {}",
            path.display()
        );
    }
    let clone = clone_database_to(&paths.prod_store, &path).with_context(|| {
        format!(
            "back up the production database {} to {}",
            paths.prod_store.display(),
            path.display()
        )
    })?;
    Ok(ProdBackup { path, clone })
}

/// The most recently taken backup, if there is one.
///
/// Ordered by the timestamp written into the name, which is the only record of
/// when a backup was taken. Neither of the obvious alternatives works: the name
/// leads with the schema, so plain lexicographic order puts an older schema's
/// backup last, and `fclonefileat` copies the *source* file's modification time
/// onto the clone, so a backup's mtime describes when production was last written
/// rather than when the backup was made.
///
/// A file whose name this cannot read is skipped rather than guessed at. Anything
/// dropped into the directory by hand can still be restored by naming it.
pub(crate) fn latest_backup(paths: &Paths) -> Result<Option<PathBuf>> {
    let entries = match fs::read_dir(&paths.prod_backups_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "list production backups in {}",
                    paths.prod_backups_dir.display()
                )
            });
        }
    };

    let mut newest: Option<(NaiveDateTime, PathBuf)> = None;
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "read production backup directory {}",
                paths.prod_backups_dir.display()
            )
        })?;
        let path = entry.path();
        let Some(taken_at) = backup_timestamp(&path) else {
            continue;
        };
        // Ties break on the name so that two backups claiming the same second still
        // resolve the same way on every run, rather than on directory order.
        if newest
            .as_ref()
            .is_none_or(|(newest_at, newest_path)| (taken_at, &path) > (*newest_at, newest_path))
        {
            newest = Some((taken_at, path));
        }
    }
    Ok(newest.map(|(_, path)| path))
}

/// Reads back the timestamp [`back_up_production`] wrote into a backup's name.
fn backup_timestamp(path: &Path) -> Option<NaiveDateTime> {
    if path.extension()? != "sqlite" {
        return None;
    }
    let name = path.file_stem()?.to_str()?;
    let (schema, taken_at) = name.strip_prefix(BACKUP_NAME_PREFIX)?.split_once('-')?;
    if schema.is_empty() || !schema.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    NaiveDateTime::parse_from_str(taken_at, BACKUP_TIMESTAMP_FORMAT).ok()
}

/// Puts a backup back in production's place.
///
/// This is a clone rather than a rename for the same reason the backup was: a
/// database is not just its main file. Renaming one over production would leave
/// production's own `-wal` and `-shm` beside it, and SQLite would replay frames
/// written *after* the backup into the database being restored. `clone_database_to`
/// checkpoints the source, publishes a sidecar-free copy atomically, and displaces
/// the destination's sidecars as part of that swap.
pub(crate) fn restore_production(paths: &Paths, backup: &Path) -> Result<DatabaseClone> {
    if database_schema_version(backup)
        .with_context(|| format!("read backup {}", backup.display()))?
        .is_none()
    {
        bail!("backup {} does not exist", backup.display());
    }
    clone_database_to(backup, &paths.prod_store).with_context(|| {
        format!(
            "restore the production database {} from {}",
            paths.prod_store.display(),
            backup.display()
        )
    })
}

pub(crate) fn clean(paths: &Paths, state: &mut State) -> Result<usize> {
    let mut removed = 0;
    // Including anything an interrupted refresh left staged, which is otherwise
    // invisible and would be recovered over the next refresh.
    let staged = path_with_suffix(&paths.dev_store, ".previous");
    for path in database_files(&paths.dev_store)
        .into_iter()
        .chain(database_files(&staged))
        .chain([staging_marker(&staged), discarding_marker(&staged)])
    {
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("remove development database {}", path.display()));
            }
        }
    }
    state.data.refreshed_at = None;
    save(paths, state)?;
    if paths.data_dir.try_exists()? {
        fs::File::open(&paths.data_dir)?.sync_all()?;
    }
    Ok(removed)
}

pub(crate) fn inspect(paths: &Paths, state: &State) -> Result<DataStatus> {
    let prod_schema = database_schema_version(&paths.prod_store)?;
    let dev_schema = database_schema_version(&paths.dev_store)?;
    let source_exists = prod_schema.is_some();
    let dev_exists = dev_schema.is_some();
    let metadata = match fs::metadata(&paths.dev_store) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "read development database metadata {}",
                    paths.dev_store.display()
                )
            });
        }
    };
    let refreshed_at = state.data.refreshed_at.or_else(|| {
        metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .map(DateTime::<Utc>::from)
    });
    Ok(DataStatus {
        source_exists,
        dev_exists,
        refreshed_at,
        prod_schema,
        dev_schema,
        logical_size: metadata.map(|metadata| metadata.len()),
    })
}

pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn database_files(database: &Path) -> [PathBuf; 4] {
    [
        database.to_path_buf(),
        path_with_suffix(database, "-wal"),
        path_with_suffix(database, "-shm"),
        path_with_suffix(database, "-journal"),
    ]
}

/// The files that can hold committed data. The shared-memory index is left out: it is
/// a rebuildable view of the write-ahead log, so one stranded on its own says nothing
/// about whether there is a database to recover.
fn carrying_database_files(database: &Path) -> [PathBuf; 3] {
    [
        database.to_path_buf(),
        path_with_suffix(database, "-wal"),
        path_with_suffix(database, "-journal"),
    ]
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use statsai_store::SyncState;

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }

    fn production_at(schema: i64, pricing: Option<u64>) -> ProdVersions {
        ProdVersions { schema, pricing }
    }

    #[test]
    fn an_upgrade_is_planned_only_forward() {
        assert_eq!(
            plan_prod_upgrade(production_at(22, Some(1)), 23, 1).expect("schema upgrade"),
            ProdUpgradePlan::Upgrade {
                from: production_at(22, Some(1)),
                schema: 23,
                pricing: 1,
            }
        );
        // A database that has never recorded a ruleset is upgraded rather than
        // refused: stamping it is exactly what the missing metadata needs.
        assert_eq!(
            plan_prod_upgrade(production_at(23, None), 23, 1).expect("pricing upgrade"),
            ProdUpgradePlan::Upgrade {
                from: production_at(23, None),
                schema: 23,
                pricing: 1,
            }
        );
        assert_eq!(
            plan_prod_upgrade(production_at(23, Some(1)), 23, 1).expect("nothing to do"),
            ProdUpgradePlan::AlreadyCurrent
        );
    }

    #[test]
    fn a_build_behind_production_may_not_touch_it() {
        let older_schema = plan_prod_upgrade(production_at(23, Some(1)), 22, 1)
            .expect_err("a build behind production must refuse");
        assert!(older_schema.to_string().contains("only go forward"));

        let older_pricing = plan_prod_upgrade(production_at(23, Some(2)), 23, 1)
            .expect_err("an older ruleset must refuse");
        assert!(older_pricing.to_string().contains("pricing ruleset 2"));
    }

    // `clone_database_to` is APFS-only, so the backup path cannot run elsewhere.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_backup_is_taken_beside_production_without_changing_it() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        drop(Store::open(&paths.prod_store).expect("create production store"));
        let before = read_prod_versions(&paths).expect("read production versions");

        let at = DateTime::parse_from_rfc3339("2026-09-04T18:20:31Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let backup = back_up_production(&paths, at).expect("back up production");

        assert_eq!(
            backup.path,
            paths.prod_backups_dir.join(format!(
                "statsai-schema{}-20260904T182031Z.sqlite",
                before.schema
            ))
        );
        assert_eq!(
            database_schema_version(&backup.path).expect("read backup schema"),
            Some(before.schema)
        );
        assert_eq!(
            read_prod_versions(&paths).expect("production is unchanged"),
            before
        );

        // A second backup in the same second would otherwise overwrite the first,
        // which is the only copy of a database from before a migration.
        let repeated = back_up_production(&paths, at).expect_err("must not overwrite a backup");
        assert!(repeated.to_string().contains("refusing to overwrite"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn restoring_a_backup_discards_the_write_ahead_log_beside_production() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        let target = "https://api.example.test/api/sync/batches";
        let now = Utc::now();
        record_cursor(&paths.prod_store, target, now, "batch-before");
        let backup = back_up_production(&paths, now).expect("back up production");
        record_cursor(&paths.prod_store, target, now, "batch-after");

        // A crashed writer, or one still holding the database, leaves sidecars
        // behind. Renaming a backup over the main file would leave them in place,
        // and SQLite would replay frames written *after* the backup into the
        // database that was just restored.
        let stale_wal = path_with_suffix(&paths.prod_store, "-wal");
        fs::write(&stale_wal, b"frames written after the backup").expect("write a stale WAL");

        restore_production(&paths, &backup.path).expect("restore production");

        assert!(
            !stale_wal.exists(),
            "a write-ahead log from after the backup survived the restore"
        );
        assert_eq!(
            cursor(&paths.prod_store, target)
                .expect("restored cursor")
                .last_batch_id,
            "batch-before"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn the_newest_backup_is_the_one_restored_by_default() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        drop(Store::open(&paths.prod_store).expect("create production store"));
        assert_eq!(latest_backup(&paths).expect("no backups yet"), None);

        let taken = Utc::now();
        let older = back_up_production(&paths, taken - chrono::Duration::hours(1))
            .expect("older backup")
            .path;
        let newer = back_up_production(&paths, taken)
            .expect("newer backup")
            .path;

        // An APFS clone inherits the source file's modification time, so both backups
        // carry production's -- and a run that ranked them by mtime would be choosing
        // between two equal values by directory order.
        assert_eq!(
            fs::metadata(&older)
                .and_then(|metadata| metadata.modified())
                .expect("older mtime"),
            fs::metadata(&newer)
                .and_then(|metadata| metadata.modified())
                .expect("newer mtime")
        );

        assert_eq!(latest_backup(&paths).expect("read backups"), Some(newer));
        assert!(older.exists());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_newer_backup_at_an_older_schema_still_wins() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        drop(Store::open(&paths.prod_store).expect("create production store"));
        fs::create_dir_all(&paths.prod_backups_dir).expect("create backups directory");

        // Sorted by name, "schema9" lands after "schema23" and an unrelated file lands
        // after both. Only the timestamp says which was taken last.
        let older = paths
            .prod_backups_dir
            .join("statsai-schema23-20260904T180000Z.sqlite");
        let newer = paths
            .prod_backups_dir
            .join("statsai-schema9-20260904T190000Z.sqlite");
        let unnamed = paths.prod_backups_dir.join("something-else.sqlite");
        for path in [&older, &newer, &unnamed] {
            fs::write(path, b"backup").expect("write backup");
        }

        assert_eq!(
            latest_backup(&paths).expect("read backups"),
            Some(newer.clone())
        );
        assert_eq!(backup_timestamp(&unnamed), None);
        assert!(backup_timestamp(&older).is_some());
    }

    #[cfg(target_os = "macos")]
    fn record_cursor(path: &Path, target: &str, at: DateTime<Utc>, batch: &str) {
        let store = Store::open(path).expect("open store");
        store
            .restore_sync_states(&[SyncState {
                sink: "http".to_string(),
                target: target.to_string(),
                last_success_at: at,
                last_batch_id: batch.to_string(),
                last_event_started_at: None,
                last_event_id: None,
                last_summary_observed_at: None,
                last_summary_id: None,
                last_task_verification_updated_at: None,
                last_task_verification_id: None,
                failure_count: 0,
                pending_resume_batch_id: None,
            }])
            .expect("record cursor");
    }

    #[cfg(target_os = "macos")]
    fn cursor(path: &Path, target: &str) -> Option<SyncState> {
        Store::open(path)
            .expect("open store")
            .sync_state("http", target)
            .expect("read cursor")
    }

    // `clone_database_to` is APFS-only, so the refresh path cannot run elsewhere.
    #[test]
    #[cfg(target_os = "macos")]
    fn refresh_keeps_the_dev_cursor_production_never_had() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let dev_target = "https://dev-api.example.test/api/sync/batches";
        let now = Utc::now();

        // Production has never synced to the dev backend, so only the clone carries
        // that cursor. Losing it makes the next sync a full-history upload.
        drop(Store::open(&paths.prod_store).expect("create production store"));
        drop(Store::open(&paths.dev_store).expect("create development store"));
        record_cursor(&paths.dev_store, dev_target, now, "batch-dev-latest");

        let mut state = State::default();
        let outcome = refresh(&paths, &mut state).expect("refresh development data");

        assert_eq!(outcome.restored_sync_states, 1);
        let kept = cursor(&paths.dev_store, dev_target).expect("dev cursor survives refresh");
        assert_eq!(kept.last_batch_id, "batch-dev-latest");
    }

    // `clone_database_to` is APFS-only, so the refresh path cannot run elsewhere.
    #[test]
    #[cfg(target_os = "macos")]
    fn refresh_does_not_rewind_a_cursor_to_a_stale_production_one() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let dev_target = "https://dev-api.example.test/api/sync/batches";
        let prod_target = "https://api.example.test/api/sync/batches";
        let now = Utc::now();
        let stale = now - chrono::Duration::hours(12);

        // Production holds an old dev cursor from an earlier era, and a *fresher*
        // production cursor. The first must not overwrite the clone's newer one; the
        // second must win, because rewinding it would be equally wrong.
        drop(Store::open(&paths.prod_store).expect("create production store"));
        record_cursor(&paths.prod_store, dev_target, stale, "batch-dev-stale");
        record_cursor(&paths.prod_store, prod_target, now, "batch-prod-fresh");
        drop(Store::open(&paths.dev_store).expect("create development store"));
        record_cursor(&paths.dev_store, dev_target, now, "batch-dev-latest");
        record_cursor(&paths.dev_store, prod_target, stale, "batch-prod-stale");

        let mut state = State::default();
        refresh(&paths, &mut state).expect("refresh development data");

        assert_eq!(
            cursor(&paths.dev_store, dev_target)
                .expect("dev cursor")
                .last_batch_id,
            "batch-dev-latest"
        );
        assert_eq!(
            cursor(&paths.dev_store, prod_target)
                .expect("prod cursor")
                .last_batch_id,
            "batch-prod-fresh"
        );
    }

    // `clone_database_to` is APFS-only, so the refresh path cannot run elsewhere.
    #[test]
    #[cfg(target_os = "macos")]
    fn refresh_still_repairs_a_corrupt_or_cursorless_clone() {
        for (label, seed) in [("corrupt", &b"not a database"[..]), ("empty", &b""[..])] {
            let directory = tempfile::tempdir().expect("temporary directory");
            let paths = Paths::for_test(directory.path());
            paths.ensure_cache_dirs().expect("create cache directories");
            drop(Store::open(&paths.prod_store).expect("create production store"));
            fs::write(&paths.dev_store, seed).expect("write unusable clone");

            let mut state = State::default();
            let outcome = refresh(&paths, &mut state).unwrap_or_else(|error| {
                panic!("{label} clone must still be repairable: {error:#}")
            });

            assert_eq!(outcome.restored_sync_states, 0);
            assert!(
                database_schema_version(&paths.dev_store)
                    .expect("read refreshed schema")
                    .is_some(),
                "{label} clone should have been replaced"
            );
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn an_interrupted_refresh_recovers_its_staged_clone() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        drop(Store::open(&paths.prod_store).expect("create production store"));
        let dev_target = "https://dev-api.example.test/api/sync/batches";

        // What an interruption between staging and cloning leaves behind: the only
        // copy of the cursors sits under the staged suffix.
        let staged = path_with_suffix(&paths.dev_store, ".previous");
        drop(Store::open(&staged).expect("create staged clone"));
        record_cursor(&staged, dev_target, Utc::now(), "batch-dev-staged");

        let mut state = State::default();
        let outcome = refresh(&paths, &mut state).expect("refresh recovers and proceeds");

        assert_eq!(outcome.restored_sync_states, 1);
        assert_eq!(
            cursor(&paths.dev_store, dev_target)
                .expect("cursor survived")
                .last_batch_id,
            "batch-dev-staged"
        );
        assert!(!staged.exists(), "the staged copy is consumed");
    }

    #[test]
    fn a_partially_staged_clone_keeps_its_live_wal() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let staged = path_with_suffix(&paths.dev_store, ".previous");

        // Staging moves the main file first, so an interruption can leave the main
        // file staged and the write-ahead log -- which in WAL mode holds the newest
        // writes, including the newest cursor -- still at the live path. No marker
        // was written, so staging is known to be incomplete.
        fs::write(&staged, b"main").expect("stage the main file");
        let live_wal = path_with_suffix(&paths.dev_store, "-wal");
        fs::write(&live_wal, b"newest-writes").expect("leave the wal behind");

        recover_staged_clone(&paths.dev_store).expect("recover the interrupted staging");

        assert_eq!(
            fs::read(&paths.dev_store).expect("main file is back"),
            b"main"
        );
        assert_eq!(
            fs::read(&live_wal).expect("wal must survive"),
            b"newest-writes",
            "the newest writes must not be deleted with the incomplete clone"
        );
        assert!(!staged.exists());
    }

    #[test]
    fn an_interrupted_restoration_finishes_instead_of_discarding_the_rest() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let staged = path_with_suffix(&paths.dev_store, ".previous");

        // Restoration moves the main file first. Stopping right after that leaves the
        // main file live and the staged write-ahead log -- which holds the newest
        // cursor writes -- still under the staged suffix, with the marker from the
        // completed staging still in place.
        fs::write(&paths.dev_store, b"main").expect("main file already restored");
        let staged_wal = path_with_suffix(&staged, "-wal");
        fs::write(&staged_wal, b"newest-writes").expect("strand the staged wal");
        fs::write(staging_marker(&staged), b"").expect("staging had completed");

        recover_staged_clone(&paths.dev_store).expect("finish the interrupted restoration");

        assert_eq!(
            fs::read(&paths.dev_store).expect("main file must survive"),
            b"main",
            "the already-restored main file must not be deleted as an incomplete clone"
        );
        assert_eq!(
            fs::read(path_with_suffix(&paths.dev_store, "-wal")).expect("wal is back"),
            b"newest-writes",
            "the stranded wal must be carried over, not discarded by the next staging"
        );
        assert!(!staged_wal.exists(), "the staged copy is consumed");
        assert!(!staging_marker(&staged).exists(), "the marker is cleared");
    }

    #[test]
    fn an_interrupted_cleanup_discards_its_leftovers() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let staged = path_with_suffix(&paths.dev_store, ".previous");

        // A refresh that succeeded: the live clone is the new one, and its write-ahead
        // log is where the cursors adoption just wrote still sit.
        fs::write(&paths.dev_store, b"refreshed").expect("write the refreshed clone");
        let live_wal = path_with_suffix(&paths.dev_store, "-wal");
        fs::write(&live_wal, b"adopted-cursors").expect("write the adopted cursors");
        // Throwing the old clone away stopped partway, with the intent recorded before
        // the first deletion. What is left belongs to a database nobody wants back.
        let staged_wal = path_with_suffix(&staged, "-wal");
        fs::write(&staged_wal, b"superseded").expect("strand the old sidecar");
        fs::write(discarding_marker(&staged), b"").expect("cleanup had started");

        recover_staged_clone(&paths.dev_store).expect("recognise the interrupted cleanup");

        assert_eq!(
            fs::read(&live_wal).expect("the live wal must survive"),
            b"adopted-cursors",
            "a superseded sidecar must not be renamed over the log holding the adopted cursors"
        );
        assert_eq!(
            fs::read(&paths.dev_store).expect("the refreshed clone must survive"),
            b"refreshed"
        );
        assert!(
            !staged_wal.exists(),
            "the leftover is discarded, not restored"
        );
    }

    #[test]
    fn a_restoration_interrupted_after_an_incomplete_staging_keeps_its_wal() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let staged = path_with_suffix(&paths.dev_store, ".previous");

        // Two interruptions in a row. Staging moved the main file and the write-ahead
        // log but stopped before recording completion; recovery then moved the main
        // file back and stopped before the log. The leftovers now look exactly like an
        // interrupted cleanup -- no marker, no staged main, one stranded sidecar --
        // but this log is the only copy of the newest cursor.
        fs::write(&paths.dev_store, b"main").expect("main file already restored");
        let staged_wal = path_with_suffix(&staged, "-wal");
        fs::write(&staged_wal, b"newest-writes").expect("strand the staged wal");

        recover_staged_clone(&paths.dev_store).expect("finish the interrupted restoration");

        assert_eq!(
            fs::read(path_with_suffix(&paths.dev_store, "-wal")).expect("the wal is back"),
            b"newest-writes",
            "only a recorded cleanup may discard a stranded log"
        );
        assert_eq!(
            fs::read(&paths.dev_store).expect("main file must survive"),
            b"main"
        );
        assert!(!staged_wal.exists(), "the staged copy is consumed");
    }

    #[test]
    fn discarding_records_its_intent_and_clears_it_again() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let staged = path_with_suffix(&paths.dev_store, ".previous");
        fs::write(&staged, b"superseded").expect("stage a clone");
        fs::write(path_with_suffix(&staged, "-wal"), b"log").expect("stage its log");
        fs::write(staging_marker(&staged), b"").expect("staging had completed");

        discard_staged_clone(&staged).expect("discard the staged clone");

        assert!(!staged.exists(), "the clone is gone");
        assert!(!staging_marker(&staged).exists());
        // A marker left behind would tell the next recovery that a cleanup was under
        // way when none was, and it would discard a clone holding the only cursor.
        assert!(
            !discarding_marker(&staged).exists(),
            "the recorded intent must be cleared once the cleanup finishes"
        );
    }

    #[test]
    fn a_cleanup_that_cannot_finish_keeps_its_marker() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let staged = path_with_suffix(&paths.dev_store, ".previous");
        fs::write(&staged, b"superseded").expect("stage a clone");
        // A directory cannot be removed as a file, so this is a deletion that fails
        // after the intent to discard has been recorded.
        let undeletable = path_with_suffix(&staged, "-shm");
        fs::create_dir(&undeletable).expect("block one staged path");

        let error = discard_staged_clone(&staged).expect_err("a failed cleanup must be reported");

        assert!(error.to_string().contains("could not discard"), "{error:#}");
        // Without the marker the next recovery reads whatever is left as an
        // interrupted staging and moves it back beside the live clone, where those
        // files belong to no database at all.
        assert!(
            discarding_marker(&staged).exists(),
            "the marker must outlive a cleanup that could not finish"
        );
    }

    #[test]
    fn a_cleanup_that_cannot_record_its_intent_deletes_nothing() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let staged = path_with_suffix(&paths.dev_store, ".previous");
        fs::write(&staged, b"the only copy").expect("stage a clone");
        let staged_wal = path_with_suffix(&staged, "-wal");
        fs::write(&staged_wal, b"newest-writes").expect("stage its log");
        // Nothing can be written at the marker path, while the directory around it
        // stays writable -- so the deletions would otherwise go through.
        fs::create_dir(discarding_marker(&staged)).expect("block the marker path");

        discard_staged_clone(&staged).expect_err("an unrecorded cleanup must fail");

        // Deleting without a record of having started is what makes an interrupted
        // cleanup look like an interrupted restoration. Leaving the clone alone is
        // recoverable; the next refresh stages over it.
        assert!(staged.exists(), "an unrecorded cleanup must not start");
        assert_eq!(
            fs::read(&staged_wal).expect("the log must survive"),
            b"newest-writes"
        );
    }

    #[test]
    fn a_staging_that_cannot_record_completion_puts_the_clone_back() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        fs::write(&paths.dev_store, b"database").expect("write the clone");
        let wal = path_with_suffix(&paths.dev_store, "-wal");
        fs::write(&wal, b"newest-writes").expect("write its log");
        // Nothing can be written at the completion marker's path, so staging fails
        // after every file has already moved aside.
        let staged = path_with_suffix(&paths.dev_store, ".previous");
        fs::create_dir(staging_marker(&staged)).expect("block the marker path");

        stage_previous_clone(&paths.dev_store)
            .expect_err("staging must fail when it cannot record completion");

        // `refresh` never receives the staged path when staging fails, so its own
        // rollback cannot run. Leaving the files aside would leave no development
        // database at the live path at all.
        assert_eq!(
            fs::read(&paths.dev_store).expect("the clone must be back"),
            b"database"
        );
        assert_eq!(
            fs::read(&wal).expect("its log must be back"),
            b"newest-writes"
        );
        assert!(
            !staged.exists(),
            "nothing may be left under the staged suffix"
        );
    }

    #[test]
    fn a_failed_staging_leaves_the_clone_whole() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        fs::write(&paths.dev_store, b"database").expect("write database");
        let wal = path_with_suffix(&paths.dev_store, "-wal");
        fs::write(&wal, b"wal").expect("write wal");
        // Something is in the way under the staged suffix that cannot be cleared: a
        // directory is neither removable as a file nor replaceable by a rename.
        fs::create_dir(path_with_suffix(
            &path_with_suffix(&paths.dev_store, ".previous"),
            "-wal",
        ))
        .expect("block the staged wal path");

        let error = stage_previous_clone(&paths.dev_store)
            .expect_err("staging must fail when the staged paths cannot be cleared");
        assert!(error.to_string().contains("could not discard"), "{error:#}");

        assert_eq!(
            fs::read(&paths.dev_store).expect("database stays at the live path"),
            b"database"
        );
        assert_eq!(fs::read(&wal).expect("wal stays at the live path"), b"wal");
    }

    #[test]
    fn a_partial_staging_is_undone_file_by_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        let staged = path_with_suffix(&paths.dev_store, ".previous");
        fs::write(&staged, b"database").expect("stage the database");
        let staged_wal = path_with_suffix(&staged, "-wal");
        fs::write(&staged_wal, b"newest-writes").expect("stage its log");

        // What `stage_previous_clone` does when a later move fails: everything already
        // set aside goes back, so the live path is never left without its clone.
        undo_staging(vec![
            (staged.clone(), paths.dev_store.clone()),
            (
                staged_wal.clone(),
                path_with_suffix(&paths.dev_store, "-wal"),
            ),
        ]);

        assert_eq!(
            fs::read(&paths.dev_store).expect("database is back"),
            b"database"
        );
        assert_eq!(
            fs::read(path_with_suffix(&paths.dev_store, "-wal")).expect("its log is back"),
            b"newest-writes"
        );
        assert!(!staged.exists() && !staged_wal.exists());
    }

    #[test]
    fn clean_does_not_touch_production_database() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = Paths::for_test(directory.path());
        paths.ensure_cache_dirs().expect("create cache directories");
        fs::write(&paths.prod_store, b"production").expect("write production marker");
        fs::write(&paths.dev_store, b"development").expect("write development marker");
        let mut state = State::default();

        assert_eq!(
            clean(&paths, &mut state).expect("clean development data"),
            1
        );
        assert_eq!(
            fs::read(&paths.prod_store).expect("read production marker"),
            b"production"
        );
        assert!(!paths.dev_store.exists());
    }
}
