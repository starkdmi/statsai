//! Readable model names for Antigravity's numeric model ids.
//!
//! Steps record only a numeric model id. `gen_metadata` holds two names for
//! it: the name the request was served under (`1.19`), on almost every
//! request, and the label the user selected (`3.28`, paired with the requested
//! id in `3.1`), which a conversation keeps for its latest model choice only.
//! A conversation that switched models therefore names its earlier model
//! nowhere, so labels are learned across every conversation in the store.

use super::proto::{Message, WireValue};
use crate::{file_metadata_signature, open_sqlite_readonly, sqlite_table_exists};
use anyhow::Result;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use statsai_core::{canonical_display, hash_text, ModelInfo, ReasoningLevel};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Names seen for each model id, with how often each was seen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelNames {
    labels: BTreeMap<u64, BTreeMap<String, u64>>,
    served: BTreeMap<u64, BTreeMap<String, u64>>,
}

impl ModelNames {
    pub(crate) fn add_label(&mut self, model_id: u64, label: &str) {
        add_name(&mut self.labels, model_id, label);
    }

    pub(crate) fn add_served(&mut self, model_id: u64, served: &str) {
        add_name(&mut self.served, model_id, served);
    }

    pub(crate) fn merge(&mut self, other: &ModelNames) {
        for (model_id, names) in &other.labels {
            for (name, count) in names {
                *self
                    .labels
                    .entry(*model_id)
                    .or_default()
                    .entry(name.clone())
                    .or_default() += count;
            }
        }
        for (model_id, names) in &other.served {
            for (name, count) in names {
                *self
                    .served
                    .entry(*model_id)
                    .or_default()
                    .entry(name.clone())
                    .or_default() += count;
            }
        }
    }

    pub(crate) fn label(&self, model_id: u64) -> Option<&str> {
        most_common(self.labels.get(&model_id)?)
    }

    pub(crate) fn served(&self, model_id: u64) -> Option<&str> {
        most_common(self.served.get(&model_id)?)
    }

    /// Whether these names fully describe `model_id` without the rest of the
    /// store: a label for `name` and a served name for `provider_model_id`.
    pub(crate) fn resolves(&self, model_id: u64) -> bool {
        self.label(model_id).is_some() && self.served(model_id).is_some()
    }
}

fn add_name(names: &mut BTreeMap<u64, BTreeMap<String, u64>>, model_id: u64, name: &str) {
    if model_id == 0 || !is_model_name(name) {
        return;
    }
    *names
        .entry(model_id)
        .or_default()
        .entry(name.to_string())
        .or_default() += 1;
}

/// The most frequent name, the alphabetically first on a tie, so the choice
/// does not depend on the order files were read in. Observed stores never
/// give one id two names; this only keeps a conflict deterministic.
fn most_common(names: &BTreeMap<String, u64>) -> Option<&str> {
    names
        .iter()
        .max_by(|(left_name, left), (right_name, right)| {
            left.cmp(right).then_with(|| right_name.cmp(left_name))
        })
        .map(|(name, _)| name.as_str())
}

/// Model names are short identifiers. Anything else in these fields is not a
/// name to show or to price by.
fn is_model_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:/+-".contains(&byte))
}

/// Reads every name a conversation's `gen_metadata` holds.
///
/// Each row embeds the full prompt, around 100 KB. Only the request (`1`) and
/// the request settings (`3`) are opened, one level deep, and every other field
/// is stepped over by its length.
pub(crate) fn read_model_names(connection: &Connection) -> Result<ModelNames> {
    let mut names = ModelNames::default();
    if !sqlite_table_exists(connection, "gen_metadata")? {
        return Ok(names);
    }
    let mut statement = connection.prepare("SELECT data FROM gen_metadata")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let ValueRef::Blob(data) = row.get_ref(0)? else {
            continue;
        };
        add_gen_metadata_names(&mut names, data);
    }
    Ok(names)
}

pub(crate) fn add_gen_metadata_names(names: &mut ModelNames, data: &[u8]) {
    let Some(row) = Message::parse(data) else {
        return;
    };
    for (field, value) in row.fields() {
        let WireValue::Bytes(bytes) = value else {
            continue;
        };
        match field {
            1 => {
                let Some(request) = Message::parse(bytes) else {
                    continue;
                };
                if let (Some(model_id), Some(served)) = (request.varint(3), request.string(19)) {
                    names.add_served(model_id, served);
                }
            }
            3 => {
                let Some(settings) = Message::parse(bytes) else {
                    continue;
                };
                // The label goes with the model the settings requested; the
                // request's own model id stands in when that is missing.
                let model_id = settings
                    .varint(1)
                    .or_else(|| row.messages(1).find_map(|request| request.varint(3)));
                if let (Some(model_id), Some(label)) = (model_id, settings.string(28)) {
                    names.add_label(model_id, label);
                }
            }
            _ => {}
        }
    }
}

