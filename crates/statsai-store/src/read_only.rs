//! Read-only access for processes that must never write the store.

use super::{
    configure_read_cache, migrations::existing_schema_version, snapshot, CacheReportQuery,
    QuotaQuery, QuotaStatus, SessionFilter, SessionSort, SessionStats, Store, SyncState,
    WeeklyResetAnchor, CURRENT_SCHEMA_VERSION, SQLITE_BUSY_TIMEOUT,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::config::DbConfig;
use rusqlite::Connection;
use statsai_core::{
    CacheReport, DailyRollup, ProviderAccount, ProviderAccountId, QuotaObservationRecordV1,
    QuotaWindowV1, SessionRollupV1, SourceAccountAssignment, SourceLocation, Subscription,
    UsageSummary,
};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// Why [`Store::open_read_only`] did not open a store.
///
/// The schema variants let a caller decide what to do: an older store needs a
/// writer such as any `statsai` command to migrate it, and a newer one needs a
/// newer StatsAI.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadOnlyOpenError {
    /// Nothing exists at the path. A read-only open never creates a store.
    #[error("no StatsAI store at {}", .path.display())]
    NotFound { path: PathBuf },
    /// The store was last migrated by an older StatsAI, or never migrated.
    #[error(
        "StatsAI store at {} has schema version {found}, older than the version this binary reads ({expected}); run statsai to migrate it",
        .path.display()
    )]
    SchemaTooOld {
        path: PathBuf,
        found: i64,
        expected: i64,
    },
    /// The store was migrated by a newer StatsAI than this binary.
    #[error(
        "StatsAI store at {} has schema version {found}, newer than this binary supports ({expected}); upgrade StatsAI",
        .path.display()
    )]
    SchemaTooNew {
        path: PathBuf,
        found: i64,
        expected: i64,
    },
    /// SQLite could not open the file or read its schema version.
    ///
    /// The message includes `reason`. It is deliberately not the error's
    /// `source`, so printing the error through a cause chain does not repeat it.
    #[error("open StatsAI store read-only at {}: {reason:#}", .path.display())]
    Open {
        path: PathBuf,
        reason: anyhow::Error,
    },
}

/// A store opened with [`Store::open_read_only`]: the read side of [`Store`]
/// and nothing else.
///
/// It is for processes that must never write the store, such as a dashboard
/// that reads it while `statsai scan` or `statsai daemon --watch` writes.
///
/// - The connection is opened with `SQLITE_OPEN_READONLY`, the flag the
///   `mode=ro` URI parameter selects, and `PRAGMA query_only` is on, so SQLite
///   refuses any write. It is deliberately not `immutable`: it takes ordinary
///   WAL read locks and sees each transaction a writer commits.
/// - Opening never creates the file, migrates, backfills, reprices, changes
///   permissions, or runs `PRAGMA optimize`, and closing never checkpoints.
/// - Only methods that read are exposed. Where [`Store`] pairs a read with
///   upkeep, as [`Store::all_sync_rollup_summaries`] does, this returns what is
///   stored and leaves the upkeep to the next writer.
///
/// Each call reads in its own transaction. Wrap several in
/// [`ReadStore::with_read_snapshot`] to see one point in time, and compare
/// [`ReadStore::data_version`] to notice that a writer has committed since.
///
/// A writer such as a newer `statsai` may migrate the store while it stays
/// open, so the schema version is checked again at the start of each of those
/// transactions, in the same snapshot as the reads. Once a migration has
/// recorded its version, a read fails with a [`ReadOnlyOpenError::SchemaTooNew`]
/// (or `SchemaTooOld`) that `anyhow::Error::downcast_ref` recovers; reopen, or
/// stop reading. A migration records its version after its schema changes, so
/// a read that lands while one is still running is not caught.
///
/// In WAL mode SQLite coordinates readers through the `-wal` and `-shm` files
/// beside the database. When no other connection has the store open they may
/// not exist, and SQLite creates them for this connection, which needs write
/// access to the directory (never to the database file). In a directory this
/// process cannot write, the open fails with [`ReadOnlyOpenError::Open`]
/// unless those files already exist.
pub struct ReadStore {
    store: Store,
    path: PathBuf,
}

impl std::fmt::Debug for ReadStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReadStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl ReadStore {
    pub(crate) fn open(path: &Path) -> std::result::Result<Self, ReadOnlyOpenError> {
        let missing = std::fs::metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        if missing {
            return Err(ReadOnlyOpenError::NotFound {
                path: path.to_path_buf(),
            });
        }
        let conn = open_read_only_connection(path).map_err(|reason| ReadOnlyOpenError::Open {
            path: path.to_path_buf(),
            reason,
        })?;
        let reader = Self {
            store: Store { conn },
            path: path.to_path_buf(),
        };
        reader.check_schema()?;
        Ok(reader)
    }

