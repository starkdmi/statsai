//! Per-conversation account evidence from the Claude desktop app.
//!
//! The desktop app runs Claude Code under its own sign-in and writes the same
//! `~/.claude/projects` transcripts as the CLI, so the cached CLI profile in
//! `~/.claude.json` says nothing about who ran a desktop session. The app does
//! keep one record per session under the account that ran it:
//! `claude-code-sessions/<accountUuid>/<organizationUuid>/local_<id>.json`,
//! whose `cliSessionId` is the transcript's `sessionId`. When the app resumes a
//! conversation in a fresh CLI session, the earlier transcripts move to
//! `priorCliSessionIds`, and they belong to the same account.
//!
//! A record can also be copied into another account's directory so the chat
//! stays visible after a switch, and either copy can be continued later. Each
//! record holds the whole session up to its own `lastActivityAt`, so the
//! session splits into stretches at those instants: a stretch only one record
//! reaches belongs to that account, and the next stretch takes over once the
//! previous one's trailing usage has had time to land. A stretch several records reach is copied history; it
//! goes to the account with the latest sign-in evidence before it ended, and is
//! marked as inferred so a binding recorded before the copy existed outranks it.
//! File creation times cannot date a copy: the app rewrites records in place,
//! so a record's creation time is its last write. An account directory's
//! creation time does date that account's first sign-in.

use super::*;
use crate::{AccountEvidenceScan, ObservedProviderAccount, CLAUDE_CODE_PROVIDER};
use chrono::Duration;
use statsai_core::{
    conversation_account_binding_id_from, hash_text, provider_account_id_from_identity,
    AccountEvidenceKind, Confidence, ConversationAccountBindingV1, SourceLocation,
    CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION, CONVERSATION_ACTIVITY_GRACE_SECONDS,
};
use std::collections::{BTreeMap, BTreeSet};

/// Session records are a few kilobytes; anything far larger is not one.
const MAX_DESKTOP_SESSION_RECORD_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct DesktopSessionRecordFile {
    #[serde(rename = "cliSessionId")]
    cli_session_id: Option<String>,
    /// Transcripts the session ran in before the current one: the app moves a
    /// conversation to a fresh CLI session when it resumes it.
    #[serde(rename = "priorCliSessionIds", default)]
    prior_cli_session_ids: Option<Vec<Value>>,
    #[serde(rename = "createdAt")]
    created_at: Option<Value>,
    #[serde(rename = "lastActivityAt")]
    last_activity_at: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DesktopSessionRecord {
    pub(crate) account_uuid: String,
    /// The app's own session ID: copies of one session share it.
    pub(crate) desktop_session_id: String,
    /// The transcript the session runs in now.
    pub(crate) cli_session_id: String,
    pub(crate) prior_cli_session_ids: Vec<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) last_activity_at: DateTime<Utc>,
}

/// The desktop sessions directory, when `root` is the configuration
/// directory the desktop app writes its transcripts into.
///
/// Only the default `~/.claude` source can hold desktop sessions, so only it
/// reads, and watches, the desktop app's records.
pub(crate) fn claude_desktop_sessions_root_for(root: &Path) -> Option<PathBuf> {
    claude_desktop_sessions_root_in(root, &home_dir()?)
}

pub(crate) fn claude_desktop_sessions_root_in(root: &Path, home: &Path) -> Option<PathBuf> {
    (root == home.join(".claude"))
        .then(|| claude_desktop_sessions_root_from_home(home))
        .flatten()
}

/// Where the desktop app keeps its per-account session records.
pub(crate) fn claude_desktop_sessions_root_from_home(home: &Path) -> Option<PathBuf> {
    let app_support = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
    };
    Some(app_support.join("Claude").join("claude-code-sessions"))
}

/// Everything the desktop app's session store says about who ran what.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DesktopSessions {
    pub(crate) records: Vec<DesktopSessionRecord>,
    /// When each account's directory appeared: its first sign-in on this
    /// machine. Copying records in later cannot move it.
    pub(crate) account_first_seen: BTreeMap<String, DateTime<Utc>>,
    /// Sessions with a record that could not be read. Without it the copies
    /// that were read would look like the only holders, so these sessions
    /// yield no evidence until every copy reads cleanly.
    pub(crate) incomplete_sessions: BTreeSet<String>,
    /// A directory could not be listed, so any session may be missing a copy.
    pub(crate) listing_failed: bool,
}

