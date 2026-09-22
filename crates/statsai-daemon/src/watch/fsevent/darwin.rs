// SPDX-License-Identifier: CC0-1.0
//
// Based on notify 8.2.0 `notify/src/fsevent.rs`:
// https://github.com/notify-rs/notify/blob/notify-8.2.0/notify/src/fsevent.rs
//
// Local changes:
// - event paths keep their raw bytes
// - unknown flags request a rescan instead of panicking
// - unsupported registrations return errors
// - CoreFoundation releases are skipped for null references
// - watches are registered at the canonical path and remembered by the configured path
// - deleted files and symlink retargets keep that mapping
// - stream restarts emit an empty-path rescan, and the callback never joins the run loop

use super::logic::{
    cf_ref_needs_release, decode_stream_flags, path_from_event_bytes, validate_registration_path,
    RegistrationError, RootMap,
};
use fsevent_sys as fs;
use fsevent_sys::core_foundation as cf;
use notify::event::Flag;
use notify::{Config, Event, EventHandler, EventKind, RecursiveMode, Result, Watcher};
use std::ffi::CStr;
use std::os::raw;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

pub(in crate::watch) struct FsEventWatcher {
    paths: cf::CFMutableArrayRef,
    since_when: fs::FSEventStreamEventId,
    latency: cf::CFTimeInterval,
    flags: fs::FSEventStreamCreateFlags,
    event_handler: Arc<Mutex<dyn EventHandler>>,
    shared: Arc<SharedState>,
    runloop: Option<(cf::CFRunLoopRef, thread::JoinHandle<()>)>,
}

struct SharedState {
    roots: Mutex<RootMap>,
    restart: AtomicBool,
}

unsafe impl Send for FsEventWatcher {}
unsafe impl Sync for FsEventWatcher {}

struct StreamContextInfo {
    event_handler: Arc<Mutex<dyn EventHandler>>,
    shared: Arc<SharedState>,
}

extern "C" fn release_context(info: *const libc::c_void) {
    unsafe {
        drop(Box::from_raw(
            info as *const StreamContextInfo as *mut StreamContextInfo,
        ));
    }
}

extern "C" {
    fn CFRunLoopIsWaiting(runloop: cf::CFRunLoopRef) -> cf::Boolean;
}

struct CFSendWrapper(cf::CFRef);
unsafe impl Send for CFSendWrapper {}

impl CFSendWrapper {
    /// Consumes the wrapper after it has been moved onto the destination thread.
    ///
    /// `CFRef` is not `Send`. Projecting `.0` inside a `move` closure captures the
    /// raw pointer and drops the wrapper's `Send` impl. Call this method only after
    /// the wrapper itself has been moved, so the FSEvents thread is the exclusive
    /// owner of the run-loop pointer.
    fn into_inner(self) -> cf::CFRef {
        self.0
    }
}

impl FsEventWatcher {
    fn from_event_handler(event_handler: Arc<Mutex<dyn EventHandler>>) -> Result<Self> {
        let paths = unsafe {
            cf::CFArrayCreateMutable(cf::kCFAllocatorDefault, 0, &cf::kCFTypeArrayCallBacks)
        };
        if !cf_ref_needs_release(paths.is_null()) {
            return Err(notify::Error::generic(
                "CFArrayCreateMutable returned a null array",
            ));
        }
        Ok(Self {
            paths,
            since_when: fs::kFSEventStreamEventIdSinceNow,
            latency: 0.0,
            flags: fs::kFSEventStreamCreateFlagFileEvents
                | fs::kFSEventStreamCreateFlagNoDefer
                | fs::kFSEventStreamCreateFlagWatchRoot,
            event_handler,
            shared: Arc::new(SharedState {
                roots: Mutex::new(RootMap::default()),
                restart: AtomicBool::new(false),
            }),
            runloop: None,
        })
    }

    pub(in crate::watch) fn poll_restart(&mut self) -> Result<()> {
        let restart_requested = self.shared.restart.swap(false, Ordering::AcqRel);
        let retargeted = self
            .shared
            .roots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .refresh_changed_targets(|configured| {
                if configured.exists() {
                    configured.canonicalize().ok()
                } else {
                    None
                }
            });
        if !restart_requested && !retargeted {
            return Ok(());
        }
        self.restart_stream()
    }

    fn watch_inner(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.append_path(path, recursive_mode)?;
        self.restart_stream()
    }

    fn unwatch_inner(&mut self, path: &Path) -> Result<()> {
        let removed = self
            .shared
            .roots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unwatch(path);
        if removed.is_none() {
            return Err(notify::Error::watch_not_found().add_path(path.to_path_buf()));
        }
        self.restart_stream()
    }