/// The name, served name, and effort a model id resolves to, looking through
/// `layers` in order: the conversation's own names first, then the store's.
///
/// `name` is the selected label, then the served name, then a stable
/// `antigravity-model-<id>`, which the returned flag marks. `provider_model_id`
/// is the served name.
pub(crate) fn resolve_model_info(model_id: u64, layers: &[&ModelNames]) -> (ModelInfo, bool) {
    let label = layers.iter().find_map(|names| names.label(model_id));
    let served = layers
        .iter()
        .find_map(|names| names.served(model_id))
        .map(ToOwned::to_owned);
    let (name, synthetic) = match label.or(served.as_deref()) {
        Some(name) => (name.to_string(), false),
        None => (format!("antigravity-model-{model_id}"), true),
    };
    let mut model = crate::model_info(&name);
    model.provider_model_id = served;
    if let Some(effort) = label_effort(&name) {
        model.reasoning_level = ReasoningLevel::parse(effort);
        model.reasoning_level_raw = Some(effort.to_string());
    }
    (model, synthetic)
}

/// The effort a selected label ends in, such as `low` in
/// `claude-sonnet-5-5-low` or `extra-low` in `gemini-3.5-flash-extra-low`.
pub(crate) fn label_effort(label: &str) -> Option<&str> {
    [
        "extra-low",
        "extra-high",
        "low",
        "medium",
        "high",
        "xhigh",
        "max",
    ]
    .into_iter()
    .find(|effort| {
        label
            .strip_suffix(effort)
            .is_some_and(|rest| rest.ends_with('-') && rest.len() > 1)
    })
}

/// Names learned from each conversation file, kept for the life of the
/// process and reused while a file is unchanged.
///
/// Reading a conversation's names means reading its whole `gen_metadata`. A
/// scan needs the names of every conversation whenever one it reads leaves a
/// model id unnamed, and a watcher scans often, so each file is read once per
/// change rather than once per scan.
fn names_memo() -> &'static Mutex<NamesMemo> {
    static MEMO: OnceLock<Mutex<NamesMemo>> = OnceLock::new();
    MEMO.get_or_init(Default::default)
}

/// Each conversation file's names, keyed by its path, with the signature the
/// file had when they were read.
type NamesMemo = HashMap<String, (String, Arc<ModelNames>)>;

/// What a conversation file holds right now: the database and the journals
/// whose pages may not have reached it yet.
pub(crate) fn conversation_file_signature(db_path: &Path) -> String {
    let path = db_path.to_string_lossy();
    let signatures = ["", "-wal", "-journal"]
        .into_iter()
        .map(|suffix| file_metadata_signature(Path::new(&format!("{path}{suffix}"))))
        .collect::<Vec<_>>();
    hash_text(&signatures.join(":"))
}

pub(crate) fn remember_file_names(db_path: &Path, signature: String, names: Arc<ModelNames>) {
    if let Ok(mut memo) = names_memo().lock() {
        memo.insert(canonical_display(db_path), (signature, names));
    }
}

fn file_names(db_path: &Path) -> Arc<ModelNames> {
    let key = canonical_display(db_path);
    let signature = conversation_file_signature(db_path);
    if let Ok(memo) = names_memo().lock() {
        if let Some((known, names)) = memo.get(&key) {
            if *known == signature {
                return names.clone();
            }
        }
    }
    // A file that cannot be read contributes no names; reading its usage
    // reports the failure.
    let names = Arc::new(
        open_sqlite_readonly(db_path)
            .and_then(|connection| read_model_names(&connection))
            .unwrap_or_default(),
    );
    remember_file_names(db_path, signature, names.clone());
    names
}

/// The names of every conversation in a store, built the first time a scan
/// asks for them.
pub(crate) struct StoreModelNames {
    db_paths: Vec<PathBuf>,
    names: Option<ModelNames>,
}

impl StoreModelNames {
    pub(crate) fn new(db_paths: Vec<PathBuf>) -> Self {
        Self {
            db_paths,
            names: None,
        }
    }

    pub(crate) fn get(&mut self) -> &ModelNames {
        self.names.get_or_insert_with(|| {
            let mut names = ModelNames::default();
            for path in &self.db_paths {
                names.merge(&file_names(path));
            }
            names
        })
    }
}