enum DesktopRecordRead {
    Record(DesktopSessionRecord),
    /// Read cleanly, but names no transcript to bind.
    NoTranscript,
    Unreadable,
}

pub(crate) fn read_claude_desktop_sessions(root: &Path) -> DesktopSessions {
    let mut sessions = DesktopSessions::default();
    if !root.exists() {
        return sessions;
    }
    let Some(account_dirs) = subdirectories(root) else {
        sessions.listing_failed = true;
        return sessions;
    };
    for account_dir in account_dirs {
        let Some(account_uuid) = account_dir
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| normalized_optional_string(Some(name)))
        else {
            continue;
        };
        if let Ok(created) = std::fs::metadata(&account_dir).and_then(|metadata| metadata.created())
        {
            sessions
                .account_first_seen
                .insert(account_uuid.clone(), DateTime::<Utc>::from(created));
        }
        let Some(organization_dirs) = subdirectories(&account_dir) else {
            sessions.listing_failed = true;
            continue;
        };
        for organization_dir in organization_dirs {
            let Some(entries) = std::fs::read_dir(&organization_dir)
                .ok()
                .and_then(|entries| entries.collect::<std::io::Result<Vec<_>>>().ok())
            else {
                sessions.listing_failed = true;
                continue;
            };
            for entry in entries {
                let path = entry.path();
                let Some(desktop_session_id) = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| name.starts_with("local_") && name.ends_with(".json"))
                    .and_then(|_| path.file_stem())
                    .and_then(|stem| stem.to_str())
                    .map(ToOwned::to_owned)
                else {
                    continue;
                };
                match read_desktop_session_record(&path, &account_uuid, &desktop_session_id) {
                    DesktopRecordRead::Record(record) => sessions.records.push(record),
                    DesktopRecordRead::NoTranscript => {}
                    DesktopRecordRead::Unreadable => {
                        sessions.incomplete_sessions.insert(desktop_session_id);
                    }
                }
            }
        }
    }
    sessions
}

/// The directories directly under `path`, or `None` when it cannot be listed in full.
fn subdirectories(path: &Path) -> Option<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in std::fs::read_dir(path).ok()? {
        let entry = entry.ok()?;
        if entry.file_type().ok()?.is_dir() {
            directories.push(entry.path());
        }
    }
    directories.sort();
    Some(directories)
}

/// One record, named by its file: copies of a session keep the file name.
fn read_desktop_session_record(
    path: &Path,
    account_uuid: &str,
    desktop_session_id: &str,
) -> DesktopRecordRead {
    let record = std::fs::metadata(path)
        .ok()
        .filter(|metadata| metadata.len() <= MAX_DESKTOP_SESSION_RECORD_BYTES)
        .and_then(|_| File::open(path).ok())
        .and_then(|file| {
            serde_json::from_reader::<_, DesktopSessionRecordFile>(BufReader::new(file)).ok()
        });
    let Some(record) = record else {
        return DesktopRecordRead::Unreadable;
    };
    let Some(cli_session_id) = normalized_optional_string(record.cli_session_id.as_deref()) else {
        return DesktopRecordRead::NoTranscript;
    };
    let created_at = record.created_at.as_ref().and_then(desktop_timestamp);
    let last_activity_at = record.last_activity_at.as_ref().and_then(desktop_timestamp);
    // Without a time the record cannot be placed against its copies.
    let Some(last_activity_at) = last_activity_at.or(created_at) else {
        return DesktopRecordRead::Unreadable;
    };
    let prior_cli_session_ids = record
        .prior_cli_session_ids
        .unwrap_or_default()
        .iter()
        .filter_map(|value| normalized_optional_string(value.as_str()))
        .filter(|prior| *prior != cli_session_id)
        .collect();
    DesktopRecordRead::Record(DesktopSessionRecord {
        account_uuid: account_uuid.to_string(),
        desktop_session_id: desktop_session_id.to_string(),
        cli_session_id,
        prior_cli_session_ids,
        created_at: created_at.unwrap_or(last_activity_at),
        last_activity_at,
    })
}

fn desktop_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    let millis = value.as_i64()?;
    Utc.timestamp_millis_opt(millis).single()
}

