//! Keeps a checkout location's git identity when its working folder can't be read.
//!
//! Adapters read `.git/config` from a session's working folder every time they
//! parse the session. Once that folder is deleted, or the scan isn't allowed to
//! read it, a re-parse yields a path-only project: `repo_remote_hash` is gone and,
//! because the identity key includes the repository, so is the `project_id`. (A
//! deleted checkout nested in another repository resolves to the outer one
//! instead.) A re-import or a stale-record prune then rewrites old usage onto a
//! different project, and the rewritten rows sync.
//!
//! The store usually still knows better. A record the scan is replacing says
//! what that same record held before, and the store holds the repository of the
//! location from other usage, archived conversations and task spans recorded
//! while the folder existed. Records whose folder can't be read take that
//! identity back. A folder that can be read is left alone, since there the
//! adapter's answer reflects the checkout.

use super::*;
use crate::events::EventWriteDirty;
use statsai_core::{project_bucket_key, ProjectInfo, TaskSpan};

/// `local_metadata` key recording that [`Store::repair_project_identities`] ran.
const PROJECT_IDENTITY_REPAIR_KEY: &str = "project_identity_repair.v1";

/// SQLite caps bound parameters, so `IN` lists go out in chunks of this size.
const PATH_HASH_CHUNK: usize = 500;

pub(crate) const EVENT_PROJECT_SQL: &str = "CASE WHEN json_valid(payload) \
     THEN json_extract(payload, '$.project') END";
const EVENT_PATH_HASH_SQL: &str = "CASE WHEN json_valid(payload) \
     THEN json_extract(payload, '$.project.path_hash') END";
const EVENT_PATH_LABEL_SQL: &str = "CASE WHEN json_valid(payload) \
     THEN json_extract(payload, '$.project.path_label') END";
const EVENT_REPO_HASH_SQL: &str = "CASE WHEN json_valid(payload) \
     THEN json_extract(payload, '$.project.repo_remote_hash') END";

/// Repository identities known for checkout locations.
///
/// By location (`path_hash`) it holds only repo-backed projects, and the newest
/// observation wins, so a remote that was renamed resolves to the name it had
/// last. For rows a write is replacing it also remembers, per event id, what
/// that row held: its repository, or that it had none.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocationIdentities {
    by_path_hash: BTreeMap<String, (DateTime<Utc>, ProjectInfo)>,
    /// Each replaced row's repository, as an index into `identities`, or
    /// `None` for a row that named a location but no repository.
    by_event: HashMap<String, Option<usize>>,
    identities: Vec<ProjectInfo>,
    identity_index: HashMap<String, usize>,
}

impl LocationIdentities {
    /// Records what the event's project says about its location.
    pub fn observe(&mut self, event: &UsageEvent) {
        if let Some(project) = event.project.as_ref() {
            self.observe_row(&event.event_id.0, project, event.session.started_at);
        }
    }

