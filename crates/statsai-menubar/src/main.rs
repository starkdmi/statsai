#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("StatsAI menu bar app is only supported on macOS.");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::{Cell, RefCell};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::unix::io::AsRawFd;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;
    use std::thread;
    use std::time::{Duration, Instant};
    use statsai::snapshot::{AppSnapshot, PrimaryAction};
    use statsai::{default_store_path, snapshot};
    use statsai_store::Store;
    use tao::event::Event;
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
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
        Refresh,
        SetIdle,
        SnapshotReady(Result<AppSnapshot, String>),
    }

    struct MenuActions {
        primary: MenuItem,
        open_dashboard: MenuItem,
        quit: MenuItem,
    }

    struct MenuUi {
        summary: MenuItem,
        stat_1: MenuItem,
        stat_2: MenuItem,
        stat_3: MenuItem,
        actions: MenuActions,
        #[cfg(debug_assertions)]
        dev_info: MenuItem,
    }

    impl MenuUi {
        fn new() -> Self {
            Self {
                summary: status_item("Getting ready…"),
                stat_1: status_item("This week · …"),
                stat_2: status_item("Today · …"),
                stat_3: status_item("Dashboard · …"),
                actions: MenuActions {
                    primary: MenuItem::new("Link This Mac…", true, None),
                    open_dashboard: MenuItem::new("Open Dashboard…", true, None),
                    quit: MenuItem::new("Quit StatsAI", true, None),
                },
                #[cfg(debug_assertions)]
                dev_info: status_item(" "),
            }
        }

        fn apply_snapshot(
            &self,
            snapshot: &AppSnapshot,
            activity: Activity,
            include_primary: bool,
        ) {
            let summary = match activity {
                Activity::Scanning => "Reading your usage logs…",
                Activity::Uploading => "Uploading to your dashboard…",
                Activity::Idle => snapshot.menu_summary.as_str(),
            };
            self.summary.set_text(summary);
            self.stat_1.set_text(&snapshot.menu_stat_1);
            self.stat_2.set_text(&snapshot.menu_stat_2);
            self.stat_3.set_text(&snapshot.menu_stat_3);

            if include_primary {
                self.actions
                    .primary
                    .set_text(primary_label(snapshot.primary_action, snapshot.pending_upload));
                self.actions.primary.set_enabled(true);
            }

            self.actions.open_dashboard.set_enabled(!snapshot.status_error);

            #[cfg(debug_assertions)]
            self.dev_info.set_text(&dev_info_line(snapshot));
        }

        fn build_menu(&self, snapshot: &AppSnapshot, activity: Activity) -> Menu {
            let menu = Menu::new();
            let sep_actions = PredefinedMenuItem::separator();
            let sep_quit = PredefinedMenuItem::separator();
            #[cfg(debug_assertions)]
            let sep_dev = PredefinedMenuItem::separator();

            let _ = menu.append_items(&[
                &self.summary,
                &self.stat_1,
                &self.stat_2,
                &self.stat_3,
            ]);

            if snapshot.status_error {
                let _ = menu.append(&sep_quit);
                let _ = menu.append(&self.actions.quit);
                return menu;
            }

            let show_primary = activity == Activity::Idle
                && snapshot.primary_action != PrimaryAction::None;
            if show_primary {
                let _ = menu.append(&sep_actions);
                let _ = menu.append(&self.actions.primary);
            }

            let _ = menu.append(&sep_actions);
            let _ = menu.append(&self.actions.open_dashboard);
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
        if activity == Activity::Idle && snapshot.primary_action != PrimaryAction::None {
            return format!("primary:{:?}", snapshot.primary_action);
        }
        format!("standard:{activity:?}")
    }

    fn menu_shell_includes_primary(shell: &str) -> bool {
        shell.starts_with("primary:")
    }

    fn sync_menu_shell(
        menu_ui: &MenuUi,
        icon: Option<&TrayIcon>,
        snapshot: &AppSnapshot,
        activity: Activity,
        menu_open: &Cell<bool>,
        menu_shell: &RefCell<String>,
        pending_shell: &RefCell<Option<String>>,
    ) {
        let shell = menu_shell_key(snapshot, activity);
        let current_shell = menu_shell.borrow().clone();
        menu_ui.apply_snapshot(
            snapshot,
            activity,
            menu_shell_includes_primary(&current_shell),
        );

        if shell == current_shell {
            pending_shell.borrow_mut().take();
            return;
        }

        if menu_open.get() {
            *pending_shell.borrow_mut() = Some(shell);
            return;
        }

        if let Some(icon) = icon {
            let _ = icon.set_menu(Some(Box::new(menu_ui.build_menu(snapshot, activity))));
        }
        *menu_shell.borrow_mut() = shell;
        pending_shell.borrow_mut().take();
    }

    fn flush_pending_menu_shell(
        menu_ui: &MenuUi,
        icon: Option<&TrayIcon>,
        snapshot: Option<AppSnapshot>,
        activity: Activity,
        menu_shell: &RefCell<String>,
        pending_shell: &RefCell<Option<String>>,
    ) {
        let Some(shell) = pending_shell.borrow_mut().take() else {
            return;
        };
        if shell == *menu_shell.borrow() {
            return;
        }
        let Some(snapshot) = snapshot else {
            *pending_shell.borrow_mut() = Some(shell);
            return;
        };
        let include_primary = menu_shell_includes_primary(&shell);
        if let Some(icon) = icon {
            let _ = icon.set_menu(Some(Box::new(menu_ui.build_menu(&snapshot, activity))));
        }
        *menu_shell.borrow_mut() = shell;
        menu_ui.apply_snapshot(&snapshot, activity, include_primary);
    }

    fn primary_label(action: PrimaryAction, pending_upload: bool) -> &'static str {
        match action {
            PrimaryAction::Link => "Link This Mac…",
            PrimaryAction::UploadNow if pending_upload => "Upload Now",
            PrimaryAction::UploadNow => "Upload Now",
            PrimaryAction::None => " ",
        }
    }

    fn loading_snapshot() -> AppSnapshot {
        AppSnapshot {
            logged_in: false,
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
            menu_stat_1: "This week · …".to_string(),
            menu_stat_2: "Today · …".to_string(),
            menu_stat_3: "Dashboard · …".to_string(),
            primary_action: PrimaryAction::None,
            menu_layout: "loading".to_string(),
            status_error: false,
            backend_api: String::new(),
            backend_web: String::new(),
            using_local_dev: false,
            tooltip: "StatsAI".to_string(),
        }
    }

    fn unavailable_snapshot(reason: &str) -> AppSnapshot {
        eprintln!("statsai menubar status error: {reason}");
        AppSnapshot {
            logged_in: false,
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
        spawn_startup_scan(init_refresh_proxy.clone(), init_refresh_proxy.clone());

        let menu_ui = MenuUi::new();
        let initial = loading_snapshot();
        let initial_shell = menu_shell_key(&initial, Activity::Scanning);
        menu_ui.apply_snapshot(&initial, Activity::Scanning, false);
        let tray_menu = menu_ui.build_menu(&initial, Activity::Scanning);
        let activity = Cell::new(Activity::Scanning);
        let logged_in = Cell::new(false);
        let refresh_in_flight = Cell::new(false);
        let last_snapshot = RefCell::new(None::<AppSnapshot>);
        let menu_open = Cell::new(false);
        let menu_shell = RefCell::new(initial_shell);
        let pending_shell = RefCell::new(None::<String>);

        let mut tray_icon: Option<TrayIcon> = None;
        let mut next_wakeup = std::time::Instant::now()
            .checked_add(REFRESH_INTERVAL)
            .unwrap_or_else(std::time::Instant::now);

        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::WaitUntil(next_wakeup);

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
                            eprintln!(
                                "StatsAI could not create the menu bar icon: {err}"
                            );
                            std::process::exit(1);
                        }
                    }
                }
                Event::NewEvents(_) => {}
                Event::UserEvent(UserEvent::TrayIcon(event)) => {
                    match event {
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Down,
                            ..
                        } => {
                            menu_open.set(true);
                        }
                        TrayIconEvent::Leave { .. } => {
                            menu_open.set(false);
                            flush_pending_menu_shell(
                                &menu_ui,
                                tray_icon.as_ref(),
                                last_snapshot.borrow().clone(),
                                activity.get(),
                                &menu_shell,
                                &pending_shell,
                            );
                        }
                        _ => {}
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
                        let _ = proxy.send_event(UserEvent::SnapshotReady(snapshot));
                    });
                }
                Event::UserEvent(UserEvent::SnapshotReady(result)) => {
                    refresh_in_flight.set(false);
                    let snapshot = match result {
                        Ok(snapshot) => {
                            *last_snapshot.borrow_mut() = Some(snapshot.clone());
                            snapshot
                        }
                        Err(reason) => last_snapshot
                            .borrow()
                            .clone()
                            .unwrap_or_else(|| unavailable_snapshot(&reason)),
                    };
                    logged_in.set(snapshot.logged_in);
                    sync_menu_shell(
                        &menu_ui,
                        tray_icon.as_ref(),
                        &snapshot,
                        activity.get(),
                        &menu_open,
                        &menu_shell,
                        &pending_shell,
                    );
                    if let Some(icon) = tray_icon.as_ref() {
                        let _ = icon.set_tooltip(Some(snapshot.tooltip.as_str()));
                    }
                }
                Event::UserEvent(UserEvent::Menu(menu_event)) => {
                    if menu_event.id == menu_ui.actions.quit.id() {
                        tray_icon.take();
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    if menu_event.id == menu_ui.actions.open_dashboard.id() {
                        open_url(&dashboard_url());
                        return;
                    }

                    if menu_event.id == menu_ui.actions.primary.id() {
                        if logged_in.get() {
                            let proxy = init_refresh_proxy.clone();
                            activity.set(Activity::Uploading);
                            menu_ui.summary.set_text("Uploading to your dashboard…");
                            menu_ui.actions.primary.set_enabled(false);
                            spawn_menu_action(move || {
                                let finish = || {
                                    let _ = proxy.send_event(UserEvent::SetIdle);
                                };
                                match run_statsai_capture(&["scan"]) {
                                Ok(_) => match run_statsai_capture(&[
                                    "sync",
                                    "--sink",
                                    "http",
                                    "--since-last",
                                ]) {
                                        Ok(_) => {
                                            finish();
                                        }
                                        Err(message) => {
                                            alert("Upload failed", &message);
                                            finish();
                                        }
                                    },
                                    Err(message) => {
                                        alert("Could not read your usage logs", &message);
                                        finish();
                                    }
                                }
                            });
                        } else {
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
                    }
                    menu_open.set(false);
                    flush_pending_menu_shell(
                        &menu_ui,
                        tray_icon.as_ref(),
                        last_snapshot.borrow().clone(),
                        activity.get(),
                        &menu_shell,
                        &pending_shell,
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

    fn spawn_startup_scan(
        refresh_proxy: tao::event_loop::EventLoopProxy<UserEvent>,
        idle_proxy: tao::event_loop::EventLoopProxy<UserEvent>,
    ) {
        spawn_menu_action(move || {
            match run_statsai_capture(&["scan"]) {
                Ok(_) => {
                    let _ = idle_proxy.send_event(UserEvent::SetIdle);
                    let _ = refresh_proxy.send_event(UserEvent::Refresh);
                }
                Err(message) => {
                    eprintln!("statsai menubar startup scan failed: {message}");
                    let _ = idle_proxy.send_event(UserEvent::SetIdle);
                }
            }
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
        load_tray_icon_from_png(include_bytes!("../assets/icon.png"))
            .unwrap_or_else(|err| {
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
        let base = std::env::var("STATSAI_WEB_URL")
            .unwrap_or_else(|_| "https://statsai.dev".to_string());
        format!("{}/dashboard/", base.trim_end_matches('/'))
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

        let mut stdout = child.stdout.take().ok_or_else(|| {
            "failed to capture statsai stdout".to_string()
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| {
            "failed to capture statsai stderr".to_string()
        })?;

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
        BINARY
            .get_or_init(resolve_statsai_binary)
            .clone()
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