/// One account's stretch of a session: everything up to the end of the
/// account's record that no earlier stretch already covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DesktopSessionTakeover {
    pub(crate) cli_session_id: String,
    pub(crate) account_uuid: String,
    /// `None` for the stretch the session started with.
    pub(crate) active_from: Option<DateTime<Utc>>,
    /// Where the stretch ends: the desktop app saw no activity after this, so
    /// any much later usage was resumed elsewhere.
    pub(crate) observed_until: Option<DateTime<Utc>>,
    /// Where the stretch ends, when only one account's record held it.
    pub(crate) certain_until: Option<DateTime<Utc>>,
    pub(crate) observed_at: DateTime<Utc>,
    /// `High` when only one account's record holds the stretch; `Medium` when
    /// copies held it too and the account signed in at the time was chosen.
    pub(crate) confidence: Confidence,
}

/// `(created_at, last_activity_at)` for one account's copies of one session.
type AccountSpan = (DateTime<Utc>, DateTime<Utc>);

/// One session's stretches in order: where each ends, and every account whose
/// record reaches that far and so could have produced it.
fn session_stretches<'a>(
    accounts: &BTreeMap<&'a str, AccountSpan>,
) -> Vec<(DateTime<Utc>, Vec<&'a str>)> {
    let ends = accounts
        .values()
        .map(|(_, last_activity_at)| *last_activity_at)
        .collect::<BTreeSet<_>>();
    ends.into_iter()
        .map(|end| {
            let holders = accounts
                .iter()
                .filter(|(_, (_, last_activity_at))| *last_activity_at >= end)
                .map(|(account_uuid, _)| *account_uuid)
                .collect();
            (end, holders)
        })
        .collect()
}

/// Decide which account ran each stretch of each session.
///
/// A record holds the whole session up to its `lastActivityAt`, so a stretch
/// held by one record is that account's for certain. A stretch several records
/// hold is history copied between accounts, and it goes to whichever holder
/// shows the latest sign-in evidence at or before the stretch ends: its
/// directory's first appearance, or a stretch only it holds.
pub(crate) fn claude_desktop_session_takeovers(
    sessions: &DesktopSessions,
) -> Vec<DesktopSessionTakeover> {
    // Per session, per account: the earliest creation and the latest activity
    // that account's records show. One account can hold a session in several
    // organization directories.
    if sessions.listing_failed {
        return Vec::new();
    }
    let mut by_session: BTreeMap<&str, BTreeMap<&str, AccountSpan>> = BTreeMap::new();
    for record in sessions.records.iter().filter(|record| {
        !sessions
            .incomplete_sessions
            .contains(&record.desktop_session_id)
    }) {
        let accounts = by_session
            .entry(record.desktop_session_id.as_str())
            .or_default();
        let entry = accounts
            .entry(record.account_uuid.as_str())
            .or_insert((record.created_at, record.last_activity_at));
        entry.0 = entry.0.min(record.created_at);
        entry.1 = entry.1.max(record.last_activity_at);
    }

    let mut signed_in: BTreeMap<&str, BTreeSet<DateTime<Utc>>> = BTreeMap::new();
    for (account_uuid, first_seen) in &sessions.account_first_seen {
        signed_in
            .entry(account_uuid.as_str())
            .or_default()
            .insert(*first_seen);
    }
    for accounts in by_session.values() {
        let mut previous_end = None;
        for (end, holders) in session_stretches(accounts) {
            if let [account_uuid] = holders[..] {
                let evidence = signed_in.entry(account_uuid).or_default();
                evidence.insert(end);
                if previous_end.is_none() {
                    evidence.insert(accounts[account_uuid].0);
                }
            }
            previous_end = Some(end);
        }
    }

    let transcripts = session_transcripts(&sessions.records);
    let activity_grace = Duration::seconds(CONVERSATION_ACTIVITY_GRACE_SECONDS);
    let mut takeovers = Vec::new();
    for (desktop_session_id, accounts) in &by_session {
        let Some(cli_session_ids) = transcripts.get(desktop_session_id) else {
            continue;
        };
        let created_at = accounts
            .values()
            .map(|(created_at, _)| *created_at)
            .min()
            .expect("a session has at least one record");
        let mut previous_end: Option<DateTime<Utc>> = None;
        for (end, holders) in session_stretches(accounts) {
            // The previous account's last reply can be logged a little after
            // its recorded activity, so the next stretch takes over once that
            // grace has passed -- or sooner, once the next account is seen
            // signed in or active, and never after the stretch itself ends.
            // Nor before the next account was first seen at all: usage before
            // then, such as a resume in the CLI, keeps the source's own
            // attribution.
            let takeover_start = |holder: Option<&str>| {
                previous_end.map(|previous_end| {
                    let evidence = holder.and_then(|holder| signed_in.get(holder));
                    let seen_after = evidence.and_then(|evidence| {
                        evidence
                            .range((
                                std::ops::Bound::Excluded(previous_end),
                                std::ops::Bound::Unbounded,
                            ))
                            .next()
                            .copied()
                    });
                    let earliest = [Some(previous_end + activity_grace), seen_after, Some(end)]
                        .into_iter()
                        .flatten()
                        .min()
                        .expect("the grace bound is always present");
                    let first_seen = evidence.and_then(|evidence| evidence.first().copied());
                    first_seen.map_or(earliest, |first_seen| earliest.max(first_seen).min(end))
                })
            };
            let (pieces, confidence, certain_until) = match holders[..] {
                [only] => (
                    vec![(takeover_start(Some(only)), only)],
                    Confidence::High,
                    Some(end),
                ),
                // An empty result means nothing tells the original from its
                // copies: the stretch is left to the source's own attribution.
                _ => (
                    shared_stretch_owners(
                        &holders,
                        &signed_in,
                        created_at,
                        takeover_start(None),
                        end,
                    ),
                    Confidence::Medium,
                    None,
                ),
            };
            previous_end = Some(end);
            // Every stretch is emitted, even when it repeats the previous
            // account: a guessed stretch can be outranked by an earlier certain
            // binding, and the stretch after it must still take over on its own.
            for (active_from, account_uuid) in pieces {
                for cli_session_id in cli_session_ids {
                    takeovers.push(DesktopSessionTakeover {
                        cli_session_id: (*cli_session_id).to_string(),
                        account_uuid: account_uuid.to_string(),
                        active_from,
                        observed_until: Some(end),
                        certain_until,
                        observed_at: active_from.unwrap_or(created_at),
                        confidence: confidence.clone(),
                    });
                }
            }
        }
    }
    takeovers
}

