#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("StatsAI menu bar app is only supported on macOS.");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod macos {
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
    use objc2_app_kit::{
        NSMenu, NSMenuDidBeginTrackingNotification, NSMenuDidEndTrackingNotification,
    };
    use objc2_foundation::{NSNotification, NSNotificationCenter};
    use statsai::snapshot::{AppSnapshot, PrimaryAction, SnapshotBackgroundStatus};
    use statsai::{default_store_path, snapshot};
    use statsai_store::Store;
    use std::cell::{Cell, RefCell};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::unix::io::AsRawFd;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::ptr::NonNull;
    use std::sync::OnceLock;
    use std::thread;
    use std::time::{Duration, Instant};
    use tao::event::Event;
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tray_icon::menu::{ContextMenu, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    const REFRESH_INTERVAL: Duration = Duration::from_secs(15);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Activity {
        Idle,
        Scanning,
        Uploading,
    }

    enum UserEvent {
        TrayIcon(#[allow(dead_code)] tray_icon::TrayIconEvent),
        Menu(tray_icon::menu::MenuEvent),
        MenuTrackingChanged(bool),
        Refresh,
        SetIdle,
        SnapshotReady(Box<Result<AppSnapshot, String>>),
    }

    struct MenuTrackingObservers {
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

    struct MenuShellContext<'a> {
        menu_shell: &'a RefCell<String>,
        pending_shell: &'a RefCell<Option<String>>,
        menu_tracking_observers: &'a RefCell<Option<MenuTrackingObservers>>,
        proxy: &'a tao::event_loop::EventLoopProxy<UserEvent>,
    }

    impl<'a> MenuShellContext<'a> {
        fn new(
            menu_shell: &'a RefCell<String>,
            pending_shell: &'a RefCell<Option<String>>,
            menu_tracking_observers: &'a RefCell<Option<MenuTrackingObservers>>,
            proxy: &'a tao::event_loop::EventLoopProxy<UserEvent>,
        ) -> Self {
            Self {
                menu_shell,
                pending_shell,
                menu_tracking_observers,
                proxy,
            }
        }

        fn replace_tray_menu(&self, icon: Option<&TrayIcon>, menu: Menu) {
            let Some(icon) = icon else {
                return;
            };
            let observers = install_menu_tracking_observers(&menu, self.proxy.clone());
            icon.set_menu(Some(Box::new(menu)));
            self.menu_tracking_observers.replace(Some(observers));
        }
    }

    struct MenuActions {
        start_tracking: MenuItem,
        scan_now: MenuItem,
        link_dashboard: MenuItem,
        upload_now: MenuItem,
        open_dashboard: MenuItem,
        add_source: MenuItem,
        help: MenuItem,
        quit: MenuItem,
    }

    struct MenuUi {
        summary: MenuItem,
        tracking: MenuItem,
        last_scan: MenuItem,
        stat_1: MenuItem,
        stat_2: MenuItem,
        stat_3: MenuItem,
        sources_header: MenuItem,
        actions: MenuActions,
        #[cfg(debug_assertions)]
        dev_info: MenuItem,
    }

    impl MenuUi {
        fn new() -> Self {
            Self {
                summary: status_item("Getting ready…"),
                tracking: status_item("Tracking · starting…"),
                last_scan: status_item("Last scan · …"),
                stat_1: status_item("Last 7 days · …"),
                stat_2: status_item("Today · …"),
                stat_3: status_item("Dashboard · …"),
                sources_header: status_item("Sources · all time"),
                actions: MenuActions {
                    start_tracking: MenuItem::new("Start Local Tracking", true, None),
                    scan_now: MenuItem::new("Scan Now", true, None),
                    link_dashboard: MenuItem::new("Link Dashboard…", true, None),
                    upload_now: MenuItem::new("Sync Now", true, None),
                    open_dashboard: MenuItem::new("Open Dashboard", true, None),
                    add_source: MenuItem::new("Add Source", true, None),
                    help: MenuItem::new("Help", true, None),
                    quit: MenuItem::new("Quit StatsAI", true, None),
                },
                #[cfg(debug_assertions)]
                dev_info: status_item(" "),
            }
        }

        fn apply_snapshot(&self, snapshot: &AppSnapshot, activity: Activity) {
            let summary = match activity {
                Activity::Scanning => "Reading your usage logs…",
                Activity::Uploading => "Uploading to your dashboard…",
                Activity::Idle => snapshot.menu_summary.as_str(),
            };
            self.summary.set_text(summary);
            self.tracking
                .set_text(tracking_line(&snapshot.background_tracking));
            self.last_scan.set_text(
                snapshot
                    .last_scan_summary
                    .as_deref()
                    .unwrap_or("Last scan · …"),
            );
            self.stat_1.set_text(&snapshot.menu_stat_1);
            self.stat_2.set_text(&snapshot.menu_stat_2);
            self.stat_3.set_text(&snapshot.menu_stat_3);

            let presentation = menu_presentation(snapshot, activity);
            self.actions
                .start_tracking
                .set_enabled(presentation.show_start_tracking);
            self.actions
                .scan_now
                .set_enabled(presentation.show_scan_now);
            self.actions
                .link_dashboard
                .set_enabled(presentation.show_link_dashboard);
            self.actions
                .upload_now
                .set_enabled(presentation.show_upload_now);
            self.actions
                .open_dashboard
                .set_enabled(presentation.open_dashboard_enabled);
            self.actions.add_source.set_enabled(true);
            self.actions.help.set_enabled(true);

            #[cfg(debug_assertions)]
            self.dev_info.set_text(dev_info_line(snapshot));
        }

        fn build_menu(&self, snapshot: &AppSnapshot, activity: Activity) -> Menu {
            let menu = Menu::new();
            let sep_status = PredefinedMenuItem::separator();
            let sep_sources = PredefinedMenuItem::separator();
            let sep_actions = PredefinedMenuItem::separator();
            let sep_quit = PredefinedMenuItem::separator();
            #[cfg(debug_assertions)]
            let sep_dev = PredefinedMenuItem::separator();

            let presentation = menu_presentation(snapshot, activity);

            let _ = menu.append(&self.summary);
            let _ = menu.append(&self.tracking);
            let _ = menu.append(&self.last_scan);
            let _ = menu.append(&sep_status);
            let _ = menu.append(&self.stat_2);
            let _ = menu.append(&self.stat_1);
            let _ = menu.append(&self.stat_3);

            if snapshot.status_error {
                let _ = menu.append(&sep_actions);
                let _ = menu.append(&self.actions.help);
                let _ = menu.append(&sep_quit);
                let _ = menu.append(&self.actions.quit);
                return menu;
            }

            if presentation.show_sources {
                let _ = menu.append(&sep_sources);
                let _ = menu.append(&self.sources_header);
                for source in &snapshot.sources {
                    let item = status_item(&source.label);
                    let _ = menu.append(&item);
                }
            }

            let _ = menu.append(&sep_actions);
            if presentation.show_start_tracking {
                let _ = menu.append(&self.actions.start_tracking);
            }
            if presentation.show_link_dashboard {
                let _ = menu.append(&self.actions.link_dashboard);
            }
            if presentation.show_upload_now {
                let _ = menu.append(&self.actions.upload_now);
            }
            let _ = menu.append(&self.actions.open_dashboard);
            let _ = menu.append(&self.actions.scan_now);
            let _ = menu.append(&self.actions.add_source);
            let _ = menu.append(&self.actions.help);
            let _ = menu.append(&sep_quit);

            #[cfg(debug_assertions)]
            {
                let _ = menu.append(&self.dev_info);
                let _ = menu.append(&sep_dev);
            }

            let _ = menu.append(&self.actions.quit);
            menu
        }
    }

    fn menu_shell_key(snapshot: &AppSnapshot, activity: Activity) -> String {
        if snapshot.status_error {
            return "error".to_string();
        }
        let sources = snapshot
            .sources
            .iter()
            .map(|source| {
                format!(
                    "{}:{}:{}:{}",
                    source.provider, source.status, source.token_count, source.label
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let presentation = menu_presentation(snapshot, activity);
        format!(
            "layout:{}:{activity:?}:first_run:{}:tracking:{}:actions:{}:{}:{}:{}:sources:{sources}",
            snapshot.menu_layout,
            snapshot.first_run,
            snapshot.background_tracking.running,
            presentation.show_start_tracking,
            presentation.show_scan_now,
            presentation.show_link_dashboard,
            presentation.show_upload_now,
        )
    }

    fn sync_menu_shell(
        menu_ui: &MenuUi,
        icon: Option<&TrayIcon>,
        snapshot: &AppSnapshot,
        activity: Activity,
        menu_open: &Cell<bool>,
        context: &MenuShellContext<'_>,
    ) {
        let shell = menu_shell_key(snapshot, activity);
        let current_shell = context.menu_shell.borrow().clone();

        if shell == current_shell {
            menu_ui.apply_snapshot(snapshot, activity);
            context.pending_shell.borrow_mut().take();
            return;
        }

        if menu_open.get() {
            *context.pending_shell.borrow_mut() = Some(shell);
            menu_ui.apply_snapshot(snapshot, activity);
            return;
        }

        let menu = menu_ui.build_menu(snapshot, activity);
        context.replace_tray_menu(icon, menu);
        *context.menu_shell.borrow_mut() = shell;
        context.pending_shell.borrow_mut().take();
        menu_ui.apply_snapshot(snapshot, activity);
    }

    fn flush_pending_menu_shell(
        menu_ui: &MenuUi,
        icon: Option<&TrayIcon>,
        snapshot: Option<AppSnapshot>,
        activity: Activity,
        context: &MenuShellContext<'_>,
    ) {
        let Some(shell) = context.pending_shell.borrow_mut().take() else {
            return;
        };
        if shell == *context.menu_shell.borrow() {
            return;
        }
        let Some(snapshot) = snapshot else {
            *context.pending_shell.borrow_mut() = Some(shell);
            return;
        };
        let menu = menu_ui.build_menu(&snapshot, activity);
        context.replace_tray_menu(icon, menu);
        *context.menu_shell.borrow_mut() = shell;
        menu_ui.apply_snapshot(&snapshot, activity);
    }

    fn rebuild_menu_for_activity(
        menu_ui: &MenuUi,
        icon: Option<&TrayIcon>,
        snapshot: Option<AppSnapshot>,
        activity: Activity,
        context: &MenuShellContext<'_>,
    ) {
        let Some(snapshot) = snapshot else {
            return;
        };
        let shell = menu_shell_key(&snapshot, activity);
        let menu = menu_ui.build_menu(&snapshot, activity);
        context.replace_tray_menu(icon, menu);
        *context.menu_shell.borrow_mut() = shell;
        context.pending_shell.borrow_mut().take();
        menu_ui.apply_snapshot(&snapshot, activity);
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct MenuPresentation {
        show_sources: bool,
        show_start_tracking: bool,
        show_scan_now: bool,
        show_link_dashboard: bool,
        show_upload_now: bool,
        open_dashboard_enabled: bool,
    }

    fn menu_presentation(snapshot: &AppSnapshot, activity: Activity) -> MenuPresentation {
        let idle = activity == Activity::Idle && !snapshot.status_error;
        MenuPresentation {
            show_sources: !snapshot.sources.is_empty(),
            show_start_tracking: idle && !snapshot.background_tracking.running,
            show_scan_now: idle,
            show_link_dashboard: idle && !snapshot.logged_in,
            show_upload_now: idle
                && snapshot.logged_in
                && (snapshot.pending_upload || snapshot.sync_failures > 0),
            open_dashboard_enabled: !snapshot.status_error,
        }
    }

    fn tracking_line(status: &SnapshotBackgroundStatus) -> String {
        format!("Tracking · {}", status.label)
    }

    fn should_run_startup_scan(snapshot: &AppSnapshot) -> bool {
        snapshot.first_run || snapshot.sessions_week == 0 || !snapshot.background_tracking.running
    }

    fn loading_snapshot() -> AppSnapshot {
        AppSnapshot {
            logged_in: false,
            first_run: true,
            last_sync_at: None,
            sync_failures: 0,
            has_synced: false,
            pending_upload: false,
            pending_days: 0,
            unsynced_events: 0,
            tokens_today: 0,
            tokens_week: 0,
            sessions_week: 0,
            cost_week_cents: None,
            menu_summary: "Getting ready…".to_string(),
            menu_stat_1: "Last 7 days · …".to_string(),
            menu_stat_2: "Today · …".to_string(),
            menu_stat_3: "Dashboard · …".to_string(),
            primary_action: PrimaryAction::None,
            menu_layout: "loading".to_string(),
            status_error: false,
            backend_api: String::new(),
            backend_web: String::new(),
            using_local_dev: false,
            background_tracking: SnapshotBackgroundStatus {
                installed: false,
                running: false,
                label: "Tracking setup needed".to_string(),
            },
            sources: Vec::new(),
            last_scan_summary: Some("Last scan · waiting to start".to_string()),
            help_url: help_url(),
            setup_url: dashboard_url(),
            tooltip: "StatsAI".to_string(),
        }
    }

    fn unavailable_snapshot(reason: &str) -> AppSnapshot {
        eprintln!("statsai menubar status error: {reason}");
        AppSnapshot {
            logged_in: false,
            first_run: false,
            last_sync_at: None,
            sync_failures: 0,
            has_synced: false,
            pending_upload: false,
            pending_days: 0,
            unsynced_events: 0,
            tokens_today: 0,
            tokens_week: 0,
            sessions_week: 0,
            cost_week_cents: None,
            menu_summary: "Can't check status right now".to_string(),
            menu_stat_1: "Try quitting and reopening StatsAI.".to_string(),
            menu_stat_2: "If macOS asked for Keychain access, click Allow.".to_string(),
            menu_stat_3: " ".to_string(),
            primary_action: PrimaryAction::None,
            menu_layout: "error".to_string(),
            status_error: true,
            backend_api: String::new(),
            backend_web: String::new(),
            using_local_dev: false,
            background_tracking: SnapshotBackgroundStatus {
                installed: false,
                running: false,
                label: "Tracking unavailable".to_string(),
            },
            sources: Vec::new(),
            last_scan_summary: Some("Last scan unavailable".to_string()),
            help_url: help_url(),
            setup_url: dashboard_url(),
            tooltip: "StatsAI — status unavailable".to_string(),
        }
    }

    struct InstanceLock {
        _file: File,
    }

    impl InstanceLock {
        fn acquire() -> Result<Self, String> {
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

    fn status_item(label: &str) -> MenuItem {
        let item = MenuItem::new(label, false, None);
        item.set_enabled(false);
        item
    }

    pub fn run() -> Result<(), String> {
        let _instance_lock = InstanceLock::acquire()?;

        let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
        let proxy = event_loop.create_proxy();
        TrayIconEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(UserEvent::TrayIcon(event));
        }));
        let proxy = event_loop.create_proxy();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(UserEvent::Menu(event));
        }));

        let refresh_proxy = event_loop.create_proxy();
        let init_refresh_proxy = refresh_proxy.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(REFRESH_INTERVAL);
            let _ = refresh_proxy.send_event(UserEvent::Refresh);
        });

        ensure_background_tracking();

        let menu_ui = MenuUi::new();
        let initial = loading_snapshot();
        let initial_shell = menu_shell_key(&initial, Activity::Idle);
        menu_ui.apply_snapshot(&initial, Activity::Idle);
        let tray_menu = menu_ui.build_menu(&initial, Activity::Idle);
        let menu_tracking_observers = RefCell::new(Some(install_menu_tracking_observers(
            &tray_menu,
            init_refresh_proxy.clone(),
        )));
        let activity = Cell::new(Activity::Idle);
        let refresh_in_flight = Cell::new(false);
        let last_snapshot = RefCell::new(None::<AppSnapshot>);
        let startup_scan_started = Cell::new(false);
        let menu_open = Cell::new(false);
        let menu_shell = RefCell::new(initial_shell);
        let pending_shell = RefCell::new(None::<String>);

        let mut tray_icon: Option<TrayIcon> = None;
        let mut next_wakeup = std::time::Instant::now()
            .checked_add(REFRESH_INTERVAL)
            .unwrap_or_else(std::time::Instant::now);

        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::WaitUntil(next_wakeup);
            let menu_shell_context = MenuShellContext::new(
                &menu_shell,
                &pending_shell,
                &menu_tracking_observers,
                &init_refresh_proxy,
            );

            match event {
                Event::NewEvents(tao::event::StartCause::Init) if tray_icon.is_none() => {
                    match TrayIconBuilder::new()
                        .with_menu(Box::new(tray_menu.clone()))
                        .with_tooltip("StatsAI")
                        .with_icon(tray_icon_image())
                        .build()
                    {
                        Ok(icon) => {
                            tray_icon = Some(icon);

                            use objc2_core_foundation::CFRunLoop;
                            if let Some(rl) = CFRunLoop::main() {
                                CFRunLoop::wake_up(&rl);
                            }

                            let _ = init_refresh_proxy.send_event(UserEvent::Refresh);
                        }
                        Err(err) => {
                            eprintln!("StatsAI could not create the menu bar icon: {err}");
                            std::process::exit(1);
                        }
                    }
                }
                Event::NewEvents(_) => {}
                Event::UserEvent(UserEvent::TrayIcon(TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Down,
                    ..
                })) => {
                    menu_open.set(true);
                }
                Event::UserEvent(UserEvent::MenuTrackingChanged(is_open)) => {
                    menu_open.set(is_open);
                    if !is_open {
                        flush_pending_menu_shell(
                            &menu_ui,
                            tray_icon.as_ref(),
                            last_snapshot.borrow().clone(),
                            activity.get(),
                            &menu_shell_context,
                        );
                    }
                }
                Event::UserEvent(UserEvent::SetIdle) => {
                    activity.set(Activity::Idle);
                    let _ = init_refresh_proxy.send_event(UserEvent::Refresh);
                }
                Event::UserEvent(UserEvent::Refresh) => {
                    next_wakeup = std::time::Instant::now()
                        .checked_add(REFRESH_INTERVAL)
                        .unwrap_or_else(std::time::Instant::now);
                    if refresh_in_flight.get() {
                        return;
                    }
                    refresh_in_flight.set(true);
                    let proxy = init_refresh_proxy.clone();
                    std::thread::spawn(move || {
                        let snapshot = fetch_snapshot();
                        let _ = proxy.send_event(UserEvent::SnapshotReady(Box::new(snapshot)));
                    });
                }
                Event::UserEvent(UserEvent::SnapshotReady(result)) => {
                    refresh_in_flight.set(false);
                    let snapshot = match *result {
                        Ok(snapshot) => {
                            *last_snapshot.borrow_mut() = Some(snapshot.clone());
                            snapshot
                        }
                        Err(reason) => last_snapshot
                            .borrow()
                            .clone()
                            .unwrap_or_else(|| unavailable_snapshot(&reason)),
                    };
                    sync_menu_shell(
                        &menu_ui,
                        tray_icon.as_ref(),
                        &snapshot,
                        activity.get(),
                        &menu_open,
                        &menu_shell_context,
                    );
                    if let Some(icon) = tray_icon.as_ref() {
                        let _ = icon.set_tooltip(Some(snapshot.tooltip.as_str()));
                    }
                    if !startup_scan_started.get() && should_run_startup_scan(&snapshot) {
                        startup_scan_started.set(true);
                        activity.set(Activity::Scanning);
                        rebuild_menu_for_activity(
                            &menu_ui,
                            tray_icon.as_ref(),
                            Some(snapshot),
                            activity.get(),
                            &menu_shell_context,
                        );
                        spawn_startup_scan(init_refresh_proxy.clone(), init_refresh_proxy.clone());
                    } else {
                        startup_scan_started.set(true);
                    }
                }
                Event::UserEvent(UserEvent::Menu(menu_event)) => {
                    if menu_event.id == menu_ui.actions.quit.id() {
                        menu_open.set(false);
                        tray_icon.take();
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    if menu_event.id == menu_ui.actions.open_dashboard.id() {
                        menu_open.set(false);
                        open_url(&dashboard_url());
                        flush_pending_menu_shell(
                            &menu_ui,
                            tray_icon.as_ref(),
                            last_snapshot.borrow().clone(),
                            activity.get(),
                            &menu_shell_context,
                        );
                        return;
                    }

                    if menu_event.id == menu_ui.actions.help.id() {
                        menu_open.set(false);
                        let url = last_snapshot
                            .borrow()
                            .as_ref()
                            .map(|snapshot| snapshot.help_url.clone())
                            .filter(|url| !url.trim().is_empty())
                            .unwrap_or_else(help_url);
                        open_url(&url);
                        flush_pending_menu_shell(
                            &menu_ui,
                            tray_icon.as_ref(),
                            last_snapshot.borrow().clone(),
                            activity.get(),
                            &menu_shell_context,
                        );
                        return;
                    }

                    if menu_event.id == menu_ui.actions.start_tracking.id() {
                        let proxy = init_refresh_proxy.clone();
                        menu_ui.summary.set_text("Starting local tracking…");
                        spawn_menu_action(move || {
                            match run_statsai_capture(&["service", "install"]) {
                                Ok(_) => {
                                    let _ = proxy.send_event(UserEvent::Refresh);
                                }
                                Err(message) => alert("Could not start local tracking", &message),
                            }
                            let _ = proxy.send_event(UserEvent::Refresh);
                        });
                    }

                    if menu_event.id == menu_ui.actions.add_source.id() {
                        spawn_add_source_action(init_refresh_proxy.clone());
                    }

                    if menu_event.id == menu_ui.actions.scan_now.id() {
                        let proxy = init_refresh_proxy.clone();
                        activity.set(Activity::Scanning);
                        menu_ui.summary.set_text("Reading your usage logs…");
                        rebuild_menu_for_activity(
                            &menu_ui,
                            tray_icon.as_ref(),
                            last_snapshot.borrow().clone(),
                            activity.get(),
                            &menu_shell_context,
                        );
                        spawn_scan_action(proxy);
                    }

                    if menu_event.id == menu_ui.actions.link_dashboard.id() {
                        let proxy = init_refresh_proxy.clone();
                        spawn_menu_action(move || {
                            match run_statsai_capture(&["auth", "login"]) {
                                Ok(_) => {
                                    let _ = proxy.send_event(UserEvent::Refresh);
                                }
                                Err(message) => alert("Could not link this Mac", &message),
                            }
                            let _ = proxy.send_event(UserEvent::Refresh);
                        });
                    }

                    if menu_event.id == menu_ui.actions.upload_now.id() {
                        let proxy = init_refresh_proxy.clone();
                        activity.set(Activity::Uploading);
                        menu_ui.summary.set_text("Uploading to your dashboard…");
                        menu_ui.actions.upload_now.set_enabled(false);
                        rebuild_menu_for_activity(
                            &menu_ui,
                            tray_icon.as_ref(),
                            last_snapshot.borrow().clone(),
                            activity.get(),
                            &menu_shell_context,
                        );
                        spawn_upload_action(proxy);
                    }
                    menu_open.set(false);
                    flush_pending_menu_shell(
                        &menu_ui,
                        tray_icon.as_ref(),
                        last_snapshot.borrow().clone(),
                        activity.get(),
                        &menu_shell_context,
                    );
                }
                _ => {}
            }
        });
    }

    fn ensure_background_tracking() {
        spawn_menu_action(|| {
            let _ = run_statsai_capture(&["service", "install"]);
        });
    }

    fn install_menu_tracking_observers(
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

    fn spawn_startup_scan(
        refresh_proxy: tao::event_loop::EventLoopProxy<UserEvent>,
        idle_proxy: tao::event_loop::EventLoopProxy<UserEvent>,
    ) {
        spawn_menu_action(move || {
            match run_statsai_capture(&["scan"]) {
                Ok(_) => {
                    let _ = refresh_proxy.send_event(UserEvent::Refresh);
                }
                Err(message) => {
                    eprintln!("statsai menubar startup scan failed: {message}");
                }
            }
            let _ = idle_proxy.send_event(UserEvent::SetIdle);
        });
    }

    fn spawn_scan_action(proxy: tao::event_loop::EventLoopProxy<UserEvent>) {
        spawn_menu_action(move || {
            match run_statsai_capture(&["scan"]) {
                Ok(_) => {}
                Err(message) => alert("Could not read your usage logs", &message),
            }
            let _ = proxy.send_event(UserEvent::SetIdle);
        });
    }

    fn spawn_upload_action(proxy: tao::event_loop::EventLoopProxy<UserEvent>) {
        spawn_menu_action(move || {
            match run_statsai_capture(&["scan"]) {
                Ok(_) => match run_statsai_capture(&["sync", "--sink", "http", "--since-last"]) {
                    Ok(_) => {}
                    Err(message) => alert("Upload failed", &message),
                },
                Err(message) => alert("Could not read your usage logs", &message),
            }
            let _ = proxy.send_event(UserEvent::SetIdle);
        });
    }

    fn spawn_add_source_action(proxy: tao::event_loop::EventLoopProxy<UserEvent>) {
        spawn_menu_action(move || {
            match choose_source_provider().and_then(|choice| match choice {
                Some((provider, display_name)) => {
                    choose_source_folder(display_name).map(|path| path.map(|path| (provider, path)))
                }
                None => Ok(None),
            }) {
                Ok(Some((provider, path))) => {
                    let args = vec![
                        "source".to_string(),
                        "add".to_string(),
                        "--provider".to_string(),
                        provider.to_string(),
                        "--path".to_string(),
                        path,
                    ];
                    match run_statsai_capture_dynamic(args) {
                        Ok(_) => {
                            let _ = run_statsai_capture(&["scan"]);
                            let _ = proxy.send_event(UserEvent::Refresh);
                        }
                        Err(message) => alert("Could not add source", &message),
                    }
                }
                Ok(None) => {}
                Err(message) => alert("Could not choose folder", &message),
            }
            let _ = proxy.send_event(UserEvent::Refresh);
        });
    }

    fn spawn_menu_action(action: impl FnOnce() + Send + 'static) {
        std::thread::spawn(action);
    }

    fn statsai_command(binary: &Path) -> Command {
        let mut command = Command::new(binary);
        for key in ["STATSAI_API_URL", "STATSAI_WEB_URL", "STATSAI_SYNC_TOKEN"] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        command
    }

    fn fetch_snapshot() -> Result<AppSnapshot, String> {
        let store = Store::open(&default_store_path()).map_err(|err| err.to_string())?;
        snapshot::collect(&store).map_err(|err| err.to_string())
    }

    fn tray_icon_image() -> tray_icon::Icon {
        load_tray_icon_from_png(include_bytes!("../assets/icon.png")).unwrap_or_else(|err| {
            eprintln!("statsai menubar: could not load bundled icon: {err}");
            fallback_tray_icon()
        })
    }

    fn load_tray_icon_from_png(bytes: &[u8]) -> Result<tray_icon::Icon, String> {
        let decoder = png::Decoder::new(bytes);
        let mut reader = decoder
            .read_info()
            .map_err(|err| format!("decode tray icon png: {err}"))?;
        let mut rgba = vec![0; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut rgba)
            .map_err(|err| format!("read tray icon png: {err}"))?;
        if info.color_type != png::ColorType::Rgba {
            return Err(format!(
                "expected RGBA tray icon, got {:?}",
                info.color_type
            ));
        }
        tray_icon::Icon::from_rgba(rgba, info.width, info.height)
            .map_err(|err| format!("build tray icon: {err}"))
    }

    fn fallback_tray_icon() -> tray_icon::Icon {
        let size = 22u32;
        let mut rgba = vec![0u8; (size * size * 4) as usize];
        for y in 4..18 {
            for x in 5..17 {
                let i = ((y * size + x) * 4) as usize;
                rgba[i] = 24;
                rgba[i + 1] = 24;
                rgba[i + 2] = 24;
                rgba[i + 3] = 255;
            }
        }
        tray_icon::Icon::from_rgba(rgba, size, size).expect("build fallback tray icon")
    }

    fn open_url(url: &str) {
        let _ = Command::new("open").arg(url).status();
    }

    fn dashboard_url() -> String {
        let base =
            std::env::var("STATSAI_WEB_URL").unwrap_or_else(|_| "https://statsai.dev".to_string());
        format!("{}/dashboard/", base.trim_end_matches('/'))
    }

    fn help_url() -> String {
        let base =
            std::env::var("STATSAI_WEB_URL").unwrap_or_else(|_| "https://statsai.dev".to_string());
        format!("{}/help/setup", base.trim_end_matches('/'))
    }

    #[cfg(debug_assertions)]
    fn dev_info_line(snapshot: &AppSnapshot) -> String {
        if snapshot.using_local_dev {
            format!(
                "Local dev · {} · {}",
                shorten_url(&snapshot.backend_api),
                shorten_url(&snapshot.backend_web),
            )
        } else {
            "Developer build".to_string()
        }
    }

    #[cfg(debug_assertions)]
    fn shorten_url(url: &str) -> String {
        url.trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_string()
    }

    fn statsai_command_timeout(args: &[&str]) -> Duration {
        match args.first() {
            Some(&"scan") => Duration::from_secs(10 * 60),
            Some(&"sync") => Duration::from_secs(5 * 60),
            Some(&"auth") => Duration::from_secs(10 * 60),
            Some(&"service") => Duration::from_secs(2 * 60),
            _ => Duration::from_secs(5 * 60),
        }
    }

    fn run_statsai_capture(args: &[&str]) -> Result<String, String> {
        let binary = statsai_binary()?;
        let timeout = statsai_command_timeout(args);
        let mut child = statsai_command(&binary)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("failed to run {}: {err}", binary.display()))?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to capture statsai stdout".to_string())?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| "failed to capture statsai stderr".to_string())?;

        let stdout_handle = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let stderr_handle = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        });

        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if started.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = stdout_handle.join();
                        let _ = stderr_handle.join();
                        return Err(format!(
                            "statsai {} timed out after {} seconds",
                            args.join(" "),
                            timeout.as_secs()
                        ));
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_handle.join();
                    let _ = stderr_handle.join();
                    return Err(format!("failed to wait for {}: {err}", binary.display()));
                }
            }
        };

        let stdout = stdout_handle.join().unwrap_or_default();
        let stderr = stderr_handle.join().unwrap_or_default();

        let mut message = String::from_utf8_lossy(&stdout).trim().to_string();
        let stderr_message = String::from_utf8_lossy(&stderr).trim().to_string();
        if !stderr_message.is_empty() {
            if !message.is_empty() {
                message.push('\n');
            }
            message.push_str(&stderr_message);
        }
        if message.is_empty() {
            message = format!("statsai {} exited with {}", args.join(" "), status);
        }

        if status.success() {
            Ok(truncate_for_alert(&message, 1200))
        } else {
            Err(truncate_for_alert(&message, 1200))
        }
    }

    fn run_statsai_capture_dynamic(args: Vec<String>) -> Result<String, String> {
        let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
        run_statsai_capture(&borrowed)
    }

    fn choose_source_folder(display_name: &str) -> Result<Option<String>, String> {
        let prompt = format!("Choose the {display_name} data folder to track.");
        let script = format!(
            "try\nPOSIX path of (choose folder with prompt {})\non error number -128\nreturn \"\"\nend try",
            applescript_string(&prompt)
        );
        let output = Command::new("osascript")
            .args(["-e", &script])
            .output()
            .map_err(|err| format!("failed to open folder picker: {err}"))?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if message.is_empty() {
                return Err("folder picker failed".to_string());
            }
            return Err(message);
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            Ok(None)
        } else {
            Ok(Some(path))
        }
    }

    fn choose_source_provider() -> Result<Option<(&'static str, &'static str)>, String> {
        let script = r#"try
set providerNames to {"Codex", "Claude Code", "OpenCode", "Grok Build"}
set chosenProvider to choose from list providerNames with prompt "Which source do you want to add?" without multiple selections allowed
if chosenProvider is false then return ""
return item 1 of chosenProvider
on error number -128
return ""
end try"#;
        let output = Command::new("osascript")
            .args(["-e", script])
            .output()
            .map_err(|err| format!("failed to open provider picker: {err}"))?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if message.is_empty() {
                return Err("provider picker failed".to_string());
            }
            return Err(message);
        }
        match String::from_utf8_lossy(&output.stdout).trim() {
            "" => Ok(None),
            "Codex" => Ok(Some(("codex", "Codex"))),
            "Claude Code" => Ok(Some(("claude_code", "Claude Code"))),
            "OpenCode" => Ok(Some(("opencode", "OpenCode"))),
            "Grok Build" => Ok(Some(("grok_build", "Grok Build"))),
            other => Err(format!("unknown source provider: {other}")),
        }
    }

    fn truncate_for_alert(message: &str, max_chars: usize) -> String {
        if message.chars().count() <= max_chars {
            return message.to_string();
        }
        let mut end = max_chars;
        while end > 0 && !message.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &message[..end])
    }

    fn applescript_string(value: &str) -> String {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }

    fn alert(title: &str, message: &str) {
        let script = format!(
            "display alert {} message {} as informational",
            applescript_string(title),
            applescript_string(message),
        );
        let _ = Command::new("osascript").args(["-e", &script]).status();
    }

    fn statsai_binary() -> Result<PathBuf, String> {
        static BINARY: OnceLock<Result<PathBuf, String>> = OnceLock::new();
        BINARY.get_or_init(resolve_statsai_binary).clone()
    }

    fn resolve_statsai_binary() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("STATSAI_CLI") {
            let path = path.trim();
            if !path.is_empty() {
                return validate_cli_path(&PathBuf::from(path));
            }
        }

        let current_exe = std::env::current_exe().ok();

        if let Some(exe) = current_exe.as_deref() {
            if let Some(bundle_binary) = bundled_statsai_binary(exe) {
                return validate_cli_path(&bundle_binary);
            }

            if let Some(parent) = exe.parent() {
                let sibling = parent.join("statsai");
                if sibling.is_file() {
                    if let Ok(path) = validate_cli_path(&sibling) {
                        return Ok(path);
                    }
                }
            }
        }

        if let Ok(path) = which_statsai() {
            if current_exe
                .as_deref()
                .is_none_or(|exe| !same_executable(&path, exe))
            {
                return validate_cli_path(&path);
            }
        }

        Err("StatsAI could not find its background service.".to_string())
    }

    fn validate_cli_path(path: &Path) -> Result<PathBuf, String> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if file_name == "statsai-menubar" || file_name == "StatsAI" {
            return Err(format!(
                "{} is the menu bar app, not the CLI.",
                path.display()
            ));
        }

        let output = Command::new(path)
            .arg("--help")
            .output()
            .map_err(|err| format!("failed to execute {}: {err}", path.display()))?;
        let help = String::from_utf8_lossy(&output.stdout);
        if help.contains("snapshot") && help.contains("scan") {
            Ok(path.to_path_buf())
        } else {
            Err(format!(
                "{} does not look like the statsai CLI.",
                path.display()
            ))
        }
    }

    fn bundled_statsai_binary(exe: &Path) -> Option<PathBuf> {
        let macos_dir = exe.parent()?;
        let contents = macos_dir.parent()?;
        if contents.file_name().and_then(|name| name.to_str()) != Some("Contents") {
            return None;
        }
        if contents
            .parent()
            .and_then(|path| path.extension())
            .and_then(|ext| ext.to_str())
            != Some("app")
        {
            return None;
        }

        let cli = macos_dir.join("statsai");
        if cli.is_file() && !same_executable(&cli, exe) {
            return Some(cli);
        }
        None
    }

    fn which_statsai() -> Result<PathBuf, ()> {
        let output = Command::new("which")
            .arg("statsai")
            .output()
            .map_err(|_| ())?;
        if !output.status.success() {
            return Err(());
        }
        let path = String::from_utf8(output.stdout).map_err(|_| ())?;
        let path = path.trim();
        if path.is_empty() {
            return Err(());
        }
        Ok(PathBuf::from(path))
    }

    fn same_executable(left: &Path, right: &Path) -> bool {
        match (left.canonicalize(), right.canonicalize()) {
            (Ok(left), Ok(right)) => left == right,
            _ => left == right,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use statsai::snapshot::SnapshotSourceStatus;

        fn test_snapshot() -> AppSnapshot {
            AppSnapshot {
                logged_in: false,
                first_run: true,
                last_sync_at: None,
                sync_failures: 0,
                has_synced: false,
                pending_upload: false,
                pending_days: 0,
                unsynced_events: 0,
                tokens_today: 0,
                tokens_week: 0,
                sessions_week: 0,
                cost_week_cents: None,
                menu_summary: "StatsAI is tracking locally".to_string(),
                menu_stat_1: "Last 7 days · no requests yet".to_string(),
                menu_stat_2: "Today · no requests yet".to_string(),
                menu_stat_3: "Dashboard · not connected".to_string(),
                primary_action: PrimaryAction::Link,
                backend_api: "https://api.statsai.dev".to_string(),
                backend_web: "https://statsai.dev".to_string(),
                using_local_dev: false,
                background_tracking: SnapshotBackgroundStatus {
                    installed: false,
                    running: false,
                    label: "Tracking setup needed".to_string(),
                },
                sources: vec![SnapshotSourceStatus {
                    provider: "codex".to_string(),
                    display_name: "Codex".to_string(),
                    configured: false,
                    discovered: true,
                    enabled: true,
                    has_data: false,
                    event_count: 0,
                    token_count: 0,
                    estimated_cost_cents: None,
                    label: "Codex · 0 tokens · $0".to_string(),
                    status: "found".to_string(),
                }],
                last_scan_summary: Some("Last scan found no requests yet".to_string()),
                help_url: "https://statsai.dev/help/setup".to_string(),
                setup_url: "https://statsai.dev/dashboard/".to_string(),
                tooltip: "StatsAI".to_string(),
                menu_layout: "unlinked".to_string(),
                status_error: false,
            }
        }

        #[test]
        fn first_run_menu_exposes_setup_scan_and_link_actions() {
            let snapshot = test_snapshot();
            let presentation = menu_presentation(&snapshot, Activity::Idle);

            assert!(presentation.show_sources);
            assert!(presentation.show_start_tracking);
            assert!(presentation.show_scan_now);
            assert!(presentation.show_link_dashboard);
            assert!(!presentation.show_upload_now);
            assert!(presentation.open_dashboard_enabled);
        }

        #[test]
        fn pending_upload_menu_hides_login_and_shows_upload() {
            let mut snapshot = test_snapshot();
            snapshot.logged_in = true;
            snapshot.first_run = false;
            snapshot.pending_upload = true;
            snapshot.background_tracking.running = true;

            let presentation = menu_presentation(&snapshot, Activity::Idle);

            assert!(!presentation.show_start_tracking);
            assert!(presentation.show_scan_now);
            assert!(!presentation.show_link_dashboard);
            assert!(presentation.show_upload_now);
        }

        #[test]
        fn busy_and_error_states_disable_mutating_menu_actions() {
            let mut snapshot = test_snapshot();
            let scanning = menu_presentation(&snapshot, Activity::Scanning);
            assert!(!scanning.show_scan_now);
            assert!(!scanning.show_link_dashboard);
            assert!(!scanning.show_upload_now);

            snapshot.status_error = true;
            let error = menu_presentation(&snapshot, Activity::Idle);
            assert!(!error.show_scan_now);
            assert!(!error.show_start_tracking);
            assert!(!error.open_dashboard_enabled);
        }

        #[test]
        fn tracking_line_uses_snapshot_label() {
            let snapshot = test_snapshot();
            assert_eq!(
                tracking_line(&snapshot.background_tracking),
                "Tracking · Tracking setup needed"
            );
        }

        #[test]
        fn startup_scan_only_runs_for_setup_or_empty_states() {
            let first_run = test_snapshot();
            assert!(should_run_startup_scan(&first_run));

            let mut ready = test_snapshot();
            ready.first_run = false;
            ready.sessions_week = 10;
            ready.background_tracking.running = true;
            assert!(!should_run_startup_scan(&ready));
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    if std::env::args().len() > 1 {
        eprintln!("statsai-menubar does not accept command-line arguments.");
        eprintln!("Use the `statsai` binary for CLI commands.");
        std::process::exit(1);
    }

    if let Err(message) = macos::run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
