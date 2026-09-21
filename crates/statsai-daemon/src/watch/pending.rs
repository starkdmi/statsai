use notify::{Event, EventKind};
use std::collections::HashSet;
use std::path::PathBuf;

pub(super) const MAX_PENDING_PATHS: usize = 4096;

/// Paths waiting for a scan, or a single coalesced full rescan.
///
/// `rescan_all` is independent of the path set. A later path event cannot clear
/// it, and restoring a failed rescan clears paths that arrived during the scan.
#[derive(Debug, Default, Clone)]
pub(super) struct PendingWatch {
    rescan_all: bool,
    paths: HashSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WatchNotice {
    Paths(Vec<PathBuf>),
    RescanAll,
}

impl PendingWatch {
    pub(super) fn note_rescan_all(&mut self) {
        self.rescan_all = true;
        self.paths.clear();
    }

    pub(super) fn note_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if self.rescan_all {
            return;
        }
        self.paths.extend(paths);
        if self.paths.len() > MAX_PENDING_PATHS {
            self.note_rescan_all();
        }
    }

    pub(super) fn take(&mut self) -> WatchNotice {
        if self.rescan_all {
            self.rescan_all = false;
            self.paths.clear();
            WatchNotice::RescanAll
        } else {
            WatchNotice::Paths(std::mem::take(&mut self.paths).into_iter().collect())
        }
    }

    pub(super) fn restore_failed(&mut self, notice: WatchNotice) {
        match notice {
            WatchNotice::RescanAll => self.note_rescan_all(),
            WatchNotice::Paths(paths) => self.note_paths(paths),
        }
    }

    #[cfg(test)]
    pub(super) fn rescan_all(&self) -> bool {
        self.rescan_all
    }

    #[cfg(test)]
    pub(super) fn paths(&self) -> &HashSet<PathBuf> {
        &self.paths
    }
}

impl WatchNotice {
    pub(super) fn paths(&self) -> &[PathBuf] {
        match self {
            Self::Paths(paths) => paths,
            Self::RescanAll => &[],
        }
    }

    pub(super) fn rescan_all(&self) -> bool {
        matches!(self, Self::RescanAll)
    }
}

pub(super) fn note_notify_event(pending: &mut PendingWatch, event: Result<Event, notify::Error>) {
    match event {
        Err(_) => pending.note_rescan_all(),
        Ok(event) => {
            if event.need_rescan() || event.paths.is_empty() {
                pending.note_rescan_all();
            } else if matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                pending.note_paths(event.paths);
            }
        }
    }
}

pub(super) fn merge_notice(pending: &mut PendingWatch, notice: WatchNotice) {
    match notice {
        WatchNotice::RescanAll => pending.note_rescan_all(),
        WatchNotice::Paths(paths) => pending.note_paths(paths),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{Flag, ModifyKind};
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn pending_paths_keep_exactly_the_cap_and_rescan_past_it() {
        let mut pending = PendingWatch::default();
        pending.note_paths(
            (0..MAX_PENDING_PATHS).map(|index| PathBuf::from(format!("/tmp/statsai-cap-{index}"))),
        );
        assert!(!pending.rescan_all());
        assert_eq!(pending.paths().len(), MAX_PENDING_PATHS);
        pending.note_paths([PathBuf::from("/tmp/statsai-one-more")]);
        assert!(pending.rescan_all());
        assert!(pending.paths().is_empty());
        assert_eq!(pending.take(), WatchNotice::RescanAll);
    }

    #[test]
    fn pending_paths_collapse_to_a_full_rescan_past_the_cap() {
        let mut pending = PendingWatch::default();
        let paths =
            (0..=MAX_PENDING_PATHS).map(|index| PathBuf::from(format!("/tmp/statsai-{index}")));
        pending.note_paths(paths);
        assert!(pending.rescan_all());
        assert!(pending.paths().is_empty());
        assert_eq!(pending.take(), WatchNotice::RescanAll);
    }

    #[test]
    fn a_failed_rescan_wins_over_paths_that_arrived_during_the_scan() {
        let mut pending = PendingWatch::default();
        pending.note_rescan_all();
        let notice = pending.take();
        pending.note_paths([PathBuf::from("/tmp/during-scan")]);
        pending.restore_failed(notice);
        assert!(pending.rescan_all());
        assert!(pending.paths().is_empty());
    }

    #[test]
    fn a_rescan_that_arrives_during_a_failed_path_scan_is_preserved() {
        let mut pending = PendingWatch::default();
        pending.note_paths([PathBuf::from("/tmp/original")]);
        let notice = pending.take();
        pending.note_rescan_all();
        pending.restore_failed(notice);
        assert!(pending.rescan_all());
        assert!(pending.paths().is_empty());
    }

    #[test]
    fn failed_path_scans_merge_with_concurrent_paths() {
        let mut pending = PendingWatch::default();
        let notice = WatchNotice::Paths(vec![PathBuf::from("/tmp/failed")]);
        pending.note_paths([PathBuf::from("/tmp/during")]);
        pending.restore_failed(notice);
        assert!(!pending.rescan_all());
        assert_eq!(pending.paths().len(), 2);
    }

    #[test]
    fn concurrent_notifications_cannot_drop_a_rescan() {
        let pending = Arc::new(Mutex::new(PendingWatch::default()));
        let rescan = {
            let pending = Arc::clone(&pending);
            thread::spawn(move || {
                pending.lock().expect("rescan").note_rescan_all();
            })
        };
        let paths = {
            let pending = Arc::clone(&pending);
            thread::spawn(move || {
                let paths = (0..100).map(|index| PathBuf::from(format!("/tmp/concurrent-{index}")));
                pending.lock().expect("paths").note_paths(paths);
            })
        };
        rescan.join().expect("rescan thread");
        paths.join().expect("path thread");
        let pending = pending.lock().expect("joined");
        assert!(pending.rescan_all());
        assert!(pending.paths().is_empty());
    }

    #[test]
    fn unknown_or_empty_notify_events_schedule_a_rescan() {
        let mut pending = PendingWatch::default();
        note_notify_event(&mut pending, Err(notify::Error::generic("watcher failed")));
        assert!(pending.rescan_all());

        let mut pending = PendingWatch::default();
        note_notify_event(
            &mut pending,
            Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan)),
        );
        assert!(pending.rescan_all());

        let mut pending = PendingWatch::default();
        note_notify_event(
            &mut pending,
            Ok(Event::new(EventKind::Modify(ModifyKind::Any)).add_path(PathBuf::from("/tmp/file"))),
        );
        assert!(!pending.rescan_all());
        assert!(pending.paths().contains(&PathBuf::from("/tmp/file")));
    }
}
