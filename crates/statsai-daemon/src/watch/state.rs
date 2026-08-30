use notify::{RecursiveMode, Watcher};
use statsai_adapters::{adapter_for_provider, ProviderAdapter};
use statsai_core::{SourceId, SourceKind, SourceLocation};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WatchScope {
    Direct,
    Recursive,
}

impl WatchScope {
    fn notify_mode(self) -> RecursiveMode {
        match self {
            Self::Direct => RecursiveMode::NonRecursive,
            Self::Recursive => RecursiveMode::Recursive,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct VerificationDependencySnapshot {
    pub(super) paths_by_source: HashMap<SourceId, Vec<PathBuf>>,
}

impl VerificationDependencySnapshot {
    pub(super) fn paths_for(&self, source: &SourceLocation) -> &[PathBuf] {
        self.paths_by_source
            .get(&source.source_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
struct CachedVerificationDependencies {
    source: SourceLocation,
    paths: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub(super) struct VerificationDependencyCache {
    entries: HashMap<SourceId, CachedVerificationDependencies>,
}

impl VerificationDependencyCache {
    fn paths_for(
        &mut self,
        adapter: &dyn ProviderAdapter,
        source: &SourceLocation,
    ) -> Vec<PathBuf> {
        if let Some(cached) = self
            .entries
            .get(&source.source_id)
            .filter(|cached| verification_dependency_source_matches(&cached.source, source))
        {
            return cached.paths.clone();
        }

        let paths = adapter.verification_dependency_paths(source);
        self.entries.insert(
            source.source_id.clone(),
            CachedVerificationDependencies {
                source: source.clone(),
                paths: paths.clone(),
            },
        );
        paths
    }

    pub(super) fn invalidate_changed(
        &mut self,
        adapters: &[Box<dyn ProviderAdapter>],
        changed: &[PathBuf],
    ) {
        self.entries.retain(|_, cached| {
            let Some(adapter) = adapters
                .iter()
                .find(|adapter| source_matches_adapter(&cached.source, adapter.as_ref()))
            else {
                return false;
            };
            !adapter.verification_dependency_paths_changed(&cached.source, changed)
        });
    }

    fn retain_snapshot(&mut self, snapshot: &VerificationDependencySnapshot) {
        self.entries
            .retain(|source_id, _| snapshot.paths_by_source.contains_key(source_id));
    }
}

#[derive(Debug, Default)]
pub(super) struct WatchPlan {
    pub(super) paths: HashMap<PathBuf, WatchScope>,
    pub(super) verification_dependencies: VerificationDependencySnapshot,
}

pub(super) fn discover_watch_plan(
    configured: &[SourceLocation],
    adapters: &[Box<dyn ProviderAdapter>],
    dependency_cache: &mut VerificationDependencyCache,
) -> WatchPlan {
    let mut plan = WatchPlan::default();
    for adapter in adapters {
        for source in watch_sources_for_adapter(adapter.as_ref(), configured) {
            let dependencies = dependency_cache.paths_for(adapter.as_ref(), &source);
            extend_source_watch_paths(&mut plan.paths, &source, &dependencies);
            plan.verification_dependencies
                .paths_by_source
                .insert(source.source_id.clone(), dependencies);
        }
    }

    for source in configured.iter().filter(|source| {
        source.enabled
            && source.source_kind == SourceKind::LocalAdapter
            && !adapters
                .iter()
                .any(|adapter| source_matches_adapter(source, adapter.as_ref()))
    }) {
        extend_source_root_watch_path(&mut plan.paths, source);
    }

    dependency_cache.retain_snapshot(&plan.verification_dependencies);
    plan
}

pub(super) fn watch_sources_for_adapter(
    adapter: &dyn ProviderAdapter,
    configured: &[SourceLocation],
) -> Vec<SourceLocation> {
    let configured = configured
        .iter()
        .filter(|source| {
            source.source_kind == SourceKind::LocalAdapter
                && source_matches_adapter(source, adapter)
        })
        .collect::<Vec<_>>();
    let mut sources = adapter
        .discover()
        .into_iter()
        .filter(|source| {
            source.enabled
                && source.source_kind == SourceKind::LocalAdapter
                && source.path_label.is_some()
                && !configured
                    .iter()
                    .any(|configured| sources_refer_to_same_location(source, configured))
        })
        .collect::<Vec<_>>();
    for source in configured
        .into_iter()
        .filter(|source| source.enabled)
        .cloned()
    {
        if !sources
            .iter()
            .any(|existing| existing.source_id == source.source_id)
        {
            sources.push(source);
        }
    }
    sources
}

fn source_matches_adapter(source: &SourceLocation, adapter: &dyn ProviderAdapter) -> bool {
    adapter_for_provider(&source.provider)
        .is_some_and(|canonical| canonical.provider() == adapter.provider())
}

fn sources_refer_to_same_location(left: &SourceLocation, right: &SourceLocation) -> bool {
    if left.source_kind != right.source_kind
        || adapter_for_provider(&left.provider)
            .zip(adapter_for_provider(&right.provider))
            .is_none_or(|(left, right)| left.provider() != right.provider())
    {
        return false;
    }
    if left.source_id == right.source_id
        || left
            .path_hash
            .as_deref()
            .zip(right.path_hash.as_deref())
            .is_some_and(|(left, right)| left == right)
    {
        return true;
    }
    left.path_label
        .as_deref()
        .zip(right.path_label.as_deref())
        .is_some_and(|(left, right)| {
            let left = PathBuf::from(left);
            let right = PathBuf::from(right);
            std::fs::canonicalize(&left).unwrap_or(left)
                == std::fs::canonicalize(&right).unwrap_or(right)
        })
}

fn verification_dependency_source_matches(left: &SourceLocation, right: &SourceLocation) -> bool {
    left.provider == right.provider
        && left.source_kind == right.source_kind
        && left.location_origin == right.location_origin
        && left.path_hash == right.path_hash
        && left.path_label == right.path_label
}

fn extend_source_root_watch_path(
    paths: &mut HashMap<PathBuf, WatchScope>,
    source: &SourceLocation,
) {
    if let Some(label) = source.path_label.as_deref().filter(|path| !path.is_empty()) {
        let path = PathBuf::from(label);
        if path.is_dir() {
            insert_watch_path(paths, path, WatchScope::Recursive);
        }
    }
}

pub(super) fn extend_source_watch_paths(
    paths: &mut HashMap<PathBuf, WatchScope>,
    source: &SourceLocation,
    dependencies: &[PathBuf],
) {
    extend_source_root_watch_path(paths, source);
    for dependency in dependencies {
        if dependency.is_dir() {
            insert_watch_path(paths, dependency.clone(), WatchScope::Recursive);
            continue;
        }
        if let Some(parent) = dependency
            .ancestors()
            .skip(1)
            .find(|ancestor| ancestor.is_dir())
        {
            insert_watch_path(paths, parent.to_path_buf(), WatchScope::Direct);
        }
    }
}

fn insert_watch_path(paths: &mut HashMap<PathBuf, WatchScope>, path: PathBuf, scope: WatchScope) {
    paths
        .entry(path)
        .and_modify(|current| {
            if matches!(scope, WatchScope::Recursive) {
                *current = WatchScope::Recursive;
            }
        })
        .or_insert(scope);
}

pub(super) fn reconcile_watch_sources<W: Watcher>(
    watcher: &mut W,
    watched: &mut HashMap<PathBuf, WatchScope>,
    uncertain: &mut HashSet<PathBuf>,
    desired: HashMap<PathBuf, WatchScope>,
) -> Vec<PathBuf> {
    let mut removed = watched
        .iter()
        .filter(|(path, scope)| desired.get(*path) != Some(*scope))
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    removed.sort();
    for path in removed {
        if let Err(error) = watcher.unwatch(&path) {
            eprintln!("daemon: cannot stop watching {}: {error}", path.display());
            uncertain.insert(path);
        } else {
            eprintln!("daemon: stopped watching {}", path.display());
            watched.remove(&path);
            uncertain.remove(&path);
        }
    }

    let mut additions = desired
        .iter()
        .filter(|(path, scope)| watched.get(*path) != Some(*scope) || uncertain.contains(*path))
        .map(|(path, scope)| (path.clone(), *scope))
        .collect::<Vec<_>>();
    additions.sort_by(|left, right| left.0.cmp(&right.0));
    let mut newly_watched = Vec::with_capacity(additions.len());
    for (path, scope) in additions {
        if let Err(error) = watcher.watch(&path, scope.notify_mode()) {
            eprintln!("daemon: cannot watch {}: {error}", path.display());
        } else {
            eprintln!("daemon: watching {}", path.display());
            watched.insert(path.clone(), scope);
            uncertain.remove(&path);
            newly_watched.push(path);
        }
    }
    newly_watched
}
