use super::*;

mod placeholders;

pub(crate) use placeholders::*;

pub(crate) fn restrict_dir_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn restrict_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub(crate) fn safe_u64_to_i64(value: u64) -> i64 {
    if value > i64::MAX as u64 {
        i64::MAX
    } else {
        value as i64
    }
}

pub(crate) fn rollback(conn: &Connection) {
    if conn.is_autocommit() {
        return;
    }
    if let Err(e) = conn.execute_batch("ROLLBACK") {
        eprintln!("store: ROLLBACK failed: {e}");
    }
}

pub(crate) fn commit_transaction(conn: &Connection) -> Result<()> {
    match conn.execute_batch("COMMIT") {
        Ok(()) => Ok(()),
        Err(error) => {
            rollback(conn);
            Err(error.into())
        }
    }
}

pub(crate) fn begin_immediate_transaction_with_retry(conn: &Connection) -> Result<()> {
    let mut last_busy_error = None;
    for attempt in 0..=SQLITE_BUSY_RETRY_ATTEMPTS {
        match conn.execute_batch("BEGIN IMMEDIATE TRANSACTION") {
            Ok(()) => return Ok(()),
            Err(error)
                if is_sqlite_busy_or_locked(&error) && attempt < SQLITE_BUSY_RETRY_ATTEMPTS =>
            {
                last_busy_error = Some(error);
                std::thread::sleep(SQLITE_BUSY_RETRY_DELAY);
            }
            Err(error) => return Err(error.into()),
        }
    }

    match last_busy_error {
        Some(error) => Err(error.into()),
        None => bail!("failed to begin immediate SQLite transaction"),
    }
}

pub(crate) fn is_sqlite_busy_or_locked(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

pub(crate) fn sync_state_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncState> {
    let last_success_at: String = row.get(2)?;
    let last_event_started_at: Option<String> = row.get(4)?;
    let last_summary_observed_at: Option<String> = row.get(6)?;
    let last_task_verification_updated_at: Option<String> = row.get(8)?;
    let failure_count: i64 = row.get(10)?;
    Ok(SyncState {
        sink: row.get(0)?,
        target: row.get(1)?,
        last_success_at: parse_rfc3339_for_row(&last_success_at, 2)?,
        last_batch_id: row.get(3)?,
        last_event_started_at: parse_optional_rfc3339_for_row(last_event_started_at, 4)?,
        last_event_id: row.get(5)?,
        last_summary_observed_at: parse_optional_rfc3339_for_row(last_summary_observed_at, 6)?,
        last_summary_id: row.get(7)?,
        last_task_verification_updated_at: parse_optional_rfc3339_for_row(
            last_task_verification_updated_at,
            8,
        )?,
        last_task_verification_id: row.get(9)?,
        failure_count: failure_count.max(0) as u64,
        pending_resume_batch_id: row.get(11)?,
    })
}

pub(crate) fn parse_optional_rfc3339_for_row(
    value: Option<String>,
    index: usize,
) -> rusqlite::Result<Option<DateTime<Utc>>> {
    value
        .as_deref()
        .map(|value| parse_rfc3339_for_row(value, index))
        .transpose()
}

pub(crate) fn parse_rfc3339_for_row(value: &str, index: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}