    /// Adds everything `other` holds, keeping the newer identity per location.
    pub fn extend(&mut self, other: Self) {
        for (_, (observed_at, project)) in other.by_path_hash {
            self.observe_project(&project, observed_at);
        }
        for (event_id, index) in other.by_event {
            let index = index.map(|index| self.intern(&other.identities[index]));
            self.by_event.insert(event_id, index);
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_path_hash.is_empty() && self.by_event.is_empty()
    }

    fn observe_row(&mut self, event_id: &str, project: &ProjectInfo, observed_at: DateTime<Utc>) {
        if non_empty(project.path_hash.as_deref()).is_none() {
            return;
        }
        let index = repo_backed_location(project).map(|_| self.intern(project));
        self.by_event.insert(event_id.to_string(), index);
        self.observe_project(project, observed_at);
    }

    fn intern(&mut self, project: &ProjectInfo) -> usize {
        if let Some(index) = self.identity_index.get(&project.project_id) {
            return *index;
        }
        self.identities.push(project.clone());
        let index = self.identities.len() - 1;
        self.identity_index
            .insert(project.project_id.clone(), index);
        index
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

    /// What the replaced row with this id held: `Some(Some(_))` its
    /// repository, `Some(None)` no repository, `None` no such row.
    fn replaced_row(&self, event_id: &str) -> Option<Option<&ProjectInfo>> {
        self.by_event
            .get(event_id)
            .map(|index| index.map(|index| &self.identities[index]))
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

/// `project` moved onto the repository `identity` holds for the same
/// location. The checkout's own labels and branch stay as parsed.
fn reattached(project: &ProjectInfo, identity: &ProjectInfo) -> ProjectInfo {
    ProjectInfo {
        project_id: identity.project_id.clone(),
        repo_remote_hash: identity.repo_remote_hash.clone(),
        repo_label: identity.repo_label.clone(),
        ..project.clone()
    }
}

/// Whether `project` already names the repository `identity` does.
fn same_identity(project: &ProjectInfo, identity: &ProjectInfo) -> bool {
    project.project_id == identity.project_id
        && project.repo_remote_hash == identity.repo_remote_hash
}

/// The items of `items` at `indices`, which are in ascending order.
fn select_mut<'a, T>(items: &'a mut [T], indices: &'a [usize]) -> impl Iterator<Item = &'a mut T> {
    let mut wanted = indices.iter().peekable();
    items
        .iter_mut()
        .enumerate()
        .filter_map(move |(index, item)| {
            (wanted.peek() == Some(&&index)).then(|| {
                wanted.next();
                item
            })
        })
}

/// `known` plus the repositories `events` (already restored) name, by location.
fn known_with_events(known: &LocationIdentities, events: &[UsageEvent]) -> LocationIdentities {
    let mut known = known.clone();
    for event in events {
        if let Some(project) = event.project.as_ref() {
            known.observe_project(project, event.session.started_at);
        }
    }
    known
}

/// Memoizes [`working_folder_unreadable`] per label: one scan can carry
/// thousands of records from the same folder.
#[derive(Default)]
struct FolderReadability(HashMap<Option<String>, bool>);

impl FolderReadability {
    fn unreadable(&mut self, path_label: Option<&str>) -> bool {
        *self
            .0
            .entry(path_label.map(ToOwned::to_owned))
            .or_insert_with(|| working_folder_unreadable(path_label))
    }

    /// The location of a path-only `project` whose folder can't be read.
    fn unreadable_path_only<'a>(&mut self, project: &'a ProjectInfo) -> Option<&'a str> {
        let path_hash = path_only_location(project)?;
        self.unreadable(project.path_label.as_deref())
            .then_some(path_hash)
    }
}

impl Store {
    /// Gives events whose working folder can't be read the repository the
    /// store holds for them.
    ///
    /// An event the write is replacing (`replaced`) gets back what its own row
    /// held, repository or none. Other path-only events take the repository of
    /// their location: from `replaced`, then from the store (other usage,
    /// archived conversations, task spans). Returns how many events changed.
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
        let mut restored = 0u64;
        let mut by_location = Vec::new();
        for (index, event) in events.iter_mut().enumerate() {
            let Some(project) = event.project.as_ref() else {
                continue;
            };
            let Some(path_hash) = non_empty(project.path_hash.as_deref()) else {
                continue;
            };
            if !folders.unreadable(project.path_label.as_deref()) {
                continue;
            }
            match replaced.replaced_row(&event.event_id.0) {
                // The same record, rewritten: it keeps what it held, provided
                // the location is still the same.
                Some(Some(identity)) if identity.path_hash.as_deref() == Some(path_hash) => {
                    if !same_identity(project, identity) {
                        let restored_project = reattached(project, identity);
                        event.project = Some(restored_project);
                        restored += 1;
                    }
                }
                // It held no repository, so it stays as parsed.
                Some(None) => {}
                _ => {
                    if path_only_location(project).is_some() {
                        by_location.push(index);
                    }
                }
            }
        }

        let mut targets = select_mut(events, &by_location)
            .filter_map(|event| event.project.as_mut())
            .collect::<Vec<_>>();
        restored += self.restore_locations(&mut targets, replaced, &mut folders)?;
        Ok(restored)
    }

    /// [`Store::restore_project_identities`] for the summaries a write
    /// replaces: path-only ones take the repository of their location.
    ///
    /// # Errors
    ///
    /// Returns an error if the store can't be read.
    pub fn restore_summary_project_identities(
        &self,
        summaries: &mut [UsageSummary],
        events: &[UsageEvent],
        known: &LocationIdentities,
    ) -> Result<u64> {
        let mut folders = FolderReadability::default();
        let known = known_with_events(known, events);
        let mut targets = summaries
            .iter_mut()
            .filter_map(|summary| summary.project.as_mut())
            .collect::<Vec<_>>();
        self.restore_locations(&mut targets, &known, &mut folders)
    }

    /// [`Store::restore_project_identities`] for task spans. A span follows
    /// its linked events among `events` (already restored); one without them
    /// takes the repository of its location. A restored span moves to the
    /// project bucket of its restored project.
    ///
    /// # Errors
    ///
    /// Returns an error if the store can't be read.
    pub fn restore_task_span_project_identities(
        &self,
        spans: &mut [TaskSpan],
        events: &[UsageEvent],
        known: &LocationIdentities,
    ) -> Result<u64> {
        let mut folders = FolderReadability::default();
        let event_projects = events
            .iter()
            .filter_map(|event| Some((event.event_id.0.as_str(), event.project.as_ref()?)))
            .collect::<HashMap<_, _>>();
        let known = known_with_events(known, events);

        let mut restored = 0u64;
        let mut by_location = Vec::new();
        for (index, span) in spans.iter_mut().enumerate() {
            let Some(project) = span.project.as_ref() else {
                continue;
            };
            let Some(path_hash) = folders.unreadable_path_only(project) else {
                continue;
            };
            let linked = span
                .linked_event_ids
                .iter()
                .filter_map(|event_id| event_projects.get(event_id.0.as_str()))
                .find(|linked| linked.path_hash.as_deref() == Some(path_hash));
            match linked {
                Some(linked) if repo_backed_location(linked).is_some() => {
                    let restored_project = reattached(project, linked);
                    span.project = Some(restored_project);
                    span.project_bucket = project_bucket_key(span.project.as_ref());
                    restored += 1;
                }
                // Its events stayed path-only, so the span does too.
                Some(_) => {}
                None => by_location.push(index),
            }
        }

        let mut targets = select_mut(spans, &by_location)
            .filter_map(|span| span.project.as_mut())
            .collect::<Vec<_>>();
        restored += self.restore_locations(&mut targets, &known, &mut folders)?;
        for span in select_mut(spans, &by_location) {
            span.project_bucket = project_bucket_key(span.project.as_ref());
        }
        Ok(restored)
    }

    /// Moves each path-only project in `targets` whose folder can't be read
    /// onto its location's repository: from `known` first, then the store.
    fn restore_locations(
        &self,
        targets: &mut [&mut ProjectInfo],
        known: &LocationIdentities,
        folders: &mut FolderReadability,
    ) -> Result<u64> {
        let mut unresolved = BTreeSet::new();
        let mut candidates = Vec::new();
        for (index, project) in targets.iter().enumerate() {
            let Some(path_hash) = folders.unreadable_path_only(project) else {
                continue;
            };
            if known.get(path_hash).is_none() {
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
            let project = &mut *targets[index];
            let Some(path_hash) = path_only_location(project) else {
                continue;
            };
            let Some(identity) = known.get(path_hash).or_else(|| stored.get(path_hash)) else {
                continue;
            };
            *project = reattached(project, identity);
            restored += 1;
        }
        Ok(restored)
    }

    /// One-time repair for records that already lost their repository.
    ///
    /// Re-attaches stored path-only usage, summaries and task spans, at
    /// locations whose folder can't be read, to the repository the store holds
    /// for the same location. It rebuilds the affected rollups and work items,
    /// and marks them for sync, so the corrected records sync again. Runs once
    /// per store; later calls only read the marker. Running the repair itself
    /// again changes nothing, because repaired records are no longer path-only.
    ///
    /// # Errors
    ///
    /// Returns an error and rolls the repair back if any write fails.
    pub fn repair_project_identities(&self) -> Result<u64> {
        if self.metadata_value(PROJECT_IDENTITY_REPAIR_KEY)?.is_some() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| {
            let repaired = self.reattach_path_only_records()?;
            self.set_metadata_value(PROJECT_IDENTITY_REPAIR_KEY, &Utc::now().to_rfc3339())?;
            Ok(repaired)
        })
    }

    fn reattach_path_only_records(&self) -> Result<u64> {
        let mut folders = FolderReadability::default();
        let mut unreadable = BTreeSet::new();
        for table in ["usage_events", "usage_summaries", "task_spans"] {
            let mut statement = self.conn.prepare(&format!(
                "SELECT DISTINCT {EVENT_PATH_HASH_SQL}, {EVENT_PATH_LABEL_SQL}
                 FROM {table}
                 WHERE COALESCE({EVENT_PATH_HASH_SQL}, '') <> ''
                   AND COALESCE({EVENT_REPO_HASH_SQL}, '') = ''"
            ))?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            for row in rows {
                let (path_hash, path_label) = row?;
                if folders.unreadable(path_label.as_deref()) {
                    unreadable.insert(path_hash);
                }
            }
        }
        if unreadable.is_empty() {
            return Ok(0);
        }
        let identities =
            self.stored_location_identities(&unreadable.into_iter().collect::<Vec<_>>())?;
        if identities.is_empty() {
            return Ok(0);
        }
        let known = identities.by_path_hash.keys().cloned().collect::<Vec<_>>();
        let identity_for = |project: &ProjectInfo, folders: &mut FolderReadability| {
            folders
                .unreadable_path_only(project)
                .and_then(|path_hash| identities.get(path_hash))
                .map(|identity| reattached(project, identity))
        };

        let mut repaired = 0u64;
        let mut dirty = EventWriteDirty::default();
        for payload in self.path_only_payloads("usage_events", &known)? {
            let Ok(mut event) = serde_json::from_str::<UsageEvent>(&payload) else {
                continue;
            };
            let Some(project) = event.project.as_ref() else {
                continue;
            };
            let Some(restored_project) = identity_for(project, &mut folders) else {
                continue;
            };
            event.project = Some(restored_project);
            dirty.extend(self.update_event_payload(&event)?);
            repaired += 1;
        }
        self.refresh_event_rollups(&dirty)?;

        let mut summaries = Vec::new();
        for payload in self.path_only_payloads("usage_summaries", &known)? {
            let Ok(mut summary) = serde_json::from_str::<UsageSummary>(&payload) else {
                continue;
            };
            let Some(project) = summary.project.as_ref() else {
                continue;
            };
            let Some(restored_project) = identity_for(project, &mut folders) else {
                continue;
            };
            summary.project = Some(restored_project);
            summaries.push(summary);
        }
        repaired += summaries.len() as u64;
        self.upsert_summaries(&summaries)?;

        let mut spans = Vec::new();
        let mut buckets = BTreeSet::new();
        for payload in self.path_only_payloads("task_spans", &known)? {
            let Ok(mut span) = serde_json::from_str::<TaskSpan>(&payload) else {
                continue;
            };
            let Some(project) = span.project.as_ref() else {
                continue;
            };
            let Some(restored_project) = identity_for(project, &mut folders) else {
                continue;
            };
            buckets.insert(span.project_bucket.clone());
            span.project = Some(restored_project);
            span.project_bucket = project_bucket_key(span.project.as_ref());
            buckets.insert(span.project_bucket.clone());
            spans.push(span);
        }
        repaired += spans.len() as u64;
        if !spans.is_empty() {
            self.upsert_task_spans_in_tx(&spans)?;
            self.rebuild_task_work_items_for_project_buckets(&buckets)?;
        }
        Ok(repaired)
    }

    /// Payloads of `table`'s path-only rows at these locations.
    fn path_only_payloads(&self, table: &str, path_hashes: &[String]) -> Result<Vec<String>> {
        let mut payloads = Vec::new();
        for chunk in path_hashes.chunks(PATH_HASH_CHUNK) {
            let mut statement = self.conn.prepare(&format!(
                "SELECT payload FROM {table}
                 WHERE COALESCE({EVENT_REPO_HASH_SQL}, '') = ''
                   AND {EVENT_PATH_HASH_SQL} IN ({})",
                sqlite_in_clause_placeholders(chunk.len())
            ))?;
            let rows = statement.query_map(
                rusqlite::params_from_iter(sqlite_string_params(chunk)),
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                payloads.push(row?);
            }
        }
        Ok(payloads)
    }

    /// Records, into `identities`, what a usage row about to be deleted held.
    pub(crate) fn observe_replaced_row(
        identities: &mut LocationIdentities,
        event_id: &str,
        project_json: Option<&str>,
        started_at: &str,
    ) {
        let Some(json) = project_json else {
            return;
        };
        let (Ok(project), Ok(started_at)) = (
            serde_json::from_str::<ProjectInfo>(json),
            DateTime::parse_from_rfc3339(started_at),
        ) else {
            return;
        };
        identities.observe_row(event_id, &project, started_at.with_timezone(&Utc));
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