    fn append_path(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        let bytes = path.as_os_str().as_bytes();
        match validate_registration_path(bytes, path.exists()) {
            Ok(()) => {}
            Err(RegistrationError::InteriorNul) => {
                return Err(notify::Error::generic(
                    "cannot watch a path that contains an interior NUL",
                )
                .add_path(path.to_path_buf()));
            }
            Err(RegistrationError::MissingPath) => {
                return Err(notify::Error::path_not_found().add_path(path.to_path_buf()));
            }
        }
        let canonical = path
            .canonicalize()
            .map_err(|error| notify::Error::io(error).add_path(path.to_path_buf()))?;
        self.shared
            .roots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                path.to_path_buf(),
                canonical,
                matches!(recursive_mode, RecursiveMode::Recursive),
            );
        Ok(())
    }

    fn restart_stream(&mut self) -> Result<()> {
        let was_running = self.runloop.is_some();
        self.stop();
        self.rebuild_paths()?;
        if unsafe { cf::CFArrayGetCount(self.paths) } == 0 {
            return Ok(());
        }
        self.run()?;
        if was_running {
            self.emit_rescan();
        }
        Ok(())
    }

    fn rebuild_paths(&mut self) -> Result<()> {
        unsafe {
            while cf::CFArrayGetCount(self.paths) > 0 {
                cf::CFArrayRemoveValueAtIndex(self.paths, 0);
            }
        }
        let canonicals = self
            .shared
            .roots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .canonical_paths();
        for path in canonicals {
            let cf_path = cfstring_from_path(&path)?;
            unsafe {
                cf::CFArrayAppendValue(self.paths, cf_path);
                cf::CFRelease(cf_path);
            }
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.runloop.is_some()
    }

    fn stop(&mut self) {
        if !self.is_running() {
            return;
        }
        if let Some((runloop, thread_handle)) = self.runloop.take() {
            unsafe {
                let runloop = runloop as *mut raw::c_void;
                while CFRunLoopIsWaiting(runloop) == 0 {
                    thread::yield_now();
                }
                cf::CFRunLoopStop(runloop);
            }
            thread_handle.join().expect("FSEvents thread to shut down");
        }
    }

    fn run(&mut self) -> Result<()> {
        if unsafe { cf::CFArrayGetCount(self.paths) } == 0 {
            return Err(notify::Error::path_not_found());
        }
        let context = Box::into_raw(Box::new(StreamContextInfo {
            event_handler: Arc::clone(&self.event_handler),
            shared: Arc::clone(&self.shared),
        }));
        let stream_context = fs::FSEventStreamContext {
            version: 0,
            info: context as *mut libc::c_void,
            retain: None,
            release: Some(release_context),
            copy_description: None,
        };
        let stream = unsafe {
            fs::FSEventStreamCreate(
                cf::kCFAllocatorDefault,
                callback,
                &stream_context,
                self.paths,
                self.since_when,
                self.latency,
                self.flags,
            )
        };
        if stream.is_null() {
            unsafe {
                drop(Box::from_raw(context));
            }
            return Err(notify::Error::generic("FSEventStreamCreate returned null"));
        }
        let stream = CFSendWrapper(stream);
        let (rl_tx, rl_rx) = std::sync::mpsc::channel();
        let thread_handle = thread::Builder::new()
            .name("statsai-fsevents".to_string())
            .spawn(move || {
                let stream = stream.into_inner();
                unsafe {
                    let current = cf::CFRunLoopGetCurrent();
                    fs::FSEventStreamScheduleWithRunLoop(
                        stream,
                        current,
                        cf::kCFRunLoopDefaultMode,
                    );
                    fs::FSEventStreamStart(stream);
                    rl_tx
                        .send(CFSendWrapper(current))
                        .expect("send FSEvents run loop");
                    cf::CFRunLoopRun();
                    fs::FSEventStreamStop(stream);
                    let event_id = fs::FSEventsGetCurrentEventId();
                    let device = fs::FSEventStreamGetDeviceBeingWatched(stream);
                    fs::FSEventsPurgeEventsForDeviceUpToEventId(device, event_id);
                    fs::FSEventStreamInvalidate(stream);
                    fs::FSEventStreamRelease(stream);
                }
            })?;
        self.runloop = Some((
            rl_rx
                .recv()
                .expect("receive FSEvents run loop")
                .into_inner(),
            thread_handle,
        ));
        Ok(())
    }

    fn emit_rescan(&self) {
        let event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        self.event_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .handle_event(Ok(event));
    }
}