    /// The schema version recorded in the store now.
    ///
    /// It was [`CURRENT_SCHEMA_VERSION`] when the store opened, but a writer
    /// may have migrated the store since; see [`ReadStore::check_schema`].
    /// A store that predates `schema_migrations` has version zero, as
    /// [`crate::database_schema_version`] reports it.
    pub fn schema_version(&self) -> Result<i64> {
        // Not `Store::schema_version`: that creates `schema_migrations` and
        // sets the journal mode first, which a read-only connection must not.
        Ok(existing_schema_version(&self.store.conn)?.unwrap_or(0))
    }

    /// Checks again, as the open did, that the store's schema is the one this
    /// binary reads. Every read already does this; this checks without
    /// reading anything else.
    ///
    /// # Errors
    ///
    /// Returns [`ReadOnlyOpenError::SchemaTooOld`] or
    /// [`ReadOnlyOpenError::SchemaTooNew`] when a writer has changed the
    /// schema since the open, or [`ReadOnlyOpenError::Open`] when the version
    /// cannot be read.
    pub fn check_schema(&self) -> std::result::Result<(), ReadOnlyOpenError> {
        let found = self
            .schema_version()
            .map_err(|reason| ReadOnlyOpenError::Open {
                path: self.path.clone(),
                reason,
            })?;
        self.schema_mismatch(found).map_or(Ok(()), Err)
    }

    /// The error for a store at schema version `found`, if this binary does
    /// not read that version.
    fn schema_mismatch(&self, found: i64) -> Option<ReadOnlyOpenError> {
        let path = self.path.clone();
        let expected = CURRENT_SCHEMA_VERSION;
        match found.cmp(&expected) {
            Ordering::Less => Some(ReadOnlyOpenError::SchemaTooOld {
                path,
                found,
                expected,
            }),
            Ordering::Greater => Some(ReadOnlyOpenError::SchemaTooNew {
                path,
                found,
                expected,
            }),
            Ordering::Equal => None,
        }
    }

    /// See [`Store::data_version`]: changes when another connection commits.
    pub fn data_version(&self) -> Result<i64> {
        self.store.data_version()
    }

    /// Runs several reads against one point-in-time view, after checking that
    /// the schema in that view is still the one this binary reads. See
    /// [`Store::with_read_snapshot`].
    pub fn with_read_snapshot<T>(&self, operation: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        self.read(|_| operation(self))
    }

    /// Runs `operation` in a read transaction that first checks the schema.
    /// The check is the transaction's first read, so it and `operation` see
    /// the same snapshot and a migration cannot land between them. Inside
    /// [`ReadStore::with_read_snapshot`] it joins that transaction, which has
    /// already checked.
    fn read<T>(&self, operation: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        if !self.store.conn.is_autocommit() {
            return operation(&self.store);
        }
        self.store.with_read_snapshot(|store| {
            // Only a mismatch is a `ReadOnlyOpenError`. A failure to read the
            // version, such as a busy or I/O error, is the read's own error.
            let found = self
                .schema_version()
                .context("read the store's schema version")?;
            if let Some(mismatch) = self.schema_mismatch(found) {
                return Err(mismatch.into());
            }
            operation(store)
        })
    }

    /// The pricing ruleset the store's costs were last computed with. A reader
    /// never reprices, so this can trail the binary's own ruleset until a
    /// writer such as `statsai scan` next opens the store.
    pub fn applied_pricing_ruleset_version(&self) -> Result<Option<u64>> {
        self.read(|store| store.applied_pricing_ruleset_version())
    }

    pub fn event_count(&self) -> Result<u64> {
        self.read(|store| store.event_count())
    }

    pub fn token_total(&self) -> Result<u64> {
        self.read(|store| store.token_total())
    }

