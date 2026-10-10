//! Read-only access for processes that must never write the store.

use super::{
    configure_read_cache, enable_sqlite_defensive, migrations::existing_schema_version,
    CacheReportQuery, QuotaQuery, QuotaStatus, SessionFilter, SessionSort, SessionStats, Store,
    SyncState, WeeklyResetAnchor, CURRENT_SCHEMA_VERSION, SQLITE_BUSY_TIMEOUT,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};
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
    #[error("open StatsAI store read-only at {}", .path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: anyhow::Error,
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
/// In WAL mode SQLite coordinates readers through the `-wal` and `-shm` files
/// beside the database. When no other connection has the store open they may
/// not exist, and SQLite creates them for this connection, which needs write
/// access to the directory (never to the database file). In a directory this
/// process cannot write, the open fails with [`ReadOnlyOpenError::Open`]
/// unless those files already exist.
pub struct ReadStore {
    store: Store,
}

impl std::fmt::Debug for ReadStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReadStore")
            .field("path", &self.store.conn.path())
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
        let open_error = |source: anyhow::Error| ReadOnlyOpenError::Open {
            path: path.to_path_buf(),
            source,
        };
        let conn = open_read_only_connection(path).map_err(open_error)?;
        // A store that predates `schema_migrations` has version zero, as
        // `database_schema_version` reports it.
        let found = existing_schema_version(&conn)
            .map_err(open_error)?
            .unwrap_or(0);
        let expected = CURRENT_SCHEMA_VERSION;
        match found.cmp(&expected) {
            Ordering::Less => Err(ReadOnlyOpenError::SchemaTooOld {
                path: path.to_path_buf(),
                found,
                expected,
            }),
            Ordering::Greater => Err(ReadOnlyOpenError::SchemaTooNew {
                path: path.to_path_buf(),
                found,
                expected,
            }),
            Ordering::Equal => Ok(Self {
                store: Store { conn },
            }),
        }
    }

    /// The store's schema version, which is [`CURRENT_SCHEMA_VERSION`]
    /// whenever the open succeeded.
    pub fn schema_version(&self) -> Result<i64> {
        self.store.schema_version()
    }

    /// See [`Store::data_version`]: changes when another connection commits.
    pub fn data_version(&self) -> Result<i64> {
        self.store.data_version()
    }

    /// Runs several reads against one point-in-time view. See
    /// [`Store::with_read_snapshot`].
    pub fn with_read_snapshot<T>(&self, operation: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        self.store.with_read_snapshot(|_| operation(self))
    }

    /// The pricing ruleset the store's costs were last computed with. A reader
    /// never reprices, so this can trail the binary's own ruleset until a
    /// writer such as `statsai scan` next opens the store.
    pub fn applied_pricing_ruleset_version(&self) -> Result<Option<u64>> {
        self.store.applied_pricing_ruleset_version()
    }

    pub fn event_count(&self) -> Result<u64> {
        self.store.event_count()
    }

    pub fn token_total(&self) -> Result<u64> {
        self.store.token_total()
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
        self.store
            .session_rollups_in_period(since, until, filter, sort, limit, offset)
    }

    /// See [`Store::session_stats_in_period`].
    pub fn session_stats_in_period(
        &self,
        since: Option<DateTime<Utc>>,
        until: DateTime<Utc>,
        filter: &SessionFilter,
    ) -> Result<SessionStats> {
        self.store.session_stats_in_period(since, until, filter)
    }

    /// Every daily summary as stored. Unlike
    /// [`Store::all_sync_rollup_summaries`], summaries an older binary built
    /// are returned as they are rather than rebuilt; the next writer that
    /// reads them through [`Store`] rebuilds them.
    pub fn all_sync_rollup_summaries(&self) -> Result<Vec<UsageSummary>> {
        self.store.stored_sync_rollup_summaries()
    }

    /// See [`Store::daily_rollups_between`].
    pub fn daily_rollups_between(
        &self,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<DailyRollup>> {
        self.store.daily_rollups_between(start_date, end_date)
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
        self.store.cache_report(query, timeline_zone)
    }

    /// See [`Store::quota_status`].
    pub fn quota_status(&self, query: &QuotaQuery) -> Result<QuotaStatus> {
        self.store.quota_status(query)
    }

    /// See [`Store::quota_windows`].
    pub fn quota_windows(&self, query: &QuotaQuery) -> Result<Vec<QuotaWindowV1>> {
        self.store.quota_windows(query)
    }

    /// See [`Store::quota_observations`].
    pub fn quota_observations(
        &self,
        query: &QuotaQuery,
        collapse_duplicates: bool,
    ) -> Result<Vec<QuotaObservationRecordV1>> {
        self.store.quota_observations(query, collapse_duplicates)
    }

    /// See [`Store::weekly_reset_anchor`].
    pub fn weekly_reset_anchor(
        &self,
        provider: &str,
        provider_account_id: &ProviderAccountId,
    ) -> Result<Option<WeeklyResetAnchor>> {
        self.store
            .weekly_reset_anchor(provider, provider_account_id)
    }

    pub fn list_sources(&self) -> Result<Vec<SourceLocation>> {
        self.store.list_sources()
    }

    pub fn list_accounts(&self) -> Result<Vec<ProviderAccount>> {
        self.store.list_accounts()
    }

    pub fn list_source_account_assignments(&self) -> Result<Vec<SourceAccountAssignment>> {
        self.store.list_source_account_assignments()
    }

    pub fn list_subscriptions(&self) -> Result<Vec<Subscription>> {
        self.store.list_subscriptions()
    }

    pub fn list_sync_states(&self) -> Result<Vec<SyncState>> {
        self.store.list_sync_states()
    }
}

/// Opens `path` so that nothing this connection does can change the file.
///
/// `SQLITE_OPEN_READONLY` is what the `mode=ro` URI parameter sets; passing the
/// flag avoids escaping the path into a URI. Without `SQLITE_OPEN_CREATE` a
/// missing file is an error rather than a new database.
fn open_read_only_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    enable_sqlite_defensive(&conn)?;
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
