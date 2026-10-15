//! Keeps a checkout location's git identity when its working folder can't be read.
//!
//! Adapters read `.git/config` from a session's working folder every time they
//! parse the session. Once that folder is deleted, or the scan isn't allowed to
//! read it, a re-parse yields a path-only project: `repo_remote_hash` is gone and,
//! because the identity key includes the repository, so is the `project_id`. A
//! re-import or a stale-record prune then rewrites old usage onto a different
//! project, and the rewritten rows sync.
//!
//! The store usually still knows the repository at that location: from the rows a
//! scan is replacing, from other usage at the same `path_hash`, or from archived
//! conversations and task spans recorded while the folder existed. A path-only
//! project whose folder can't be read takes that identity back. A folder that can
//! be read is left alone, since there the adapter's answer reflects the checkout.

use super::*;
use crate::events::EventWriteDirty;
use statsai_core::ProjectInfo;

/// `local_metadata` key recording that [`Store::repair_project_identities`] ran.
const PROJECT_IDENTITY_REPAIR_KEY: &str = "project_identity_repair.v1";

/// SQLite caps bound parameters, so `IN` lists go out in chunks of this size.
const PATH_HASH_CHUNK: usize = 500;

const EVENT_PROJECT_SQL: &str = "CASE WHEN json_valid(payload) \
     THEN json_extract(payload, '$.project') END";
const EVENT_PATH_HASH_SQL: &str = "CASE WHEN json_valid(payload) \
     THEN json_extract(payload, '$.project.path_hash') END";
const EVENT_PATH_LABEL_SQL: &str = "CASE WHEN json_valid(payload) \
     THEN json_extract(payload, '$.project.path_label') END";
const EVENT_REPO_HASH_SQL: &str = "CASE WHEN json_valid(payload) \
     THEN json_extract(payload, '$.project.repo_remote_hash') END";

/// Repository identities known for checkout locations, keyed by `path_hash`.
///
/// Holds only repo-backed projects. For each location the newest observation
/// wins, so a remote that was renamed resolves to the name it had last.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocationIdentities {
    by_path_hash: BTreeMap<String, (DateTime<Utc>, ProjectInfo)>,
}

impl LocationIdentities {
    /// Records the event's project if it names both a location and a repository.
    pub fn observe(&mut self, event: &UsageEvent) {
        if let Some(project) = event.project.as_ref() {
            self.observe_project(project, event.session.started_at);
        }
    }