/// Every transcript each desktop session ran in.
///
/// A prior transcript that another session currently runs in is not this
/// session's to claim, so it is left out.
fn session_transcripts(records: &[DesktopSessionRecord]) -> BTreeMap<&str, BTreeSet<&str>> {
    let mut current_owner: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for record in records {
        current_owner
            .entry(record.cli_session_id.as_str())
            .or_default()
            .insert(record.desktop_session_id.as_str());
    }
    let mut transcripts: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for record in records {
        let session = transcripts
            .entry(record.desktop_session_id.as_str())
            .or_default();
        session.insert(record.cli_session_id.as_str());
        for prior in &record.prior_cli_session_ids {
            let claimed_elsewhere = current_owner.get(prior.as_str()).is_some_and(|owners| {
                owners
                    .iter()
                    .any(|owner| *owner != record.desktop_session_id)
            });
            if !claimed_elsewhere {
                session.insert(prior.as_str());
            }
        }
    }
    transcripts
}

/// Who ran a stretch of copied history from `start` to `end`.
///
/// The holder signed in most recently before `end` ran it, but only from when
/// that sign-in began: anything earlier belongs to whoever was signed in then.
fn shared_stretch_owners<'a>(
    holders: &[&'a str],
    signed_in: &BTreeMap<&str, BTreeSet<DateTime<Utc>>>,
    session_created_at: DateTime<Utc>,
    start: Option<DateTime<Utc>>,
    end: DateTime<Utc>,
) -> Vec<(Option<DateTime<Utc>>, &'a str)> {
    let Some(owner) = signed_in_at(holders, signed_in, end) else {
        return Vec::new();
    };
    let others = holders
        .iter()
        .copied()
        .filter(|holder| *holder != owner)
        .collect::<Vec<_>>();
    // The owner's current sign-in began with its first evidence after the last
    // time another holder was seen signed in, not with its first sign-in ever:
    // an account seen again in between ran everything up to then.
    let others_last_seen = others
        .iter()
        .filter_map(|holder| signed_in.get(holder)?.range(..=end).next_back())
        .max()
        .copied();
    let first_seen = signed_in
        .get(owner)
        .and_then(|evidence| match others_last_seen {
            Some(after) => evidence
                .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
                .next(),
            None => evidence.first(),
        })
        .copied()
        .expect("the owner has evidence later than every other holder's");
    // Signed in by the time the stretch began: who held the account before
    // then cannot matter. Before the session existed nothing could run it.
    if first_seen <= start.unwrap_or(session_created_at) {
        return vec![(start, owner)];
    }
    // Every holder stays eligible for the earlier part: accounts can take
    // turns, and the same one can own several separate stretches.
    let mut owners = shared_stretch_owners(
        holders,
        signed_in,
        session_created_at,
        start,
        first_seen - Duration::milliseconds(1),
    );
    match owners.last() {
        // Nobody is known signed in before the owner's sign-in began, so that
        // earlier history stays unbound and keeps the source's own attribution.
        None => vec![(Some(first_seen), owner)],
        Some((_, previous)) if *previous == owner => owners,
        Some(_) => {
            owners.push((Some(first_seen), owner));
            owners
        }
    }
}