    /// See [`Store::session_rollups_in_period`].
    pub fn session_rollups_in_period(
        &self,
        since: Option<DateTime<Utc>>,
        until: DateTime<Utc>,
        filter: &SessionFilter,
        sort: SessionSort,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SessionRollupV1>> {
        self.read(|store| {
            store.session_rollups_in_period(since, until, filter, sort, limit, offset)
        })
    }

    /// See [`Store::session_stats_in_period`].
    pub fn session_stats_in_period(
        &self,
        since: Option<DateTime<Utc>>,
        until: DateTime<Utc>,
        filter: &SessionFilter,
    ) -> Result<SessionStats> {
        self.read(|store| store.session_stats_in_period(since, until, filter))
    }

    /// Every daily summary as stored. Unlike
    /// [`Store::all_sync_rollup_summaries`], summaries an older binary built
    /// are returned as they are rather than rebuilt; the next writer that
    /// reads them through [`Store`] rebuilds them.
    pub fn all_sync_rollup_summaries(&self) -> Result<Vec<UsageSummary>> {
        self.read(|store| store.stored_sync_rollup_summaries())
    }

    /// See [`Store::daily_rollups_between`].
    pub fn daily_rollups_between(
        &self,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<DailyRollup>> {
        self.read(|store| store.daily_rollups_between(start_date, end_date))
    }

    /// See [`Store::cache_report`].
    pub fn cache_report<Tz: chrono::TimeZone>(
        &self,
        query: &CacheReportQuery,
        timeline_zone: &Tz,
    ) -> Result<CacheReport>
    where
        Tz::Offset: std::fmt::Display,
    {
        self.read(|store| store.cache_report(query, timeline_zone))
    }

    /// See [`Store::quota_status`].
    pub fn quota_status(&self, query: &QuotaQuery) -> Result<QuotaStatus> {
        self.read(|store| store.quota_status(query))
    }

    /// See [`Store::quota_windows`].
    pub fn quota_windows(&self, query: &QuotaQuery) -> Result<Vec<QuotaWindowV1>> {
        self.read(|store| store.quota_windows(query))
    }

    /// See [`Store::quota_observations`].
    pub fn quota_observations(
        &self,
        query: &QuotaQuery,
        collapse_duplicates: bool,
    ) -> Result<Vec<QuotaObservationRecordV1>> {
        self.read(|store| store.quota_observations(query, collapse_duplicates))
    }

    /// See [`Store::weekly_reset_anchor`].
    pub fn weekly_reset_anchor(
        &self,
        provider: &str,
        provider_account_id: &ProviderAccountId,
    ) -> Result<Option<WeeklyResetAnchor>> {
        self.read(|store| store.weekly_reset_anchor(provider, provider_account_id))
    }

    pub fn list_sources(&self) -> Result<Vec<SourceLocation>> {
        self.read(|store| store.list_sources())
    }

    pub fn list_accounts(&self) -> Result<Vec<ProviderAccount>> {
        self.read(|store| store.list_accounts())
    }

    pub fn list_source_account_assignments(&self) -> Result<Vec<SourceAccountAssignment>> {
        self.read(|store| store.list_source_account_assignments())
    }

    pub fn list_subscriptions(&self) -> Result<Vec<Subscription>> {
        self.read(|store| store.list_subscriptions())
    }

    pub fn list_sync_states(&self) -> Result<Vec<SyncState>> {
        self.read(|store| store.list_sync_states())
    }
}

/// [`snapshot::open_read_only_unlabelled`], set up for a reader that stays open
/// while other processes write. [`ReadOnlyOpenError::Open`] names the path.
fn open_read_only_connection(path: &Path) -> Result<Connection> {
    let conn = snapshot::open_read_only_unlabelled(path)?;
    // A read-only connection cannot checkpoint anyway; this keeps it from
    // trying when it is the last one to close.
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA query_only = ON;")?;
    configure_read_cache(&conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_store_is_the_read_s_own_error_not_a_schema_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("statsai.sqlite");
        drop(Store::open(&path).expect("create store"));
        let writer = Connection::open(&path).expect("writer");
        writer
            .execute_batch("PRAGMA journal_mode = DELETE;")
            .expect("rollback journal");
        let reader = Store::open_read_only(&path).expect("open read-only");
        reader
            .store
            .conn
            .busy_timeout(std::time::Duration::ZERO)
            .expect("no busy wait");

        // An exclusive lock in rollback-journal mode keeps every reader out.
        writer
            .execute_batch("BEGIN EXCLUSIVE;")
            .expect("exclusive lock");
        let error = reader.event_count().expect_err("the store is locked");
        assert!(
            error.downcast_ref::<ReadOnlyOpenError>().is_none(),
            "a busy store was reported as a schema or open error: {error:#}"
        );
        assert!(
            format!("{error:#}").contains("read the store's schema version"),
            "{error:#}"
        );

        writer.execute_batch("COMMIT;").expect("release");
        reader.event_count().expect("read once the lock is gone");
    }

    #[test]
    fn a_failed_open_names_the_path_once() {
        let directory = tempfile::tempdir().expect("tempdir");
        // A directory exists, so it gets past the missing-file check, but
        // SQLite cannot open it.
        let path = directory.path().join("statsai.sqlite");
        std::fs::create_dir(&path).expect("directory");

        let error = Store::open_read_only(&path).expect_err("not a database file");
        assert!(matches!(error, ReadOnlyOpenError::Open { .. }), "{error:?}");
        let message = error.to_string();
        assert!(
            message.starts_with(&format!(
                "open StatsAI store read-only at {}: ",
                path.display()
            )),
            "{message}"
        );
        assert!(
            !message.contains("open database read-only at"),
            "the snapshot open's context repeats the path: {message}"
        );
    }

    #[test]
    fn the_read_only_connection_refuses_writes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("statsai.sqlite");
        drop(Store::open(&path).expect("create store"));

        let reader = Store::open_read_only(&path).expect("open read-only");
        let conn = &reader.store.conn;
        assert!(conn.is_readonly(rusqlite::MAIN_DB).expect("main db"));
        for write in [
            "INSERT INTO local_metadata (key, value, updated_at) VALUES ('probe', '1', 'now')",
            "CREATE TABLE probe (id INTEGER)",
            "CREATE TEMP TABLE probe (id INTEGER)",
            "PRAGMA user_version = 7",
        ] {
            assert!(
                conn.execute_batch(write).is_err(),
                "the read-only connection accepted {write:?}"
            );
        }
    }
}