    /// Adds every identity `other` holds, keeping the newer one per location.
    pub fn extend(&mut self, other: Self) {
        for (_, (observed_at, project)) in other.by_path_hash {
            self.observe_project(&project, observed_at);
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_path_hash.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_path_hash.len()
    }

    fn observe_project(&mut self, project: &ProjectInfo, observed_at: DateTime<Utc>) {
        let Some(path_hash) = repo_backed_location(project) else {
            return;
        };
        match self.by_path_hash.get(path_hash) {
            Some((existing_at, _)) if *existing_at >= observed_at => {}
            _ => {
                self.by_path_hash
                    .insert(path_hash.to_string(), (observed_at, project.clone()));
            }
        }
    }

    /// Keeps an identity already recorded for the location; used when the
    /// caller reads sources in priority order, newest first.
    fn observe_if_absent(&mut self, project: ProjectInfo) {
        let Some(path_hash) = repo_backed_location(&project).map(ToOwned::to_owned) else {
            return;
        };
        self.by_path_hash
            .entry(path_hash)
            .or_insert((DateTime::<Utc>::MIN_UTC, project));
    }

    fn get(&self, path_hash: &str) -> Option<&ProjectInfo> {
        self.by_path_hash.get(path_hash).map(|(_, project)| project)
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

/// The location of a project that names both a checkout and a repository.
fn repo_backed_location(project: &ProjectInfo) -> Option<&str> {
    non_empty(project.repo_remote_hash.as_deref())?;
    non_empty(project.path_hash.as_deref())
}

/// The location of a project that names a checkout but no repository.
fn path_only_location(project: &ProjectInfo) -> Option<&str> {
    if non_empty(project.repo_remote_hash.as_deref()).is_some() {
        return None;
    }
    non_empty(project.path_hash.as_deref())
}

/// Whether the scan could not have read git metadata from this working folder:
/// it no longer exists, or listing it fails (for example, a privacy setting
/// denies access). A project without a folder label can't be checked, so it is
/// treated as unreadable.
fn working_folder_unreadable(path_label: Option<&str>) -> bool {
    match non_empty(path_label) {
        Some(path) => std::fs::read_dir(path).is_err(),
        None => true,
    }
}

/// The path-only `project` moved onto the repository `identity` holds for the
/// same location. The checkout's own labels and branch stay as parsed.
fn reattached(project: &ProjectInfo, identity: &ProjectInfo) -> ProjectInfo {
    ProjectInfo {
        project_id: identity.project_id.clone(),
        repo_remote_hash: identity.repo_remote_hash.clone(),
        repo_label: identity.repo_label.clone(),
        ..project.clone()
    }
}

/// Memoizes [`working_folder_unreadable`] per label: one scan can carry
/// thousands of events from the same folder.
#[derive(Default)]
struct FolderReadability(HashMap<Option<String>, bool>);

impl FolderReadability {
    fn unreadable(&mut self, path_label: Option<&str>) -> bool {
        *self
            .0
            .entry(path_label.map(ToOwned::to_owned))
            .or_insert_with(|| working_folder_unreadable(path_label))
    }
}

impl Store {
    /// Moves path-only projects whose working folder can't be read back onto the
    /// repository the store holds for that location.
    ///
    /// Identities come from `replaced` first (the rows this write is replacing),
    /// then from the store: other usage at the same location, archived
    /// conversations, then task spans. Returns how many events changed.
    ///
    /// # Errors
    ///
    /// Returns an error if the store can't be read.
    pub fn restore_project_identities(
        &self,
        events: &mut [UsageEvent],
        replaced: &LocationIdentities,
    ) -> Result<u64> {
        let mut folders = FolderReadability::default();
        let mut candidates = Vec::new();
        let mut unresolved = BTreeSet::new();
        for (index, event) in events.iter().enumerate() {
            let Some(project) = event.project.as_ref() else {
                continue;
            };
            let Some(path_hash) = path_only_location(project) else {
                continue;
            };
            if !folders.unreadable(project.path_label.as_deref()) {
                continue;
            }
            if replaced.get(path_hash).is_none() {
                unresolved.insert(path_hash.to_string());
            }
            candidates.push(index);
        }
        if candidates.is_empty() {
            return Ok(0);
        }

        let unresolved = unresolved.into_iter().collect::<Vec<_>>();
        let stored = if unresolved.is_empty() {
            LocationIdentities::default()
        } else {
            self.stored_location_identities(&unresolved)?
        };

        let mut restored = 0u64;
        for index in candidates {
            let event = &mut events[index];
            let Some(project) = event.project.as_ref() else {
                continue;
            };
            let Some(path_hash) = path_only_location(project) else {
                continue;
            };
            let Some(identity) = replaced.get(path_hash).or_else(|| stored.get(path_hash)) else {
                continue;
            };
            let restored_project = reattached(project, identity);
            event.project = Some(restored_project);
            restored += 1;
        }
        Ok(restored)
    }

    /// One-time repair for usage that already lost its repository.
    ///
    /// Re-attaches stored path-only usage, at locations whose folder can't be
    /// read, to the repository the store holds for the same location, and
    /// rebuilds the affected rollups so the corrected rows sync again. Runs once
    /// per store; later calls only read the marker. Running the repair itself
    /// again changes nothing, because repaired rows are no longer path-only.
    ///
    /// # Errors
    ///
    /// Returns an error and rolls the repair back if any write fails.
    pub fn repair_project_identities(&self) -> Result<u64> {
        if self.metadata_value(PROJECT_IDENTITY_REPAIR_KEY)?.is_some() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| {
            let repaired = self.reattach_path_only_events()?;
            self.set_metadata_value(PROJECT_IDENTITY_REPAIR_KEY, &Utc::now().to_rfc3339())?;
            Ok(repaired)
        })
    }

    fn reattach_path_only_events(&self) -> Result<u64> {
        let locations = {
            let mut statement = self.conn.prepare(&format!(
                "SELECT DISTINCT {EVENT_PATH_HASH_SQL}, {EVENT_PATH_LABEL_SQL}
                 FROM usage_events
                 WHERE COALESCE({EVENT_PATH_HASH_SQL}, '') <> ''
                   AND COALESCE({EVENT_REPO_HASH_SQL}, '') = ''"
            ))?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let mut folders = FolderReadability::default();
        let unreadable = locations
            .into_iter()
            .filter(|(_, path_label)| folders.unreadable(path_label.as_deref()))
            .map(|(path_hash, _)| path_hash)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if unreadable.is_empty() {
            return Ok(0);
        }

        let identities = self.stored_location_identities(&unreadable)?;
        if identities.is_empty() {
            return Ok(0);
        }
        let known = identities.by_path_hash.keys().cloned().collect::<Vec<_>>();

        let mut repaired = 0u64;
        let mut dirty = EventWriteDirty::default();
        for chunk in known.chunks(PATH_HASH_CHUNK) {
            let payloads = {
                let mut statement = self.conn.prepare(&format!(
                    "SELECT payload FROM usage_events
                     WHERE COALESCE({EVENT_REPO_HASH_SQL}, '') = ''
                       AND {EVENT_PATH_HASH_SQL} IN ({})",
                    sqlite_in_clause_placeholders(chunk.len())
                ))?;
                let rows = statement.query_map(
                    rusqlite::params_from_iter(sqlite_string_params(chunk)),
                    |row| row.get::<_, String>(0),
                )?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for payload in payloads {
                let Ok(mut event) = serde_json::from_str::<UsageEvent>(&payload) else {
                    continue;
                };
                let Some(project) = event.project.as_ref() else {
                    continue;
                };
                if !folders.unreadable(project.path_label.as_deref()) {
                    continue;
                }
                let Some(identity) = path_only_location(project).and_then(|ph| identities.get(ph))
                else {
                    continue;
                };
                let restored_project = reattached(project, identity);
                event.project = Some(restored_project);
                dirty.extend(self.update_event_payload(&event)?);
                repaired += 1;
            }
        }
        self.refresh_event_rollups(&dirty)?;
        Ok(repaired)
    }

    /// Repository identities this source's usage holds, by location.
    pub(crate) fn repo_identities_for_source(
        &self,
        source_id: &SourceId,
    ) -> Result<LocationIdentities> {
        let mut identities = LocationIdentities::default();
        let mut statement = self.conn.prepare(&format!(
            "SELECT {EVENT_PROJECT_SQL}, started_at FROM usage_events
             WHERE source_id = ?1 AND COALESCE({EVENT_REPO_HASH_SQL}, '') <> ''"
        ))?;
        let rows = statement.query_map(params![&source_id.0], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (Some(json), started_at) = row? else {
                continue;
            };
            let (Ok(project), Ok(started_at)) = (
                serde_json::from_str::<ProjectInfo>(&json),
                DateTime::parse_from_rfc3339(&started_at),
            ) else {
                continue;
            };
            identities.observe_project(&project, started_at.with_timezone(&Utc));
        }
        Ok(identities)
    }

    /// Repository identities the store holds for these locations, read in
    /// priority order: usage rows, archived conversations, task spans, then the
    /// legacy `tasks` table when an older store still has it.
    pub(crate) fn stored_location_identities(
        &self,
        path_hashes: &[String],
    ) -> Result<LocationIdentities> {
        let mut identities = LocationIdentities::default();
        let has_legacy_tasks = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tasks')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        let mut sources = vec![
            format!(
                "SELECT {EVENT_PROJECT_SQL} FROM usage_events
                 WHERE COALESCE({EVENT_REPO_HASH_SQL}, '') <> ''
                   AND {EVENT_PATH_HASH_SQL} IN ({{placeholders}})
                 ORDER BY started_at DESC, event_id DESC"
            ),
            "SELECT project_json FROM archive_conversations
             WHERE CASE WHEN json_valid(project_json)
                   THEN COALESCE(json_extract(project_json, '$.repo_remote_hash'), '') <> ''
                    AND json_extract(project_json, '$.path_hash') IN ({placeholders}) END
             ORDER BY COALESCE(updated_at, started_at, imported_at) DESC, conversation_id DESC"
                .to_string(),
            format!(
                "SELECT {EVENT_PROJECT_SQL} FROM task_spans
                 WHERE COALESCE({EVENT_REPO_HASH_SQL}, '') <> ''
                   AND {EVENT_PATH_HASH_SQL} IN ({{placeholders}})
                 ORDER BY started_at DESC, span_id DESC"
            ),
        ];
        if has_legacy_tasks {
            sources.push(format!(
                "SELECT {EVENT_PROJECT_SQL} FROM tasks
                 WHERE COALESCE({EVENT_REPO_HASH_SQL}, '') <> ''
                   AND {EVENT_PATH_HASH_SQL} IN ({{placeholders}})"
            ));
        }

        for sql in sources {
            for chunk in path_hashes.chunks(PATH_HASH_CHUNK) {
                let sql = sql.replace(
                    "{placeholders}",
                    &sqlite_in_clause_placeholders(chunk.len()),
                );
                let mut statement = self.conn.prepare(&sql)?;
                let rows = statement.query_map(
                    rusqlite::params_from_iter(sqlite_string_params(chunk)),
                    |row| row.get::<_, Option<String>>(0),
                )?;
                for row in rows {
                    let Some(json) = row? else {
                        continue;
                    };
                    if let Ok(project) = serde_json::from_str::<ProjectInfo>(&json) {
                        identities.observe_if_absent(project);
                    }
                }
            }
        }
        Ok(identities)
    }
}