/// The holder with the latest sign-in evidence at or before `at`, if exactly one has it.
fn signed_in_at<'a>(
    holders: &[&'a str],
    signed_in: &BTreeMap<&str, BTreeSet<DateTime<Utc>>>,
    at: DateTime<Utc>,
) -> Option<&'a str> {
    let mut latest = holders
        .iter()
        .filter_map(|account_uuid| {
            let evidence = signed_in.get(account_uuid)?.range(..=at).next_back()?;
            Some((*evidence, *account_uuid))
        })
        .collect::<Vec<_>>();
    latest.sort();
    match latest.as_slice() {
        [] => None,
        [.., (newest, account_uuid)] => {
            let tied = latest.iter().filter(|(time, _)| time == newest).count();
            (tied == 1).then_some(*account_uuid)
        }
    }
}

/// Bind each desktop session, or each stretch of one, to the account that ran it.
pub(crate) fn collect_claude_desktop_session_evidence(
    source: &SourceLocation,
    desktop_sessions_root: &Path,
    scan: &mut AccountEvidenceScan,
) {
    let sessions = read_claude_desktop_sessions(desktop_sessions_root);
    bind_claude_desktop_sessions(source, &sessions, scan);
}

pub(crate) fn bind_claude_desktop_sessions(
    source: &SourceLocation,
    sessions: &DesktopSessions,
    scan: &mut AccountEvidenceScan,
) {
    let mut latest_by_account: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
    for takeover in claude_desktop_session_takeovers(sessions) {
        let Some(provider_account_id) = provider_account_id_from_identity(
            CLAUDE_CODE_PROVIDER,
            Some(&takeover.account_uuid),
            None,
        ) else {
            continue;
        };
        latest_by_account
            .entry(takeover.account_uuid.clone())
            .and_modify(|latest| *latest = (*latest).max(takeover.observed_at))
            .or_insert(takeover.observed_at);
        let conversation_id_hash = hash_text(&takeover.cli_session_id);
        scan.conversation_bindings
            .push(ConversationAccountBindingV1 {
                schema_version: CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
                binding_id: conversation_account_binding_id_from(
                    &source.source_id,
                    &conversation_id_hash,
                    None,
                    &provider_account_id,
                    takeover.active_from,
                ),
                provider: CLAUDE_CODE_PROVIDER.to_string(),
                source_id: source.source_id.clone(),
                provider_account_id,
                conversation_id_hash,
                turn_id_hash: None,
                active_from: takeover.active_from,
                observed_until: takeover.observed_until,
                certain_until: takeover.certain_until,
                observed_at: takeover.observed_at,
                // Bindings are local-only and nothing reads their kind; this is
                // the app's own snapshot of which account holds the session.
                evidence_kind: AccountEvidenceKind::AuthSnapshot,
                confidence: takeover.confidence,
            });
    }
    for (account_uuid, observed_at) in latest_by_account {
        scan.accounts.push(ObservedProviderAccount {
            provider_user_id: Some(account_uuid),
            email: None,
            plan_name: None,
            observed_at,
        });
    }
}
