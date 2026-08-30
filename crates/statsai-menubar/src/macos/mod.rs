mod app;
mod capture;
mod menu;

pub use app::run;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{NSMenu, NSMenuDidBeginTrackingNotification, NSMenuDidEndTrackingNotification};
use objc2_foundation::{NSNotification, NSNotificationCenter};
use statsai::snapshot::AppSnapshot;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::time::Duration;
use tray_icon::menu::{ContextMenu, Menu};

pub(super) const REFRESH_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Activity {
    Idle,
    Scanning,
    Uploading,
}

pub(super) enum UserEvent {
    TrayIcon(#[allow(dead_code)] tray_icon::TrayIconEvent),
    Menu(tray_icon::menu::MenuEvent),
    MenuTrackingChanged(bool),
    Refresh,
    SetIdle,
    SnapshotReady(Box<Result<AppSnapshot, String>>),
}

pub(super) struct MenuTrackingObservers {
    #[allow(dead_code)]
    begin_token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    #[allow(dead_code)]
    end_token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    #[allow(dead_code)]
    begin_block: RcBlock<dyn Fn(NonNull<NSNotification>)>,
    #[allow(dead_code)]
    end_block: RcBlock<dyn Fn(NonNull<NSNotification>)>,
}

impl Drop for MenuTrackingObservers {
    fn drop(&mut self) {
        let center = NSNotificationCenter::defaultCenter();
        let begin_token: &ProtocolObject<dyn NSObjectProtocol> = self.begin_token.as_ref();
        let end_token: &ProtocolObject<dyn NSObjectProtocol> = self.end_token.as_ref();
        let begin_observer: &AnyObject = begin_token.as_ref();
        let end_observer: &AnyObject = end_token.as_ref();
        unsafe {
            center.removeObserver(begin_observer);
            center.removeObserver(end_observer);
        }
    }
}

pub(super) struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    pub(super) fn acquire() -> Result<Self, String> {
        let path = instance_lock_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|err| format!("open {}: {err}", path.display()))?;
        let fd = file.as_raw_fd();
        let locked = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0 };
        if !locked {
            return Err(
                "StatsAI menu bar is already running. Quit the existing instance first."
                    .to_string(),
            );
        }

        file.set_len(0).map_err(|err| err.to_string())?;
        let pid = std::process::id();
        write!(&file, "{pid}").map_err(|err| err.to_string())?;

        Ok(Self { _file: file })
    }
}

fn instance_lock_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".statsai")
        .join("menubar.lock")
}

pub(super) fn install_menu_tracking_observers(
    menu: &Menu,
    proxy: tao::event_loop::EventLoopProxy<UserEvent>,
) -> MenuTrackingObservers {
    let menu_ptr = menu.ns_menu();
    let ns_menu = unsafe { (menu_ptr as *mut NSMenu).as_ref() }
        .expect("tray menu should expose an NSMenu on macOS");
    let menu_object = unsafe { (menu_ptr as *mut AnyObject).as_ref() }
        .expect("tray menu should expose an NSObject on macOS");
    let center = NSNotificationCenter::defaultCenter();

    let begin_proxy = proxy.clone();
    let begin_block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
        let _ = begin_proxy.send_event(UserEvent::MenuTrackingChanged(true));
    });
    let begin_token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSMenuDidBeginTrackingNotification),
            Some(menu_object),
            None,
            &begin_block,
        )
    };

    let end_block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
        let _ = proxy.send_event(UserEvent::MenuTrackingChanged(false));
    });
    let end_token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSMenuDidEndTrackingNotification),
            Some(ns_menu.as_ref()),
            None,
            &end_block,
        )
    };

    MenuTrackingObservers {
        begin_token,
        end_token,
        begin_block,
        end_block,
    }
}