fn cfstring_from_path(path: &Path) -> Result<cf::CFStringRef> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(
            notify::Error::generic("cannot watch a path that contains an interior NUL")
                .add_path(path.to_path_buf()),
        );
    }
    unsafe {
        let url = cf::CFURLCreateFromFileSystemRepresentation(
            cf::kCFAllocatorDefault,
            bytes.as_ptr() as *const raw::c_char,
            bytes.len() as cf::CFIndex,
            false,
        );
        if !cf_ref_needs_release(url.is_null()) {
            return Err(
                notify::Error::generic("CoreFoundation rejected the filesystem path")
                    .add_path(path.to_path_buf()),
            );
        }
        let string = cf::CFURLCopyFileSystemPath(url, cf::kCFURLPOSIXPathStyle);
        cf::CFRelease(url);
        if !cf_ref_needs_release(string.is_null()) {
            return Err(notify::Error::generic(
                "CoreFoundation could not copy the filesystem path",
            )
            .add_path(path.to_path_buf()));
        }
        Ok(string)
    }
}

extern "C" fn callback(
    stream_ref: fs::FSEventStreamRef,
    info: *mut libc::c_void,
    num_events: libc::size_t,
    event_paths: *mut libc::c_void,
    event_flags: *const fs::FSEventStreamEventFlags,
    event_ids: *const fs::FSEventStreamEventId,
) {
    unsafe {
        callback_impl(
            stream_ref,
            info,
            num_events,
            event_paths,
            event_flags,
            event_ids,
        )
    }
}

unsafe fn callback_impl(
    _stream_ref: fs::FSEventStreamRef,
    info: *mut libc::c_void,
    num_events: libc::size_t,
    event_paths: *mut libc::c_void,
    event_flags: *const fs::FSEventStreamEventFlags,
    _event_ids: *const fs::FSEventStreamEventId,
) {
    let event_paths = event_paths as *const *const libc::c_char;
    let info = info as *const StreamContextInfo;
    let shared = &(*info).shared;
    let event_handler = &(*info).event_handler;

    for index in 0..num_events {
        let raw_bytes = CStr::from_ptr(*event_paths.add(index)).to_bytes();
        let raw_path = path_from_event_bytes(raw_bytes);
        let decoded = decode_stream_flags(*event_flags.add(index));
        if decoded.ignore {
            continue;
        }
        if decoded.refresh_root {
            shared.restart.store(true, Ordering::Release);
        }
        let translated = shared
            .roots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .translate(&raw_path);
        let mut handler = event_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if decoded.rescan || translated.rescan || raw_path.as_os_str().is_empty() {
            handler.handle_event(Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan)));
            continue;
        }
        for path in translated.paths {
            handler.handle_event(Ok(Event::new(EventKind::Modify(
                notify::event::ModifyKind::Any,
            ))
            .add_path(path)));
        }
    }
}

impl Watcher for FsEventWatcher {
    fn new<F: EventHandler>(event_handler: F, _config: Config) -> Result<Self> {
        Self::from_event_handler(Arc::new(Mutex::new(event_handler)))
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.watch_inner(path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    fn kind() -> notify::WatcherKind {
        notify::WatcherKind::Fsevent
    }
}

impl Drop for FsEventWatcher {
    fn drop(&mut self) {
        self.stop();
        unsafe {
            if cf_ref_needs_release(self.paths.is_null()) {
                cf::CFRelease(self.paths);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::RecursiveMode;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn live_watcher_detects_an_atomic_symlink_retarget_and_new_writes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let old_target = directory.path().join("old");
        let new_target = directory.path().join("new");
        std::fs::create_dir(&old_target).expect("old");
        std::fs::create_dir(&new_target).expect("new");
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&old_target, &link).expect("symlink");

        let (tx, rx) = mpsc::channel();
        let mut watcher = FsEventWatcher::new(
            move |event| {
                let _ = tx.send(event);
            },
            Config::default(),
        )
        .expect("watcher");
        watcher
            .watch(&link, RecursiveMode::Recursive)
            .expect("watch symlink");

        let replacement = directory.path().join("link.next");
        std::os::unix::fs::symlink(&new_target, &replacement).expect("next link");
        std::fs::rename(&replacement, &link).expect("atomic retarget");

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut saw_rescan = false;
        while Instant::now() < deadline {
            watcher.poll_restart().expect("poll retarget");
            while let Ok(event) = rx.try_recv() {
                if event.as_ref().ok().is_some_and(|event| event.need_rescan()) {
                    saw_rescan = true;
                }
            }
            if saw_rescan {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            saw_rescan,
            "retargeted symlink must rebuild registrations without a manual refresh"
        );

        std::fs::write(new_target.join("observed.txt"), b"after retarget").expect("write");
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut saw_write = false;
        while Instant::now() < deadline {
            watcher.poll_restart().expect("poll write");
            while let Ok(event) = rx.try_recv() {
                if let Ok(event) = event {
                    saw_write |= event.paths.iter().any(|path| {
                        path.ends_with("observed.txt") || path.ends_with("link/observed.txt")
                    });
                }
            }
            if saw_write {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            saw_write,
            "writes under the new symlink target must reach the watcher"
        );
    }
}
